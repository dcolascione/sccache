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

use crate::errors::*;
use fs_err as fs;
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use std::io::Seek;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::cache_io::{CacheWrite, DecompressionFailure};
/// Metadata for one object in a directory-backed cache entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DirectoryCacheObject {
    pub key: String,
    pub file: String,
    pub mode: Option<u32>,
}

/// Metadata for a directory-backed cache entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DirectoryCacheManifest {
    pub version: u32,
    pub objects: Vec<DirectoryCacheObject>,
}

/// Reserved child for directory-cache data. `LruDiskCache` skips this exact
/// child at the cache root; disk entries start with one-character shard
/// directories, so both local caches may safely share a root.
pub(crate) const DIRECTORY_CACHE_DIR: &str = "directory";
pub(crate) const DIRECTORY_CACHE_MANIFEST_VERSION: u32 = 1;
pub(crate) const DIRECTORY_CACHE_MANIFEST_FILE: &str = "manifest";
pub(crate) const DIRECTORY_CACHE_OBJECTS_DIR: &str = "objects";
pub(crate) const DIRECTORY_CACHE_STDOUT_FILE: &str = "stdout";
pub(crate) const DIRECTORY_CACHE_STDERR_FILE: &str = "stderr";

pub(crate) const DIRECTORY_CACHE_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
pub(crate) const DIRECTORY_CACHE_STDIO_MAX_BYTES: u64 = 128 * 1024 * 1024;

fn ensure_directory_cache_object_file_is_plain_name(file: &str) -> Result<()> {
    let mut components = Path::new(file).components();
    if !matches!(components.next(), Some(Component::Normal(name)) if name.to_str() == Some(file))
        || components.next().is_some()
        || file.is_empty()
    {
        bail!("Invalid directory cache object file name {:?}", file);
    }
    Ok(())
}

pub(crate) fn ensure_directory_cache_manifest_is_safe_to_read(
    manifest: &DirectoryCacheManifest,
) -> Result<()> {
    if manifest.version != DIRECTORY_CACHE_MANIFEST_VERSION {
        bail!(
            "Unsupported directory cache entry version {}",
            manifest.version
        );
    }
    for object in &manifest.objects {
        ensure_directory_cache_object_file_is_plain_name(&object.file)?;
    }
    Ok(())
}

pub(crate) fn directory_cache_stdio_read_limit(max_size: Option<u64>) -> u64 {
    max_size
        .unwrap_or(DIRECTORY_CACHE_STDIO_MAX_BYTES)
        .min(DIRECTORY_CACHE_STDIO_MAX_BYTES)
}

pub(crate) fn read_file_limited(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let file = open_regular_file_no_follow(path)?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        bail!(
            "Directory cache file {} is too large to read: {} > {}",
            path.display(),
            len,
            max_bytes
        );
    }

    let mut bytes = Vec::new();
    let mut limited = file.take(max_bytes.saturating_add(1));
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!(
            "Directory cache file {} exceeded read limit {}",
            path.display(),
            max_bytes
        );
    }
    Ok(bytes)
}

fn read_optional_file_limited(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    match read_file_limited(path, max_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn read_directory_cache_optional_file(
    root: &Path,
    name: &str,
    max_size: Option<u64>,
) -> Result<Vec<u8>> {
    read_optional_file_limited(&root.join(name), directory_cache_stdio_read_limit(max_size))
}

pub(crate) fn serialize_directory_cache_manifest(
    manifest: &DirectoryCacheManifest,
) -> Result<Vec<u8>> {
    let bytes = bincode::serialize(manifest)?;
    if bytes.len() as u64 > DIRECTORY_CACHE_MANIFEST_MAX_BYTES {
        bail!(
            "Directory cache manifest is too large to store: {} > {}",
            bytes.len(),
            DIRECTORY_CACHE_MANIFEST_MAX_BYTES
        );
    }
    Ok(bytes)
}

pub(crate) fn read_directory_cache_manifest(root: &Path) -> Result<DirectoryCacheManifest> {
    let manifest: DirectoryCacheManifest = bincode::deserialize(&read_file_limited(
        &root.join(DIRECTORY_CACHE_MANIFEST_FILE),
        DIRECTORY_CACHE_MANIFEST_MAX_BYTES,
    )?)?;
    ensure_directory_cache_manifest_is_safe_to_read(&manifest)?;
    Ok(manifest)
}

pub(crate) fn open_directory_cache_object_file(root: &Path, file: &str) -> Result<fs::File> {
    ensure_directory_cache_object_file_is_plain_name(file)?;
    let path = root.join(DIRECTORY_CACHE_OBJECTS_DIR).join(file);
    open_regular_file_no_follow(&path).map_err(|_| DecompressionFailure.into())
}

fn ensure_plain_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "Directory cache path is not a plain directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn open_regular_file_no_follow(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(not(unix))]
    {
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache file is a symlink",
            ));
        }
    }

    let file = open_cache_file_no_follow(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_cache_file_no_follow(path: &Path) -> std::io::Result<fs::File> {
    use fs_err::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_cache_file_no_follow(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new().read(true).open(path)
}

pub(crate) struct DirectoryCacheRead {
    objects: Vec<DirectoryCacheObjectRead>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct DirectoryCacheObjectRead {
    key: String,
    file: fs::File,
    mode: Option<u32>,
}
impl DirectoryCacheRead {
    pub(crate) fn from_path(root: PathBuf, max_size: Option<u64>) -> Result<Self> {
        ensure_plain_directory(&root)?;
        ensure_plain_directory(&root.join(DIRECTORY_CACHE_OBJECTS_DIR))?;
        let manifest = read_directory_cache_manifest(&root)?;

        let mut objects = Vec::with_capacity(manifest.objects.len());
        for object in manifest.objects {
            let file = open_directory_cache_object_file(&root, &object.file)?;
            objects.push(DirectoryCacheObjectRead {
                key: object.key,
                file,
                mode: object.mode,
            });
        }

        let stdio_limit = directory_cache_stdio_read_limit(max_size);
        Ok(Self {
            objects,
            stdout: read_optional_file_limited(
                &root.join(DIRECTORY_CACHE_STDOUT_FILE),
                stdio_limit,
            )?,
            stderr: read_optional_file_limited(
                &root.join(DIRECTORY_CACHE_STDERR_FILE),
                stdio_limit,
            )?,
        })
    }

    fn object(&self, name: &str) -> Result<&DirectoryCacheObjectRead> {
        self.objects
            .iter()
            .find(|object| object.key == name)
            .ok_or_else(|| DecompressionFailure.into())
    }

    pub(crate) fn get_object<T>(&self, name: &str, to: &mut T) -> Result<Option<u32>>
    where
        T: Write,
    {
        let object = self.object(name)?;
        let mut file = PositionedFileReader::new(&object.file);
        std::io::copy(&mut file, to)?;
        Ok(object.mode)
    }

    pub(crate) fn copy_object_to_path(&self, name: &str, to: &Path) -> Result<Option<u32>> {
        let object = self.object(name)?;
        copy_file_reflink_or_copy(&object.file, to)?;
        Ok(object.mode)
    }

    pub(crate) fn get_bytes(&self, name: &str) -> Vec<u8> {
        match name {
            DIRECTORY_CACHE_STDOUT_FILE => self.stdout.clone(),
            DIRECTORY_CACHE_STDERR_FILE => self.stderr.clone(),
            _ => Vec::new(),
        }
    }
}
pub(crate) fn copy_file_reflink_or_copy(from: &fs::File, to: &Path) -> Result<u64> {
    match reflink_file(from, to) {
        Ok(()) => Ok(from.metadata()?.len()),
        Err(err) => {
            trace!(
                "Failed to reflink file to {}, falling back to copy: {}",
                to.display(),
                err
            );
            let mut from = PositionedFileReader::new(from);
            let mut to = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(to)?;
            std::io::copy(&mut from, &mut to).map_err(Into::into)
        }
    }
}

pub(crate) struct PositionedFileReader<'a> {
    file: &'a fs::File,
    offset: u64,
}

impl<'a> PositionedFileReader<'a> {
    pub(crate) fn new(file: &'a fs::File) -> Self {
        Self { file, offset: 0 }
    }
}

impl Read for PositionedFileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = read_file_at(self.file, buf, self.offset)?;
        self.offset += read as u64;
        Ok(read)
    }
}

#[cfg(unix)]
fn read_file_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use fs_err::os::unix::fs::FileExt;

    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_file_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;

    file.seek_read(buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_file_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut file = file.try_clone()?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    file.read(buf)
}

#[cfg(any(test, windows))]
fn reflink_aligned_ranges(len: u64, alignment: u64, max_chunk: u64) -> Vec<(u64, u64)> {
    if alignment == 0 || max_chunk < alignment {
        return Vec::new();
    }

    let aligned_len = len - (len % alignment);
    let chunk_size = (max_chunk / alignment) * alignment;
    if aligned_len == 0 || chunk_size == 0 {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < aligned_len {
        let byte_count = (aligned_len - offset).min(chunk_size);
        ranges.push((offset, byte_count));
        offset += byte_count;
    }
    ranges
}

#[cfg(target_os = "linux")]
fn reflink_file(from: &fs::File, to: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let to = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(to)?;
    let rc = unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn reflink_file(from: &fs::File, to: &Path) -> std::io::Result<()> {
    const WINDOWS_REFLINK_MAX_CHUNK: u64 = u32::MAX as u64;

    let len = from.metadata()?.len();
    let to_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(to)?;

    if len == 0 {
        return Ok(());
    }

    let cluster_size = windows_reflink_cluster_size(to)?;
    let ranges = reflink_aligned_ranges(len, cluster_size, WINDOWS_REFLINK_MAX_CHUNK);
    if ranges.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "file has no cluster-aligned range to reflink on Windows",
        ));
    }

    to_file.set_len(len)?;

    for (offset, byte_count) in &ranges {
        windows_duplicate_extents(from, &to_file, *offset, *byte_count)?;
    }

    let cloned_len = ranges
        .last()
        .map(|(offset, byte_count)| offset + byte_count)
        .unwrap_or(0);
    copy_file_tail(from, &to_file, cloned_len, len - cloned_len)?;

    Ok(())
}

#[cfg(windows)]
fn path_to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn windows_reflink_cluster_size(path: &Path) -> std::io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};

    let canonical_path = fs::canonicalize(path)?;
    let path_wide = path_to_wide(&canonical_path);
    let mut volume_root = vec![0_u16; 32768];
    let rc = unsafe {
        GetVolumePathNameW(
            path_wide.as_ptr(),
            volume_root.as_mut_ptr(),
            volume_root.len() as u32,
        )
    };
    if rc == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut sectors_per_cluster = 0;
    let mut bytes_per_sector = 0;
    let mut free_clusters = 0;
    let mut total_clusters = 0;
    let rc = unsafe {
        GetDiskFreeSpaceW(
            volume_root.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if rc == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let cluster_size = u64::from(sectors_per_cluster) * u64::from(bytes_per_sector);
    if cluster_size == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume reported zero cluster size",
        ));
    }

    Ok(cluster_size)
}

#[cfg(windows)]
fn windows_duplicate_extents(
    from: &fs::File,
    to: &fs::File,
    offset: u64,
    byte_count: u64,
) -> std::io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
    };

    let source_offset = i64::try_from(offset).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source offset is too large to reflink on Windows",
        )
    })?;
    let target_offset = source_offset;
    let byte_count = i64::try_from(byte_count).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "byte count is too large to reflink on Windows",
        )
    })?;

    let input = DUPLICATE_EXTENTS_DATA {
        FileHandle: from.as_raw_handle() as _,
        SourceFileOffset: source_offset,
        TargetFileOffset: target_offset,
        ByteCount: byte_count,
    };
    let mut bytes_returned = 0;
    let rc = unsafe {
        DeviceIoControl(
            to.as_raw_handle() as _,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &input as *const _ as *const core::ffi::c_void,
            size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if rc == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn copy_file_tail(
    from: &fs::File,
    to: &fs::File,
    offset: u64,
    byte_count: u64,
) -> std::io::Result<()> {
    if byte_count == 0 {
        return Ok(());
    }

    let mut from = from.try_clone()?;
    let mut to = to.try_clone()?;
    from.seek(std::io::SeekFrom::Start(offset))?;
    to.seek(std::io::SeekFrom::Start(offset))?;

    let mut tail = from.take(byte_count);
    let copied = std::io::copy(&mut tail, &mut to)?;
    if copied != byte_count {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "source file ended while copying reflink tail",
        ));
    }

    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn reflink_file(_from: &fs::File, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reflink is not supported on this platform",
    ))
}

/// File identity metadata observed by the client before it asks the daemon to
/// reopen a file-backed object.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StorageFileObjectIdentity {
    pub len: u64,
    pub modified_secs: Option<u64>,
    pub modified_nanos: Option<u32>,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

impl StorageFileObjectIdentity {
    pub(crate) fn from_file(file: &std::fs::File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        let (device, inode) = file_identity(file, &metadata)?;

        let (modified_secs, modified_nanos) = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| (Some(duration.as_secs()), Some(duration.subsec_nanos())))
            .unwrap_or((None, None));

        Ok(Self {
            len: metadata.len(),
            modified_secs,
            modified_nanos,
            device,
            inode,
        })
    }

    pub(crate) fn is_stable(&self) -> bool {
        self.device.is_some()
            && self.inode.is_some()
            && self.modified_secs.is_some()
            && self.modified_nanos.is_some()
    }
}

#[cfg(unix)]
fn file_identity(
    _file: &std::fs::File,
    metadata: &std::fs::Metadata,
) -> std::io::Result<(Option<u64>, Option<u64>)> {
    use std::os::unix::fs::MetadataExt;

    Ok((Some(metadata.dev()), Some(metadata.ino())))
}

#[cfg(windows)]
fn file_identity(
    file: &std::fs::File,
    _metadata: &std::fs::Metadata,
) -> std::io::Result<(Option<u64>, Option<u64>)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let rc = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) };
    if rc == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    let file_index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);

    Ok((Some(u64::from(info.dwVolumeSerialNumber)), Some(file_index)))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(
    _file: &std::fs::File,
    _metadata: &std::fs::Metadata,
) -> std::io::Result<(Option<u64>, Option<u64>)> {
    Ok((None, None))
}

/// A file-backed object source for client-side storage writes.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageFileObjectSource {
    pub key: String,
    pub path: PathBuf,
    pub mode: Option<u32>,
    pub identity: StorageFileObjectIdentity,
}

pub(crate) struct CacheWriteObject {
    pub(crate) key: String,
    pub(crate) source: CacheWriteObjectSource,
    pub(crate) mode: Option<u32>,
}

pub(crate) enum CacheWriteObjectSource {
    File {
        file: fs::File,
        path: Option<PathBuf>,
    },
    Bytes(Vec<u8>),
}

impl CacheWriteObject {
    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn mode(&self) -> Option<u32> {
        self.mode
    }

    #[cfg(test)]
    pub(crate) fn try_clone(&self) -> Result<Self> {
        let source = match &self.source {
            CacheWriteObjectSource::File { file, path } => CacheWriteObjectSource::File {
                file: file.try_clone()?,
                path: path.clone(),
            },
            CacheWriteObjectSource::Bytes(bytes) => CacheWriteObjectSource::Bytes(bytes.clone()),
        };
        Ok(Self {
            key: self.key.clone(),
            source,
            mode: self.mode,
        })
    }

    pub(crate) fn file_source(&self) -> Result<Option<StorageFileObjectSource>> {
        match &self.source {
            CacheWriteObjectSource::File {
                file,
                path: Some(path),
            } => {
                let identity = StorageFileObjectIdentity::from_file(file.file())?;
                if identity.is_stable() {
                    Ok(Some(StorageFileObjectSource {
                        key: self.key.clone(),
                        path: path.clone(),
                        mode: self.mode,
                        identity,
                    }))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn write_to_path(&self, to: &Path) -> Result<()> {
        match &self.source {
            CacheWriteObjectSource::File { file, .. } => {
                copy_file_reflink_or_copy(file, to)?;
            }
            CacheWriteObjectSource::Bytes(bytes) => {
                fs::write(to, bytes)?;
            }
        }
        Ok(())
    }

    pub(crate) fn write_to_zip(self, zip: &mut ZipWriter<Cursor<Vec<u8>>>) -> Result<()> {
        let Self { key, source, mode } = self;
        match source {
            CacheWriteObjectSource::File { file, .. } => {
                let mut reader = PositionedFileReader::new(&file);
                put_zip_object(zip, &key, &mut reader, mode)
            }
            CacheWriteObjectSource::Bytes(bytes) => {
                put_zip_object(zip, &key, &mut Cursor::new(bytes), mode)
            }
        }
    }
}
pub(crate) fn put_zip_object<T>(
    zip: &mut ZipWriter<Cursor<Vec<u8>>>,
    name: &str,
    from: &mut T,
    mode: Option<u32>,
) -> Result<()>
where
    T: Read,
{
    // We're going to declare the compression method as "stored",
    // but we're actually going to store zstd-compressed blobs.
    let opts = FileOptions::default().compression_method(CompressionMethod::Stored);
    let opts = if let Some(mode) = mode {
        opts.unix_permissions(mode)
    } else {
        opts
    };
    zip.start_file(name, opts)
        .context("Failed to start cache entry object")?;

    let compression_level = std::env::var("SCCACHE_CACHE_ZSTD_LEVEL")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(3);
    zstd::stream::copy_encode(from, zip, compression_level)?;
    Ok(())
}
pub(crate) fn cache_write_from_file_objects(
    objects: Vec<StorageFileObjectSource>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<CacheWrite> {
    let mut entry = CacheWrite::new();
    for object in objects {
        let StorageFileObjectSource {
            key,
            path,
            mode,
            identity,
        } = object;
        let file = fs::File::open(&path)
            .with_context(|| format!("StoragePutFileObjects: open {}", path.display()))?;
        verify_storage_file_object_identity(&path, &file, &identity)?;
        entry.put_file_object(key, file, mode);
    }
    entry.put_stdout(&stdout)?;
    entry.put_stderr(&stderr)?;
    Ok(entry)
}

fn verify_storage_file_object_identity(
    path: &Path,
    file: &fs::File,
    expected: &StorageFileObjectIdentity,
) -> Result<()> {
    if !expected.is_stable() {
        bail!(
            "StoragePutFileObjects: {} does not have stable file identity",
            path.display()
        );
    }

    let actual = StorageFileObjectIdentity::from_file(file.file())?;
    if &actual != expected {
        bail!(
            "StoragePutFileObjects: {} changed after the client opened it",
            path.display()
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::super::cache_io::{CacheRead, FileObjectSource};
    use super::*;
    #[test]
    fn cache_write_from_file_objects_stores_matching_path() {
        let tempdir = tempfile::tempdir().unwrap();
        let object_path = tempdir.path().join("object.o");
        fs::write(&object_path, b"object bytes").unwrap();

        let entry = cache_write_from_file_objects(
            vec![StorageFileObjectSource {
                key: "test_key".to_string(),
                path: object_path.clone(),
                mode: None,
                identity: StorageFileObjectIdentity::from_file(
                    fs::File::open(&object_path).unwrap().file(),
                )
                .unwrap(),
            }],
            b"stdout".to_vec(),
            b"stderr".to_vec(),
        )
        .unwrap();

        let mut read = CacheRead::from(std::io::Cursor::new(entry.finish().unwrap())).unwrap();
        let mut object = Vec::new();
        read.get_object("test_key", &mut object).unwrap();
        assert_eq!(object, b"object bytes");
        assert_eq!(read.get_stdout(), b"stdout");
        assert_eq!(read.get_stderr(), b"stderr");
    }

    #[test]
    fn cache_write_from_file_objects_rejects_replaced_path() {
        let tempdir = tempfile::tempdir().unwrap();
        let object_path = tempdir.path().join("object.o");
        fs::write(&object_path, b"object bytes").unwrap();
        let identity =
            StorageFileObjectIdentity::from_file(fs::File::open(&object_path).unwrap().file())
                .unwrap();

        fs::write(&object_path, b"replacement object bytes").unwrap();

        let err = match cache_write_from_file_objects(
            vec![StorageFileObjectSource {
                key: "test_key".to_string(),
                path: object_path,
                mode: None,
                identity,
            }],
            Vec::new(),
            Vec::new(),
        ) {
            Ok(_) => panic!("expected replaced path to be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("changed after the client opened it")
        );
    }

    #[test]
    fn directory_cache_manifest_serialization_enforces_read_limit() {
        let small = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![],
        };
        serialize_directory_cache_manifest(&small).unwrap();

        let oversized = DirectoryCacheManifest {
            version: DIRECTORY_CACHE_MANIFEST_VERSION,
            objects: vec![DirectoryCacheObject {
                key: "x".repeat(DIRECTORY_CACHE_MANIFEST_MAX_BYTES as usize),
                file: "0".to_owned(),
                mode: None,
            }],
        };
        assert!(serialize_directory_cache_manifest(&oversized).is_err());
    }
    #[test]
    fn cache_write_from_objects_keeps_file_sources() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let pool = runtime.handle();
        let tempdir = tempfile::tempdir().unwrap();
        let object_path = tempdir.path().join("object.o");
        fs::write(&object_path, b"object bytes").unwrap();

        let entry = runtime
            .block_on(CacheWrite::from_objects(
                vec![FileObjectSource {
                    key: "test_key".to_string(),
                    path: object_path.clone(),
                    optional: false,
                }],
                pool,
            ))
            .unwrap();

        let sources = entry.file_object_sources().unwrap().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].key, "test_key");
        assert_eq!(sources[0].path, object_path);
        assert_eq!(
            sources[0].identity,
            StorageFileObjectIdentity::from_file(fs::File::open(&sources[0].path).unwrap().file())
                .unwrap()
        );
        let raw = entry.finish().unwrap();
        let mut read = CacheRead::from(Cursor::new(raw)).unwrap();
        let mut object = Vec::new();
        read.get_object("test_key", &mut object).unwrap();
        assert_eq!(object, b"object bytes");
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn cache_write_file_sources_require_stable_identity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let pool = runtime.handle();
        let tempdir = tempfile::tempdir().unwrap();
        let object_path = tempdir.path().join("object.o");
        fs::write(&object_path, b"object bytes").unwrap();

        let entry = runtime
            .block_on(CacheWrite::from_objects(
                vec![FileObjectSource {
                    key: "test_key".to_string(),
                    path: object_path,
                    optional: false,
                }],
                pool,
            ))
            .unwrap();

        assert!(entry.file_object_sources().unwrap().is_none());
    }
    #[test]
    fn cloned_file_sources_can_finish_concurrently() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let pool = runtime.handle();
        let tempdir = tempfile::tempdir().unwrap();
        let object_path = tempdir.path().join("object.o");
        let object_bytes: Vec<u8> = (0..(2 * 1024 * 1024))
            .map(|index| (index % 251) as u8)
            .collect();
        fs::write(&object_path, &object_bytes).unwrap();

        let entry = runtime
            .block_on(CacheWrite::from_objects(
                vec![FileObjectSource {
                    key: "test_key".to_string(),
                    path: object_path,
                    optional: false,
                }],
                pool,
            ))
            .unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles = (0..4)
            .map(|_| {
                let entry = entry.try_clone().unwrap();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let raw = entry.finish().unwrap();
                    let mut read = CacheRead::from(Cursor::new(raw)).unwrap();
                    let mut object = Vec::new();
                    read.get_object("test_key", &mut object).unwrap();
                    object
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert_eq!(handle.join().unwrap(), object_bytes);
        }
    }
    #[test]
    fn reflink_aligned_ranges_split_on_alignment_and_max_chunk() {
        assert_eq!(
            reflink_aligned_ranges(13 * 1024, 4 * 1024, 8 * 1024),
            vec![(0, 8 * 1024), (8 * 1024, 4 * 1024)]
        );
    }

    #[test]
    fn reflink_aligned_ranges_skip_unaligned_only_files() {
        assert!(reflink_aligned_ranges(4095, 4096, 8192).is_empty());
        assert!(reflink_aligned_ranges(8192, 4096, 2048).is_empty());
        assert!(reflink_aligned_ranges(8192, 0, 8192).is_empty());
    }
}
