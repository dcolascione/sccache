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

use super::directory_io::{
    CacheWriteObject, CacheWriteObjectSource, DirectoryCacheRead, StorageFileObjectSource,
    put_zip_object,
};
use super::utils::{get_file_mode, set_file_mode};
use crate::errors::*;
use fs_err as fs;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Cache object sourced by a file.
#[derive(Clone)]
pub struct FileObjectSource {
    /// Identifier for this object. Should be unique within a compilation unit.
    /// Note that a compilation unit is a single source file in C/C++ and a crate in Rust.
    pub key: String,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Whether the file must be present on disk and is essential for the compilation.
    pub optional: bool,
}

/// Result of a cache lookup.
pub enum Cache {
    /// Result was found in cache.
    Hit(CacheRead),
    /// Result was not found in cache.
    Miss,
    /// Do not cache the results of the compilation.
    None,
    /// Cache entry should be ignored, force compilation.
    Recache,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Cache::Hit(_) => write!(f, "Cache::Hit(...)"),
            Cache::Miss => write!(f, "Cache::Miss"),
            Cache::None => write!(f, "Cache::None"),
            Cache::Recache => write!(f, "Cache::Recache"),
        }
    }
}

/// CacheMode is used to represent which mode we are using.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheMode {
    /// Only read cache from storage.
    ReadOnly,
    /// Full support of cache storage: read and write.
    ReadWrite,
}

/// Trait objects can't be bounded by more than one non-builtin trait.
pub trait ReadSeek: Read + Seek + Send {}

impl<T: Read + Seek + Send> ReadSeek for T {}

/// Data stored in the compiler cache.
pub struct CacheRead {
    source: CacheReadSource,
}

enum CacheReadSource {
    Zip(ZipArchive<Box<dyn ReadSeek>>),
    Directory(DirectoryCacheRead),
}

/// Represents a failure to decompress stored object data.
#[derive(Debug)]
pub struct DecompressionFailure;

impl std::fmt::Display for DecompressionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decompress content")
    }
}

impl std::error::Error for DecompressionFailure {}

impl CacheRead {
    /// Create a cache entry from a legacy serialized ZIP `reader`.
    pub fn from<R>(reader: R) -> Result<CacheRead>
    where
        R: ReadSeek + 'static,
    {
        let z = ZipArchive::new(Box::new(reader) as Box<dyn ReadSeek>)
            .context("Failed to parse cache entry")?;
        Ok(CacheRead {
            source: CacheReadSource::Zip(z),
        })
    }

    /// Create a cache entry from a directory-backed entry.
    pub(crate) fn from_directory_with_max_size(
        root: PathBuf,
        max_size: Option<u64>,
    ) -> Result<CacheRead> {
        Ok(CacheRead {
            source: CacheReadSource::Directory(DirectoryCacheRead::from_path(root, max_size)?),
        })
    }

    /// Get an object from this cache entry at `name` and write it to `to`.
    /// If the file has stored permissions, return them.
    pub fn get_object<T>(&mut self, name: &str, to: &mut T) -> Result<Option<u32>>
    where
        T: Write,
    {
        match &mut self.source {
            CacheReadSource::Zip(zip) => get_zip_object(zip, name, to),
            CacheReadSource::Directory(directory) => directory.get_object(name, to),
        }
    }

    fn get_object_to_temp(&mut self, name: &str, tmp: &mut NamedTempFile) -> Result<Option<u32>> {
        match &mut self.source {
            CacheReadSource::Zip(zip) => get_zip_object(zip, name, tmp),
            CacheReadSource::Directory(directory) => {
                directory.copy_object_to_path(name, tmp.path())
            }
        }
    }

    /// Get the stdout from this cache entry, if it exists.
    pub fn get_stdout(&mut self) -> Vec<u8> {
        self.get_bytes("stdout")
    }

    /// Get the stderr from this cache entry, if it exists.
    pub fn get_stderr(&mut self) -> Vec<u8> {
        self.get_bytes("stderr")
    }

    fn get_bytes(&mut self, name: &str) -> Vec<u8> {
        match &mut self.source {
            CacheReadSource::Zip(_) => {
                let mut bytes = Vec::new();
                drop(self.get_object(name, &mut bytes));
                bytes
            }
            CacheReadSource::Directory(directory) => directory.get_bytes(name),
        }
    }

    pub async fn extract_objects<T>(
        mut self,
        objects: T,
        pool: &tokio::runtime::Handle,
    ) -> Result<()>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
            for FileObjectSource {
                key,
                path,
                optional,
            } in objects
            {
                if is_path_null(&path) {
                    // For unix, this is just a fast path to discard such outputs,
                    // so it is not an issue if `is_path_null` has false-negatives.
                    // But for Windows, since `NUL` looks like a relative path, the
                    // temporary file creation logic would happily succeed, creating
                    // a temp file in the CWD, but then the subsequent `persist`
                    // would fail with `ERROR_ALREADY_EXISTS`, since `NUL` always
                    // exists and cannot be `replaced`, so we really need to
                    // short-circuit more of such cases on Windows.
                    debug!("Skipping output to {}", path.display());
                    continue;
                }
                let dir = match path.parent() {
                    Some(d) => d,
                    None => bail!("Output file without a parent directory!"),
                };
                // Write the cache entry to a tempfile and then atomically
                // move it to its final location so that other rustc invocations
                // happening in parallel don't see a partially-written file.
                match (NamedTempFile::new_in(dir), optional) {
                    (Ok(mut tmp), _) => match (self.get_object_to_temp(&key, &mut tmp), optional) {
                        (Ok(mode), _) => {
                            tmp.persist(&path)?;
                            if let Some(mode) = mode {
                                set_file_mode(path.as_path(), mode)?;
                            }
                        }
                        (Err(e), false) => return Err(e),
                        // skip if no object found and it's optional
                        (Err(_), true) => continue,
                    },
                    (Err(e), false) => {
                        // Fall back to writing directly to the final location
                        warn!("Failed to create temp file on the same file system: {e}");
                        let mut f = std::fs::File::create(&path)?;
                        // `optional` is false in this branch, so do not ignore errors
                        let mode = self.get_object(&key, &mut f)?;
                        if let Some(mode) = mode {
                            if let Err(e) = set_file_mode(path.as_path(), mode) {
                                // Here we ignore errors from setting file mode because
                                // if we could not create a temp file in the same directory,
                                // we probably can't set the mode either (e.g. /dev/stuff)
                                warn!("Failed to reset file mode: {e}");
                            }
                        }
                    }
                    // skip if no object found and it's optional
                    (Err(_), true) => continue,
                }
            }
            Ok(())
        })
        .await?
    }
}

fn get_zip_object<T>(
    zip: &mut ZipArchive<Box<dyn ReadSeek>>,
    name: &str,
    to: &mut T,
) -> Result<Option<u32>>
where
    T: Write,
{
    let file = zip.by_name(name).or(Err(DecompressionFailure))?;
    if file.compression() != CompressionMethod::Stored {
        bail!(DecompressionFailure);
    }
    let mode = file.unix_mode();
    zstd::stream::copy_decode(file, to).or(Err(DecompressionFailure))?;
    Ok(mode)
}

#[cfg(unix)]
fn is_path_null(path: &Path) -> bool {
    path == Path::new("/dev/null")
}

#[cfg(windows)]
fn is_path_null(path: &Path) -> bool {
    // For Windows, it appears that `NUL` with whatever extension is also a blackhole
    // (at least for `CreateFileX`), so it does not suffice to check for an exact match.
    // Also note that gcc, cl.exe, et al. append a correct extension automatically even
    // if the user asks for output to `NUL`.
    let Some(stem) = path.file_stem() else {
        return false;
    };
    stem.eq_ignore_ascii_case("NUL")
}

/// Data to be stored in the compiler cache.
pub struct CacheWrite {
    objects: Vec<CacheWriteObject>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CacheWrite {
    /// Create a new, empty cache entry.
    pub fn new() -> CacheWrite {
        CacheWrite {
            objects: Vec::new(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    /// Create a new cache entry populated with the contents of `objects`.
    pub async fn from_objects<T>(objects: T, pool: &tokio::runtime::Handle) -> Result<CacheWrite>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
            let mut entry = CacheWrite::new();
            for FileObjectSource {
                key,
                path,
                optional,
            } in objects
            {
                let f = fs::File::open(&path)
                    .with_context(|| format!("failed to open file `{:?}`", path));
                match (f, optional) {
                    (Ok(f), _) => {
                        let mode = get_file_mode(&f)?;
                        entry.put_file_object_from_path(key, path, f, mode);
                    }
                    (Err(e), false) => return Err(e),
                    (Err(_), true) => continue,
                }
            }
            Ok(entry)
        })
        .await?
    }

    pub(crate) fn put_file_object(&mut self, name: String, file: fs::File, mode: Option<u32>) {
        self.objects.push(CacheWriteObject {
            key: name,
            source: CacheWriteObjectSource::File { file, path: None },
            mode,
        });
    }

    fn put_file_object_from_path(
        &mut self,
        name: String,
        path: PathBuf,
        file: fs::File,
        mode: Option<u32>,
    ) {
        self.objects.push(CacheWriteObject {
            key: name,
            source: CacheWriteObjectSource::File {
                file,
                path: Some(path),
            },
            mode,
        });
    }

    /// Add an object containing the contents of `from` to this cache entry at `name`.
    /// If `mode` is `Some`, store the file entry with that mode.
    pub fn put_object<T>(&mut self, name: &str, from: &mut T, mode: Option<u32>) -> Result<()>
    where
        T: Read,
    {
        let mut bytes = Vec::new();
        from.read_to_end(&mut bytes)?;
        self.objects.push(CacheWriteObject {
            key: name.to_owned(),
            source: CacheWriteObjectSource::Bytes(bytes),
            mode,
        });
        Ok(())
    }

    pub fn put_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        self.stdout = bytes.to_vec();
        Ok(())
    }

    pub fn put_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        self.stderr = bytes.to_vec();
        Ok(())
    }

    #[cfg(test)]
    fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut cursor = Cursor::new(bytes);
        self.put_object(name, &mut cursor, None)
    }

    #[cfg(test)]
    pub(crate) fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            objects: self
                .objects
                .iter()
                .map(CacheWriteObject::try_clone)
                .collect::<Result<Vec<_>>>()?,
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
        })
    }

    pub(crate) fn file_object_sources(&self) -> Result<Option<Vec<StorageFileObjectSource>>> {
        self.objects
            .iter()
            .map(CacheWriteObject::file_source)
            .collect()
    }
    pub(crate) fn objects(&self) -> &[CacheWriteObject] {
        &self.objects
    }

    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Finish writing data to the cache entry writer, and return legacy ZIP bytes.
    pub fn finish(self) -> Result<Vec<u8>> {
        let mut zip = ZipWriter::new(Cursor::new(vec![]));
        for object in self.objects {
            object.write_to_zip(&mut zip)?;
        }
        if !self.stdout.is_empty() {
            put_zip_object(&mut zip, "stdout", &mut Cursor::new(self.stdout), None)?;
        }
        if !self.stderr.is_empty() {
            put_zip_object(&mut zip, "stderr", &mut Cursor::new(self.stderr), None)?;
        }
        let cur = zip.finish().context("Failed to finish cache entry zip")?;
        Ok(cur.into_inner())
    }

    /// Finish writing legacy ZIP bytes on a blocking worker thread.
    pub(crate) async fn finish_blocking(self) -> Result<Vec<u8>> {
        tokio::task::spawn_blocking(move || self.finish())
            .await
            .context("spawn_blocking panicked")?
    }
}

impl Default for CacheWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_devnull_works() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let cache_data = CacheWrite::new();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: PathBuf::from("/dev/null"),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(result.is_ok(), "Extracting to /dev/null should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_dev_fd_something() {
        // Open a pipe, write to `/dev/fd/{fd}` and check the other end that the correct data was written.
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncReadExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let pool = runtime.handle();
        let mut cache_data = CacheWrite::new();
        let data = b"test data";
        cache_data.put_bytes("test_key", data).unwrap();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();
        runtime.block_on(async {
            let (sender, mut receiver) = tokio::net::unix::pipe::pipe().unwrap();
            let sender_fd = sender.into_blocking_fd().unwrap();
            let raw_fd = sender_fd.as_raw_fd();
            let fd_path = PathBuf::from(format!("/dev/fd/{raw_fd}"));
            let objects = vec![FileObjectSource {
                key: "test_key".to_string(),
                path: fd_path.clone(),
                optional: false,
            }];
            // On FreeBSD, `/dev/fd/{fd}` does not always exist (i.e. without mounting `fdescfs`), so we skip this test if we get `ENOENT`.
            if ! fd_path.exists() {
                info!("Skipping test_extract_object_to_dev_fd_something because /dev/fd/{raw_fd} does not exist");
                return;
            }
            let result = cache_read.extract_objects(objects, pool).await;
            assert!(
                result.is_ok(),
                "Extracting to /dev/fd/{raw_fd} should succeed"
            );
            let mut buf = vec![0; data.len()];
            let n = receiver.read_exact(&mut buf).await.unwrap();
            assert_eq!(n, data.len(), "Read the correct number of bytes");
            assert_eq!(buf, data, "Read the correct data from /dev/fd/{raw_fd}");
        });
    }

    #[test]
    fn test_extract_object_to_non_writable_path() {
        // See `test_extract_object_to_dev_fd_something`: we still cannot cover all platforms by the other tests. Here we test a more portable case of creating a file and making its parent directory non-writable, in which case we should still be able to extract the object successfully.

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let mut cache_data = CacheWrite::new();
        cache_data.put_bytes("test_key", b"real_test_data").unwrap();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let tmpdir = tempfile::tempdir().unwrap();
        let target_path = tmpdir.path().join("test_file");
        std::fs::write(&target_path, b"test").unwrap();
        // The current Rust fs permissions API is kind of awkward...
        let mut perm = tmpdir.path().metadata().unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(tmpdir.path(), perm.clone()).unwrap();
        // Note that this doesn't guarantee that the a new file cannot be created anymore.
        // For example, as documented in `std::fs::Permissions::set_readonly`, the
        // `FILE_ATTRIBUTE_READONLY` attribute on Windows is entirely ignored for directories.
        // std::fs::File::create(tmpdir.path().join("another_file")).unwrap_err();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: target_path.clone(),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(
            result.is_ok(),
            "Extracting to the target path should succeed"
        );
        // Test the content; make sure the old content is overwritten
        let content = std::fs::read(&target_path).unwrap();
        assert_eq!(
            content, b"real_test_data",
            "Extracted content should be correct"
        );

        // `tempfile` needs us to reset permissions for cleanup to work
        #[allow(
            clippy::permissions_set_readonly_false,
            reason = "The affected directory is immediately deleted with no security implications"
        )]
        perm.set_readonly(false);
        std::fs::set_permissions(tmpdir.path(), perm).unwrap();
        tmpdir.close().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn test_extract_object_to_nul_works() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let cache_data = CacheWrite::new();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: PathBuf::from("NUL"),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(result.is_ok(), "Extracting to NUL should succeed");
    }
}
