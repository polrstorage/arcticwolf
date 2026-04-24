// Local Filesystem Backend
//
// Implements the Filesystem trait for local filesystem access.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, warn};

use std::sync::Arc;

use super::handle::{FileHandle, HandleManager};
use super::quota::{QuotaManager, allocated_path_size};
use super::{DirEntry, FileAttributes, FileTime, FileType, Filesystem, FsStats};
use crate::config::QuotaConfig;

/// Local filesystem implementation
pub struct LocalFilesystem {
    /// Root directory for exports
    root_path: PathBuf,
    /// File handle manager
    handle_manager: HandleManager,
    /// Root file handle
    root_handle: FileHandle,
    /// Optional folder quota manager (present when quota is enabled in config)
    quota_manager: Option<Arc<QuotaManager>>,
}

impl LocalFilesystem {
    /// Create a new local filesystem backend
    ///
    /// # Arguments
    /// * `root_path` - Root directory to export (e.g., "/export")
    /// * `quota` - Optional quota configuration. When present and `enabled` is
    ///   true, a [`QuotaManager`] is opened against the configured redb file.
    pub fn new<P: AsRef<Path>>(root_path: P, quota: Option<&QuotaConfig>) -> Result<Self> {
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

        let quota_manager = match quota {
            Some(cfg) if cfg.enabled => {
                // Refuse to host the quota DB inside the exported tree:
                // an NFS client would be able to read, modify or delete
                // it via normal lookup/write/remove operations and
                // corrupt enforcement state. Compare the canonicalised
                // db parent against root_path so symlink shenanigans do
                // not slip through.
                check_db_path_outside_root(&cfg.db_path, &root_path)?;

                let qm = QuotaManager::new(&cfg.db_path, root_path.clone()).context(format!(
                    "Failed to initialise quota manager at {:?}",
                    cfg.db_path
                ))?;
                debug!("LocalFilesystem: quota enabled, db={:?}", cfg.db_path);
                Some(Arc::new(qm))
            }
            _ => None,
        };

        debug!("LocalFilesystem created with root: {:?}", root_path);

        Ok(Self {
            root_path,
            handle_manager,
            root_handle,
            quota_manager,
        })
    }

    /// Access the quota manager, if one is configured.
    #[allow(dead_code)]
    pub fn quota_manager(&self) -> Option<&QuotaManager> {
        self.quota_manager.as_deref()
    }

    /// Resolve a path to its owning quota directory, if any. Returns the
    /// quota manager together with the directory name so callers can run
    /// check/add/sub without repeated plumbing.
    ///
    /// The path is canonicalised first so a symlink that crosses PVCs
    /// (e.g. `pvc-a/link -> pvc-b/data`) is accounted to the PVC where
    /// the data actually lives, not the one the link sits in. When the
    /// path does not exist yet — the common case for `write()` on a
    /// fresh file — canonicalisation fails and we fall back to the
    /// literal path, which is also where the new file will land. There
    /// is a small TOCTOU window between this lookup and the underlying
    /// FS call, but exploiting it requires racing two NFS operations
    /// to swap a regular file for a symlink at exactly the right
    /// moment, which is bounded and benign for the PVC use case.
    async fn quota_target(&self, path: &Path) -> Option<(&QuotaManager, String)> {
        let qm = self.quota_manager.as_ref()?;
        let canonical = tokio_fs::canonicalize(path).await.ok();
        let lookup = canonical.as_deref().unwrap_or(path);
        let dir = qm.resolve_quota_dir(lookup)?;
        // Only return a target if a quota is actually configured for this dir.
        qm.get_quota_info(&dir).await?;
        Some((qm, dir))
    }

    /// Return the first-level quota-directory name for `path`, or `None`
    /// when no quota manager is configured or the path sits at the export
    /// root. Unlike [`quota_target`] this does not require the directory
    /// to have an active quota entry — rename needs to compare both sides
    /// even when only one side is tracked.
    fn quota_dir_of(&self, path: &Path) -> Option<String> {
        self.quota_manager.as_ref()?.resolve_quota_dir(path)
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
    fn metadata_to_attr(&self, metadata: &fs::Metadata, _path: &Path) -> FileAttributes {
        use std::os::unix::fs::FileTypeExt;
        let file_type = metadata.file_type();

        let ftype = if file_type.is_dir() {
            FileType::Directory
        } else if file_type.is_file() {
            FileType::RegularFile
        } else if file_type.is_symlink() {
            FileType::SymbolicLink
        } else if file_type.is_fifo() {
            FileType::NamedPipe
        } else if file_type.is_char_device() {
            FileType::CharDevice
        } else if file_type.is_block_device() {
            FileType::BlockDevice
        } else if file_type.is_socket() {
            FileType::Socket
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

#[async_trait]
impl Filesystem for LocalFilesystem {
    async fn root_handle(&self) -> FileHandle {
        self.root_handle.clone()
    }

    async fn lookup(&self, dir_handle: &FileHandle, name: &str) -> Result<FileHandle> {
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

    async fn getattr(&self, handle: &FileHandle) -> Result<FileAttributes> {
        let path = self.resolve_handle(handle)?;

        let metadata = tokio_fs::metadata(&path)
            .await
            .context(format!("Failed to stat: {:?}", path))?;

        Ok(self.metadata_to_attr(&metadata, &path))
    }

    async fn read(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<Vec<u8>> {
        let path = self.resolve_handle(handle)?;

        let mut file = tokio_fs::File::open(&path)
            .await
            .context(format!("Failed to open file: {:?}", path))?;

        // Seek to offset
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .context("Failed to seek")?;

        // Read up to count bytes
        let mut buffer = vec![0u8; count as usize];
        let bytes_read = file
            .read(&mut buffer)
            .await
            .context("Failed to read file")?;

        // Truncate buffer to actual bytes read
        buffer.truncate(bytes_read);

        debug!(
            "READ: {:?} offset={} count={} -> {} bytes",
            path, offset, count, bytes_read
        );

        Ok(buffer)
    }

    async fn readdir(
        &self,
        dir_handle: &FileHandle,
        cookie: u64,
        count: u32,
    ) -> Result<(Vec<DirEntry>, bool)> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Verify it's a directory
        let metadata = tokio_fs::metadata(&dir_path)
            .await
            .context(format!("Failed to stat directory: {:?}", dir_path))?;

        if !metadata.is_dir() {
            return Err(anyhow!("Not a directory: {:?}", dir_path));
        }

        // Read directory entries
        let mut read_dir = tokio_fs::read_dir(&dir_path)
            .await
            .context(format!("Failed to read directory: {:?}", dir_path))?;

        // Collect all entries
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut index: u64 = 0;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .context("Failed to read directory entry")?
        {
            let entry_path = entry.path();
            let entry_metadata = entry
                .metadata()
                .await
                .context(format!("Failed to get metadata for: {:?}", entry_path))?;

            use std::os::unix::fs::FileTypeExt;
            let ft = entry_metadata.file_type();

            let file_type = if ft.is_dir() {
                FileType::Directory
            } else if ft.is_file() {
                FileType::RegularFile
            } else if ft.is_symlink() {
                FileType::SymbolicLink
            } else if ft.is_fifo() {
                FileType::NamedPipe
            } else if ft.is_char_device() {
                FileType::CharDevice
            } else if ft.is_block_device() {
                FileType::BlockDevice
            } else if ft.is_socket() {
                FileType::Socket
            } else {
                FileType::RegularFile // Default
            };

            let name = entry.file_name().to_string_lossy().to_string();

            // Skip entries before cookie (cookie is 0-based index + 1)
            if cookie > 0 && index < cookie {
                index += 1;
                continue;
            }

            entries.push(DirEntry {
                fileid: entry_metadata.ino(),
                name,
                file_type,
            });

            index += 1;

            // Check if we've reached the requested count
            if entries.len() >= count as usize {
                debug!(
                    "READDIR: {:?} cookie={} count={} -> {} entries (more available)",
                    dir_path,
                    cookie,
                    count,
                    entries.len()
                );
                return Ok((entries, false)); // Not EOF, more entries available
            }
        }

        debug!(
            "READDIR: {:?} cookie={} count={} -> {} entries (EOF)",
            dir_path,
            cookie,
            count,
            entries.len()
        );

        Ok((entries, true)) // EOF reached
    }

    async fn write(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        let path = self.resolve_handle(handle)?;
        // File handles map to paths and are not invalidated when the
        // path is replaced with a symlink. Re-validate the resolved
        // path so a stale handle cannot be combined with a symlink
        // swap to write outside the export root — `OpenOptions::open`
        // below follows symlinks, so without this check a client
        // could escape the exported tree via the resolved target.
        self.validate_path(&path)?;

        // Quota: charge by allocated bytes (st_blocks * 512), not by logical
        // length. A client cannot bypass the quota by extending a file with
        // setattr_size and then writing into the existing logical range,
        // because hole-filling writes increase the on-disk footprint even
        // when the logical size is unchanged.
        let quota_target = self.quota_target(&path).await;
        let old_alloc = if quota_target.is_some() {
            // write() opens with `tokio_fs::OpenOptions` which follows
            // symlinks, so account for the resolved target's footprint
            // — otherwise a symlink in a quota dir lets the client
            // bypass enforcement by writing through the link.
            allocated_bytes_following(&path).await?
        } else {
            0
        };
        if let Some((qm, dir)) = quota_target.as_ref() {
            // Pre-check is conservative: assume the entire write will land
            // in freshly allocated blocks. We trade two things to keep
            // the hot path simple:
            //   * Overwrites in a near-full PVC may be rejected with
            //     EDQUOT even though the actual on-disk usage would not
            //     grow (e.g. rewriting an existing block with new data).
            //     A precise check would need fiemap/SEEK_HOLE per
            //     write, which is Linux-specific and slow.
            //   * Concurrent writers can each pass this check against
            //     the same usage snapshot — see the concurrency note
            //     on `QuotaManager::check_quota`. The post-write
            //     `add_usage` accounts for the real delta either way,
            //     so over-shoot is bounded and reconciliation repairs
            //     it.
            qm.check_quota(dir, data.len() as u64).await?;
        }

        let mut file = tokio_fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .await
            .context(format!("Failed to open file for writing: {:?}", path))?;

        // Seek to offset
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .context("Failed to seek")?;

        // Write data
        let bytes_written = file.write(data).await.context("Failed to write file")?;

        // Flush to disk
        file.sync_all().await.context("Failed to sync file")?;

        // Quota: apply the real delta in allocated bytes after the write.
        // The data has already been written and synced; surfacing a quota
        // bookkeeping error here would tell the client the WRITE failed
        // even though bytes are on disk, and the QuotaManager rolls back
        // its cache on persist failure so future checks would also be
        // wrong. Log it as a degraded state instead and let background
        // reconciliation repair the drift.
        if let Some((qm, dir)) = quota_target.as_ref() {
            match allocated_bytes_following(&path).await {
                Ok(new_alloc) => {
                    let delta = new_alloc.saturating_sub(old_alloc);
                    if delta > 0
                        && let Err(err) = qm.add_usage(dir, delta).await
                    {
                        warn!(
                            "WRITE quota accounting failed after data was persisted: \
                             path={:?} dir={} delta={} error={:#}",
                            path, dir, delta, err
                        );
                    }
                }
                Err(err) => {
                    warn!(
                        "WRITE quota accounting skipped (stat failed) after \
                         data was persisted: path={:?} dir={} error={:#}",
                        path, dir, err
                    );
                }
            }
        }

        debug!(
            "WRITE: {:?} offset={} count={} -> {} bytes",
            path,
            offset,
            data.len(),
            bytes_written
        );

        Ok(bytes_written as u32)
    }

    async fn setattr_size(&self, handle: &FileHandle, size: u64) -> Result<()> {
        let path = self.resolve_handle(handle)?;
        // See `write()` for the rationale: validate the resolved path
        // again so a stale handle + symlink swap cannot truncate or
        // extend a file outside the export root.
        self.validate_path(&path)?;

        // Quota: track using allocated bytes, consistent with write().
        // Truncating down releases blocks; sparse extension allocates
        // none, so the delta is naturally zero — no special-case needed
        // for the "extend" branch.
        let quota_target = self.quota_target(&path).await;
        let old_alloc = if quota_target.is_some() {
            // setattr_size opens through the symlink (set_len follows),
            // so quota tracking must look at the same target the kernel
            // actually truncates.
            allocated_bytes_following(&path).await?
        } else {
            0
        };

        let file = tokio_fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .context(format!("Failed to open file for setattr: {:?}", path))?;

        file.set_len(size)
            .await
            .context("Failed to set file size")?;

        // Quota release is best-effort: the truncate has already taken
        // effect on disk, so a redb error here must not propagate as a
        // SETATTR failure (the client would re-issue and observe the new
        // size anyway). See WRITE for the same rationale.
        if let Some((qm, dir)) = quota_target {
            match allocated_bytes_following(&path).await {
                Ok(new_alloc) if new_alloc < old_alloc => {
                    let freed = old_alloc - new_alloc;
                    if let Err(err) = qm.sub_usage(&dir, freed).await {
                        warn!(
                            "SETATTR quota release failed after truncate: \
                             path={:?} dir={} freed={} error={:#}",
                            path, dir, freed, err
                        );
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(
                        "SETATTR quota release skipped (stat failed) after \
                         truncate: path={:?} dir={} error={:#}",
                        path, dir, err
                    );
                }
            }
        }

        debug!("SETATTR: {:?} size={}", path, size);

        Ok(())
    }

    async fn setattr_mode(&self, handle: &FileHandle, mode: u32) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        let permissions = fs::Permissions::from_mode(mode);
        tokio_fs::set_permissions(&path, permissions)
            .await
            .context(format!("Failed to set permissions: {:?}", path))?;

        debug!("SETATTR: {:?} mode={:o}", path, mode);

        Ok(())
    }

    async fn setattr_owner(
        &self,
        handle: &FileHandle,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        // Note: chown requires root privileges on Unix systems
        // For now, we'll just log this and return success
        // In production, you might want to use nix::unistd::chown
        debug!(
            "SETATTR: {:?} uid={:?} gid={:?} (not implemented)",
            path, uid, gid
        );

        Ok(())
    }

    async fn create(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid filename: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Create file
        let file = tokio_fs::File::create(&full_path)
            .await
            .context(format!("Failed to create file: {:?}", full_path))?;

        // Set permissions
        let permissions = fs::Permissions::from_mode(mode);
        file.set_permissions(permissions)
            .await
            .context("Failed to set permissions")?;

        // Create handle
        let handle = self.handle_manager.create_handle(full_path.clone());

        debug!("CREATE: {:?} mode={:o} -> handle", full_path, mode);

        Ok(handle)
    }

    async fn remove(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid filename: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Quota: figure out how many bytes this unlink will actually free.
        // `refundable_bytes_on_unlink` accounts for symlinks (refund the
        // link, not the target) and hard links (refund nothing while
        // `nlink > 1`, otherwise a hard-linked file could be used to
        // reclaim quota the data still occupies on disk).
        let quota_target = self.quota_target(&full_path).await;
        let freed_bytes = if quota_target.is_some() {
            refundable_bytes_on_unlink(&full_path).await?
        } else {
            0
        };

        // Remove file
        tokio_fs::remove_file(&full_path)
            .await
            .context(format!("Failed to remove file: {:?}", full_path))?;

        // Quota release is best-effort: the file is already gone, so a
        // redb error here must not propagate as a REMOVE failure (the
        // client would re-issue and observe NOENT). See WRITE for the
        // same rationale.
        if let Some((qm, dir)) = quota_target
            && freed_bytes > 0
            && let Err(err) = qm.sub_usage(&dir, freed_bytes).await
        {
            warn!(
                "REMOVE quota release failed after unlink: \
                 path={:?} dir={} freed={} error={:#}",
                full_path, dir, freed_bytes, err
            );
        }

        debug!("REMOVE: {:?} (freed {} bytes)", full_path, freed_bytes);

        Ok(())
    }

    async fn mkdir(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid directory name: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Create directory
        tokio_fs::create_dir(&full_path)
            .await
            .context(format!("Failed to create directory: {:?}", full_path))?;

        // Set permissions
        let permissions = fs::Permissions::from_mode(mode);
        tokio_fs::set_permissions(&full_path, permissions)
            .await
            .context("Failed to set permissions")?;

        // Create handle
        let handle = self.handle_manager.create_handle(full_path.clone());

        debug!("MKDIR: {:?} mode={:o} -> handle", full_path, mode);

        Ok(handle)
    }

    async fn rmdir(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        let dir_path = self.resolve_handle(dir_handle)?;

        // Security: prevent path traversal
        if name.contains('/') || name.contains("..") {
            return Err(anyhow!("Invalid directory name: {}", name));
        }

        let full_path = dir_path.join(name);

        // Validate path is within export root
        self.validate_path(&full_path)?;

        // Remove directory
        tokio_fs::remove_dir(&full_path)
            .await
            .context(format!("Failed to remove directory: {:?}", full_path))?;

        debug!("RMDIR: {:?}", full_path);

        Ok(())
    }

    async fn rename(
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

        // Quota: only need to do work when the rename crosses a quota
        // boundary. Within the same quota directory (or between two
        // untracked directories) the usage is unchanged. Even when the
        // first-level names differ, skip the (potentially expensive)
        // recursive size walk if neither side has an active quota
        // entry — a rename between untracked PVCs should not pay for it.
        let from_quota = self.quota_dir_of(&from_full_path);
        let to_quota = self.quota_dir_of(&to_full_path);
        let cross_quota = from_quota != to_quota;

        let need_size = if cross_quota && let Some(ref qm) = self.quota_manager {
            let from_tracked = match from_quota.as_ref() {
                Some(d) => qm.get_quota_info(d).await.is_some(),
                None => false,
            };
            let to_tracked = match to_quota.as_ref() {
                Some(d) => qm.get_quota_info(d).await.is_some(),
                None => false,
            };
            from_tracked || to_tracked
        } else {
            false
        };

        let size_bytes = if need_size {
            // Compute the total byte footprint of the source before renaming.
            let src = from_full_path.clone();
            tokio::task::spawn_blocking(move || allocated_path_size(&src))
                .await
                .context("spawn_blocking failed")??
        } else {
            0
        };

        if cross_quota
            && size_bytes > 0
            && let Some(ref qm) = self.quota_manager
            && let Some(ref dir) = to_quota
            && qm.get_quota_info(dir).await.is_some()
        {
            qm.check_quota(dir, size_bytes).await?;
        }

        // Rename/move the file or directory
        tokio_fs::rename(&from_full_path, &to_full_path)
            .await
            .context(format!(
                "Failed to rename {:?} to {:?}",
                from_full_path, to_full_path
            ))?;

        // Quota transfer is best-effort: the rename has already taken
        // place on disk, so a redb error here must not propagate as a
        // RENAME failure (the client would re-issue and observe NOENT
        // on the source). See WRITE for the same rationale.
        if cross_quota
            && size_bytes > 0
            && let Some(ref qm) = self.quota_manager
        {
            if let Some(ref dir) = from_quota
                && let Err(err) = qm.sub_usage(dir, size_bytes).await
            {
                warn!(
                    "RENAME quota release on source failed after rename: \
                     dir={} bytes={} error={:#}",
                    dir, size_bytes, err
                );
            }
            if let Some(ref dir) = to_quota
                && let Err(err) = qm.add_usage(dir, size_bytes).await
            {
                warn!(
                    "RENAME quota charge on target failed after rename: \
                     dir={} bytes={} error={:#}",
                    dir, size_bytes, err
                );
            }
        }

        debug!("RENAME: {:?} -> {:?}", from_full_path, to_full_path);

        Ok(())
    }

    async fn symlink(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        target: &str,
    ) -> Result<FileHandle> {
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
            return Err(anyhow!(
                "File or symlink already exists: {:?}",
                symlink_path
            ));
        }

        // Create symbolic link
        tokio_fs::symlink(target, &symlink_path)
            .await
            .context(format!(
                "Failed to create symlink {:?} -> {}",
                symlink_path, target
            ))?;

        debug!("SYMLINK: {:?} -> {}", symlink_path, target);

        // Create handle for the new symlink
        let handle = self.handle_manager.create_handle(symlink_path.clone());
        Ok(handle)
    }

    async fn readlink(&self, handle: &FileHandle) -> Result<String> {
        let path = self.resolve_handle(handle)?;

        // Verify the path is a symlink
        let metadata = tokio_fs::symlink_metadata(&path)
            .await
            .context(format!("Failed to get metadata for {:?}", path))?;

        if !metadata.file_type().is_symlink() {
            return Err(anyhow!("Not a symbolic link: {:?}", path));
        }

        // Read the symlink target
        let target = tokio_fs::read_link(&path)
            .await
            .context(format!("Failed to read symlink {:?}", path))?;

        let target_str = target.to_string_lossy().to_string();

        debug!("READLINK: {:?} -> {}", path, target_str);

        Ok(target_str)
    }

    async fn link(
        &self,
        file_handle: &FileHandle,
        dir_handle: &FileHandle,
        name: &str,
    ) -> Result<FileHandle> {
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
        let metadata = tokio_fs::metadata(&file_path)
            .await
            .context(format!("Failed to get metadata for {:?}", file_path))?;

        // Cannot create hard link to a directory (POSIX restriction)
        if metadata.is_dir() {
            return Err(anyhow!(
                "Cannot create hard link to directory: {:?}",
                file_path
            ));
        }

        // Create hard link
        tokio_fs::hard_link(&file_path, &link_path)
            .await
            .context(format!(
                "Failed to create hard link {:?} -> {:?}",
                link_path, file_path
            ))?;

        debug!("LINK: {:?} -> {:?}", link_path, file_path);

        // Return the same file handle (hard links share the same inode)
        Ok(file_handle.clone())
    }

    async fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<()> {
        let path = self.resolve_handle(handle)?;

        // Open file for syncing
        let file = tokio_fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .context(format!("Failed to open file for commit: {:?}", path))?;

        // Sync data to disk
        // Note: For a more sophisticated implementation, we could:
        // 1. Only sync the specified range (offset, count) if the OS supports it
        // 2. Use sync_data() instead of sync_all() to skip metadata sync
        // 3. Track UNSTABLE writes and only sync those
        //
        // For now, we sync all data in the file for simplicity
        file.sync_all()
            .await
            .context(format!("Failed to sync file: {:?}", path))?;

        debug!("COMMIT: {:?} (offset={}, count={})", path, offset, count);

        Ok(())
    }

    async fn mknod(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        file_type: FileType,
        mode: u32,
        rdev: (u32, u32),
    ) -> Result<FileHandle> {
        let dir_path = self.resolve_handle(dir_handle)?;
        let file_path = dir_path.join(name);

        debug!(
            "MKNOD: {:?}/{} type={:?} mode={:o} rdev=({}, {})",
            dir_path, name, file_type, mode, rdev.0, rdev.1
        );

        // Create special files using libc on Linux; blocking operations are wrapped in spawn_blocking.
        let file_path_clone = file_path.clone();
        let file_type_clone = file_type;

        tokio::task::spawn_blocking(move || {
            match file_type_clone {
                FileType::NamedPipe => {
                    use std::ffi::CString;
                    let c_path = CString::new(file_path_clone.to_str().unwrap())?;
                    let result = unsafe { libc::mkfifo(c_path.as_ptr(), mode as libc::mode_t) };
                    if result != 0 {
                        return Err(anyhow::anyhow!(
                            "Failed to create FIFO: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                }
                FileType::Socket => {
                    // Unix domain sockets are typically created by bind(), not mknod
                    return Err(anyhow::anyhow!(
                        "Socket creation via MKNOD not fully supported"
                    ));
                }
                FileType::CharDevice | FileType::BlockDevice => {
                    use std::ffi::CString;
                    let c_path = CString::new(file_path_clone.to_str().unwrap())?;
                    let dev = libc::makedev(rdev.0, rdev.1);
                    let mode_with_type = (mode as libc::mode_t)
                        | match file_type_clone {
                            FileType::CharDevice => libc::S_IFCHR as libc::mode_t,
                            FileType::BlockDevice => libc::S_IFBLK as libc::mode_t,
                            _ => 0,
                        };
                    let result = unsafe { libc::mknod(c_path.as_ptr(), mode_with_type, dev) };
                    if result != 0 {
                        return Err(anyhow::anyhow!(
                            "Failed to create device: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Invalid file type for MKNOD: {:?}",
                        file_type_clone
                    ));
                }
            }
            Ok(())
        })
        .await
        .context("spawn_blocking failed")??;

        // Create handle for the new special file
        let handle = self.handle_manager.create_handle(file_path.clone());
        Ok(handle)
    }

    async fn statvfs(&self, handle: &FileHandle) -> Result<FsStats> {
        let path = self.resolve_handle(handle)?;
        let path_owned = path.clone();

        // Always fetch the real filesystem stats: we reuse its inode fields
        // unconditionally, and its byte fields when no quota applies.
        let real_stats = tokio::task::spawn_blocking(move || statvfs_on_path(&path_owned))
            .await
            .context("spawn_blocking failed")??;

        if let Some(ref qm) = self.quota_manager
            && let Some(dir) = qm.resolve_quota_dir(&path)
            && let Some((limit, usage)) = qm.get_quota_info(&dir).await
        {
            let free = limit.saturating_sub(usage);
            return Ok(FsStats {
                total_bytes: limit,
                free_bytes: free,
                avail_bytes: free,
                // Inode counts always come from the underlying filesystem.
                total_files: real_stats.total_files,
                free_files: real_stats.free_files,
                avail_files: real_stats.avail_files,
            });
        }

        Ok(real_stats)
    }

    fn start_quota_reconciliation(&self) {
        let Some(qm) = self.quota_manager.clone() else {
            return;
        };
        tokio::spawn(async move {
            tracing::info!("Quota reconciliation task started");
            qm.reconcile_all().await;
            tracing::info!("Quota reconciliation task finished");
        });
    }

    async fn apply_quota_bootstrap(
        &self,
        bootstrap: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let Some(ref qm) = self.quota_manager else {
            if !bootstrap.is_empty() {
                tracing::warn!(
                    "Quota bootstrap requested but quota is disabled; ignoring {} entries",
                    bootstrap.len()
                );
            }
            return Ok(());
        };
        qm.apply_bootstrap(bootstrap).await
    }
}

/// Logically normalize a path: collapse `.` and `..` components and
/// drop empty segments. Does not touch the filesystem and so is safe to
/// run on paths whose components do not exist yet — that is exactly the
/// case `check_db_path_outside_root` needs when the DB has not been
/// created.
///
/// Leading `..` components on a relative path are preserved (`"../x"`
/// stays `"../x"`), since dropping them would silently change the
/// semantics of the path. On a rooted path, a `..` at the root is
/// discarded (`"/.."` resolves to `"/"`).
fn normalize_path(path: &Path) -> PathBuf {
    use std::ffi::OsStr;
    use std::path::Component;
    let parent = OsStr::new("..");
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut prefix: Option<std::ffi::OsString> = None;
    let mut has_root = false;
    for c in path.components() {
        match c {
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_os_string()),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(last) if last != parent => {
                    // Pop a real segment (cancel it out).
                    out.pop();
                }
                _ if !has_root => {
                    // Either out is empty or the tail is already "..":
                    // on a relative path we must preserve the `..` so
                    // the path keeps its original anchor.
                    out.push(parent.to_os_string());
                }
                _ => {
                    // Rooted path: `..` past the root collapses to root.
                }
            },
            Component::Normal(s) => out.push(s.to_os_string()),
        }
    }
    let mut result = PathBuf::new();
    if let Some(p) = prefix {
        result.push(p);
    }
    if has_root {
        result.push("/");
    }
    for s in out {
        result.push(s);
    }
    result
}

/// Reject quota DB paths that resolve under the export root.
///
/// A DB inside the exported tree is reachable via NFS lookup/read/write/
/// remove and would let any client tamper with the bookkeeping. The
/// containment check is done in absolute terms so a relative `db_path`
/// (whose parent might not exist yet) cannot slip past — we anchor it
/// against `current_dir` and normalize away `..` components before the
/// `starts_with` test. Existing paths/parents are still canonicalised to
/// resolve symlinks; only the literal-tail fallback uses normalisation.
fn check_db_path_outside_root(db_path: &Path, root_path: &Path) -> Result<()> {
    // Anchor relative paths against the current working directory so the
    // containment comparison is meaningful (root_path is absolute).
    let abs_db = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Failed to read current directory while validating quota db path")?
            .join(db_path)
    };

    let canonical_db = if abs_db.exists() {
        abs_db.canonicalize().context(format!(
            "Failed to canonicalize quota db path: {:?}",
            abs_db
        ))?
    } else {
        let parent = abs_db
            .parent()
            .ok_or_else(|| anyhow!("Quota db_path has no parent directory: {:?}", abs_db))?;
        let canonical_parent = if parent.as_os_str().is_empty() {
            std::env::current_dir()
                .context("Failed to read current directory while validating quota db path")?
        } else if parent.exists() {
            parent.canonicalize().context(format!(
                "Failed to canonicalize quota db parent: {:?}",
                parent
            ))?
        } else {
            // Parent will be created by QuotaManager::new(); we cannot
            // ask the kernel to resolve symlinks/`..` for us, so do a
            // logical normalisation. The path is already absolute thanks
            // to the anchor step above, so the comparison below is sound.
            normalize_path(parent)
        };
        let file_name = abs_db
            .file_name()
            .ok_or_else(|| anyhow!("Quota db_path has no file name component: {:?}", abs_db))?;
        canonical_parent.join(file_name)
    };

    if canonical_db.starts_with(root_path) {
        return Err(anyhow!(
            "Quota db_path {:?} resolves inside the export root {:?}; \
             move the database outside the exported tree so NFS clients \
             cannot read or modify it",
            db_path,
            root_path
        ));
    }
    Ok(())
}

/// Return the on-disk byte footprint of the **symlink-resolved** path,
/// computed from `st_blocks`. Used by content-mutating operations
/// (`write`, `setattr_size`) where the kernel itself follows the link
/// when opening — accounting must follow the same target so writes
/// through a symlink in a quota directory still increase tracked usage.
///
/// `NotFound` maps to zero; other stat failures are propagated so a
/// transient I/O error cannot silently undercount quota usage.
async fn allocated_bytes_following(path: &Path) -> Result<u64> {
    match tokio_fs::metadata(path).await {
        Ok(m) => Ok(m.blocks().saturating_mul(512)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => {
            Err(anyhow::Error::from(e)).context(format!("Failed to stat for quota: {:?}", path))
        }
    }
}

/// Return how many bytes the quota should refund when `path` is unlinked.
///
/// Quota is charged exclusively on operations that mutate regular-file
/// content (`write`, `setattr_size`, cross-quota `rename`). Refunds must
/// be the symmetric inverse of those charges, otherwise a client can
/// drift the tracked usage below reality:
///
/// * **Regular file, last link (`nlink == 1`):** unlinking truly frees
///   the inode's blocks → refund `blocks * 512`.
/// * **Regular file with surviving hard link (`nlink > 1`):** the inode
///   and its blocks remain reachable via another name (possibly in a
///   different PVC, since `link()` bypasses quota_dir attribution).
///   No bytes are freed → refund 0.
/// * **Symlinks, FIFOs, sockets, devices:** these are *not* charged at
///   creation time (the FSAL's `symlink`/`mknod` paths skip the quota
///   accounting). Refunding their `st_blocks` would let a client
///   create then remove long symlinks (each occupying a real block on
///   most filesystems) to depress the usage counter and reclaim quota
///   for data they never paid for. Refund 0 to keep the create/remove
///   accounting symmetric.
///
/// There is a small TOCTOU between this stat and the actual `unlink`,
/// but it cannot introduce a leak in the dangerous direction — at
/// worst a refund-of-blocks gets skipped that should have been issued,
/// which background reconciliation will repair.
///
/// `NotFound` maps to zero; other stat failures are propagated so a
/// transient I/O error cannot silently undercount quota usage.
async fn refundable_bytes_on_unlink(path: &Path) -> Result<u64> {
    match tokio_fs::symlink_metadata(path).await {
        Ok(m) if m.is_file() && m.nlink() == 1 => Ok(m.blocks().saturating_mul(512)),
        Ok(_) => Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => {
            Err(anyhow::Error::from(e)).context(format!("Failed to stat for quota: {:?}", path))
        }
    }
}

/// Call `libc::statvfs` on `path` and convert the result into `FsStats`.
fn statvfs_on_path(path: &Path) -> Result<FsStats> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path =
        CString::new(path.as_os_str().as_bytes()).context("Path contains interior NUL byte")?;

    // Safety: zero-initializing libc::statvfs is valid; the struct is a POD
    // descriptor that the kernel fills in.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow!("statvfs failed for {:?}: {}", path, err));
    }

    // f_frsize is the fragment size used for f_blocks/f_bfree/f_bavail.
    // Linux always populates it, but some exotic backends report 0; fall
    // back to the logical block size (f_bsize) so FSSTAT never returns
    // a bogus 0-byte filesystem. Multiplications use saturating_mul as a
    // belt-and-braces guard against overflow on huge filesystems.
    let block_size = if stat.f_frsize == 0 {
        stat.f_bsize
    } else {
        stat.f_frsize
    };
    Ok(FsStats {
        total_bytes: stat.f_blocks.saturating_mul(block_size),
        free_bytes: stat.f_bfree.saturating_mul(block_size),
        avail_bytes: stat.f_bavail.saturating_mul(block_size),
        total_files: stat.f_files,
        free_files: stat.f_ffree,
        avail_files: stat.f_favail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: Create a test filesystem with a temporary directory
    fn create_test_fs() -> (LocalFilesystem, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let fs = LocalFilesystem::new(temp_dir.path(), None).expect("Failed to create filesystem");
        (fs, temp_dir)
    }

    #[tokio::test]
    async fn test_root_handle() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;
        assert!(!root.is_empty(), "Root handle should not be empty");
        assert_eq!(root.len(), 32, "Root handle should be 32 bytes");
    }

    #[tokio::test]
    async fn test_getattr_root() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        let attr = fs
            .getattr(&root)
            .await
            .expect("Failed to get root attributes");
        assert_eq!(
            attr.ftype,
            FileType::Directory,
            "Root should be a directory"
        );
    }

    #[tokio::test]
    async fn test_create_and_lookup_file() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create a file
        let file_handle = fs
            .create(&root, "test.txt", 0o644)
            .await
            .expect("Failed to create file");

        // Lookup the file
        let lookup_handle = fs
            .lookup(&root, "test.txt")
            .await
            .expect("Failed to lookup file");

        assert_eq!(file_handle, lookup_handle, "Handles should match");

        // Get attributes
        let attr = fs
            .getattr(&file_handle)
            .await
            .expect("Failed to get attributes");
        assert_eq!(
            attr.ftype,
            FileType::RegularFile,
            "Should be a regular file"
        );
        assert_eq!(attr.size, 0, "New file should be empty");
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create file
        let file_handle = fs
            .create(&root, "data.txt", 0o644)
            .await
            .expect("Failed to create file");

        // Write data
        let data = b"Hello, NFS World!";
        let written = fs
            .write(&file_handle, 0, data)
            .await
            .expect("Failed to write");
        assert_eq!(written, data.len() as u32, "Should write all bytes");

        // Read data back
        let read_data = fs
            .read(&file_handle, 0, data.len() as u32)
            .await
            .expect("Failed to read");
        assert_eq!(read_data, data, "Read data should match written data");

        // Read partial data
        let partial = fs
            .read(&file_handle, 7, 3)
            .await
            .expect("Failed to read partial");
        assert_eq!(partial, b"NFS", "Partial read should work");
    }

    #[tokio::test]
    async fn test_mkdir_and_lookup() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create directory
        let dir_handle = fs
            .mkdir(&root, "subdir", 0o755)
            .await
            .expect("Failed to create directory");

        // Lookup directory
        let lookup_handle = fs
            .lookup(&root, "subdir")
            .await
            .expect("Failed to lookup directory");

        assert_eq!(dir_handle, lookup_handle, "Handles should match");

        // Get attributes
        let attr = fs
            .getattr(&dir_handle)
            .await
            .expect("Failed to get attributes");
        assert_eq!(attr.ftype, FileType::Directory, "Should be a directory");
    }

    #[tokio::test]
    async fn test_nested_operations() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create nested directory structure
        let dir1 = fs
            .mkdir(&root, "dir1", 0o755)
            .await
            .expect("Failed to create dir1");

        let dir2 = fs
            .mkdir(&dir1, "dir2", 0o755)
            .await
            .expect("Failed to create dir2");

        // Create file in nested directory
        let file = fs
            .create(&dir2, "nested.txt", 0o644)
            .await
            .expect("Failed to create nested file");

        // Write and read
        fs.write(&file, 0, b"nested content")
            .await
            .expect("Failed to write");

        let content = fs.read(&file, 0, 100).await.expect("Failed to read");
        assert_eq!(content, b"nested content");
    }

    #[tokio::test]
    async fn test_remove_file() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create and remove file
        fs.create(&root, "temp.txt", 0o644)
            .await
            .expect("Failed to create file");

        fs.remove(&root, "temp.txt")
            .await
            .expect("Failed to remove file");

        // Lookup should fail
        let result = fs.lookup(&root, "temp.txt").await;
        assert!(result.is_err(), "Lookup should fail after removal");
    }

    #[tokio::test]
    async fn test_rmdir() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create and remove directory
        fs.mkdir(&root, "tempdir", 0o755)
            .await
            .expect("Failed to create directory");

        fs.rmdir(&root, "tempdir")
            .await
            .expect("Failed to remove directory");

        // Lookup should fail
        let result = fs.lookup(&root, "tempdir").await;
        assert!(result.is_err(), "Lookup should fail after rmdir");
    }

    #[tokio::test]
    async fn test_path_traversal_prevention() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Try to create file with path traversal
        let result = fs.create(&root, "../etc/passwd", 0o644).await;
        assert!(result.is_err(), "Should prevent path traversal with ..");

        let result = fs.create(&root, "subdir/../file", 0o644).await;
        assert!(result.is_err(), "Should prevent .. in filename");

        let result = fs.create(&root, "dir/file", 0o644).await;
        assert!(result.is_err(), "Should prevent / in filename");
    }

    #[tokio::test]
    async fn test_lookup_nonexistent() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        let result = fs.lookup(&root, "nonexistent.txt").await;
        assert!(result.is_err(), "Lookup should fail for nonexistent file");
    }

    #[tokio::test]
    async fn test_handle_idempotency() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        // Create file
        fs.create(&root, "file.txt", 0o644)
            .await
            .expect("Failed to create file");

        // Lookup multiple times should return same handle
        let handle1 = fs
            .lookup(&root, "file.txt")
            .await
            .expect("Failed to lookup");
        let handle2 = fs
            .lookup(&root, "file.txt")
            .await
            .expect("Failed to lookup");

        assert_eq!(
            handle1, handle2,
            "Multiple lookups should return same handle"
        );
    }

    #[tokio::test]
    async fn test_statvfs_returns_real_stats() {
        let (fs, _temp_dir) = create_test_fs();
        let root = fs.root_handle().await;

        let stats = fs.statvfs(&root).await.expect("statvfs should succeed");

        // We can't predict exact values across different hosts, but a real
        // filesystem will report non-zero totals and some non-zero inode
        // capacity; available <= free <= total.
        assert!(stats.total_bytes > 0, "total_bytes should be non-zero");
        assert!(stats.free_bytes <= stats.total_bytes);
        assert!(stats.avail_bytes <= stats.free_bytes);
        assert!(stats.total_files > 0, "total_files should be non-zero");
        assert!(stats.free_files <= stats.total_files);
        assert!(stats.avail_files <= stats.free_files);
    }

    #[tokio::test]
    async fn test_statvfs_invalid_handle() {
        let (fs, _temp_dir) = create_test_fs();
        let bogus: FileHandle = vec![0xAA; 32];

        let result = fs.statvfs(&bogus).await;
        assert!(result.is_err(), "statvfs with invalid handle should fail");
    }

    /// Helper: Create a test filesystem with a QuotaManager wired up.
    /// Returns (fs, export_tempdir, db_tempdir) — all three temp dirs are
    /// kept alive by the caller.
    fn create_test_fs_with_quota() -> (LocalFilesystem, TempDir, TempDir) {
        let export_dir = TempDir::new().expect("Failed to create export temp dir");
        let db_dir = TempDir::new().expect("Failed to create db temp dir");
        let quota_cfg = QuotaConfig {
            enabled: true,
            db_path: db_dir.path().join("quota.db"),
            bootstrap: std::collections::HashMap::new(),
        };
        let fs = LocalFilesystem::new(export_dir.path(), Some(&quota_cfg))
            .expect("Failed to create filesystem with quota");
        (fs, export_dir, db_dir)
    }

    #[tokio::test]
    async fn test_quota_manager_created_when_enabled() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        assert!(
            fs.quota_manager().is_some(),
            "Quota manager should exist when config.enabled=true"
        );
    }

    #[tokio::test]
    async fn test_quota_manager_absent_when_disabled() {
        let export = TempDir::new().unwrap();
        let db = TempDir::new().unwrap();
        let quota_cfg = QuotaConfig {
            enabled: false,
            db_path: db.path().join("quota.db"),
            bootstrap: std::collections::HashMap::new(),
        };
        let fs = LocalFilesystem::new(export.path(), Some(&quota_cfg)).unwrap();
        assert!(
            fs.quota_manager().is_none(),
            "Quota manager should be absent when config.enabled=false"
        );
    }

    #[tokio::test]
    async fn test_quota_manager_absent_when_no_config() {
        let export = TempDir::new().unwrap();
        let fs = LocalFilesystem::new(export.path(), None).unwrap();
        assert!(fs.quota_manager().is_none());
    }

    #[tokio::test]
    async fn test_db_path_inside_export_root_is_rejected() {
        let export = TempDir::new().unwrap();
        // Place the DB literally inside the exported tree — NFS clients
        // could otherwise reach it via lookup/write/remove.
        let cfg = QuotaConfig {
            enabled: true,
            db_path: export.path().join("quota.db"),
            bootstrap: std::collections::HashMap::new(),
        };
        match LocalFilesystem::new(export.path(), Some(&cfg)) {
            Ok(_) => panic!("LocalFilesystem::new should reject db inside export"),
            Err(e) => assert!(
                e.to_string().contains("inside the export root"),
                "got: {:#}",
                e
            ),
        }
    }

    #[tokio::test]
    async fn test_db_path_in_nested_subdir_inside_export_is_rejected() {
        let export = TempDir::new().unwrap();
        let nested = export.path().join("a/b/c");
        let cfg = QuotaConfig {
            enabled: true,
            db_path: nested.join("quota.db"),
            bootstrap: std::collections::HashMap::new(),
        };
        match LocalFilesystem::new(export.path(), Some(&cfg)) {
            Ok(_) => panic!("nested-under-root db path should be rejected"),
            Err(e) => assert!(
                e.to_string().contains("inside the export root"),
                "got: {:#}",
                e
            ),
        }
    }

    #[tokio::test]
    async fn test_db_path_outside_export_is_accepted() {
        let export = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let cfg = QuotaConfig {
            enabled: true,
            db_path: db_dir.path().join("quota.db"),
            bootstrap: std::collections::HashMap::new(),
        };
        let fs = LocalFilesystem::new(export.path(), Some(&cfg))
            .expect("db outside export should be accepted");
        assert!(fs.quota_manager().is_some());
    }

    #[tokio::test]
    async fn test_db_path_with_dotdot_resolving_inside_export_is_rejected() {
        let export = TempDir::new().unwrap();
        // Construct a path that uses ".." but normalizes back into the
        // export root: <export>/decoy/../<file>. The intermediate
        // "decoy" directory is non-existent, so the old code took the
        // literal-parent branch and missed the containment check.
        let sneaky = export.path().join("decoy").join("..").join("quota.db");
        let cfg = QuotaConfig {
            enabled: true,
            db_path: sneaky,
            bootstrap: std::collections::HashMap::new(),
        };
        match LocalFilesystem::new(export.path(), Some(&cfg)) {
            Ok(_) => panic!("dotdot-relative db inside export should be rejected"),
            Err(e) => assert!(
                e.to_string().contains("inside the export root"),
                "got: {:#}",
                e
            ),
        }
    }

    #[test]
    fn test_normalize_path_collapses_dotdot() {
        assert_eq!(
            normalize_path(Path::new("/tmp/foo/../bar")),
            PathBuf::from("/tmp/bar")
        );
        assert_eq!(
            normalize_path(Path::new("/tmp/./foo")),
            PathBuf::from("/tmp/foo")
        );
        assert_eq!(
            normalize_path(Path::new("/a/b/c/../../d")),
            PathBuf::from("/a/d")
        );
    }

    #[test]
    fn test_normalize_path_preserves_relative() {
        assert_eq!(
            normalize_path(Path::new("foo/bar/../baz")),
            PathBuf::from("foo/baz")
        );
    }

    #[test]
    fn test_normalize_path_empty_and_root() {
        assert_eq!(normalize_path(Path::new("/")), PathBuf::from("/"));
        assert_eq!(normalize_path(Path::new("")), PathBuf::from(""));
    }

    #[test]
    fn test_normalize_path_preserves_leading_parent_on_relative() {
        // Relative paths must keep their leading "..": dropping them
        // would silently change which directory the path resolves
        // against once it gets joined with a base.
        assert_eq!(normalize_path(Path::new("../x")), PathBuf::from("../x"));
        assert_eq!(
            normalize_path(Path::new("../../x")),
            PathBuf::from("../../x")
        );
        // Once a real segment is consumed by "..", further "..":
        // the remaining one is preserved.
        assert_eq!(
            normalize_path(Path::new("foo/../../x")),
            PathBuf::from("../x")
        );
    }

    #[test]
    fn test_normalize_path_dotdot_past_root_is_clamped() {
        // On a rooted path, ".." that would escape the root has no
        // effect — POSIX semantics treat the parent of "/" as "/".
        assert_eq!(normalize_path(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize_path(Path::new("/../foo")), PathBuf::from("/foo"));
        assert_eq!(normalize_path(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[tokio::test]
    async fn test_statvfs_reports_quota_for_quota_dir() {
        let (fs, export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;

        // Create a quota-tracked subdirectory and set a 1 MiB quota on it.
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        let qm = fs.quota_manager().unwrap();
        qm.set_quota("pvc-a", 1024 * 1024).await.unwrap();
        qm.add_usage("pvc-a", 200 * 1024).await.unwrap();

        // Look up the subdir via the FSAL so we get its handle.
        let pvc_handle = fs.lookup(&root, "pvc-a").await.unwrap();
        let stats = fs.statvfs(&pvc_handle).await.unwrap();

        assert_eq!(stats.total_bytes, 1024 * 1024);
        assert_eq!(stats.free_bytes, 1024 * 1024 - 200 * 1024);
        assert_eq!(stats.avail_bytes, stats.free_bytes);
        // Inode counts still come from the real filesystem.
        assert!(stats.total_files > 0);

        drop(export);
    }

    #[tokio::test]
    async fn test_statvfs_falls_back_outside_quota_dir() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;

        // Root itself has no quota directory mapping — should return real stats.
        let stats = fs.statvfs(&root).await.unwrap();
        assert!(stats.total_bytes > 0);
        // Real filesystem: free <= total, avail <= free.
        assert!(stats.free_bytes <= stats.total_bytes);
        assert!(stats.avail_bytes <= stats.free_bytes);
    }

    #[tokio::test]
    async fn test_statvfs_for_file_inside_quota_dir() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;

        fs.mkdir(&root, "pvc-b", 0o755).await.unwrap();
        let qm = fs.quota_manager().unwrap();
        qm.set_quota("pvc-b", 5000).await.unwrap();
        qm.add_usage("pvc-b", 1000).await.unwrap();

        let dir = fs.lookup(&root, "pvc-b").await.unwrap();
        // Create a file inside the quota dir; statvfs on the file should
        // still report the parent quota.
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();

        let stats = fs.statvfs(&file).await.unwrap();
        assert_eq!(stats.total_bytes, 5000);
        assert_eq!(stats.free_bytes, 4000);
    }

    // -- Stage 5: quota enforcement ---------------------------------------

    // Quota usage is tracked in allocated bytes (st_blocks * 512). On
    // tmpfs / ext4 the page size is 4 KiB, so the tests below use writes
    // and limits in multiples of 4 KiB to keep expected values exact and
    // independent of the underlying filesystem's rounding.
    //
    // Test-environment assumption: tempfile creates temp dirs on the
    // OS default scratch filesystem (tmpfs on Linux CI, ext4 on most
    // Linux distros) — both round 1-byte writes up to a 4 KiB block.
    // If these tests are ever run on a filesystem with a different
    // allocation unit (e.g. ZFS records, btrfs CoW, network FS) the
    // exact comparisons below will need to be relaxed to ranges.
    const BLOCK: usize = 4096;

    #[tokio::test]
    async fn test_write_within_quota_succeeds() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (4 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();

        let payload = vec![0u8; BLOCK];
        let written = fs.write(&file, 0, &payload).await.expect("write ok");
        assert_eq!(written as usize, payload.len());

        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );
    }

    #[tokio::test]
    async fn test_write_exceeds_quota_is_rejected() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", BLOCK as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();

        let payload = vec![0u8; 2 * BLOCK];
        let err = fs
            .write(&file, 0, &payload)
            .await
            .expect_err("write over quota should fail");
        assert!(err.to_string().contains("Quota exceeded"), "got: {}", err);

        // Nothing was accounted (and nothing was written, since we check before).
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some((BLOCK as u64, 0))
        );
    }

    #[tokio::test]
    async fn test_write_overwriting_existing_data_does_not_double_count() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (4 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();

        fs.write(&file, 0, &vec![0u8; BLOCK]).await.unwrap();
        // Overwrite the same block; allocated footprint is unchanged.
        fs.write(&file, 0, &vec![1u8; BLOCK]).await.unwrap();

        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );
    }

    #[tokio::test]
    async fn test_remove_releases_quota() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (4 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; BLOCK]).await.unwrap();

        fs.remove(&dir, "data.bin").await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, 0))
        );
    }

    #[tokio::test]
    async fn test_remove_hardlink_does_not_refund_quota_until_last_unlink() {
        // Hard link bypass attack: create file -> hard-link it ->
        // remove the original. Without the nlink check, the FSAL would
        // refund the data's blocks on the first unlink even though
        // those blocks remain reachable via the surviving link, and
        // the client could rewrite the same volume of data, doubling
        // their effective quota each round.
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (4 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; BLOCK]).await.unwrap();
        // Sanity: one block was charged.
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );

        // Add a second hard link to the same inode.
        fs.link(&file, &dir, "data2.bin").await.unwrap();

        // Removing the first name must NOT refund quota — the inode
        // and its blocks survive via "data2.bin".
        fs.remove(&dir, "data.bin").await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64)),
            "quota must not be refunded while another hard link still exists"
        );

        // Removing the last name finally frees the blocks; quota refunds.
        fs.remove(&dir, "data2.bin").await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, 0)),
            "last unlink frees the blocks → quota refunds"
        );
    }

    #[tokio::test]
    async fn test_remove_symlink_does_not_refund_quota() {
        // Symlink bypass attack: symlink creation is *not* charged
        // against quota, so refunding bytes on remove would let a
        // client drift the usage counter below reality. The attacker
        // writes legitimate data (charged), then creates and removes
        // long symlinks repeatedly to claw back quota for storage
        // they actually paid for, eventually doubling their effective
        // budget.
        let (fs, export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (8 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; BLOCK]).await.unwrap();
        let charged = fs
            .quota_manager()
            .unwrap()
            .get_quota_info("pvc-a")
            .await
            .unwrap();
        assert_eq!(charged, ((8 * BLOCK) as u64, BLOCK as u64));

        // Create a symlink with a long target so the kernel has to
        // allocate at least one block for it on tmpfs/ext4 (fast
        // symlinks fit inline, so a long target ensures st_blocks > 0).
        // Note: the FSAL's `symlink` API does not charge quota, mirror
        // that here by going through the host filesystem directly.
        let long_target = "x".repeat(200);
        std::os::unix::fs::symlink(&long_target, export.path().join("pvc-a/long-link")).unwrap();

        // Remove the symlink via the FSAL — must NOT refund anything.
        fs.remove(&dir, "long-link").await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(charged),
            "removing a symlink must not refund quota — symlink \
             creation is not charged, so refunding here would let a \
             client drift the usage counter below reality"
        );
    }

    #[tokio::test]
    async fn test_write_rejects_handle_after_symlink_swap_outside_export() {
        // File handles map to paths and are not invalidated when the
        // path is replaced. If a client gets a handle for a real file,
        // then (out of band) replaces the file with a symlink pointing
        // outside the export, the resolved path would still look fine
        // to `resolve_handle` — but the kernel-level open follows the
        // symlink. validate_path() must catch this so WRITE cannot
        // escape the exported tree.
        let (fs, _export) = create_test_fs();
        let root = fs.root_handle().await;
        let original_handle = fs.create(&root, "victim.bin", 0o644).await.unwrap();

        // Pretend the file got swapped for a symlink to /etc/hostname,
        // a path that is guaranteed to exist on Linux test runners and
        // is definitely outside the temp export.
        let victim_path = _export.path().join("victim.bin");
        std::fs::remove_file(&victim_path).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &victim_path).unwrap();

        let err = fs
            .write(&original_handle, 0, b"pwned")
            .await
            .expect_err("WRITE through swapped symlink should be rejected");
        assert!(
            err.to_string().contains("outside export root"),
            "got: {:#}",
            err
        );

        // /etc/hostname must still contain its original content.
        let hostname = std::fs::read_to_string("/etc/hostname").unwrap();
        assert!(
            !hostname.contains("pwned"),
            "WRITE leaked outside export: /etc/hostname now contains the payload"
        );
    }

    #[tokio::test]
    async fn test_setattr_size_rejects_handle_after_symlink_swap_outside_export() {
        // Same attack, exercised against SETATTR truncate: a stale
        // handle plus a symlink swap could otherwise let a client
        // truncate any file outside the export tree.
        let (fs, _export) = create_test_fs();
        let root = fs.root_handle().await;
        let handle = fs.create(&root, "victim.bin", 0o644).await.unwrap();

        let victim_path = _export.path().join("victim.bin");
        std::fs::remove_file(&victim_path).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &victim_path).unwrap();

        let err = fs
            .setattr_size(&handle, 0)
            .await
            .expect_err("SETATTR size through swapped symlink should be rejected");
        assert!(
            err.to_string().contains("outside export root"),
            "got: {:#}",
            err
        );

        // /etc/hostname must still be a normal non-empty file.
        let meta = std::fs::metadata("/etc/hostname").unwrap();
        assert!(
            meta.len() > 0,
            "SETATTR leaked outside export: /etc/hostname was truncated"
        );
    }

    #[tokio::test]
    async fn test_truncate_down_releases_quota() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (8 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "data.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; 4 * BLOCK]).await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((8 * BLOCK) as u64, (4 * BLOCK) as u64))
        );

        // Truncate down to one block: 3 blocks worth of allocated bytes
        // are released back to the quota.
        fs.setattr_size(&file, BLOCK as u64).await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((8 * BLOCK) as u64, BLOCK as u64))
        );
    }

    #[tokio::test]
    async fn test_truncate_up_is_not_tracked() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", 1000)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "sparse.bin", 0o644).await.unwrap();

        // Extend far beyond the quota limit: sparse files don't consume
        // real bytes, so the usage counter should stay at zero. Real
        // writes into those holes are still blocked by write().
        fs.setattr_size(&file, 100_000).await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some((1000, 0))
        );
    }

    #[tokio::test]
    async fn test_rename_within_same_quota_dir_does_not_change_usage() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.quota_manager()
            .unwrap()
            .set_quota("pvc-a", (4 * BLOCK) as u64)
            .await
            .unwrap();

        let dir = fs.lookup(&root, "pvc-a").await.unwrap();
        let file = fs.create(&dir, "a.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; BLOCK]).await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );

        fs.rename(&dir, "a.bin", &dir, "b.bin").await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );
    }

    #[tokio::test]
    async fn test_rename_across_quota_dirs_transfers_usage() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.mkdir(&root, "pvc-b", 0o755).await.unwrap();
        let qm = fs.quota_manager().unwrap();
        qm.set_quota("pvc-a", (4 * BLOCK) as u64).await.unwrap();
        qm.set_quota("pvc-b", (4 * BLOCK) as u64).await.unwrap();

        let dir_a = fs.lookup(&root, "pvc-a").await.unwrap();
        let dir_b = fs.lookup(&root, "pvc-b").await.unwrap();

        let file = fs.create(&dir_a, "f.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; BLOCK]).await.unwrap();

        fs.rename(&dir_a, "f.bin", &dir_b, "f.bin").await.unwrap();

        assert_eq!(
            qm.get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, 0))
        );
        assert_eq!(
            qm.get_quota_info("pvc-b").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );
    }

    #[tokio::test]
    async fn test_start_quota_reconciliation_runs_scan() {
        let (fs, export, _db) = create_test_fs_with_quota();
        let qm = fs.quota_manager().unwrap();
        qm.set_quota("pvc-a", 1_000_000).await.unwrap();
        // Stale usage recorded in redb/in-memory.
        qm.add_usage("pvc-a", 9999).await.unwrap();

        // Put real files on disk totaling two blocks. Reconciliation now
        // accounts in allocated bytes (st_blocks * 512), so block-aligned
        // writes give a deterministic expected value.
        let pvc_dir = export.path().join("pvc-a");
        std::fs::create_dir_all(&pvc_dir).unwrap();
        std::fs::write(pvc_dir.join("a.bin"), vec![0u8; BLOCK]).unwrap();
        std::fs::write(pvc_dir.join("b.bin"), vec![0u8; BLOCK]).unwrap();
        let expected_usage = (2 * BLOCK) as u64;

        fs.start_quota_reconciliation();

        // Poll until the background task has reconciled. Use a short loop
        // with a cap to avoid hanging a broken test.
        let mut waited = 0u64;
        loop {
            let info = fs.quota_manager().unwrap().get_quota_info("pvc-a").await;
            if info == Some((1_000_000, expected_usage)) {
                break;
            }
            if waited > 2000 {
                panic!("Reconciliation did not complete, last={:?}", info);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 20;
        }
    }

    #[tokio::test]
    async fn test_start_quota_reconciliation_no_quota_is_noop() {
        let (fs, _export) = create_test_fs();
        // Must not panic even though no quota manager is configured.
        fs.start_quota_reconciliation();
    }

    #[tokio::test]
    async fn test_apply_quota_bootstrap_seeds_new_entries() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let mut bootstrap = std::collections::HashMap::new();
        bootstrap.insert("pvc-a".to_string(), "1MB".to_string());

        fs.apply_quota_bootstrap(&bootstrap).await.unwrap();
        assert_eq!(
            fs.quota_manager().unwrap().get_quota_info("pvc-a").await,
            Some((1024 * 1024, 0))
        );
    }

    #[tokio::test]
    async fn test_apply_quota_bootstrap_noop_when_quota_disabled() {
        let (fs, _export) = create_test_fs();
        let mut bootstrap = std::collections::HashMap::new();
        bootstrap.insert("pvc-a".to_string(), "1MB".to_string());

        // Should not error even though no quota manager is configured;
        // the entries are logged and discarded.
        fs.apply_quota_bootstrap(&bootstrap).await.unwrap();
    }

    #[tokio::test]
    async fn test_rename_across_quota_dirs_rejected_when_target_full() {
        let (fs, _export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.mkdir(&root, "pvc-b", 0o755).await.unwrap();
        let qm = fs.quota_manager().unwrap();
        qm.set_quota("pvc-a", (4 * BLOCK) as u64).await.unwrap();
        qm.set_quota("pvc-b", BLOCK as u64).await.unwrap();

        let dir_a = fs.lookup(&root, "pvc-a").await.unwrap();
        let dir_b = fs.lookup(&root, "pvc-b").await.unwrap();

        let file = fs.create(&dir_a, "f.bin", 0o644).await.unwrap();
        fs.write(&file, 0, &vec![0u8; 2 * BLOCK]).await.unwrap();

        // Target quota is one block; source file occupies two blocks —
        // the cross-quota transfer must be rejected.
        let err = fs
            .rename(&dir_a, "f.bin", &dir_b, "f.bin")
            .await
            .expect_err("rename into full quota should fail");
        assert!(err.to_string().contains("Quota exceeded"), "got: {}", err);

        // Source unchanged; target still empty.
        assert_eq!(
            qm.get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, (2 * BLOCK) as u64))
        );
        assert_eq!(qm.get_quota_info("pvc-b").await, Some((BLOCK as u64, 0)));
    }

    #[tokio::test]
    async fn test_write_through_cross_pvc_symlink_charges_target_quota() {
        let (fs, export, _db) = create_test_fs_with_quota();
        let root = fs.root_handle().await;
        fs.mkdir(&root, "pvc-a", 0o755).await.unwrap();
        fs.mkdir(&root, "pvc-b", 0o755).await.unwrap();
        let qm = fs.quota_manager().unwrap();
        qm.set_quota("pvc-a", (4 * BLOCK) as u64).await.unwrap();
        qm.set_quota("pvc-b", (4 * BLOCK) as u64).await.unwrap();

        let dir_b = fs.lookup(&root, "pvc-b").await.unwrap();
        // Create the real target file inside pvc-b.
        let target = fs.create(&dir_b, "data.bin", 0o644).await.unwrap();

        // Hand-create a cross-PVC symlink (the FSAL symlink op validates
        // names but not targets, and we do not need to go through it for
        // this test — point pvc-a/link at pvc-b/data.bin directly on the
        // host filesystem to set up the scenario).
        std::os::unix::fs::symlink(
            export.path().join("pvc-b").join("data.bin"),
            export.path().join("pvc-a").join("link"),
        )
        .unwrap();

        // Look up the link via the FSAL so we get a handle for it.
        let dir_a = fs.lookup(&root, "pvc-a").await.unwrap();
        let link_handle = fs.lookup(&dir_a, "link").await.unwrap();

        // Write through the link. The kernel will follow the symlink and
        // mutate pvc-b/data.bin; quota_target now canonicalises first,
        // so the bytes must be charged to pvc-b, not pvc-a.
        fs.write(&link_handle, 0, &vec![0u8; BLOCK]).await.unwrap();
        // Sanity: the target's blocks really did grow.
        let target_alloc = fs.statvfs(&target).await.unwrap();
        assert!(target_alloc.free_bytes < (4 * BLOCK) as u64);

        assert_eq!(
            qm.get_quota_info("pvc-a").await,
            Some(((4 * BLOCK) as u64, 0))
        );
        assert_eq!(
            qm.get_quota_info("pvc-b").await,
            Some(((4 * BLOCK) as u64, BLOCK as u64))
        );
    }
}
