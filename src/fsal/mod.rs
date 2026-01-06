// Filesystem Abstraction Layer (FSAL)
//
// Provides a common interface for filesystem operations, abstracting the
// underlying storage backend (local filesystem, network filesystem, etc.)

pub mod handle;
pub mod local;

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

pub use handle::{FileHandle, HandleManager};
pub use local::LocalFilesystem;

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
#[async_trait]
pub trait Filesystem: Send + Sync {
    /// Get the root file handle
    ///
    /// This is typically the starting point for all filesystem operations.
    async fn root_handle(&self) -> FileHandle;

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
    async fn readdir(&self, dir_handle: &FileHandle, cookie: u64, count: u32) -> Result<(Vec<DirEntry>, bool)>;

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
    async fn setattr_owner(&self, handle: &FileHandle, uid: Option<u32>, gid: Option<u32>) -> Result<()>;

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
    async fn symlink(&self, dir_handle: &FileHandle, name: &str, target: &str) -> Result<FileHandle>;

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
    async fn link(&self, file_handle: &FileHandle, dir_handle: &FileHandle, name: &str) -> Result<FileHandle>;

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

/// Filesystem backend types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendType {
    /// Local filesystem backend
    Local,
    /// S3 backend (future)
    #[allow(dead_code)]
    S3,
    /// Ceph backend (future)
    #[allow(dead_code)]
    Ceph,
    /// In-memory backend (testing)
    #[allow(dead_code)]
    Memory,
}

/// Filesystem backend configuration
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Backend type
    pub backend_type: BackendType,
    /// Root path for local backend
    pub local_root: Option<PathBuf>,
    /// S3 configuration (future)
    #[allow(dead_code)]
    pub s3_config: Option<S3Config>,
    /// Ceph configuration (future)
    #[allow(dead_code)]
    pub ceph_config: Option<CephConfig>,
}

/// S3 backend configuration (placeholder for future)
#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

/// Ceph backend configuration (placeholder for future)
#[derive(Debug, Clone)]
pub struct CephConfig {
    pub monitors: Vec<String>,
    pub pool: String,
}

impl BackendConfig {
    /// Create a local filesystem backend configuration
    pub fn local<P: Into<PathBuf>>(root: P) -> Self {
        Self {
            backend_type: BackendType::Local,
            local_root: Some(root.into()),
            s3_config: None,
            ceph_config: None,
        }
    }

    /// Create filesystem instance from configuration
    pub fn create_filesystem(&self) -> Result<Box<dyn Filesystem>> {
        match self.backend_type {
            BackendType::Local => {
                let root = self
                    .local_root
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Local root path not configured"))?;
                let fs = LocalFilesystem::new(root)?;
                Ok(Box::new(fs))
            }
            BackendType::S3 => {
                // TODO: Implement S3 backend
                Err(anyhow::anyhow!("S3 backend not yet implemented"))
            }
            BackendType::Ceph => {
                // TODO: Implement Ceph backend
                Err(anyhow::anyhow!("Ceph backend not yet implemented"))
            }
            BackendType::Memory => {
                // TODO: Implement memory backend
                Err(anyhow::anyhow!("Memory backend not yet implemented"))
            }
        }
    }
}
