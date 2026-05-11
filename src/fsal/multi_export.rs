// Multi-export filesystem router
//
// Owns one `LocalFilesystem` per configured export and routes every
// `Filesystem` operation to the inner backend identified by the export uid
// embedded in the file handle prefix (see `fsal::handle`).
//
// MOUNT consumes this type via `ExportRegistry`; the NFS dispatcher still
// consumes it via `Filesystem` (Phase 5 of issue #26 will split that).

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{BackendConfig as ConfigBackend, ExportConfig};

use super::handle::{FileHandle, FileHandleExt};
use super::local::LocalFilesystem;
use super::{DirEntry, ExportInfo, ExportRegistry, FileAttributes, FileType, Filesystem};

/// Internal record for one configured export.
struct ExportEntry {
    name: String,
    read_only: bool,
    /// Concrete backend so the wrapper can call the inherent
    /// `LocalFilesystem::root_file_handle()` without going through an
    /// `async fn` trait dispatch.
    fs: Arc<LocalFilesystem>,
}

/// Routes NFS operations to the right backend based on the export uid
/// prefix carried in every file handle.
///
/// Indexes:
/// - `exports`: uid → entry, used to dispatch handle-based operations.
/// - `name_index`: name → uid, used by MOUNT MNT to resolve a path to its
///   root handle.
pub struct MultiExportFilesystem {
    exports: HashMap<u32, ExportEntry>,
    name_index: HashMap<String, u32>,
}

impl MultiExportFilesystem {
    /// Build the router from a validated list of exports.
    ///
    /// Assumes `Config::validate()` has already enforced uniqueness of
    /// `uid`/`name` and rejected `uid == 0`; this method only constructs
    /// the per-export `LocalFilesystem` instances and asserts the
    /// invariants defensively.
    pub fn build_from_config(exports: &[ExportConfig]) -> Result<Self> {
        if exports.is_empty() {
            return Err(anyhow!(
                "MultiExportFilesystem requires at least one export"
            ));
        }

        let mut entries: HashMap<u32, ExportEntry> = HashMap::with_capacity(exports.len());
        let mut name_index: HashMap<String, u32> = HashMap::with_capacity(exports.len());

        for export in exports {
            if export.uid == 0 {
                return Err(anyhow!(
                    "Export '{}' has uid 0; uid must be non-zero",
                    export.name
                ));
            }

            let fs = match &export.backend {
                ConfigBackend::Local { path } => LocalFilesystem::new(path, export.uid)
                    .with_context(|| {
                        format!(
                            "Failed to initialize local backend for export '{}' at {:?}",
                            export.name, path
                        )
                    })?,
            };

            let entry = ExportEntry {
                name: export.name.clone(),
                read_only: export.read_only,
                fs: Arc::new(fs),
            };

            if entries.insert(export.uid, entry).is_some() {
                return Err(anyhow!(
                    "Duplicate export uid {} for '{}' (Config::validate should have caught this)",
                    export.uid,
                    export.name
                ));
            }
            if name_index.insert(export.name.clone(), export.uid).is_some() {
                return Err(anyhow!(
                    "Duplicate export name '{}' (Config::validate should have caught this)",
                    export.name
                ));
            }
        }

        Ok(Self {
            exports: entries,
            name_index,
        })
    }

    /// Look up the entry that owns `handle`, decoding the uid prefix.
    ///
    /// Error strings start with `"Invalid handle"` so the per-operation NFS
    /// handlers in `src/nfs/{read,write,access,fsstat,fsinfo}.rs` map them to
    /// `NFS3ERR_STALE` via their substring matchers; without that prefix they
    /// would fall through to `NFS3ERR_IO`.
    fn entry_for_handle(&self, handle: &FileHandle) -> Result<&ExportEntry> {
        let uid = handle
            .as_slice()
            .export_uid()
            .ok_or_else(|| anyhow!("Invalid handle: too short to carry an export uid"))?;
        self.exports
            .get(&uid)
            .ok_or_else(|| anyhow!("Invalid handle: stale export uid {} (no such export)", uid))
    }
}

impl ExportRegistry for MultiExportFilesystem {
    fn root_handle_for(&self, name: &str) -> Option<FileHandle> {
        let uid = self.name_index.get(name)?;
        let entry = self.exports.get(uid)?;
        Some(entry.fs.root_file_handle())
    }

    fn list_exports(&self) -> Vec<ExportInfo> {
        // Sort by uid for stable, predictable output (HashMap iteration
        // order is otherwise nondeterministic and would destabilize logs
        // and the eventual MOUNT EXPORT response).
        let mut uids: Vec<u32> = self.exports.keys().copied().collect();
        uids.sort_unstable();
        uids.into_iter()
            .map(|uid| {
                let entry = &self.exports[&uid];
                ExportInfo {
                    name: entry.name.clone(),
                    uid,
                    read_only: entry.read_only,
                }
            })
            .collect()
    }

    fn is_read_only(&self, handle: &FileHandle) -> bool {
        match handle.as_slice().export_uid() {
            Some(uid) => self.exports.get(&uid).is_some_and(|e| e.read_only),
            None => false,
        }
    }

    fn export_for_handle(&self, handle: &FileHandle) -> Option<u32> {
        handle.as_slice().export_uid()
    }
}

#[async_trait]
impl Filesystem for MultiExportFilesystem {
    async fn lookup(&self, dir_handle: &FileHandle, name: &str) -> Result<FileHandle> {
        self.entry_for_handle(dir_handle)?
            .fs
            .lookup(dir_handle, name)
            .await
    }

    async fn getattr(&self, handle: &FileHandle) -> Result<FileAttributes> {
        self.entry_for_handle(handle)?.fs.getattr(handle).await
    }

    async fn read(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<Vec<u8>> {
        self.entry_for_handle(handle)?
            .fs
            .read(handle, offset, count)
            .await
    }

    async fn readdir(
        &self,
        dir_handle: &FileHandle,
        cookie: u64,
        count: u32,
    ) -> Result<(Vec<DirEntry>, bool)> {
        self.entry_for_handle(dir_handle)?
            .fs
            .readdir(dir_handle, cookie, count)
            .await
    }

    async fn write(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        self.entry_for_handle(handle)?
            .fs
            .write(handle, offset, data)
            .await
    }

    async fn setattr_size(&self, handle: &FileHandle, size: u64) -> Result<()> {
        self.entry_for_handle(handle)?
            .fs
            .setattr_size(handle, size)
            .await
    }

    async fn setattr_mode(&self, handle: &FileHandle, mode: u32) -> Result<()> {
        self.entry_for_handle(handle)?
            .fs
            .setattr_mode(handle, mode)
            .await
    }

    async fn setattr_owner(
        &self,
        handle: &FileHandle,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<()> {
        self.entry_for_handle(handle)?
            .fs
            .setattr_owner(handle, uid, gid)
            .await
    }

    async fn create(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        self.entry_for_handle(dir_handle)?
            .fs
            .create(dir_handle, name, mode)
            .await
    }

    async fn remove(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        self.entry_for_handle(dir_handle)?
            .fs
            .remove(dir_handle, name)
            .await
    }

    async fn mkdir(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        self.entry_for_handle(dir_handle)?
            .fs
            .mkdir(dir_handle, name, mode)
            .await
    }

    async fn rmdir(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        self.entry_for_handle(dir_handle)?
            .fs
            .rmdir(dir_handle, name)
            .await
    }

    async fn rename(
        &self,
        from_dir_handle: &FileHandle,
        from_name: &str,
        to_dir_handle: &FileHandle,
        to_name: &str,
    ) -> Result<()> {
        // Cross-export rename is not supported: both directories must live
        // in the same backend so the rename(2) call stays on a single
        // filesystem. The error string contains "cross-device" so the NFS
        // RENAME handler maps it to NFS3ERR_XDEV (RFC 1813 §3.3.14) rather
        // than the catch-all NFS3ERR_IO.
        let from_uid = from_dir_handle
            .as_slice()
            .export_uid()
            .ok_or_else(|| anyhow!("Invalid handle: source too short to carry an export uid"))?;
        let to_uid = to_dir_handle
            .as_slice()
            .export_uid()
            .ok_or_else(|| anyhow!("Invalid handle: target too short to carry an export uid"))?;
        if from_uid != to_uid {
            return Err(anyhow!(
                "cross-device rename not supported (source export uid {}, target export uid {})",
                from_uid,
                to_uid
            ));
        }
        let entry = self.exports.get(&from_uid).ok_or_else(|| {
            anyhow!(
                "Invalid handle: stale export uid {} (no such export)",
                from_uid
            )
        })?;
        entry
            .fs
            .rename(from_dir_handle, from_name, to_dir_handle, to_name)
            .await
    }

    async fn symlink(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        target: &str,
    ) -> Result<FileHandle> {
        self.entry_for_handle(dir_handle)?
            .fs
            .symlink(dir_handle, name, target)
            .await
    }

    async fn readlink(&self, handle: &FileHandle) -> Result<String> {
        self.entry_for_handle(handle)?.fs.readlink(handle).await
    }

    async fn link(
        &self,
        file_handle: &FileHandle,
        dir_handle: &FileHandle,
        name: &str,
    ) -> Result<FileHandle> {
        // Hard links cannot cross filesystems; require both handles to
        // live in the same export. The error string contains "cross-device"
        // so the NFS LINK handler maps it to NFS3ERR_XDEV (RFC 1813 §3.3.15).
        let file_uid = file_handle
            .as_slice()
            .export_uid()
            .ok_or_else(|| anyhow!("Invalid handle: file too short to carry an export uid"))?;
        let dir_uid = dir_handle
            .as_slice()
            .export_uid()
            .ok_or_else(|| anyhow!("Invalid handle: dir too short to carry an export uid"))?;
        if file_uid != dir_uid {
            return Err(anyhow!(
                "cross-device link not supported (file export uid {}, dir export uid {})",
                file_uid,
                dir_uid
            ));
        }
        let entry = self.exports.get(&file_uid).ok_or_else(|| {
            anyhow!(
                "Invalid handle: stale export uid {} (no such export)",
                file_uid
            )
        })?;
        entry.fs.link(file_handle, dir_handle, name).await
    }

    async fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<()> {
        self.entry_for_handle(handle)?
            .fs
            .commit(handle, offset, count)
            .await
    }

    async fn mknod(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        file_type: FileType,
        mode: u32,
        rdev: (u32, u32),
    ) -> Result<FileHandle> {
        self.entry_for_handle(dir_handle)?
            .fs
            .mknod(dir_handle, name, file_type, mode, rdev)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig as ConfigBackend;
    use crate::fsal::handle::HANDLE_LEN;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn export(name: &str, uid: u32, path: PathBuf, read_only: bool) -> ExportConfig {
        ExportConfig {
            name: name.to_string(),
            uid,
            read_only,
            backend: ConfigBackend::Local { path },
        }
    }

    /// Build a router with two exports rooted at fresh temp dirs.
    /// Returns the router plus the temp dirs (kept alive for the test).
    fn build_two_export_router() -> (MultiExportFilesystem, TempDir, TempDir) {
        let data_dir = TempDir::new().unwrap();
        let backup_dir = TempDir::new().unwrap();
        let exports = vec![
            export("/data", 1, data_dir.path().to_path_buf(), false),
            export("/backup", 2, backup_dir.path().to_path_buf(), true),
        ];
        let router = MultiExportFilesystem::build_from_config(&exports)
            .expect("build_from_config must succeed for valid exports");
        (router, data_dir, backup_dir)
    }

    #[test]
    fn build_from_config_succeeds_with_two_exports() {
        let (router, _data, _backup) = build_two_export_router();
        assert_eq!(router.exports.len(), 2);
        assert_eq!(router.name_index.get("/data"), Some(&1));
        assert_eq!(router.name_index.get("/backup"), Some(&2));
    }

    #[test]
    fn build_from_config_rejects_empty() {
        // `expect_err` would force `Debug` on MultiExportFilesystem just for
        // this one negative test; match on the Err arm instead so the
        // production type stays `Debug`-free.
        let result = MultiExportFilesystem::build_from_config(&[]);
        match result {
            Ok(_) => panic!("empty exports must fail"),
            Err(err) => assert!(err.to_string().contains("at least one"), "got: {err}"),
        }
    }

    #[test]
    fn root_handle_for_known_name_returns_handle() {
        let (router, _data, _backup) = build_two_export_router();
        let h = router
            .root_handle_for("/data")
            .expect("known name must resolve");
        assert_eq!(h.len(), HANDLE_LEN);
        assert_eq!(h.as_slice().export_uid(), Some(1));
    }

    #[test]
    fn root_handle_for_unknown_name_returns_none() {
        let (router, _data, _backup) = build_two_export_router();
        assert!(router.root_handle_for("/missing").is_none());
    }

    #[test]
    fn list_exports_is_sorted_by_uid() {
        let (router, _data, _backup) = build_two_export_router();
        let infos = router.list_exports();
        assert_eq!(
            infos,
            vec![
                ExportInfo {
                    name: "/data".to_string(),
                    uid: 1,
                    read_only: false,
                },
                ExportInfo {
                    name: "/backup".to_string(),
                    uid: 2,
                    read_only: true,
                },
            ]
        );
    }

    #[test]
    fn is_read_only_reflects_owning_export() {
        let (router, _data, _backup) = build_two_export_router();
        let rw = router.root_handle_for("/data").unwrap();
        let ro = router.root_handle_for("/backup").unwrap();
        assert!(!router.is_read_only(&rw));
        assert!(router.is_read_only(&ro));
    }

    #[test]
    fn is_read_only_unknown_uid_returns_false() {
        let (router, _data, _backup) = build_two_export_router();
        let mut bogus = vec![0u8; HANDLE_LEN];
        bogus[..4].copy_from_slice(&999u32.to_be_bytes());
        assert!(!router.is_read_only(&bogus));
    }

    #[test]
    fn export_for_handle_decodes_prefix_or_returns_none() {
        let (router, _data, _backup) = build_two_export_router();
        let h = router.root_handle_for("/data").unwrap();
        assert_eq!(router.export_for_handle(&h), Some(1));

        // All-zero prefix decodes to Some(0) — that's a real (if invalid)
        // uid; the registry distinguishes "too short to decode" (None) from
        // "decoded but unknown" (Some).
        let zero = vec![0u8; HANDLE_LEN];
        assert_eq!(router.export_for_handle(&zero), Some(0));

        // Slice shorter than the prefix length cannot be decoded at all.
        let short: FileHandle = vec![0u8; 3];
        assert_eq!(router.export_for_handle(&short), None);
    }

    #[tokio::test]
    async fn lookup_routes_to_inner_filesystem_by_handle_prefix() {
        let (router, data_dir, backup_dir) = build_two_export_router();

        // Plant a distinguishable file in each export so we can prove
        // routing went to the right backend.
        std::fs::write(data_dir.path().join("hello.txt"), b"data").unwrap();
        std::fs::write(backup_dir.path().join("hello.txt"), b"backup").unwrap();

        let data_root = router.root_handle_for("/data").unwrap();
        let backup_root = router.root_handle_for("/backup").unwrap();

        let from_data = router.lookup(&data_root, "hello.txt").await.unwrap();
        let from_backup = router.lookup(&backup_root, "hello.txt").await.unwrap();

        assert_eq!(from_data.as_slice().export_uid(), Some(1));
        assert_eq!(from_backup.as_slice().export_uid(), Some(2));

        // And the actual file contents are export-specific, confirming
        // the routing reached different backends rather than just
        // returning prefixed handles for the same path.
        assert_eq!(router.read(&from_data, 0, 64).await.unwrap(), b"data");
        assert_eq!(router.read(&from_backup, 0, 64).await.unwrap(), b"backup");
    }

    #[tokio::test]
    async fn lookup_with_short_handle_errors() {
        let (router, _data, _backup) = build_two_export_router();
        let short: FileHandle = vec![0u8; 3];
        let err = router
            .lookup(&short, "anything")
            .await
            .expect_err("must fail");
        // Substring "Invalid handle" is what the NFS handler matchers in
        // src/nfs/{read,write,access,fsstat,fsinfo}.rs key on to map to
        // NFS3ERR_STALE; assert on it so a future rename can't silently
        // demote stale-handle errors back to NFS3ERR_IO.
        assert!(err.to_string().contains("Invalid handle"), "got: {err}");
    }

    #[tokio::test]
    async fn lookup_with_unknown_uid_errors() {
        let (router, _data, _backup) = build_two_export_router();
        let mut bogus = vec![0u8; HANDLE_LEN];
        bogus[..4].copy_from_slice(&999u32.to_be_bytes());
        let err = router
            .lookup(&bogus, "anything")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("Invalid handle"), "got: {err}");
        assert!(err.to_string().contains("stale export uid"), "got: {err}");
    }

    #[tokio::test]
    async fn cross_export_rename_rejects() {
        let (router, _data, _backup) = build_two_export_router();
        let from = router.root_handle_for("/data").unwrap();
        let to = router.root_handle_for("/backup").unwrap();
        let err = router
            .rename(&from, "a", &to, "b")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("cross-device"), "got: {err}");
    }

    #[tokio::test]
    async fn cross_export_link_rejects() {
        let (router, _data, _backup) = build_two_export_router();
        let file = router.root_handle_for("/data").unwrap();
        let dir = router.root_handle_for("/backup").unwrap();
        let err = router
            .link(&file, &dir, "alias")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("cross-device"), "got: {err}");
    }

    #[test]
    fn is_read_only_with_short_handle_returns_false() {
        // A handle shorter than the export-uid prefix cannot resolve to any
        // export, so the read-only question has no answer; the registry
        // returns false rather than panicking or defaulting to true.
        let (router, _data, _backup) = build_two_export_router();
        let short: FileHandle = vec![0u8; 3];
        assert!(!router.is_read_only(&short));
    }
}
