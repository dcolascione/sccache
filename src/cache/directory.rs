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

use crate::cache::bulk_stat::{BULK_STAT_BATCH_SIZE, bulk_stat};
use crate::cache::directory_io::{
    DIRECTORY_CACHE_DIR, DIRECTORY_CACHE_MANIFEST_FILE, DIRECTORY_CACHE_MANIFEST_VERSION,
    DIRECTORY_CACHE_OBJECTS_DIR, DIRECTORY_CACHE_STDERR_FILE, DIRECTORY_CACHE_STDIO_MAX_BYTES,
    DIRECTORY_CACHE_STDOUT_FILE, DirectoryCacheManifest, DirectoryCacheObject,
    directory_cache_stdio_read_limit, open_directory_cache_object_file,
    read_directory_cache_manifest, read_directory_cache_optional_file,
    serialize_directory_cache_manifest,
};
use crate::cache::{Cache, CacheMode, CacheRead, CacheWrite, DecompressionFailure, Storage};
use crate::compiler::PreprocessorCacheEntry;
use crate::config::{DirectoryCacheLinkConfig, PreprocessorCacheModeConfig};
use crate::errors::*;
use crate::lru_disk_cache::{LruDiskCache, ReadSeek};
use async_trait::async_trait;
use bytes::Bytes;
use filetime::{FileTime, set_file_times};
use fs_err as fs;
use fs2::FileExt;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tempfile::Builder as TempBuilder;
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipArchive};

use super::utils::{normalize_key, set_file_mode};

const CACHE_LOCK_FILE: &str = ".sccache.lock";
const TEMP_ENTRY_PREFIX: &str = ".sccachetmp";
const TEMP_ENTRY_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// A local cache that stores each compile cache entry as a directory.
///
/// Each output object is a regular file under `objects/`, so cache hits can
/// link or copy the object directly into the requested compiler output path.
pub struct DirectoryCache {
    root: PathBuf,
    max_size: u64,
    pool: tokio::runtime::Handle,
    preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
    rw_mode: CacheMode,
    link: DirectoryCacheLinkConfig,
    basedirs: Vec<Vec<u8>>,
}

impl DirectoryCache {
    /// Create a directory cache in the reserved namespace beneath `cache_root`.
    pub fn new<T: AsRef<OsStr>>(
        cache_root: T,
        max_size: u64,
        pool: &tokio::runtime::Handle,
        preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
        rw_mode: CacheMode,
        link: DirectoryCacheLinkConfig,
        basedirs: Vec<Vec<u8>>,
    ) -> DirectoryCache {
        Self::new_at_path(
            Path::new(cache_root.as_ref()).join(DIRECTORY_CACHE_DIR),
            max_size,
            pool,
            preprocessor_cache_mode_config,
            rw_mode,
            link,
            basedirs,
        )
    }

    fn new_at_path(
        root: PathBuf,
        max_size: u64,
        pool: &tokio::runtime::Handle,
        preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
        rw_mode: CacheMode,
        link: DirectoryCacheLinkConfig,
        basedirs: Vec<Vec<u8>>,
    ) -> DirectoryCache {
        DirectoryCache {
            root,
            max_size,
            pool: pool.clone(),
            preprocessor_cache_mode_config,
            rw_mode,
            link,
            basedirs,
        }
    }
}

#[derive(Clone, Copy)]
enum CacheLockMode {
    Shared,
    Exclusive,
}

struct CacheLock {
    file: std::fs::File,
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn lock_cache_file(file: std::fs::File, path: &Path, mode: CacheLockMode) -> Result<CacheLock> {
    match mode {
        CacheLockMode::Shared => FileExt::lock_shared(&file),
        CacheLockMode::Exclusive => FileExt::lock_exclusive(&file),
    }
    .with_context(|| format!("Failed to lock directory cache {}", path.display()))?;
    Ok(CacheLock { file })
}

fn acquire_shared_cache_lock(root: &Path, create_if_missing: bool) -> Result<Option<CacheLock>> {
    if !ensure_plain_directory(root, false)? {
        return Ok(None);
    }

    let path = root.join(CACHE_LOCK_FILE);
    let file = match open_cache_lock_file(&path, false) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound && !create_if_missing => {
            // Read-only legacy caches may predate the lock file. They cannot
            // participate in write coordination, but cache reads still pin all
            // source files before returning.
            return Ok(None);
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => open_cache_lock_file(&path, true)
            .with_context(|| format!("Failed to create directory cache lock {}", path.display()))?,
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to open directory cache lock {}", path.display())
            });
        }
    };

    Ok(Some(lock_cache_file(file, &path, CacheLockMode::Shared)?))
}

fn acquire_exclusive_cache_lock(root: &Path) -> Result<CacheLock> {
    ensure_plain_directory(root, true)?;
    let path = root.join(CACHE_LOCK_FILE);
    let file = open_cache_lock_file(&path, true)
        .with_context(|| format!("Failed to open directory cache lock {}", path.display()))?;
    lock_cache_file(file, &path, CacheLockMode::Exclusive)
}

#[cfg(unix)]
fn open_cache_lock_file(path: &Path, create: bool) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if create {
        options.write(true).create(true);
    }
    let file = options.custom_flags(libc::O_NOFOLLOW).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory cache lock is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_cache_lock_file(path: &Path, create: bool) -> io::Result<std::fs::File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory cache lock is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound && create => {}
        Err(err) => return Err(err),
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if create {
        options.write(true).create(true);
    }
    let file = options.open(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory cache lock is not a regular file",
        ));
    }
    Ok(file)
}

fn make_key_path(key: &str) -> Result<PathBuf> {
    if key.len() < 3
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("Invalid cache key {:?}", key);
    }
    Ok(Path::new(&key[0..1])
        .join(&key[1..2])
        .join(&key[2..3])
        .join(key))
}

fn ensure_plain_directory(path: &Path, create: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "Directory cache path is not a plain directory: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound && create => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "Directory cache path is not a plain directory: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn ensure_relative_directory(
    root: &Path,
    relative: &Path,
    create: bool,
) -> Result<Option<PathBuf>> {
    if !ensure_plain_directory(root, create)? {
        return Ok(None);
    }

    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!(
                "Invalid directory cache relative path {}",
                relative.display()
            );
        };
        current.push(component);
        if !ensure_plain_directory(&current, create)? {
            return Ok(None);
        }
    }

    Ok(Some(current))
}

fn is_temp_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(TEMP_ENTRY_PREFIX))
}

fn is_stale_temp_entry(entry: &walkdir::DirEntry) -> bool {
    entry
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= TEMP_ENTRY_MAX_AGE)
}

fn clean_temporary_entries(root: &Path) {
    if !root.exists() {
        return;
    }
    for entry in WalkDir::new(root)
        .min_depth(1)
        .max_depth(1)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if is_temp_entry(path) && is_stale_temp_entry(&entry) {
            let result = if entry.file_type().is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            if let Err(err) = result {
                warn!(
                    "Failed to remove temporary directory cache entry {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut size = 0;
    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if entry.file_type().is_file() {
            size += entry.metadata()?.len();
        }
    }
    Ok(size)
}

#[derive(Debug)]
struct DirectoryEntry {
    path: PathBuf,
    size: u64,
    last_used: SystemTime,
}

struct ScannedDirectoryEntry {
    entry: DirectoryEntry,
    has_manifest: bool,
}

fn scanned_entry_path<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    // `make_key_path` places entries beneath three shards and the full key.
    const ENTRY_COMPONENTS: usize = 4;

    let depth = path.strip_prefix(root).ok()?.components().count();
    if depth <= ENTRY_COMPONENTS {
        return None;
    }
    path.ancestors().nth(depth - ENTRY_COMPONENTS)
}

fn analyze_stat_batch(
    root: &Path,
    paths: &[PathBuf],
    entries: &mut Vec<ScannedDirectoryEntry>,
    entry_indices: &mut HashMap<PathBuf, usize>,
) -> Result<()> {
    for (path, stat) in paths.iter().zip(bulk_stat(paths)) {
        let stat = match stat {
            Ok(stat) => stat,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to stat cache path {}", path.display()));
            }
        };
        if !stat.is_file {
            continue;
        }

        let Some(entry_path) = scanned_entry_path(root, path) else {
            continue;
        };
        let index = match entry_indices.get(entry_path) {
            Some(&index) => index,
            None => {
                let index = entries.len();
                entry_indices.insert(entry_path.to_path_buf(), index);
                entries.push(ScannedDirectoryEntry {
                    entry: DirectoryEntry {
                        path: entry_path.to_path_buf(),
                        size: 0,
                        last_used: SystemTime::UNIX_EPOCH,
                    },
                    has_manifest: false,
                });
                index
            }
        };
        let scanned = &mut entries[index];
        scanned.entry.size += stat.size;

        if path.parent() == Some(entry_path)
            && path.file_name() == Some(OsStr::new(DIRECTORY_CACHE_MANIFEST_FILE))
        {
            scanned.has_manifest = true;
            if let Some(modified) = stat.modified {
                scanned.entry.last_used = scanned.entry.last_used.max(modified);
            }
        }

        // Use the newer of manifest mtime and data-object atime for
        // eviction. Hard-linked and symlinked outputs can advance the
        // latter through ordinary reads. `relatime` can make that signal
        // coarse, while `noatime` suppresses automatic updates.
        let is_data_object = path.parent().is_some_and(|parent| {
            parent.file_name() == Some(OsStr::new(DIRECTORY_CACHE_OBJECTS_DIR))
                && parent.parent() == Some(entry_path)
        });
        if is_data_object {
            if let Some(accessed) = stat.accessed {
                scanned.entry.last_used = scanned.entry.last_used.max(accessed);
            }
        }
    }

    Ok(())
}

fn scan_entries(root: &Path) -> Result<Vec<DirectoryEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut entry_indices = HashMap::new();
    let mut paths = Vec::with_capacity(BULK_STAT_BATCH_SIZE);

    // Bound memory while streaming directory enumeration through bulk stat
    // and immediate entry aggregation.
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        paths.push(entry.into_path());
        if paths.len() == BULK_STAT_BATCH_SIZE {
            analyze_stat_batch(root, &paths, &mut entries, &mut entry_indices)?;
            paths.clear();
        }
    }
    if !paths.is_empty() {
        analyze_stat_batch(root, &paths, &mut entries, &mut entry_indices)?;
    }

    Ok(entries
        .into_iter()
        .filter(|entry| entry.has_manifest)
        .map(|entry| entry.entry)
        .collect())
}

fn make_space(root: &Path, max_size: u64, new_size: u64, replacing: &Path) -> Result<()> {
    if new_size > max_size {
        bail!(
            "Directory cache entry is too large to fit in the cache: {} > {}",
            new_size,
            max_size
        );
    }

    let mut entries = scan_entries(root)?;
    let mut total = entries
        .iter()
        .filter(|entry| entry.path != replacing)
        .map(|entry| entry.size)
        .sum::<u64>();

    entries.retain(|entry| entry.path != replacing);
    entries.sort_by_key(|entry| entry.last_used);

    while total + new_size > max_size {
        let Some(entry) = entries.first() else {
            bail!("Unable to make space in directory cache");
        };
        fs::remove_dir_all(&entry.path)
            .with_context(|| format!("Failed to evict cache entry {}", entry.path.display()))?;
        total = total.saturating_sub(entry.size);
        entries.remove(0);
    }

    Ok(())
}

fn touch_manifest(path: &Path) {
    let manifest = path.join(DIRECTORY_CACHE_MANIFEST_FILE);
    let now = FileTime::now();
    // Setting both timestamps to now permits a group-writable non-owner to
    // record the hit; eviction uses only manifest mtime.
    if let Err(err) = set_file_times(&manifest, now, now) {
        trace!(
            "Failed to update cache entry manifest time {}: {}",
            manifest.display(),
            err
        );
    }
}

fn ensure_stdio_size(name: &str, len: usize) -> Result<()> {
    if len as u64 > DIRECTORY_CACHE_STDIO_MAX_BYTES {
        bail!(
            "Directory cache {} is too large to store: {} > {}",
            name,
            len,
            DIRECTORY_CACHE_STDIO_MAX_BYTES
        );
    }
    Ok(())
}

fn write_entry_contents(path: &Path, entry: &CacheWrite) -> Result<()> {
    for (name, bytes) in [
        (DIRECTORY_CACHE_STDOUT_FILE, entry.stdout()),
        (DIRECTORY_CACHE_STDERR_FILE, entry.stderr()),
    ] {
        ensure_stdio_size(name, bytes.len())?;
    }

    ensure_relative_directory(path, Path::new(DIRECTORY_CACHE_OBJECTS_DIR), true)?
        .ok_or_else(|| anyhow!("Failed to create directory cache objects directory"))?;

    let mut manifest_objects = Vec::new();
    for (index, object) in entry.objects().iter().enumerate() {
        let file = index.to_string();
        let object_path = path.join(DIRECTORY_CACHE_OBJECTS_DIR).join(&file);
        object
            .write_to_path(&object_path)
            .with_context(|| format!("Failed to write cache object {}", object.key()))?;
        if let Some(mode) = object.mode() {
            set_file_mode(&object_path, mode)?;
        }
        manifest_objects.push(DirectoryCacheObject {
            key: object.key().to_owned(),
            file,
            mode: object.mode(),
        });
    }

    if !entry.stdout().is_empty() {
        fs::write(path.join(DIRECTORY_CACHE_STDOUT_FILE), entry.stdout())?;
    }
    if !entry.stderr().is_empty() {
        fs::write(path.join(DIRECTORY_CACHE_STDERR_FILE), entry.stderr())?;
    }

    let manifest = DirectoryCacheManifest {
        version: DIRECTORY_CACHE_MANIFEST_VERSION,
        objects: manifest_objects,
    };
    fs::write(
        path.join(DIRECTORY_CACHE_MANIFEST_FILE),
        serialize_directory_cache_manifest(&manifest)?,
    )?;

    Ok(())
}

fn read_manifest(path: &Path) -> Result<DirectoryCacheManifest> {
    read_directory_cache_manifest(path)
}

fn read_entry_as_cache_write(path: &Path, max_size: u64) -> Result<CacheWrite> {
    let manifest = read_manifest(path)?;
    let mut entry = CacheWrite::new();

    for object in manifest.objects {
        let file = open_directory_cache_object_file(path, &object.file)?;
        entry.put_file_object(object.key, file, object.mode);
    }

    entry.put_stdout(&read_directory_cache_optional_file(
        path,
        DIRECTORY_CACHE_STDOUT_FILE,
        Some(max_size),
    )?)?;
    entry.put_stderr(&read_directory_cache_optional_file(
        path,
        DIRECTORY_CACHE_STDERR_FILE,
        Some(max_size),
    )?)?;

    Ok(entry)
}

struct SizeLimitedWriter<W> {
    inner: W,
    remaining: u64,
    written: u64,
}

impl<W> SizeLimitedWriter<W> {
    fn new(inner: W, remaining: u64) -> Self {
        Self {
            inner,
            remaining,
            written: 0,
        }
    }

    fn written(&self) -> u64 {
        self.written
    }
}

impl<W: Write> Write for SizeLimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 && !buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "directory cache entry exceeds maximum size",
            ));
        }

        let writable = buf
            .len()
            .min(self.remaining.try_into().unwrap_or(usize::MAX));
        if writable < buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "directory cache entry exceeds maximum size",
            ));
        }

        let written = self.inner.write(buf)?;
        self.remaining = self.remaining.saturating_sub(written as u64);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn copy_decode_limited<R, W>(from: R, to: W, remaining: u64) -> Result<u64>
where
    R: Read,
    W: Write,
{
    let mut limited = SizeLimitedWriter::new(to, remaining);
    zstd::stream::copy_decode(from, &mut limited)?;
    Ok(limited.written())
}

fn write_raw_entry_contents(path: &Path, data: &[u8], max_size: u64) -> Result<()> {
    ensure_relative_directory(path, Path::new(DIRECTORY_CACHE_OBJECTS_DIR), true)?
        .ok_or_else(|| anyhow!("Failed to create directory cache objects directory"))?;

    let stdio_limit = directory_cache_stdio_read_limit(Some(max_size));
    let mut total_size = 0_u64;
    let mut manifest_objects = Vec::new();
    let mut zip = ZipArchive::new(Cursor::new(data)).context("Failed to parse cache entry")?;
    for index in 0..zip.len() {
        let file = zip.by_index(index).or(Err(DecompressionFailure))?;
        if file.is_dir() {
            continue;
        }
        if file.compression() != CompressionMethod::Stored {
            bail!(DecompressionFailure);
        }

        let name = file.name().to_owned();
        let mode = file.unix_mode();
        let remaining = max_size.saturating_sub(total_size);
        match name.as_str() {
            DIRECTORY_CACHE_STDOUT_FILE => {
                let output = fs::File::create(path.join(DIRECTORY_CACHE_STDOUT_FILE))?;
                total_size += copy_decode_limited(file, output, remaining.min(stdio_limit))?;
            }
            DIRECTORY_CACHE_STDERR_FILE => {
                let output = fs::File::create(path.join(DIRECTORY_CACHE_STDERR_FILE))?;
                total_size += copy_decode_limited(file, output, remaining.min(stdio_limit))?;
            }
            _ => {
                let object_file = manifest_objects.len().to_string();
                let object_path = path.join(DIRECTORY_CACHE_OBJECTS_DIR).join(&object_file);
                let output = fs::File::create(&object_path)?;
                total_size += copy_decode_limited(file, output, remaining)?;
                if let Some(mode) = mode {
                    set_file_mode(&object_path, mode)?;
                }
                manifest_objects.push(DirectoryCacheObject {
                    key: name,
                    file: object_file,
                    mode,
                });
            }
        }
    }

    let manifest = DirectoryCacheManifest {
        version: DIRECTORY_CACHE_MANIFEST_VERSION,
        objects: manifest_objects,
    };
    fs::write(
        path.join(DIRECTORY_CACHE_MANIFEST_FILE),
        serialize_directory_cache_manifest(&manifest)?,
    )?;

    Ok(())
}

struct PreparedEntry {
    tempdir: tempfile::TempDir,
    size: u64,
}

fn prepare_entry(root: &Path, entry: &CacheWrite) -> Result<PreparedEntry> {
    ensure_plain_directory(root, true)?;
    let tempdir = TempBuilder::new()
        .prefix(TEMP_ENTRY_PREFIX)
        .tempdir_in(root)
        .context("Failed to create temporary directory cache entry")?;
    write_entry_contents(tempdir.path(), entry)?;
    let size = directory_size(tempdir.path())?;
    Ok(PreparedEntry { tempdir, size })
}

fn prepare_raw_entry(root: &Path, data: &[u8], max_size: u64) -> Result<PreparedEntry> {
    ensure_plain_directory(root, true)?;
    let tempdir = TempBuilder::new()
        .prefix(TEMP_ENTRY_PREFIX)
        .tempdir_in(root)
        .context("Failed to create temporary directory cache entry")?;
    write_raw_entry_contents(tempdir.path(), data, max_size)?;
    let size = directory_size(tempdir.path())?;
    Ok(PreparedEntry { tempdir, size })
}

#[derive(Clone, Copy)]
enum PublishMode {
    Replace,
    IfAbsent,
}

fn publish_entry(
    root: &Path,
    max_size: u64,
    relative: &Path,
    prepared: PreparedEntry,
    mode: PublishMode,
) -> Result<bool> {
    let target_exists = ensure_relative_directory(root, relative, false)?.is_some();
    if target_exists && matches!(mode, PublishMode::IfAbsent) {
        return Ok(false);
    }

    let parent = relative
        .parent()
        .ok_or_else(|| anyhow!("Directory cache target without parent"))?;
    ensure_relative_directory(root, parent, true)?
        .ok_or_else(|| anyhow!("Failed to create directory cache target parent"))?;
    clean_temporary_entries(root);

    let target = root.join(relative);
    make_space(root, max_size, prepared.size, &target)?;
    if target_exists {
        fs::remove_dir_all(&target)
            .with_context(|| format!("Failed to replace cache entry {}", target.display()))?;
    }

    let temp_path = prepared.tempdir.into_path();
    if let Err(err) = fs::rename(&temp_path, &target) {
        let _ = fs::remove_dir_all(&temp_path);
        return Err(err)
            .with_context(|| format!("Failed to commit cache entry {}", target.display()));
    }

    Ok(true)
}

impl DirectoryCache {
    async fn put_entry(
        &self,
        key: &str,
        entry: CacheWrite,
        mode: PublishMode,
    ) -> Result<(Duration, bool)> {
        if self.rw_mode == CacheMode::ReadOnly {
            bail!("Cannot write to a read-only cache");
        }

        let root = self.root.clone();
        let max_size = self.max_size;
        let relative = make_key_path(key)?;
        self.pool
            .spawn_blocking(move || {
                let start = Instant::now();
                let prepared = prepare_entry(&root, &entry)?;
                let _lock = acquire_exclusive_cache_lock(&root)?;
                let inserted = publish_entry(&root, max_size, &relative, prepared, mode)?;
                Ok((start.elapsed(), inserted))
            })
            .await?
    }

    async fn put_raw_entry(
        &self,
        key: &str,
        data: Bytes,
        mode: PublishMode,
    ) -> Result<(Duration, bool)> {
        if self.rw_mode == CacheMode::ReadOnly {
            bail!("Cannot write to a read-only cache");
        }

        let root = self.root.clone();
        let max_size = self.max_size;
        let relative = make_key_path(key)?;
        self.pool
            .spawn_blocking(move || {
                let start = Instant::now();
                let prepared = prepare_raw_entry(&root, &data, max_size)?;
                let _lock = acquire_exclusive_cache_lock(&root)?;
                let inserted = publish_entry(&root, max_size, &relative, prepared, mode)?;
                Ok((start.elapsed(), inserted))
            })
            .await?
    }
}

#[async_trait]
impl Storage for DirectoryCache {
    async fn get(&self, key: &str) -> Result<Cache> {
        trace!("DirectoryCache::get({})", key);
        let root = self.root.clone();
        let relative = make_key_path(key)?;
        let max_size = self.max_size;
        let link = self.link;
        let create_lock = self.rw_mode == CacheMode::ReadWrite;
        self.pool
            .spawn_blocking(move || {
                let _lock = acquire_shared_cache_lock(&root, create_lock)?;
                let Some(path) = ensure_relative_directory(&root, &relative, false)? else {
                    return Ok(Cache::Miss);
                };
                let read =
                    CacheRead::from_directory_with_max_size(path.clone(), Some(max_size), link)?;
                // Prefer the data inode's atime so hard-linked and symlinked
                // outputs can extend retention without another metadata write.
                if !read.touch_directory_data_atime() {
                    touch_manifest(&path);
                }
                Ok(Cache::Hit(read))
            })
            .await?
    }

    async fn put(&self, key: &str, entry: CacheWrite) -> Result<Duration> {
        trace!("DirectoryCache::put({})", key);
        Ok(self.put_entry(key, entry, PublishMode::Replace).await?.0)
    }

    async fn put_if_absent(&self, key: &str, entry: CacheWrite) -> Result<Duration> {
        trace!("DirectoryCache::put_if_absent({})", key);
        Ok(self.put_entry(key, entry, PublishMode::IfAbsent).await?.0)
    }

    async fn get_raw(&self, key: &str) -> Result<Option<Bytes>> {
        trace!("DirectoryCache::get_raw({})", key);
        let root = self.root.clone();
        let relative = make_key_path(key)?;
        let max_size = self.max_size;
        let create_lock = self.rw_mode == CacheMode::ReadWrite;
        self.pool
            .spawn_blocking(move || {
                let entry = {
                    let _lock = acquire_shared_cache_lock(&root, create_lock)?;
                    let Some(path) = ensure_relative_directory(&root, &relative, false)? else {
                        return Ok(None);
                    };
                    let entry = read_entry_as_cache_write(&path, max_size)?;
                    touch_manifest(&path);
                    entry
                };
                Ok(Some(Bytes::from(entry.finish()?)))
            })
            .await?
    }

    async fn put_raw(&self, key: &str, data: Bytes) -> Result<Duration> {
        trace!("DirectoryCache::put_raw({}, {} bytes)", key, data.len());
        Ok(self.put_raw_entry(key, data, PublishMode::Replace).await?.0)
    }
    async fn check(&self) -> Result<CacheMode> {
        Ok(self.rw_mode)
    }

    fn location(&self) -> String {
        format!("Local directory: {:?}", self.root)
    }

    fn cache_type_name(&self) -> &'static str {
        "directory"
    }

    async fn current_size(&self) -> Result<Option<u64>> {
        let root = self.root.clone();
        self.pool
            .spawn_blocking(move || {
                let _lock = acquire_exclusive_cache_lock(&root)?;
                clean_temporary_entries(&root);
                Ok(Some(
                    scan_entries(&root)?
                        .into_iter()
                        .map(|entry| entry.size)
                        .sum(),
                ))
            })
            .await?
    }

    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(Some(self.max_size))
    }

    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        self.preprocessor_cache_mode_config
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        &self.basedirs
    }

    async fn get_preprocessor_cache_entry(&self, key: &str) -> Result<Option<Box<dyn ReadSeek>>> {
        let root = self.root.clone();
        let max_size = self.max_size;
        let key = normalize_key(key);
        self.pool
            .spawn_blocking(move || {
                let _lock = acquire_exclusive_cache_lock(&root)?;
                let mut cache = LruDiskCache::new(root.join("preprocessor"), max_size)?;
                Ok(cache.get(key).ok())
            })
            .await?
    }

    async fn put_preprocessor_cache_entry(
        &self,
        key: &str,
        preprocessor_cache_entry: PreprocessorCacheEntry,
    ) -> Result<()> {
        if self.rw_mode == CacheMode::ReadOnly {
            bail!("Cannot write to a read-only cache");
        }

        let mut serialized = Vec::new();
        preprocessor_cache_entry.serialize_to(&mut serialized)?;

        let root = self.root.clone();
        let max_size = self.max_size;
        let key = normalize_key(key);
        self.pool
            .spawn_blocking(move || {
                let _lock = acquire_exclusive_cache_lock(&root)?;
                let mut cache = LruDiskCache::new(root.join("preprocessor"), max_size)?;
                let mut entry = cache.prepare_add(key, serialized.len() as u64)?;
                entry.as_file_mut().write_all(&serialized)?;
                Ok(cache.commit(entry)?)
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FileObjectSource;
    use bytes::Bytes;
    use std::io::{Cursor, Read};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const KEY: &str = "abcdef0123456789";
    const OTHER_KEY: &str = "abcdef0123456788";

    fn new_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn new_cache(root: PathBuf, pool: &tokio::runtime::Handle) -> DirectoryCache {
        new_cache_with_link(root, pool, DirectoryCacheLinkConfig::default())
    }

    fn new_cache_with_link(
        root: PathBuf,
        pool: &tokio::runtime::Handle,
        link: DirectoryCacheLinkConfig,
    ) -> DirectoryCache {
        DirectoryCache::new_at_path(
            root,
            u64::MAX,
            pool,
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadWrite,
            link,
            vec![],
        )
    }

    fn cache_write_with_object(object: &[u8]) -> CacheWrite {
        let mut write = CacheWrite::new();
        write
            .put_object("o", &mut Cursor::new(object), None)
            .unwrap();
        write
    }

    fn raw_cache_entry(object: &[u8]) -> Bytes {
        Bytes::from(cache_write_with_object(object).finish().unwrap())
    }

    fn spawn_put(
        root: PathBuf,
        barrier: Arc<Barrier>,
        key: &'static str,
        object: &'static [u8],
    ) -> thread::JoinHandle<bool> {
        thread::spawn(move || {
            let runtime = new_runtime();
            let cache = new_cache(root, runtime.handle());
            let write = cache_write_with_object(object);
            barrier.wait();
            runtime
                .block_on(cache.put_entry(key, write, PublishMode::IfAbsent))
                .unwrap()
                .1
        })
    }

    fn read_cached_object(
        runtime: &tokio::runtime::Runtime,
        cache: &DirectoryCache,
        key: &str,
    ) -> Vec<u8> {
        let mut read = match runtime.block_on(cache.get(key)).unwrap() {
            Cache::Hit(read) => read,
            other => panic!("expected cache hit, got {other:?}"),
        };
        let mut object = Vec::new();
        read.get_object("o", &mut object).unwrap();
        object
    }

    #[test]
    fn directory_cache_miss_does_not_create_root() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), runtime.handle());

        assert!(matches!(
            runtime.block_on(cache.get(KEY)).unwrap(),
            Cache::Miss
        ));
        assert!(!cache_root.exists());
    }

    #[test]
    fn directory_cache_independent_instances_write_distinct_keys_concurrently() {
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let barrier = Arc::new(Barrier::new(2));

        let first = spawn_put(
            cache_root.clone(),
            Arc::clone(&barrier),
            KEY,
            b"first object",
        );
        let second = spawn_put(
            cache_root.clone(),
            Arc::clone(&barrier),
            OTHER_KEY,
            b"second object",
        );
        assert!(first.join().unwrap());
        assert!(second.join().unwrap());

        let runtime = new_runtime();
        let cache = new_cache(cache_root, runtime.handle());
        assert_eq!(read_cached_object(&runtime, &cache, KEY), b"first object");
        assert_eq!(
            read_cached_object(&runtime, &cache, OTHER_KEY),
            b"second object"
        );
    }

    #[test]
    fn directory_cache_independent_instances_choose_one_same_key_winner() {
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let barrier = Arc::new(Barrier::new(2));

        let first = spawn_put(
            cache_root.clone(),
            Arc::clone(&barrier),
            KEY,
            b"first contender",
        );
        let second = spawn_put(
            cache_root.clone(),
            Arc::clone(&barrier),
            KEY,
            b"second contender",
        );
        let first_inserted = first.join().unwrap();
        let second_inserted = second.join().unwrap();
        assert_ne!(
            first_inserted, second_inserted,
            "exactly one same-key publication should win"
        );

        let winner: &[u8] = if first_inserted {
            b"first contender"
        } else {
            b"second contender"
        };
        let loser: &[u8] = if first_inserted {
            b"second contender"
        } else {
            b"first contender"
        };

        let runtime = new_runtime();
        let cache = new_cache(cache_root, runtime.handle());
        let mut pinned = match runtime.block_on(cache.get(KEY)).unwrap() {
            Cache::Hit(read) => read,
            other => panic!("expected cache hit, got {other:?}"),
        };

        runtime
            .block_on(cache.put(KEY, cache_write_with_object(loser)))
            .unwrap();

        let mut pinned_object = Vec::new();
        pinned.get_object("o", &mut pinned_object).unwrap();
        assert_eq!(pinned_object, winner);
        assert_eq!(read_cached_object(&runtime, &cache, KEY), loser);
    }

    #[test]
    fn directory_cache_preprocessor_cache_reloads_from_disk_each_operation() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let writer = new_cache(cache_root.clone(), runtime.handle());
        let reader = new_cache(cache_root, runtime.handle());

        assert!(
            runtime
                .block_on(reader.get_preprocessor_cache_entry(KEY))
                .unwrap()
                .is_none()
        );

        let expected = PreprocessorCacheEntry::default();
        runtime
            .block_on(writer.put_preprocessor_cache_entry(KEY, expected.clone()))
            .unwrap();

        let mut stored = runtime
            .block_on(reader.get_preprocessor_cache_entry(KEY))
            .unwrap()
            .expect("preprocessor cache entry should be visible");
        let mut serialized = Vec::new();
        stored.read_to_end(&mut serialized).unwrap();
        assert_eq!(PreprocessorCacheEntry::read(&serialized).unwrap(), expected);
    }
    #[test]
    fn directory_cache_stores_and_restores_raw_objects() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), &pool);

        let mut write = CacheWrite::new();
        write
            .put_object("o", &mut Cursor::new(b"object bytes"), None)
            .unwrap();
        write.put_stdout(b"stdout").unwrap();
        write.put_stderr(b"stderr").unwrap();

        runtime.block_on(cache.put(KEY, write)).unwrap();

        let entry_path = cache_root.join(make_key_path(KEY).unwrap());
        assert!(entry_path.is_dir());
        assert_eq!(
            fs::read(entry_path.join(DIRECTORY_CACHE_STDOUT_FILE)).unwrap(),
            b"stdout"
        );
        assert_eq!(
            fs::read(entry_path.join(DIRECTORY_CACHE_STDERR_FILE)).unwrap(),
            b"stderr"
        );

        let manifest = read_manifest(&entry_path).unwrap();
        assert_eq!(manifest.objects.len(), 1);
        assert_eq!(manifest.objects[0].key, "o");
        let object_path = entry_path
            .join(DIRECTORY_CACHE_OBJECTS_DIR)
            .join(&manifest.objects[0].file);
        assert!(fs::metadata(&object_path).unwrap().is_file());
        assert_eq!(fs::read(&object_path).unwrap(), b"object bytes");

        let output = tempdir.path().join("output.o");
        let mut read = match runtime.block_on(cache.get(KEY)).unwrap() {
            Cache::Hit(read) => read,
            other => panic!("expected cache hit, got {other:?}"),
        };

        assert_eq!(read.get_stdout(), b"stdout");
        assert_eq!(read.get_stderr(), b"stderr");
        runtime
            .block_on(read.extract_objects(
                vec![FileObjectSource {
                    key: "o".to_owned(),
                    path: output.clone(),
                    optional: false,
                }],
                &pool,
            ))
            .unwrap();

        assert_eq!(fs::read(output).unwrap(), b"object bytes");
    }

    #[test]
    fn directory_cache_hard_links_restored_objects() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache_with_link(
            cache_root.clone(),
            &pool,
            DirectoryCacheLinkConfig {
                link_type: crate::config::DirectoryCacheLinkType::HardLink,
                required: true,
            },
        );

        runtime
            .block_on(cache.put(KEY, cache_write_with_object(b"object bytes")))
            .unwrap();

        let output = tempdir.path().join("output.o");
        let read = match runtime.block_on(cache.get(KEY)).unwrap() {
            Cache::Hit(read) => read,
            other => panic!("expected cache hit, got {other:?}"),
        };
        runtime
            .block_on(read.extract_objects(
                vec![FileObjectSource {
                    key: "o".to_owned(),
                    path: output.clone(),
                    optional: false,
                }],
                &pool,
            ))
            .unwrap();

        let cached = fs::File::open(
            cache_root
                .join(make_key_path(KEY).unwrap())
                .join(DIRECTORY_CACHE_OBJECTS_DIR)
                .join("0"),
        )
        .unwrap();
        let output = fs::File::open(output).unwrap();
        assert_eq!(
            crate::cache::directory_io::StorageFileObjectIdentity::from_file(cached.file())
                .unwrap(),
            crate::cache::directory_io::StorageFileObjectIdentity::from_file(output.file())
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_symlinks_restored_objects() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache_with_link(
            cache_root.clone(),
            &pool,
            DirectoryCacheLinkConfig {
                link_type: crate::config::DirectoryCacheLinkType::Symlink,
                required: true,
            },
        );

        runtime
            .block_on(cache.put(KEY, cache_write_with_object(b"object bytes")))
            .unwrap();

        let output = tempdir.path().join("output.o");
        let read = match runtime.block_on(cache.get(KEY)).unwrap() {
            Cache::Hit(read) => read,
            other => panic!("expected cache hit, got {other:?}"),
        };
        runtime
            .block_on(read.extract_objects(
                vec![FileObjectSource {
                    key: "o".to_owned(),
                    path: output.clone(),
                    optional: false,
                }],
                &pool,
            ))
            .unwrap();

        assert!(
            fs::symlink_metadata(&output)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(output).unwrap(), b"object bytes");
    }

    #[test]
    fn directory_cache_does_not_expose_mutable_entry_path() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = new_cache(tempdir.path().join("cache"), &pool);

        let mut write = CacheWrite::new();
        write
            .put_object("o", &mut Cursor::new(b"object bytes"), None)
            .unwrap();
        runtime.block_on(cache.put(KEY, write)).unwrap();

        assert!(matches!(
            runtime.block_on(cache.get_path(KEY)),
            crate::cache::GetPathResult::Unsupported
        ));
    }

    #[test]
    fn directory_cache_put_raw_and_get_raw_bridge_legacy_zip() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), &pool);

        let mut legacy = CacheWrite::new();
        legacy
            .put_object("o", &mut Cursor::new(b"legacy object"), None)
            .unwrap();
        legacy.put_stdout(b"legacy stdout").unwrap();
        legacy.put_stderr(b"legacy stderr").unwrap();
        let raw = legacy.finish().unwrap();

        runtime
            .block_on(cache.put_raw(KEY, Bytes::from(raw)))
            .unwrap();

        let entry_path = cache_root.join(make_key_path(KEY).unwrap());
        let manifest = read_manifest(&entry_path).unwrap();
        assert_eq!(manifest.objects.len(), 1);
        assert_eq!(
            fs::read(
                entry_path
                    .join(DIRECTORY_CACHE_OBJECTS_DIR)
                    .join(&manifest.objects[0].file),
            )
            .unwrap(),
            b"legacy object"
        );

        let raw = runtime.block_on(cache.get_raw(KEY)).unwrap().unwrap();
        let mut read = CacheRead::from(Cursor::new(raw.to_vec())).unwrap();
        let mut object = Vec::new();
        read.get_object("o", &mut object).unwrap();

        assert_eq!(object, b"legacy object");
        assert_eq!(read.get_stdout(), b"legacy stdout");
        assert_eq!(read.get_stderr(), b"legacy stderr");
    }

    #[test]
    fn directory_cache_put_raw_replaces_existing_entry() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = new_cache(tempdir.path().join("cache"), runtime.handle());

        runtime
            .block_on(cache.put_raw(KEY, raw_cache_entry(b"first object")))
            .unwrap();
        runtime
            .block_on(cache.put_raw(KEY, raw_cache_entry(b"second object")))
            .unwrap();

        let raw = runtime.block_on(cache.get_raw(KEY)).unwrap().unwrap();
        let mut read = CacheRead::from(Cursor::new(raw.to_vec())).unwrap();
        let mut object = Vec::new();
        read.get_object("o", &mut object).unwrap();
        assert_eq!(object, b"second object");
    }

    #[test]
    fn directory_cache_get_raw_refreshes_lru_timestamp() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), &pool);

        let mut write = CacheWrite::new();
        write
            .put_object("o", &mut Cursor::new(b"object bytes"), None)
            .unwrap();
        runtime.block_on(cache.put(KEY, write)).unwrap();

        let manifest_path = cache_root
            .join(make_key_path(KEY).unwrap())
            .join(DIRECTORY_CACHE_MANIFEST_FILE);
        let old = FileTime::from_unix_time(1, 0);
        set_file_times(&manifest_path, old, old).unwrap();
        let before = fs::metadata(&manifest_path).unwrap().modified().unwrap();

        assert!(runtime.block_on(cache.get_raw(KEY)).unwrap().is_some());

        let after = fs::metadata(&manifest_path).unwrap().modified().unwrap();
        assert!(
            after > before,
            "raw cache hit should refresh the manifest timestamp"
        );
    }

    #[test]
    fn directory_cache_hit_prefers_data_atime_over_manifest_mtime() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), runtime.handle());

        runtime
            .block_on(cache.put(KEY, cache_write_with_object(b"object")))
            .unwrap();

        let entry_path = cache_root.join(make_key_path(KEY).unwrap());
        let manifest = entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE);
        let object = entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR).join("0");
        let old = FileTime::from_unix_time(1, 0);
        set_file_times(&manifest, old, old).unwrap();
        set_file_times(&object, old, old).unwrap();
        let manifest_mtime = fs::metadata(&manifest).unwrap().modified().unwrap();

        assert!(matches!(
            runtime.block_on(cache.get(KEY)).unwrap(),
            Cache::Hit(_)
        ));

        assert!(
            fs::metadata(object).unwrap().accessed().unwrap()
                > SystemTime::UNIX_EPOCH + Duration::from_secs(1)
        );
        assert_eq!(
            fs::metadata(manifest).unwrap().modified().unwrap(),
            manifest_mtime
        );
    }

    #[test]
    fn directory_cache_hit_falls_back_to_manifest_without_data_object() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), runtime.handle());

        runtime
            .block_on(cache.put(KEY, CacheWrite::default()))
            .unwrap();

        let manifest = cache_root
            .join(make_key_path(KEY).unwrap())
            .join(DIRECTORY_CACHE_MANIFEST_FILE);
        let old = FileTime::from_unix_time(1, 0);
        set_file_times(&manifest, old, old).unwrap();

        assert!(matches!(
            runtime.block_on(cache.get(KEY)).unwrap(),
            Cache::Hit(_)
        ));
        assert!(
            fs::metadata(manifest).unwrap().modified().unwrap()
                > SystemTime::UNIX_EPOCH + Duration::from_secs(1)
        );
    }

    #[test]
    fn directory_cache_lru_considers_object_atime() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), runtime.handle());

        runtime
            .block_on(cache.put(KEY, cache_write_with_object(b"object")))
            .unwrap();

        let entry_path = cache_root.join(make_key_path(KEY).unwrap());
        let manifest = entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE);
        let object = entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR).join("0");
        let expected_size = directory_size(&entry_path).unwrap();
        let output = tempdir.path().join("hard-linked-output");
        fs::hard_link(&object, &output).unwrap();
        let old = FileTime::from_unix_time(1, 0);
        let recently_accessed = FileTime::from_unix_time(2, 0);
        set_file_times(manifest, old, old).unwrap();
        // This test runs on `noatime`, so explicitly simulate a read updating
        // the inode through the hard-linked output path.
        set_file_times(output, recently_accessed, old).unwrap();

        // A hard-linked output shares this object inode. Its reads can advance
        // object atime, which eviction treats as a newer last-use hint.
        let entries = scan_entries(&cache_root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, expected_size);
        assert_eq!(
            entries[0].last_used,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2)
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_put_reads_from_open_file_source() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), &pool);
        let source_path = tempdir.path().join("source.o");

        fs::write(&source_path, b"original object").unwrap();
        let source = fs::File::open(&source_path).unwrap();

        let mut write = CacheWrite::new();
        write.put_file_object("o".to_owned(), source, None);

        fs::remove_file(&source_path).unwrap();
        fs::write(&source_path, b"replacement object").unwrap();

        runtime.block_on(cache.put(KEY, write)).unwrap();

        let entry_path = cache_root.join(make_key_path(KEY).unwrap());
        let manifest = read_manifest(&entry_path).unwrap();
        assert_eq!(
            fs::read(
                entry_path
                    .join(DIRECTORY_CACHE_OBJECTS_DIR)
                    .join(&manifest.objects[0].file),
            )
            .unwrap(),
            b"original object"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_hit_pins_open_object_files() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), &pool);

        let mut write = CacheWrite::new();
        write
            .put_object("o", &mut Cursor::new(b"pinned object"), None)
            .unwrap();
        write.put_stdout(b"pinned stdout").unwrap();
        write.put_stderr(b"pinned stderr").unwrap();

        runtime.block_on(cache.put(KEY, write)).unwrap();

        let entry_path = cache_root.join(make_key_path(KEY).unwrap());
        let mut read = match runtime.block_on(cache.get(KEY)).unwrap() {
            Cache::Hit(read) => read,
            other => panic!("expected cache hit, got {other:?}"),
        };

        fs::remove_dir_all(&entry_path).unwrap();

        assert_eq!(read.get_stdout(), b"pinned stdout");
        assert_eq!(read.get_stderr(), b"pinned stderr");

        let output = tempdir.path().join("output.o");
        runtime
            .block_on(read.extract_objects(
                vec![FileObjectSource {
                    key: "o".to_owned(),
                    path: output.clone(),
                    optional: false,
                }],
                &pool,
            ))
            .unwrap();

        assert_eq!(fs::read(output).unwrap(), b"pinned object");
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_reads_existing_and_legacy_read_only_roots() {
        let runtime = new_runtime();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let writer = new_cache(cache_root.clone(), runtime.handle());
        runtime
            .block_on(writer.put(KEY, cache_write_with_object(b"read-only object")))
            .unwrap();

        let lock_path = cache_root.join(CACHE_LOCK_FILE);
        let mut permissions = fs::metadata(&lock_path).unwrap().permissions();
        permissions.set_mode(0o444);
        fs::set_permissions(&lock_path, permissions).unwrap();
        let mut permissions = fs::metadata(&cache_root).unwrap().permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(&cache_root, permissions).unwrap();

        let reader = DirectoryCache::new_at_path(
            cache_root.clone(),
            u64::MAX,
            runtime.handle(),
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadOnly,
            DirectoryCacheLinkConfig::default(),
            vec![],
        );
        assert_eq!(
            read_cached_object(&runtime, &reader, KEY),
            b"read-only object"
        );

        let mut permissions = fs::metadata(&cache_root).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cache_root, permissions).unwrap();
        fs::remove_file(&lock_path).unwrap();
        let mut permissions = fs::metadata(&cache_root).unwrap().permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(&cache_root, permissions).unwrap();

        assert_eq!(
            read_cached_object(&runtime, &reader, KEY),
            b"read-only object"
        );
        assert!(!lock_path.exists());

        let mut permissions = fs::metadata(&cache_root).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cache_root, permissions).unwrap();
    }

    #[test]
    fn directory_cache_rejects_manifest_object_paths() {
        let tempdir = tempfile::tempdir().unwrap();
        let entry_path = tempdir.path().join("entry");
        fs::create_dir_all(entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR)).unwrap();

        let manifest = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![DirectoryCacheObject {
                key: "o".to_owned(),
                file: "../outside".to_owned(),
                mode: None,
            }],
        };
        fs::write(
            entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE),
            bincode::serialize(&manifest).unwrap(),
        )
        .unwrap();

        assert!(
            CacheRead::from_directory_with_max_size(
                entry_path,
                Some(u64::MAX),
                DirectoryCacheLinkConfig::default()
            )
            .is_err()
        );
    }

    #[test]
    fn directory_cache_keeps_fresh_temp_entries() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let cache = new_cache(cache_root.clone(), &pool);

        let fresh_temp = cache_root.join(format!("{TEMP_ENTRY_PREFIX}-fresh"));
        fs::create_dir(&fresh_temp).unwrap();
        fs::write(fresh_temp.join("partial"), b"partial").unwrap();

        assert_eq!(runtime.block_on(cache.current_size()).unwrap(), Some(0));
        assert!(fresh_temp.exists());
    }

    #[test]
    fn directory_cache_removes_stale_temp_entries() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let cache = new_cache(cache_root.clone(), &pool);

        let stale_temp = cache_root.join(format!("{TEMP_ENTRY_PREFIX}-stale"));
        fs::create_dir(&stale_temp).unwrap();
        fs::write(stale_temp.join("partial"), b"partial").unwrap();
        let old = FileTime::from_system_time(
            SystemTime::now() - TEMP_ENTRY_MAX_AGE - Duration::from_secs(1),
        );
        set_file_times(&stale_temp, old, old).unwrap();

        assert_eq!(runtime.block_on(cache.current_size()).unwrap(), Some(0));
        assert!(!stale_temp.exists());
    }

    #[test]
    fn directory_cache_put_raw_rejects_entries_larger_than_max_size() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = DirectoryCache::new_at_path(
            cache_root.clone(),
            16,
            &pool,
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadWrite,
            DirectoryCacheLinkConfig::default(),
            vec![],
        );

        let mut legacy = CacheWrite::new();
        let object = vec![0xAB; 128];
        legacy
            .put_object("o", &mut Cursor::new(object), None)
            .unwrap();
        let raw = legacy.finish().unwrap();

        let result = runtime.block_on(cache.put_raw(KEY, Bytes::from(raw)));
        assert!(result.is_err());
        assert!(!cache_root.join(make_key_path(KEY).unwrap()).exists());
    }

    #[test]
    fn directory_cache_rejects_invalid_keys_without_panicking() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = new_cache(tempdir.path().join("cache"), &pool);

        for key in ["x", "../escape", "/tmp/escape", "éscape"] {
            assert!(
                runtime
                    .block_on(cache.put(key, CacheWrite::default()))
                    .is_err()
            );
            assert!(runtime.block_on(cache.get(key)).is_err());
            assert!(runtime.block_on(cache.get_raw(key)).is_err());
        }
    }

    #[test]
    fn directory_cache_rejects_unreadable_stdio_sizes() {
        ensure_stdio_size(
            DIRECTORY_CACHE_STDOUT_FILE,
            DIRECTORY_CACHE_STDIO_MAX_BYTES as usize,
        )
        .unwrap();
        assert!(
            ensure_stdio_size(
                DIRECTORY_CACHE_STDOUT_FILE,
                DIRECTORY_CACHE_STDIO_MAX_BYTES as usize + 1,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_rejects_symlinked_reserved_root() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let outside = tempdir.path().join("outside");
        fs::create_dir_all(&cache_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("marker"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, cache_root.join(DIRECTORY_CACHE_DIR)).unwrap();
        let cache = DirectoryCache::new(
            cache_root,
            u64::MAX,
            &pool,
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadWrite,
            DirectoryCacheLinkConfig::default(),
            vec![],
        );

        assert!(
            runtime
                .block_on(cache.put(KEY, CacheWrite::default()))
                .is_err()
        );
        assert!(runtime.block_on(cache.get(KEY)).is_err());
        assert!(
            runtime
                .block_on(cache.get_preprocessor_cache_entry(KEY))
                .is_err()
        );
        assert!(runtime.block_on(cache.current_size()).is_err());
        assert_eq!(fs::read(outside.join("marker")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_rejects_symlinked_shard_directory() {
        let runtime = new_runtime();
        let pool = runtime.handle().clone();
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let cache = new_cache(cache_root.clone(), &pool);
        let outside = tempdir.path().join("outside");
        fs::create_dir_all(&cache_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("marker"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, cache_root.join("a")).unwrap();

        assert!(
            runtime
                .block_on(cache.put(KEY, CacheWrite::default()))
                .is_err()
        );
        assert!(runtime.block_on(cache.get(KEY)).is_err());
        assert_eq!(fs::read(outside.join("marker")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_rejects_symlinked_objects_directory() {
        let tempdir = tempfile::tempdir().unwrap();
        let entry_path = tempdir.path().join("entry");
        let outside_objects = tempdir.path().join("outside_objects");
        fs::create_dir_all(&entry_path).unwrap();
        fs::create_dir_all(&outside_objects).unwrap();
        fs::write(outside_objects.join("0"), b"outside").unwrap();
        std::os::unix::fs::symlink(
            &outside_objects,
            entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR),
        )
        .unwrap();

        let manifest = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![DirectoryCacheObject {
                key: "o".to_owned(),
                file: "0".to_owned(),
                mode: None,
            }],
        };
        fs::write(
            entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE),
            bincode::serialize(&manifest).unwrap(),
        )
        .unwrap();

        assert!(
            CacheRead::from_directory_with_max_size(
                entry_path,
                Some(u64::MAX),
                DirectoryCacheLinkConfig::default()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_rejects_symlink_object_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let entry_path = tempdir.path().join("entry");
        fs::create_dir_all(entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR)).unwrap();
        let outside = tempdir.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(
            &outside,
            entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR).join("0"),
        )
        .unwrap();

        let manifest = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![DirectoryCacheObject {
                key: "o".to_owned(),
                file: "0".to_owned(),
                mode: None,
            }],
        };
        fs::write(
            entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE),
            bincode::serialize(&manifest).unwrap(),
        )
        .unwrap();

        assert!(
            CacheRead::from_directory_with_max_size(
                entry_path,
                Some(u64::MAX),
                DirectoryCacheLinkConfig::default()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_rejects_symlink_manifest() {
        let tempdir = tempfile::tempdir().unwrap();
        let entry_path = tempdir.path().join("entry");
        fs::create_dir_all(&entry_path).unwrap();
        let outside_manifest = tempdir.path().join("outside_manifest");
        let manifest = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![],
        };
        fs::write(&outside_manifest, bincode::serialize(&manifest).unwrap()).unwrap();
        std::os::unix::fs::symlink(
            &outside_manifest,
            entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE),
        )
        .unwrap();

        assert!(
            CacheRead::from_directory_with_max_size(
                entry_path,
                Some(u64::MAX),
                DirectoryCacheLinkConfig::default()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_cache_rejects_symlink_stdout() {
        let tempdir = tempfile::tempdir().unwrap();
        let entry_path = tempdir.path().join("entry");
        fs::create_dir_all(entry_path.join(DIRECTORY_CACHE_OBJECTS_DIR)).unwrap();
        let manifest = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![],
        };
        fs::write(
            entry_path.join(DIRECTORY_CACHE_MANIFEST_FILE),
            bincode::serialize(&manifest).unwrap(),
        )
        .unwrap();
        let outside = tempdir.path().join("outside_stdout");
        fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, entry_path.join(DIRECTORY_CACHE_STDOUT_FILE)).unwrap();

        assert!(
            CacheRead::from_directory_with_max_size(
                entry_path,
                Some(u64::MAX),
                DirectoryCacheLinkConfig::default()
            )
            .is_err()
        );
    }
}
