// Filesystem Abstraction Layer (FSAL)
//
// Provides a common interface for filesystem operations, abstracting the
// underlying storage backend (local filesystem, network filesystem, etc.)

pub mod handle;
pub mod local;
pub mod multi_export;

// Future backends (uncomment when implemented)
// #[cfg(feature = "s3")]
// pub mod s3;
// #[cfg(feature = "ceph")]
// pub mod ceph;
// #[cfg(test)]
// pub mod memory;

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

#[allow(unused_imports)]
pub use handle::{FileHandle, FileHandleExt, HandleManager};
pub use local::LocalFilesystem;
pub use multi_export::MultiExportFilesystem;

/// File attributes
///
/// Represents metadata about a file or directory.
/// Maps to NFSv3 fattr3 structure.
#[derive(Debug, Clone)]
pub struct FileAttributes {
    /// File type
    pub ftype: FileType,
    /// File mode (permissions)
    pub mode: u32,
    /// Number of hard links
    pub nlink: u32,
    /// User ID
    pub uid: u32,
    /// Group ID
    pub gid: u32,
    /// File size in bytes
    pub size: u64,
    /// Disk space used (in bytes)
    pub used: u64,
    /// Device ID (for special files)
    pub rdev: (u32, u32),
    /// Filesystem ID
    pub fsid: u64,
    /// File ID (inode number)
    pub fileid: u64,
    /// Last access time
    pub atime: FileTime,
    /// Last modification time
    pub mtime: FileTime,
    /// Last status change time
    pub ctime: FileTime,
}

/// File type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    RegularFile = 1,
    Directory = 2,
    BlockDevice = 3,
    CharDevice = 4,
    SymbolicLink = 5,
    Socket = 6,
    NamedPipe = 7,
}

/// File time (seconds, nanoseconds)
#[derive(Debug, Clone, Copy)]
pub struct FileTime {
    pub seconds: u64,
    pub nseconds: u32,
}

/// Directory entry
///
/// Represents a single entry in a directory listing.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// File ID (inode number)
    pub fileid: u64,
    /// Entry name
    pub name: String,
    /// File type
    pub file_type: FileType,
}

/// Filesystem trait
///
/// This trait defines the interface that all filesystem backends must implement.
/// It provides operations for file/directory access, metadata queries, and I/O.
///
/// Note: root handles for individual exports are obtained via
/// [`ExportRegistry::root_handle_for`] — they don't live on `Filesystem`
/// because a multi-export backend has no single "the" root handle.
#[async_trait]
pub trait Filesystem: Send + Sync {
    /// Look up a name in a directory
    ///
    /// Given a directory handle and a filename, return the file handle
    /// for the named entry.
    ///
    /// # Arguments
    /// * `dir_handle` - File handle of the directory
    /// * `name` - Name to look up
    ///
    /// # Returns
    /// File handle of the found entry
    async fn lookup(&self, dir_handle: &FileHandle, name: &str) -> Result<FileHandle>;

    /// Get file attributes
    ///
    /// # Arguments
    /// * `handle` - File handle
    ///
    /// # Returns
    /// File attributes
    async fn getattr(&self, handle: &FileHandle) -> Result<FileAttributes>;

    /// Read data from a file
    ///
    /// # Arguments
    /// * `handle` - File handle
    /// * `offset` - Starting offset
    /// * `count` - Number of bytes to read
    ///
    /// # Returns
    /// Vector of bytes read (may be shorter than count if EOF reached)
    async fn read(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<Vec<u8>>;

    /// Read directory entries
    ///
    /// # Arguments
    /// * `dir_handle` - Directory handle
    /// * `cookie` - Starting position (0 = from beginning)
    /// * `count` - Maximum number of entries to return
    ///
    /// # Returns
    /// Tuple of (entries, eof) where eof indicates if all entries were returned
    async fn readdir(
        &self,
        dir_handle: &FileHandle,
        cookie: u64,
        count: u32,
    ) -> Result<(Vec<DirEntry>, bool)>;

    /// Write data to a file
    ///
    /// # Arguments
    /// * `handle` - File handle
    /// * `offset` - Starting offset
    /// * `data` - Data to write
    ///
    /// # Returns
    /// Number of bytes actually written
    async fn write(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<u32>;

    /// Set file size (truncate/extend)
    ///
    /// # Arguments
    /// * `handle` - File handle
    /// * `size` - New size in bytes
    async fn setattr_size(&self, handle: &FileHandle, size: u64) -> Result<()>;

    /// Set file mode (permissions)
    ///
    /// # Arguments
    /// * `handle` - File handle
    /// * `mode` - New file mode (permissions)
    async fn setattr_mode(&self, handle: &FileHandle, mode: u32) -> Result<()>;

    /// Set file owner (uid/gid)
    ///
    /// # Arguments
    /// * `handle` - File handle
    /// * `uid` - New user ID (None to keep current)
    /// * `gid` - New group ID (None to keep current)
    async fn setattr_owner(
        &self,
        handle: &FileHandle,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<()>;

    /// Create a file
    ///
    /// # Arguments
    /// * `dir_handle` - Directory handle
    /// * `name` - Name of new file
    /// * `mode` - File permissions
    ///
    /// # Returns
    /// File handle of created file
    async fn create(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle>;

    /// Remove a file
    ///
    /// # Arguments
    /// * `dir_handle` - Directory handle
    /// * `name` - Name of file to remove
    async fn remove(&self, dir_handle: &FileHandle, name: &str) -> Result<()>;

    /// Create a directory
    ///
    /// # Arguments
    /// * `dir_handle` - Parent directory handle
    /// * `name` - Name of new directory
    /// * `mode` - Directory permissions
    ///
    /// # Returns
    /// File handle of created directory
    async fn mkdir(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle>;

    /// Remove a directory
    ///
    /// # Arguments
    /// * `dir_handle` - Parent directory handle
    /// * `name` - Name of directory to remove
    async fn rmdir(&self, dir_handle: &FileHandle, name: &str) -> Result<()>;

    /// Rename a file or directory
    ///
    /// # Arguments
    /// * `from_dir_handle` - Source directory handle
    /// * `from_name` - Source name
    /// * `to_dir_handle` - Target directory handle
    /// * `to_name` - Target name
    async fn rename(
        &self,
        from_dir_handle: &FileHandle,
        from_name: &str,
        to_dir_handle: &FileHandle,
        to_name: &str,
    ) -> Result<()>;

    /// Create a symbolic link
    ///
    /// # Arguments
    /// * `dir_handle` - Parent directory handle
    /// * `name` - Symlink name
    /// * `target` - Target path the symlink points to
    async fn symlink(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        target: &str,
    ) -> Result<FileHandle>;

    /// Read a symbolic link
    ///
    /// # Arguments
    /// * `handle` - Symlink file handle
    ///
    /// # Returns
    /// Target path the symlink points to
    async fn readlink(&self, handle: &FileHandle) -> Result<String>;

    /// Create a hard link
    ///
    /// # Arguments
    /// * `file_handle` - Source file handle
    /// * `dir_handle` - Target directory handle
    /// * `name` - New link name
    ///
    /// # Returns
    /// The file handle (should be the same as source file handle since they share the same inode)
    async fn link(
        &self,
        file_handle: &FileHandle,
        dir_handle: &FileHandle,
        name: &str,
    ) -> Result<FileHandle>;

    /// Commit cached data to stable storage
    ///
    /// Ensures that all data for the specified file that was written with WRITE
    /// procedure calls with stable=UNSTABLE are committed to stable storage.
    ///
    /// # Arguments
    /// * `handle` - File handle
    /// * `offset` - Starting offset (0 means from beginning)
    /// * `count` - Number of bytes (0 means to end of file)
    ///
    /// # Returns
    /// Ok if data is committed to stable storage
    async fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<()>;

    /// Create a special file (device, FIFO, socket)
    ///
    /// # Arguments
    /// * `dir_handle` - Parent directory handle
    /// * `name` - Name of special file to create
    /// * `file_type` - Type of special file (BlockDevice, CharDevice, Socket, NamedPipe)
    /// * `mode` - File permissions
    /// * `rdev` - Device numbers (major, minor) for device files, ignored for FIFO/Socket
    ///
    /// # Returns
    /// File handle of created special file
    async fn mknod(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        file_type: FileType,
        mode: u32,
        rdev: (u32, u32),
    ) -> Result<FileHandle>;
}

/// Metadata describing a single configured export.
///
/// Returned by [`ExportRegistry::list_exports`] so MOUNT EXPORT (and startup
/// banners) can enumerate exports without touching backend internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportInfo {
    /// Export path as advertised to NFS clients (e.g. `/data`).
    pub name: String,
    /// Non-zero export uid embedded in every file handle for this export.
    pub uid: u32,
    /// True if writes are denied against this export.
    pub read_only: bool,
}

/// Registry of NFS exports, decoupled from per-handle filesystem operations.
///
/// MOUNT MNT and MOUNT EXPORT operate against this view; NFS write paths
/// will use [`is_read_only`](Self::is_read_only) to gate mutating operations
/// once Phase 5 lands. [`export_for_handle`](Self::export_for_handle) is
/// exposed mostly for diagnostics and tests.
///
/// `is_read_only` and `export_for_handle` are not wired into the NFS write
/// path yet — they carry `#[allow(dead_code)]` so the trait shape stays
/// stable through Phase 5 without producing build warnings now.
pub trait ExportRegistry: Send + Sync {
    /// Look up the root file handle for the export advertised as `name`.
    ///
    /// Returns `None` if no export matches.
    fn root_handle_for(&self, name: &str) -> Option<FileHandle>;

    /// List every configured export in deterministic order.
    fn list_exports(&self) -> Vec<ExportInfo>;

    /// Report whether the export that owns `handle` is read-only.
    ///
    /// Returns `false` for handles whose export uid prefix is unknown or
    /// missing — write paths should treat that as a stale-handle error
    /// elsewhere; this method only answers the read-only question.
    #[allow(dead_code)]
    fn is_read_only(&self, handle: &FileHandle) -> bool;

    /// Decode the export uid embedded in `handle`'s prefix, if present.
    #[allow(dead_code)]
    fn export_for_handle(&self, handle: &FileHandle) -> Option<u32>;
}

/// Combined trait used by [`crate::rpc::server::RpcServer`].
///
/// The server holds a single `Arc<dyn NfsBackend>` and hands each dispatcher
/// the trait view it needs: MOUNT receives `&dyn ExportRegistry`, NFS still
/// receives `&dyn Filesystem`.
pub trait NfsBackend: Filesystem + ExportRegistry {}

impl<T: Filesystem + ExportRegistry> NfsBackend for T {}

/// FSAL-side backend configuration.
///
/// Mirrors [`crate::config::BackendConfig`] one variant at a time. v1 only
/// ships the `Local` variant; future S3/Ceph backends gain their own
/// variants here so [`BackendConfig::create_filesystem`] can dispatch by
/// `match self`.
///
/// Phase 3 moved the production translation into
/// [`MultiExportFilesystem::build_from_config`], so this enum is now
/// referenced only from the per-operation NFS unit tests in `src/nfs/*.rs`.
/// Marked `#[allow(dead_code)]` because the main binary doesn't see those
/// `#[cfg(test)]` call sites.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum BackendConfig {
    /// Local filesystem backend rooted at `path`.
    Local { path: PathBuf },
}

#[allow(dead_code)]
impl BackendConfig {
    /// Build the backend for this configuration, binding it to `export_uid`
    /// so every file handle it produces carries that uid in its prefix.
    ///
    /// Returns a `Box<LocalFilesystem>` (concrete) rather than a
    /// `Box<dyn Filesystem>`: the per-op NFS unit tests need the inherent
    /// `LocalFilesystem::root_file_handle()` to seed their test handles, and
    /// no longer have a `Filesystem::root_handle()` trait method to fall
    /// back on. When a second backend variant lands, this signature widens
    /// to `Result<Box<dyn FilesystemWithRoot>>` or similar — for now the
    /// concrete return keeps the test surface minimal.
    pub fn create_filesystem(&self, export_uid: u32) -> Result<Box<LocalFilesystem>> {
        match self {
            BackendConfig::Local { path } => {
                let fs = LocalFilesystem::new(path, export_uid)?;
                Ok(Box::new(fs))
            }
        }
    }
}
