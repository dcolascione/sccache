// Copyright 2026 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug)]
pub(crate) struct FileStat {
    pub(crate) is_file: bool,
    pub(crate) size: u64,
    pub(crate) accessed: Option<SystemTime>,
    pub(crate) modified: Option<SystemTime>,
}

impl FileStat {
    fn from_metadata(metadata: fs::Metadata) -> Self {
        Self {
            is_file: metadata.is_file(),
            size: metadata.len(),
            accessed: metadata.accessed().ok(),
            modified: metadata.modified().ok(),
        }
    }
}

pub(crate) const BULK_STAT_BATCH_SIZE: usize = 16_384;

/// Stat all paths, using io_uring on supported Linux targets when available.
///
/// A ring failure permanently selects the synchronous fallback for this thread.
/// Per-path stat failures are returned normally and do not disable io_uring.
pub(crate) fn bulk_stat(paths: &[PathBuf]) -> Vec<io::Result<FileStat>> {
    if paths.is_empty() {
        return Vec::new();
    }

    #[cfg(all(
        target_os = "linux",
        any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "powerpc64"
        )
    ))]
    if let Some(results) = linux::try_bulk_stat(paths) {
        return results;
    }

    paths
        .iter()
        .map(|path| fs::symlink_metadata(path).map(FileStat::from_metadata))
        .collect()
}

#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64",
        target_arch = "powerpc64"
    )
))]
mod linux {
    use super::{BULK_STAT_BATCH_SIZE, FileStat};
    use io_uring::{IoUring, opcode, types};
    use log::{debug, error, trace};
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::io;
    use std::mem;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, SystemTime};

    const MAX_RING_BATCH_SIZE: u32 = BULK_STAT_BATCH_SIZE as u32;
    const STATX_MASK: u32 =
        libc::STATX_TYPE | libc::STATX_SIZE | libc::STATX_ATIME | libc::STATX_MTIME;

    struct RingState {
        ring: Option<IoUring>,
        failed: bool,
    }

    thread_local! {
        static RING: RefCell<RingState> = const {
            RefCell::new(RingState {
                ring: None,
                failed: false,
            })
        };
    }

    pub(super) fn try_bulk_stat(paths: &[PathBuf]) -> Option<Vec<io::Result<FileStat>>> {
        RING.with(|state| {
            let ring = {
                let mut state = state.borrow_mut();
                if state.failed {
                    return None;
                }
                // Leave the state failed while the ring is in use so a panic
                // cannot leave it eligible for reuse.
                state.failed = true;
                match state.ring.take() {
                    Some(ring) => ring,
                    None => match new_ring() {
                        Ok(ring) => ring,
                        Err(err) => {
                            debug!("Failed to initialize directory-cache io_uring: {err}");
                            return None;
                        }
                    },
                }
            };

            match stat_all(ring, paths) {
                Ok((ring, results)) => {
                    let mut state = state.borrow_mut();
                    state.ring = Some(ring);
                    state.failed = false;
                    Some(results)
                }
                Err(err) => {
                    debug!("Directory-cache io_uring failed; using regular stat: {err}");
                    None
                }
            }
        })
    }

    fn new_ring() -> io::Result<IoUring> {
        IoUring::builder()
            .setup_single_issuer()
            .setup_coop_taskrun()
            .build(MAX_RING_BATCH_SIZE)
    }

    fn stat_all(
        mut ring: IoUring,
        paths: &[PathBuf],
    ) -> io::Result<(IoUring, Vec<io::Result<FileStat>>)> {
        let mut all_results = Vec::with_capacity(paths.len());

        for paths in paths.chunks(MAX_RING_BATCH_SIZE as usize) {
            let mut batch = StatBatch::new(ring, paths)?;
            let results = batch.run()?;
            ring = batch
                .ring
                .take()
                .expect("successful stat batch retains its ring");
            all_results.extend(results);
        }

        trace!(
            "Used io_uring for {} directory-cache stat calls",
            paths.len()
        );
        Ok((ring, all_results))
    }

    struct StatBatch {
        // Drop cancels submitted work before the buffers below are released.
        ring: Option<IoUring>,
        pathnames: Vec<CString>,
        statx_bufs: Vec<libc::statx>,
        submitted: bool,
    }

    impl StatBatch {
        fn new(ring: IoUring, paths: &[PathBuf]) -> io::Result<Self> {
            let pathnames = paths
                .iter()
                .map(|path| {
                    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("path contains NUL: {}", path.display()),
                        )
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            let statx_bufs = std::iter::repeat_with(|| unsafe { mem::zeroed() })
                .take(paths.len())
                .collect();

            Ok(Self {
                ring: Some(ring),
                pathnames,
                statx_bufs,
                submitted: false,
            })
        }

        fn run(&mut self) -> io::Result<Vec<io::Result<FileStat>>> {
            {
                let ring = self.ring.as_mut().expect("stat batch has a ring");
                let mut submission = ring.submission();
                for (index, (pathname, statx_buf)) in self
                    .pathnames
                    .iter()
                    .zip(self.statx_bufs.iter_mut())
                    .enumerate()
                {
                    let entry = opcode::Statx::new(
                        types::Fd(libc::AT_FDCWD),
                        pathname.as_ptr(),
                        (statx_buf as *mut libc::statx).cast::<types::statx>(),
                    )
                    .flags(libc::AT_SYMLINK_NOFOLLOW)
                    .mask(STATX_MASK)
                    .build()
                    .user_data(index as u64);
                    unsafe {
                        submission.push(&entry).map_err(|_| {
                            io::Error::other("io_uring submission queue unexpectedly full")
                        })?;
                    }
                }
            }

            // io_uring_enter may submit work even when it returns an error.
            self.submitted = true;
            self.ring
                .as_ref()
                .expect("stat batch has a ring")
                .submit_and_wait(self.pathnames.len())?;

            let mut results = std::iter::repeat_with(|| None)
                .take(self.pathnames.len())
                .collect::<Vec<_>>();
            {
                let mut completions = self
                    .ring
                    .as_mut()
                    .expect("stat batch has a ring")
                    .completion();
                for _ in 0..self.pathnames.len() {
                    let completion = completions.next().ok_or_else(|| {
                        io::Error::other("io_uring returned too few stat completions")
                    })?;
                    let index = usize::try_from(completion.user_data())
                        .ok()
                        .filter(|index| *index < results.len())
                        .ok_or_else(|| {
                            io::Error::other("io_uring returned an invalid stat completion")
                        })?;
                    results[index] = Some(if completion.result() < 0 {
                        Err(io::Error::from_raw_os_error(-completion.result()))
                    } else {
                        Ok(file_stat(&self.statx_bufs[index]))
                    });
                }
            }

            results
                .into_iter()
                .map(|result| {
                    result.ok_or_else(|| io::Error::other("missing io_uring stat completion"))
                })
                .collect()
        }
    }

    impl Drop for StatBatch {
        fn drop(&mut self) {
            let Some(ring) = self.ring.as_ref().filter(|_| self.submitted) else {
                return;
            };

            // Closing a ring cancels asynchronously. Synchronous cancellation
            // must finish before these user buffers can be released.
            loop {
                match ring
                    .submitter()
                    .register_sync_cancel(None, types::CancelBuilder::any())
                {
                    Ok(()) => return,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return,
                    Err(err) => {
                        error!(
                            "Failed to cancel directory-cache io_uring requests; retrying: {err}"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
    }

    fn file_stat(stat: &libc::statx) -> FileStat {
        FileStat {
            is_file: stat.stx_mask & libc::STATX_TYPE != 0
                && libc::mode_t::from(stat.stx_mode) & libc::S_IFMT == libc::S_IFREG,
            size: stat.stx_size,
            accessed: (stat.stx_mask & libc::STATX_ATIME != 0)
                .then(|| system_time(stat.stx_atime))
                .flatten(),
            modified: (stat.stx_mask & libc::STATX_MTIME != 0)
                .then(|| system_time(stat.stx_mtime))
                .flatten(),
        }
    }

    fn system_time(timestamp: libc::statx_timestamp) -> Option<SystemTime> {
        let nanos = Duration::from_nanos(u64::from(timestamp.tv_nsec));
        if timestamp.tv_sec >= 0 {
            SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(timestamp.tv_sec as u64))?
                .checked_add(nanos)
        } else {
            let before_epoch =
                Duration::from_secs(timestamp.tv_sec.unsigned_abs()).checked_sub(nanos)?;
            SystemTime::UNIX_EPOCH.checked_sub(before_epoch)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::bulk_stat;
        use filetime::{FileTime, set_file_times};
        use std::fs;
        use std::time::{Duration, SystemTime};

        #[test]
        fn bulk_stat_reports_file_size_and_times() {
            let tempdir = tempfile::tempdir().unwrap();
            let path = tempdir.path().join("object");
            fs::write(&path, b"object bytes").unwrap();
            let accessed = FileTime::from_unix_time(123, 456);
            let modified = FileTime::from_unix_time(789, 123);
            set_file_times(&path, accessed, modified).unwrap();

            let stats = bulk_stat(std::slice::from_ref(&path));
            let stat = stats[0].as_ref().unwrap();

            assert_eq!(stats.len(), 1);
            assert!(stat.is_file);
            assert_eq!(stat.size, 12);
            assert_eq!(
                stat.accessed,
                Some(SystemTime::UNIX_EPOCH + Duration::new(123, 456))
            );
            assert_eq!(
                stat.modified,
                Some(SystemTime::UNIX_EPOCH + Duration::new(789, 123))
            );
        }

        #[test]
        fn bulk_stat_reports_missing_paths_individually() {
            let tempdir = tempfile::tempdir().unwrap();
            let existing = tempdir.path().join("existing");
            let missing = tempdir.path().join("missing");
            fs::write(&existing, b"x").unwrap();

            let stats = bulk_stat(&[existing, missing]);

            assert!(stats[0].is_ok());
            assert_eq!(
                stats[1].as_ref().unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
    }
}
