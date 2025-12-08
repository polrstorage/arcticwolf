// Local Filesystem Backend
//
// Implements the Filesystem trait for local filesystem access.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use super::handle::{FileHandle, HandleManager};
use super::{DirEntry, FileAttributes, FileTime, FileType, Filesystem};

/// Local filesystem implementation
pub struct LocalFilesystem {
    /// Root directory for exports
    root_path: PathBuf,
    /// File handle manager
    handle_manager: HandleManager,
    /// Root file handle
    root_handle: FileHandle,
}

impl LocalFilesystem {
    /// Create a new local filesystem backend
    ///
    /// # Arguments
    /// * `root_path` - Root directory to export (e.g., "/export")
    pub fn new<P: AsRef<Path>>(root_path: P) -> Result<Self> {
        let root_path = root_path.as_ref().canonicalize().context(format!(
            "Failed to canonicalize root path: {:?}",
            root_path.as_ref()
        ))?;

        // Verify root path exists and is a directory
        let metadata = fs::metadata(&root_path)
            .context(format!("Failed to stat root path: {:?}", root_path))?;

        if !metadata.is_dir() {
            return Err(anyhow!("Root path is not a directory: {:?}", root_path));
        }

        let handle_manager = HandleManager::new();

        // Create root handle
        let root_handle = handle_manager.create_handle(root_path.clone());

        debug!("LocalFilesystem created with root: {:?}", root_path);

        Ok(Self {
            root_path,
            handle_manager,
            root_handle,
        })
    }

    /// Resolve a file handle to a full path
    fn resolve_handle(&self, handle: &FileHandle) -> Result<PathBuf> {
        self.handle_manager
            .lookup_path(handle)
            .ok_or_else(|| anyhow!("Invalid file handle"))
    }

    /// Validate that a path is within the export root
    ///
    /// This prevents path traversal attacks (e.g., "../../../etc/passwd")
    fn validate_path(&self, path: &Path) -> Result<()> {
        // For paths that don't exist yet, we need to validate the parent directory
        // and then check if the final component would be safe
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            // This shouldn't happen in our code, but handle it defensively
            self.root_path.join(path)
        };

        // Check if path exists - if yes, canonicalize it
        // If not, canonicalize the parent and validate the final component
        let canonical = if absolute_path.exists() {
            absolute_path
                .canonicalize()
                .context(format!("Failed to canonicalize existing path: {:?}", path))?
        } else {
            // Get parent and canonicalize that
            let parent = absolute_path
                .parent()
                .ok_or_else(|| anyhow!("Path has no parent: {:?}", absolute_path))?;

            let canonical_parent = parent
                .canonicalize()
                .context(format!("Failed to canonicalize parent path: {:?}", parent))?;

            // Check if parent is within root
            if !canonical_parent.starts_with(&self.root_path) {
                warn!(
                    "Path traversal attempt: parent {:?} is outside root {:?}",
                    canonical_parent, self.root_path
                );
                return Err(anyhow!("Path is outside export root"));
            }

            // Get final component and validate it doesn't contain traversal attempts
            if let Some(file_name) = absolute_path.file_name() {
                let file_name_str = file_name
                    .to_str()
                    .ok_or_else(|| anyhow!("Invalid filename encoding"))?;

                if file_name_str.contains("..") || file_name_str.contains('/') {
                    return Err(anyhow!("Invalid filename: {}", file_name_str));
                }

                // Return the would-be canonical path
                canonical_parent.join(file_name)
            } else {
                return Err(anyhow!("Path has no filename component"));
            }
        };

        if !canonical.starts_with(&self.root_path) {
            warn!(
                "Path traversal attempt: {:?} is outside root {:?}",
                canonical, self.root_path
            );
            return Err(anyhow!("Path is outside export root"));
        }

        Ok(())
    }

    /// Convert std::fs::Metadata to FileAttributes
    fn metadata_to_attr(&self, metadata: &fs::Metadata, path: &Path) -> FileAttributes {
        let ftype = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_file() {
            FileType::RegularFile
        } else if metadata.is_symlink() {
            FileType::SymbolicLink
        } else {
            FileType::RegularFile // Default
        };

        FileAttributes {
            ftype,
            mode: metadata.permissions().mode(),
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: metadata.len(),
            used: metadata.blocks() * 512, // blocks are typically 512 bytes
            rdev: (metadata.rdev() as u32, 0),
            fsid: metadata.dev(),
            fileid: metadata.ino(),
            atime: FileTime {
                seconds: metadata.atime() as u64,
                nseconds: metadata.atime_nsec() as u32,
            },
            mtime: FileTime {
                seconds: metadata.mtime() as u64,
                nseconds: metadata.mtime_nsec() as u32,
            },
            ctime: FileTime {
                seconds: metadata.ctime() as u64,
                nseconds: metadata.ctime_nsec() as u32,
            },
        }
    }
}

impl Filesystem for LocalFilesystem {
    fn root_handle(&self) -> FileHandle {
        self.root_handle.clone()
    }

    fn lookup(&self, dir_handle: &FileHandle, name: &str) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid filename: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Check if file exists
        if !full_path.exists() {
            return Err(anyhow!("File not found: {}", name));
        }

        // Create or get existing handle
        let handle = self.handle_manager.create_handle(full_path);

        debug!("LOOKUP: {:?}/{} -> handle", dir_path, name);

        Ok(handle)
    }

    fn getattr(&self, handle: &FileHandle) -> Result<FileAttributes> {
        let path = self.resolve_handle(handle)?;

        let metadata = fs::metadata(&path).context(format!("Failed to stat: {:?}", path))?;

        Ok(self.metadata_to_attr(&metadata, &path))
    }

    fn read(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<Vec<u8>> {
        let path = self.resolve_handle(handle)?;

        let mut file =
            fs::File::open(&path).context(format!("Failed to open file: {:?}", path))?;

        // Seek to offset
        file.seek(SeekFrom::Start(offset))
            .context("Failed to seek")?;

        // Read up to count bytes
        let mut buffer = vec![0u8; count as usize];
        let bytes_read = file.read(&mut buffer).context("Failed to read file")?;

        // Truncate buffer to actual bytes read
        buffer.truncate(bytes_read);

        debug!(
            "READ: {:?} offset={} count={} -> {} bytes",
            path, offset, count, bytes_read
        );

        Ok(buffer)
    }

    fn readdir(&self, dir_handle: &FileHandle, cookie: u64, count: u32) -> Result<(Vec<DirEntry>, bool)> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Verify it's a directory
        let metadata = fs::metadata(&dir_path)
            .context(format!("Failed to stat directory: {:?}", dir_path))?;

        if !metadata.is_dir() {
            return Err(anyhow!("Not a directory: {:?}", dir_path));
        }

        // Read directory entries
        let read_dir = fs::read_dir(&dir_path)
            .context(format!("Failed to read directory: {:?}", dir_path))?;

        // Collect all entries
        let mut entries: Vec<DirEntry> = Vec::new();

        for (index, entry_result) in read_dir.enumerate() {
            let entry = entry_result.context("Failed to read directory entry")?;
            let entry_path = entry.path();
            let entry_metadata = entry.metadata()
                .context(format!("Failed to get metadata for: {:?}", entry_path))?;

            let file_type = if entry_metadata.is_dir() {
                FileType::Directory
            } else if entry_metadata.is_file() {
                FileType::RegularFile
            } else if entry_metadata.is_symlink() {
                FileType::SymbolicLink
            } else {
                FileType::RegularFile // Default
            };

            let name = entry.file_name()
                .to_string_lossy()
                .to_string();

            // Skip entries before cookie (cookie is 0-based index + 1)
            if cookie > 0 && (index as u64) < cookie {
                continue;
            }

            entries.push(DirEntry {
                fileid: entry_metadata.ino(),
                name,
                file_type,
            });

            // Check if we've reached the requested count
            if entries.len() >= count as usize {
                debug!(
                    "READDIR: {:?} cookie={} count={} -> {} entries (more available)",
                    dir_path, cookie, count, entries.len()
                );
                return Ok((entries, false)); // Not EOF, more entries available
            }
        }

        debug!(
            "READDIR: {:?} cookie={} count={} -> {} entries (EOF)",
            dir_path, cookie, count, entries.len()
        );

        Ok((entries, true)) // EOF reached
    }

    fn write(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        let path = self.resolve_handle(handle)?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path)
            .context(format!("Failed to open file for writing: {:?}", path))?;

        // Seek to offset
        file.seek(SeekFrom::Start(offset))
            .context("Failed to seek")?;

        // Write data
        let bytes_written = file.write(data).context("Failed to write file")?;

        // Flush to disk
        file.sync_all().context("Failed to sync file")?;

        debug!(
            "WRITE: {:?} offset={} count={} -> {} bytes",
            path,
            offset,
            data.len(),
            bytes_written
        );

        Ok(bytes_written as u32)
    }

    fn setattr_size(&self, handle: &FileHandle, size: u64) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .context(format!("Failed to open file for setattr: {:?}", path))?;

        file.set_len(size)
            .context("Failed to set file size")?;

        debug!("SETATTR: {:?} size={}", path, size);

        Ok(())
    }

    fn setattr_mode(&self, handle: &FileHandle, mode: u32) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(&path, permissions)
            .context(format!("Failed to set permissions: {:?}", path))?;

        debug!("SETATTR: {:?} mode={:o}", path, mode);

        Ok(())
    }

    fn setattr_owner(&self, handle: &FileHandle, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        // Note: chown requires root privileges on Unix systems
        // For now, we'll just log this and return success
        // In production, you might want to use nix::unistd::chown
        debug!("SETATTR: {:?} uid={:?} gid={:?} (not implemented)", path, uid, gid);

        Ok(())
    }

    fn create(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid filename: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Create file
        let file = fs::File::create(&full_path)
            .context(format!("Failed to create file: {:?}", full_path))?;

        // Set permissions
        let permissions = fs::Permissions::from_mode(mode);
        file.set_permissions(permissions)
            .context("Failed to set permissions")?;

        // Create handle
        let handle = self.handle_manager.create_handle(full_path.clone());

        debug!("CREATE: {:?} mode={:o} -> handle", full_path, mode);

        Ok(handle)
    }

    fn remove(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid filename: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Remove file
        fs::remove_file(&full_path).context(format!("Failed to remove file: {:?}", full_path))?;

        debug!("REMOVE: {:?}", full_path);

        Ok(())
    }

    fn mkdir(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid directory name: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Create directory
        fs::create_dir(&full_path).context(format!("Failed to create directory: {:?}", full_path))?;

        // Set permissions
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(&full_path, permissions).context("Failed to set permissions")?;

        // Create handle
        let handle = self.handle_manager.create_handle(full_path.clone());

        debug!("MKDIR: {:?} mode={:o} -> handle", full_path, mode);

        Ok(handle)
    }

    fn rmdir(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid directory name: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Remove directory
        fs::remove_dir(&full_path)
            .context(format!("Failed to remove directory: {:?}", full_path))?;

        debug!("RMDIR: {:?}", full_path);

        Ok(())
    }

    fn rename(
        &self,
        from_dir_handle: &FileHandle,
        from_name: &str,
        to_dir_handle: &FileHandle,
        to_name: &str,
    ) -> Result<()> {
        let from_dir_path = self.resolve_handle(from_dir_handle)?;
        let to_dir_path = self.resolve_handle(to_dir_handle)?;

        // Security: prevent path traversal
        if from_name.contains('/') || from_name.contains("..") {
            return Err(anyhow!("Invalid source name: {}", from_name));
        }
        if to_name.contains('/') || to_name.contains("..") {
            return Err(anyhow!("Invalid target name: {}", to_name));
        }

        let from_full_path = from_dir_path.join(from_name);
        let to_full_path = to_dir_path.join(to_name);

        // Validate both paths are within export root
        self.validate_path(&from_full_path)?;
        self.validate_path(&to_full_path)?;

        // Rename/move the file or directory
        fs::rename(&from_full_path, &to_full_path)
            .context(format!("Failed to rename {:?} to {:?}", from_full_path, to_full_path))?;

        debug!("RENAME: {:?} -> {:?}", from_full_path, to_full_path);

        Ok(())
    }

    fn symlink(&self, dir_handle: &FileHandle, name: &str, target: &str) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal in symlink name
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid symlink name: {}", name));
        }

        let symlink_path = dir_path.join(name);

        // Validate symlink path is within export root
        self.validate_path(&symlink_path)?;

        // Check if file/symlink already exists
        if symlink_path.exists() {
            return Err(anyhow!("File or symlink already exists: {:?}", symlink_path));
        }

        // Create symbolic link
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &symlink_path)
            .context(format!("Failed to create symlink {:?} -> {}", symlink_path, target))?;

        #[cfg(not(unix))]
        return Err(anyhow!("Symbolic links are only supported on Unix systems"));

        debug!("SYMLINK: {:?} -> {}", symlink_path, target);

        // Create handle for the new symlink
        let handle = self.handle_manager.create_handle(symlink_path.clone());
        Ok(handle)
    }

    fn readlink(&self, handle: &FileHandle) -> Result<String> {
        let path = self.resolve_handle(handle)?;

        // Verify the path is a symlink
        let metadata = fs::symlink_metadata(&path)
            .context(format!("Failed to get metadata for {:?}", path))?;

        if !metadata.file_type().is_symlink() {
            return Err(anyhow!("Not a symbolic link: {:?}", path));
        }

        // Read the symlink target
        let target = fs::read_link(&path)
            .context(format!("Failed to read symlink {:?}", path))?;

        let target_str = target.to_string_lossy().to_string();

        debug!("READLINK: {:?} -> {}", path, target_str);

        Ok(target_str)
    }

    fn link(&self, file_handle: &FileHandle, dir_handle: &FileHandle, name: &str) -> Result<FileHandle> {
        let file_path = self.resolve_handle(file_handle)?;
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal in link name
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid link name: {}", name));
        }

        let link_path = dir_path.join(name);

        // Validate link path is within export root
        self.validate_path(&link_path)?;

        // Check if target already exists
        if link_path.exists() {
            return Err(anyhow!("File already exists: {:?}", link_path));
        }

        // Get source file metadata to check if it's a directory
        let metadata = fs::metadata(&file_path)
            .context(format!("Failed to get metadata for {:?}", file_path))?;

        // Cannot create hard link to a directory (POSIX restriction)
        if metadata.is_dir() {
            return Err(anyhow!("Cannot create hard link to directory: {:?}", file_path));
        }

        // Create hard link
        fs::hard_link(&file_path, &link_path)
            .context(format!("Failed to create hard link {:?} -> {:?}", link_path, file_path))?;

        debug!("LINK: {:?} -> {:?}", link_path, file_path);

        // Return the same file handle (hard links share the same inode)
        Ok(file_handle.clone())
    }

    fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        // Open file for syncing
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .context(format!("Failed to open file for commit: {:?}", path))?;

        // Sync data to disk
        // Note: For a more sophisticated implementation, we could:
        // 1. Only sync the specified range (offset, count) if the OS supports it
        // 2. Use sync_data() instead of sync_all() to skip metadata sync
        // 3. Track UNSTABLE writes and only sync those
        //
        // For now, we sync all data in the file for simplicity
        file.sync_all()
            .context(format!("Failed to sync file: {:?}", path))?;

        debug!(
            "COMMIT: {:?} (offset={}, count={})",
            path, offset, count
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper: Create a test filesystem with a temporary directory
    fn create_test_fs() -> (LocalFilesystem, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let fs = LocalFilesystem::new(temp_dir.path()).expect("Failed to create filesystem");
        (fs, temp_dir)
    }

    #[test]
    fn test_root_handle() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();
        assert!(!root.is_empty(), "Root handle should not be empty");
        assert_eq!(root.len(), 32, "Root handle should be 32 bytes");
    }

    #[test]
    fn test_getattr_root() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        let attr = fs.getattr(&root).expect("Failed to get root attributes");
        assert_eq!(attr.ftype, FileType::Directory, "Root should be a directory");
    }

    #[test]
    fn test_create_and_lookup_file() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create a file
        let file_handle = fs.create(&root, "test.txt", 0o644)
            .expect("Failed to create file");

        // Lookup the file
        let lookup_handle = fs.lookup(&root, "test.txt")
            .expect("Failed to lookup file");

        assert_eq!(file_handle, lookup_handle, "Handles should match");

        // Get attributes
        let attr = fs.getattr(&file_handle).expect("Failed to get attributes");
        assert_eq!(attr.ftype, FileType::RegularFile, "Should be a regular file");
        assert_eq!(attr.size, 0, "New file should be empty");
    }

    #[test]
    fn test_write_and_read() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create file
        let file_handle = fs.create(&root, "data.txt", 0o644)
            .expect("Failed to create file");

        // Write data
        let data = b"Hello, NFS World!";
        let written = fs.write(&file_handle, 0, data)
            .expect("Failed to write");
        assert_eq!(written, data.len() as u32, "Should write all bytes");

        // Read data back
        let read_data = fs.read(&file_handle, 0, data.len() as u32)
            .expect("Failed to read");
        assert_eq!(read_data, data, "Read data should match written data");

        // Read partial data
        let partial = fs.read(&file_handle, 7, 3)
            .expect("Failed to read partial");
        assert_eq!(partial, b"NFS", "Partial read should work");
    }

    #[test]
    fn test_mkdir_and_lookup() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create directory
        let dir_handle = fs.mkdir(&root, "subdir", 0o755)
            .expect("Failed to create directory");

        // Lookup directory
        let lookup_handle = fs.lookup(&root, "subdir")
            .expect("Failed to lookup directory");

        assert_eq!(dir_handle, lookup_handle, "Handles should match");

        // Get attributes
        let attr = fs.getattr(&dir_handle).expect("Failed to get attributes");
        assert_eq!(attr.ftype, FileType::Directory, "Should be a directory");
    }

    #[test]
    fn test_nested_operations() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create nested directory structure
        let dir1 = fs.mkdir(&root, "dir1", 0o755)
            .expect("Failed to create dir1");

        let dir2 = fs.mkdir(&dir1, "dir2", 0o755)
            .expect("Failed to create dir2");

        // Create file in nested directory
        let file = fs.create(&dir2, "nested.txt", 0o644)
            .expect("Failed to create nested file");

        // Write and read
        fs.write(&file, 0, b"nested content")
            .expect("Failed to write");

        let content = fs.read(&file, 0, 100)
            .expect("Failed to read");
        assert_eq!(content, b"nested content");
    }

    #[test]
    fn test_remove_file() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create and remove file
        fs.create(&root, "temp.txt", 0o644)
            .expect("Failed to create file");

        fs.remove(&root, "temp.txt")
            .expect("Failed to remove file");

        // Lookup should fail
        let result = fs.lookup(&root, "temp.txt");
        assert!(result.is_err(), "Lookup should fail after removal");
    }

    #[test]
    fn test_rmdir() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create and remove directory
        fs.mkdir(&root, "tempdir", 0o755)
            .expect("Failed to create directory");

        fs.rmdir(&root, "tempdir")
            .expect("Failed to remove directory");

        // Lookup should fail
        let result = fs.lookup(&root, "tempdir");
        assert!(result.is_err(), "Lookup should fail after rmdir");
    }

    #[test]
    fn test_path_traversal_prevention() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Try to create file with path traversal
        let result = fs.create(&root, "../etc/passwd", 0o644);
        assert!(result.is_err(), "Should prevent path traversal with ..");

        let result = fs.create(&root, "subdir/../file", 0o644);
        assert!(result.is_err(), "Should prevent .. in filename");

        let result = fs.create(&root, "dir/file", 0o644);
        assert!(result.is_err(), "Should prevent / in filename");
    }

    #[test]
    fn test_lookup_nonexistent() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        let result = fs.lookup(&root, "nonexistent.txt");
        assert!(result.is_err(), "Lookup should fail for nonexistent file");
    }

    #[test]
    fn test_handle_idempotency() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle();

        // Create file
        fs.create(&root, "file.txt", 0o644)
            .expect("Failed to create file");

        // Lookup multiple times should return same handle
        let handle1 = fs.lookup(&root, "file.txt").expect("Failed to lookup");
        let handle2 = fs.lookup(&root, "file.txt").expect("Failed to lookup");

        assert_eq!(handle1, handle2, "Multiple lookups should return same handle");
    }
}
