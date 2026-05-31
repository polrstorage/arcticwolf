// Multi-export filesystem router
//
// Owns one `LocalFilesystem` per configured export and routes every
// `Filesystem` operation to the inner backend identified by the export uid
// embedded in the file handle prefix (see `fsal::handle`).
//
// MOUNT consumes this type via `ExportRegistry`; the NFS dispatcher consumes
// it via the `NfsBackend` super-trait, which combines `Filesystem` and
// `ExportRegistry` (see #26).
//
// Live state lives behind `ArcSwap<ExportsSnapshot>` so the admin path
// (add/remove/update_export, wired up by issue #25 Phase 5) can replace the
// snapshot atomically while NFS RPCs are in flight. The contract every trait
// method below upholds is: `load()` the snapshot, clone the inner
// `Arc<LocalFilesystem>` out, drop the snapshot guard, and only then
// `.await`. No `Guard` ever crosses an await point.

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::config::{BackendConfig as ConfigBackend, ExportConfig};

use super::handle::{FileHandle, FileHandleExt};
use super::local::LocalFilesystem;
use super::{DirEntry, ExportInfo, ExportRegistry, FileAttributes, FileType, Filesystem, FsStats};

/// Normalize and validate an export name.
///
/// Shared by [`MultiExportFilesystem::add_export`] and
/// [`crate::config::Config::validate`] so admin input and config-file input
/// are held to the exact same rules. Both call sites previously only checked
/// `starts_with('/')`, which let `..`/`//`/trailing-slash forms through —
/// harmless in the static config case but dangerous on the admin path, where
/// the input is more hostile.
///
/// Rules:
/// - empty is rejected
/// - must start with `/`
/// - rejects `..` as a path component (anywhere)
/// - rejects `//` (repeated slashes)
/// - rejects trailing `/` unless the name is exactly `/`
pub(crate) fn validate_export_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("invalid export name '': must not be empty");
    }
    if !name.starts_with('/') {
        bail!("invalid export name '{name}': must start with '/' (e.g. '/data')");
    }
    if name.contains("//") {
        bail!("invalid export name '{name}': must not contain '//'");
    }
    if name.len() > 1 && name.ends_with('/') {
        bail!("invalid export name '{name}': must not end with '/'");
    }
    for component in name.split('/') {
        if component == ".." {
            bail!("invalid export name '{name}': must not contain '..'");
        }
    }
    Ok(())
}

/// Internal record for one configured export.
///
/// `Clone` so `add_export`/`remove_export`/`update_export` can build a fresh
/// snapshot from the current one with `HashMap::clone()` (which moves each
/// entry through `Clone`); cloning an `ExportEntry` only clones the inner
/// `Arc<LocalFilesystem>` (cheap refcount bump), it does not duplicate the
/// backend's `HandleManager` state.
#[derive(Clone)]
struct ExportEntry {
    name: String,
    read_only: bool,
    /// Concrete backend so the wrapper can call the inherent
    /// `LocalFilesystem::root_file_handle()` without going through an
    /// `async fn` trait dispatch.
    fs: Arc<LocalFilesystem>,
}

/// Immutable snapshot of the live export set.
///
/// `ArcSwap<ExportsSnapshot>` owns the live snapshot; admin mutations build
/// a new `ExportsSnapshot` and `store` it. Indexes inside the snapshot are
/// never mutated in place — every change is copy-on-write at the snapshot
/// granularity, so readers that have already `load()`ed a snapshot continue
/// to see a consistent view for the duration of their request.
struct ExportsSnapshot {
    /// uid → entry, used to dispatch handle-based operations.
    by_uid: HashMap<u32, ExportEntry>,
    /// name → uid, used by MOUNT MNT to resolve a path to its root handle.
    by_name: HashMap<String, u32>,
    /// Export uids that were live and then removed during this daemon's
    /// lifetime. `add_export` refuses to reuse one, because in-flight client
    /// handles encoded with the old uid would silently re-route to a new
    /// (different!) backend if the slot were recycled. Cleared on restart,
    /// since the configuration file is re-read fresh on startup.
    retired_uids: HashSet<u32>,
}

impl ExportsSnapshot {
    /// Look up the entry that owns `handle`, decoding the uid prefix.
    ///
    /// Error strings start with `"Invalid handle"` so the per-operation NFS
    /// handlers map them to `NFS3ERR_STALE`: `fsstat` and `pathconf` go
    /// through [`crate::nfs::error::classify_handle_error`] (which also walks
    /// the error chain for `io::ErrorKind::NotFound`), while `read`, `write`,
    /// `access`, and `fsinfo` still substring-match `"Invalid handle"`
    /// directly. Without that prefix they would fall through to
    /// `NFS3ERR_IO`. The wider unification of error mapping is tracked in
    /// the FSAL typed-error issue (#28).
    fn entry_for_handle(&self, handle: &FileHandle) -> Result<&ExportEntry> {
        let uid = handle
            .as_slice()
            .export_uid()
            .ok_or_else(|| anyhow!("Invalid handle: too short to carry an export uid"))?;
        self.by_uid
            .get(&uid)
            .ok_or_else(|| anyhow!("Invalid handle: stale export uid {} (no such export)", uid))
    }
}

/// Selector used by admin mutations to identify an existing export.
///
/// V1 ships only by-name and by-uid; the public admin CLI surfaces the
/// by-name form (`arcticwolfctl export remove /data`), while the by-uid
/// form is used by tests and by future audit-log replay flows that have
/// the uid but not necessarily the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportSelector {
    Name(String),
    Uid(u32),
}

/// Routes NFS operations to the right backend based on the export uid
/// prefix carried in every file handle.
pub struct MultiExportFilesystem {
    /// Atomic-swap snapshot of the live export set.
    ///
    /// Read path (NFS RPC) calls `load()`, clones the inner
    /// `Arc<LocalFilesystem>` out, drops the snapshot guard, and only then
    /// `.await`s the inner operation — no `Guard` is ever held across an
    /// await point, so the writer's `store(Arc::new(_))` cannot stall on a
    /// long-running RPC.
    ///
    /// Write path (`add_export`/`remove_export`/`update_export`) builds a
    /// new `ExportsSnapshot` from the current one (cloning the two HashMaps
    /// and the `HashSet`) and swaps it in atomically.
    state: ArcSwap<ExportsSnapshot>,
    /// Serializes admin mutations against each other.
    ///
    /// Each of `add_export`/`remove_export`/`update_export` follows a
    /// `load → check → build → store` sequence; without a writer-side lock,
    /// two concurrent mutations can both pass duplicate-checks against the
    /// same `load()`ed snapshot and one's `store` silently overwrites the
    /// other. Readers (the `Filesystem` and `ExportRegistry` impls) never
    /// touch this lock — they go straight to `state.load()`, so contention
    /// is bounded by concurrent admin calls only.
    ///
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the critical section is
    /// fully synchronous (snapshot clone + `LocalFilesystem::new`, both sync)
    /// and never crosses an `.await`.
    write_lock: Mutex<()>,
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

        let mut by_uid: HashMap<u32, ExportEntry> = HashMap::with_capacity(exports.len());
        let mut by_name: HashMap<String, u32> = HashMap::with_capacity(exports.len());

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

            if by_uid.insert(export.uid, entry).is_some() {
                return Err(anyhow!(
                    "Duplicate export uid {} for '{}' (Config::validate should have caught this)",
                    export.uid,
                    export.name
                ));
            }
            if by_name.insert(export.name.clone(), export.uid).is_some() {
                return Err(anyhow!(
                    "Duplicate export name '{}' (Config::validate should have caught this)",
                    export.name
                ));
            }
        }

        let snapshot = ExportsSnapshot {
            by_uid,
            by_name,
            retired_uids: HashSet::new(),
        };
        Ok(Self {
            state: ArcSwap::new(Arc::new(snapshot)),
            write_lock: Mutex::new(()),
        })
    }

    /// Resolve `handle` against the live snapshot and return a clone of the
    /// owning backend's `Arc<LocalFilesystem>`.
    ///
    /// This is the single chokepoint every single-handle async trait method
    /// funnels through: the snapshot `Guard` is dropped when this function
    /// returns, so the caller awaits on a plain `Arc<LocalFilesystem>` and
    /// never holds a guard across an await point.
    fn inner_for_handle(&self, handle: &FileHandle) -> Result<Arc<LocalFilesystem>> {
        let snapshot = self.state.load();
        Ok(snapshot.entry_for_handle(handle)?.fs.clone())
    }

    /// Resolve a pair of handles for an inherently single-export operation
    /// (`rename`, `link`) and return the shared backend.
    ///
    /// Both handles must decode to the same export uid; cross-export pairs
    /// produce an error string containing `"cross-device"` so the NFS
    /// dispatchers can map it to `NFS3ERR_XDEV` (RFC 1813 §3.3.14 for
    /// RENAME, §3.3.15 for LINK). The shared backend is cloned out before
    /// the snapshot guard is dropped, preserving the "no guard across
    /// `.await`" invariant the trait methods rely on.
    fn inner_for_same_export_pair(
        &self,
        a: &FileHandle,
        b: &FileHandle,
        op: &str,
        a_label: &str,
        b_label: &str,
    ) -> Result<Arc<LocalFilesystem>> {
        let a_uid = a.as_slice().export_uid().ok_or_else(|| {
            anyhow!(
                "Invalid handle: {} too short to carry an export uid",
                a_label
            )
        })?;
        let b_uid = b.as_slice().export_uid().ok_or_else(|| {
            anyhow!(
                "Invalid handle: {} too short to carry an export uid",
                b_label
            )
        })?;
        if a_uid != b_uid {
            return Err(anyhow!(
                "cross-device {} not supported ({} export uid {}, {} export uid {})",
                op,
                a_label,
                a_uid,
                b_label,
                b_uid
            ));
        }
        let snapshot = self.state.load();
        let entry = snapshot.by_uid.get(&a_uid).ok_or_else(|| {
            anyhow!(
                "Invalid handle: stale export uid {} (no such export)",
                a_uid
            )
        })?;
        Ok(entry.fs.clone())
    }

    /// Resolve `selector` to the uid of the matching live export.
    ///
    /// Reads the supplied snapshot rather than re-`load()`ing so admin
    /// mutations can do the uid lookup and the consistency checks against
    /// the same snapshot they're about to derive a successor from.
    fn resolve_selector(snapshot: &ExportsSnapshot, selector: &ExportSelector) -> Result<u32> {
        match selector {
            ExportSelector::Name(name) => snapshot
                .by_name
                .get(name.as_str())
                .copied()
                .ok_or_else(|| anyhow!("No export named {:?}", name)),
            ExportSelector::Uid(uid) => {
                if snapshot.by_uid.contains_key(uid) {
                    Ok(*uid)
                } else {
                    Err(anyhow!("No export with uid {}", uid))
                }
            }
        }
    }

    /// Add a new export to the live snapshot.
    ///
    /// Validates the same invariants `Config::validate()` enforces at
    /// startup (uid != 0, name starts with `/`, backend-specific path
    /// rules) and additionally rejects uids that were retired earlier in
    /// this daemon's lifetime — see `ExportsSnapshot::retired_uids` for why.
    ///
    /// Rebuilds the snapshot copy-on-write and `store`s it in a single
    /// atomic swap; on `Err` the live snapshot is untouched.
    ///
    /// Serialized against other admin mutations via `write_lock`; readers
    /// (NFS RPC handlers) are unaffected.
    pub fn add_export(&self, config: &ExportConfig) -> Result<()> {
        // Name normalization runs before the write lock so cheap input
        // rejections don't serialize behind a slower mutation.
        validate_export_name(&config.name)?;

        if config.uid == 0 {
            bail!(
                "Export '{}' has uid 0; uid must be a non-zero u32",
                config.name
            );
        }
        match &config.backend {
            ConfigBackend::Local { path } => {
                if !path.is_absolute() {
                    bail!(
                        "Export '{}' local backend path '{}' must be absolute",
                        config.name,
                        path.display()
                    );
                }
            }
        }

        let _guard = self.write_lock.lock().expect("write_lock poisoned");
        let current = self.state.load();
        if current.by_uid.contains_key(&config.uid) {
            bail!("Export uid {} is already in use", config.uid);
        }
        if current.retired_uids.contains(&config.uid) {
            bail!(
                "Export uid {} was retired this run and cannot be reused until daemon restart",
                config.uid
            );
        }
        if current.by_name.contains_key(&config.name) {
            bail!("Export name {:?} is already in use", config.name);
        }

        // Build the backend before mutating any indexes so a backend
        // initialization failure (path doesn't exist, not a directory, etc.)
        // surfaces with the operator's input still rejected and the live
        // snapshot unchanged.
        let fs = match &config.backend {
            ConfigBackend::Local { path } => {
                LocalFilesystem::new(path, config.uid).with_context(|| {
                    format!(
                        "Failed to initialize local backend for export '{}' at {:?}",
                        config.name, path
                    )
                })?
            }
        };

        let entry = ExportEntry {
            name: config.name.clone(),
            read_only: config.read_only,
            fs: Arc::new(fs),
        };

        let mut by_uid = current.by_uid.clone();
        let mut by_name = current.by_name.clone();
        by_uid.insert(config.uid, entry);
        by_name.insert(config.name.clone(), config.uid);

        let new_snapshot = ExportsSnapshot {
            by_uid,
            by_name,
            retired_uids: current.retired_uids.clone(),
        };
        // Drop the read guard before the store so we don't hold it across
        // the allocation in `Arc::new` (harmless but tidier).
        drop(current);
        self.state.store(Arc::new(new_snapshot));
        Ok(())
    }

    /// Remove an export by name or uid. Returns the uid that was removed
    /// (now in `retired_uids`). Errors if no live export matches.
    ///
    /// Serialized against other admin mutations via `write_lock`; readers
    /// (NFS RPC handlers) are unaffected.
    pub fn remove_export(&self, selector: &ExportSelector) -> Result<u32> {
        let _guard = self.write_lock.lock().expect("write_lock poisoned");
        let current = self.state.load();
        let uid = Self::resolve_selector(&current, selector)?;

        // `unwrap` is safe: `resolve_selector` returns only uids that are
        // present in `current.by_uid`.
        let name = current.by_uid[&uid].name.clone();

        let mut by_uid = current.by_uid.clone();
        let mut by_name = current.by_name.clone();
        let mut retired_uids = current.retired_uids.clone();
        by_uid.remove(&uid);
        by_name.remove(&name);
        retired_uids.insert(uid);

        let new_snapshot = ExportsSnapshot {
            by_uid,
            by_name,
            retired_uids,
        };
        drop(current);
        self.state.store(Arc::new(new_snapshot));
        Ok(uid)
    }

    /// Update mutable fields on an existing export.
    ///
    /// V1 only supports flipping `read_only`. Renaming an export or moving
    /// its backing path requires more care (handles already issued to
    /// clients embed the uid, not the name, so a rename is observable
    /// without invalidating handles, but a path change would silently
    /// re-route them) and is deferred to a future phase per the issue plan.
    ///
    /// Serialized against other admin mutations via `write_lock`; readers
    /// (NFS RPC handlers) are unaffected.
    pub fn update_export(
        &self,
        selector: &ExportSelector,
        new_read_only: Option<bool>,
    ) -> Result<()> {
        if new_read_only.is_none() {
            bail!("update_export called with no fields to mutate");
        }

        let _guard = self.write_lock.lock().expect("write_lock poisoned");
        let current = self.state.load();
        let uid = Self::resolve_selector(&current, selector)?;

        let mut by_uid = current.by_uid.clone();
        if let Some(ro) = new_read_only {
            // `unwrap` safe: `resolve_selector` proved the key exists, and
            // we just cloned the map so it still does.
            by_uid.get_mut(&uid).unwrap().read_only = ro;
        }

        let new_snapshot = ExportsSnapshot {
            by_uid,
            by_name: current.by_name.clone(),
            retired_uids: current.retired_uids.clone(),
        };
        drop(current);
        self.state.store(Arc::new(new_snapshot));
        Ok(())
    }
}

impl ExportRegistry for MultiExportFilesystem {
    fn root_handle_for(&self, name: &str) -> Option<FileHandle> {
        // Synchronous — no `.await` follows, so holding the guard is safe.
        let snapshot = self.state.load();
        let uid = snapshot.by_name.get(name)?;
        let entry = snapshot.by_uid.get(uid)?;
        Some(entry.fs.root_file_handle())
    }

    fn list_exports(&self) -> Vec<ExportInfo> {
        // Sort by uid for stable, predictable output (HashMap iteration
        // order is otherwise nondeterministic and would destabilize logs
        // and the eventual MOUNT EXPORT response).
        let snapshot = self.state.load();
        let mut uids: Vec<u32> = snapshot.by_uid.keys().copied().collect();
        uids.sort_unstable();
        uids.into_iter()
            .map(|uid| {
                let entry = &snapshot.by_uid[&uid];
                ExportInfo {
                    name: entry.name.clone(),
                    uid,
                    read_only: entry.read_only,
                }
            })
            .collect()
    }

    fn is_read_only(&self, handle: &FileHandle) -> bool {
        let snapshot = self.state.load();
        match handle.as_slice().export_uid() {
            Some(uid) => snapshot.by_uid.get(&uid).is_some_and(|e| e.read_only),
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
        let inner = self.inner_for_handle(dir_handle)?;
        inner.lookup(dir_handle, name).await
    }

    async fn getattr(&self, handle: &FileHandle) -> Result<FileAttributes> {
        let inner = self.inner_for_handle(handle)?;
        inner.getattr(handle).await
    }

    async fn read(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<Vec<u8>> {
        let inner = self.inner_for_handle(handle)?;
        inner.read(handle, offset, count).await
    }

    async fn readdir(
        &self,
        dir_handle: &FileHandle,
        cookie: u64,
        count: u32,
    ) -> Result<(Vec<DirEntry>, bool)> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.readdir(dir_handle, cookie, count).await
    }

    async fn write(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        let inner = self.inner_for_handle(handle)?;
        inner.write(handle, offset, data).await
    }

    async fn setattr_size(&self, handle: &FileHandle, size: u64) -> Result<()> {
        let inner = self.inner_for_handle(handle)?;
        inner.setattr_size(handle, size).await
    }

    async fn setattr_mode(&self, handle: &FileHandle, mode: u32) -> Result<()> {
        let inner = self.inner_for_handle(handle)?;
        inner.setattr_mode(handle, mode).await
    }

    async fn setattr_owner(
        &self,
        handle: &FileHandle,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<()> {
        let inner = self.inner_for_handle(handle)?;
        inner.setattr_owner(handle, uid, gid).await
    }

    async fn create(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.create(dir_handle, name, mode).await
    }

    async fn remove(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.remove(dir_handle, name).await
    }

    async fn mkdir(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.mkdir(dir_handle, name, mode).await
    }

    async fn rmdir(&self, dir_handle: &FileHandle, name: &str) -> Result<()> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.rmdir(dir_handle, name).await
    }

    async fn rename(
        &self,
        from_dir_handle: &FileHandle,
        from_name: &str,
        to_dir_handle: &FileHandle,
        to_name: &str,
    ) -> Result<()> {
        let inner = self.inner_for_same_export_pair(
            from_dir_handle,
            to_dir_handle,
            "rename",
            "source",
            "target",
        )?;
        inner
            .rename(from_dir_handle, from_name, to_dir_handle, to_name)
            .await
    }

    async fn symlink(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        target: &str,
    ) -> Result<FileHandle> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.symlink(dir_handle, name, target).await
    }

    async fn readlink(&self, handle: &FileHandle) -> Result<String> {
        let inner = self.inner_for_handle(handle)?;
        inner.readlink(handle).await
    }

    async fn link(
        &self,
        file_handle: &FileHandle,
        dir_handle: &FileHandle,
        name: &str,
    ) -> Result<FileHandle> {
        let inner =
            self.inner_for_same_export_pair(file_handle, dir_handle, "link", "file", "dir")?;
        inner.link(file_handle, dir_handle, name).await
    }

    async fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<()> {
        let inner = self.inner_for_handle(handle)?;
        inner.commit(handle, offset, count).await
    }

    async fn mknod(
        &self,
        dir_handle: &FileHandle,
        name: &str,
        file_type: FileType,
        mode: u32,
        rdev: (u32, u32),
    ) -> Result<FileHandle> {
        let inner = self.inner_for_handle(dir_handle)?;
        inner.mknod(dir_handle, name, file_type, mode, rdev).await
    }

    async fn fs_stats(&self, handle: &FileHandle) -> Result<FsStats> {
        let inner = self.inner_for_handle(handle)?;
        inner.fs_stats(handle).await
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
        let snap = router.state.load();
        assert_eq!(snap.by_uid.len(), 2);
        assert_eq!(snap.by_name.get("/data"), Some(&1));
        assert_eq!(snap.by_name.get("/backup"), Some(&2));
        assert!(snap.retired_uids.is_empty());
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

    #[tokio::test]
    async fn fs_stats_routes_to_the_handles_export() {
        // Two exports rooted at different tempdirs; the router must call
        // fs_stats on the backend that owns the handle's uid prefix.
        // We can't directly check which backend was called, but each
        // tempdir lives on the same filesystem in CI, so we settle for
        // verifying both routes return populated stats with the same
        // shape — and that a third, bogus handle errors out.
        let (router, _data, _backup) = build_two_export_router();

        let data_root = router.root_handle_for("/data").unwrap();
        let backup_root = router.root_handle_for("/backup").unwrap();

        let s_data = router.fs_stats(&data_root).await.expect("data fs_stats");
        let s_backup = router
            .fs_stats(&backup_root)
            .await
            .expect("backup fs_stats");

        assert!(s_data.total_bytes > 0);
        assert!(s_backup.total_bytes > 0);
        assert!(s_data.block_size > 0);
        assert!(s_backup.block_size > 0);
    }

    #[tokio::test]
    async fn fs_stats_with_short_handle_returns_stale_error() {
        let (router, _data, _backup) = build_two_export_router();
        let short: FileHandle = vec![0u8; 3];
        let err = router.fs_stats(&short).await.expect_err("must fail");
        // Same "Invalid handle" substring contract every other handle-keyed
        // op satisfies — the fsstat/fsinfo/pathconf handlers map it to STALE.
        assert!(err.to_string().contains("Invalid handle"), "got: {err}");
    }

    #[tokio::test]
    async fn fs_stats_with_unknown_uid_returns_stale_error() {
        let (router, _data, _backup) = build_two_export_router();
        let mut bogus = vec![0u8; HANDLE_LEN];
        bogus[..4].copy_from_slice(&999u32.to_be_bytes());
        let err = router.fs_stats(&bogus).await.expect_err("must fail");
        assert!(err.to_string().contains("Invalid handle"), "got: {err}");
        assert!(err.to_string().contains("stale export uid"), "got: {err}");
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

    // ---------------------------------------------------------------
    // Phase 4: add_export / remove_export / update_export unit tests.
    // ---------------------------------------------------------------

    #[test]
    fn add_export_makes_it_visible_in_list_and_routes() {
        let (router, _data, _backup) = build_two_export_router();
        let new_dir = TempDir::new().unwrap();
        let cfg = export("/extra", 99, new_dir.path().to_path_buf(), false);

        router.add_export(&cfg).expect("add must succeed");

        let infos = router.list_exports();
        assert!(
            infos
                .iter()
                .any(|i| i.name == "/extra" && i.uid == 99 && !i.read_only),
            "list missing new export: {infos:?}"
        );

        // The new export must be reachable by name; its root handle must
        // carry the configured uid in the prefix.
        let h = router.root_handle_for("/extra").expect("name must resolve");
        assert_eq!(h.as_slice().export_uid(), Some(99));
    }

    #[test]
    fn add_export_rejects_existing_uid() {
        let (router, _data, _backup) = build_two_export_router();
        let dup_dir = TempDir::new().unwrap();
        // uid 1 is already taken by /data.
        let cfg = export("/other", 1, dup_dir.path().to_path_buf(), false);

        let err = router
            .add_export(&cfg)
            .expect_err("duplicate uid must fail")
            .to_string();
        assert!(err.contains("already in use"), "got: {err}");

        // Live snapshot is unchanged.
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn add_export_rejects_existing_name() {
        let (router, _data, _backup) = build_two_export_router();
        let dup_dir = TempDir::new().unwrap();
        // name /data is already taken by uid 1.
        let cfg = export("/data", 7, dup_dir.path().to_path_buf(), false);

        let err = router
            .add_export(&cfg)
            .expect_err("duplicate name must fail")
            .to_string();
        assert!(err.contains("already in use"), "got: {err}");
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn add_export_with_invalid_config_errors() {
        let (router, _data, _backup) = build_two_export_router();
        let new_dir = TempDir::new().unwrap();

        // uid 0 is reserved.
        let bad_uid = export("/zero", 0, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&bad_uid)
            .expect_err("uid 0 must fail")
            .to_string();
        assert!(err.contains("uid 0"), "got: {err}");

        // Name must start with '/'.
        let bad_name = export("nope", 11, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&bad_name)
            .expect_err("bad name must fail")
            .to_string();
        assert!(err.contains("must start with '/'"), "got: {err}");

        // Local backend path must be absolute.
        let bad_path = export("/rel", 12, PathBuf::from("relative/path"), false);
        let err = router
            .add_export(&bad_path)
            .expect_err("relative path must fail")
            .to_string();
        assert!(err.contains("must be absolute"), "got: {err}");

        // None of the failures should have touched the live snapshot.
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn remove_export_by_name_disappears_from_list() {
        let (router, _data, _backup) = build_two_export_router();
        let removed = router
            .remove_export(&ExportSelector::Name("/data".to_string()))
            .expect("remove must succeed");
        assert_eq!(removed, 1);

        let infos = router.list_exports();
        assert!(!infos.iter().any(|i| i.name == "/data"), "got: {infos:?}");
        assert_eq!(infos.len(), 1);
        assert!(router.root_handle_for("/data").is_none());
    }

    #[test]
    fn remove_export_by_uid_disappears_from_list() {
        let (router, _data, _backup) = build_two_export_router();
        let removed = router
            .remove_export(&ExportSelector::Uid(2))
            .expect("remove must succeed");
        assert_eq!(removed, 2);
        assert!(router.root_handle_for("/backup").is_none());
    }

    #[test]
    fn remove_export_moves_uid_to_retired_and_blocks_reuse() {
        let (router, _data, _backup) = build_two_export_router();
        router
            .remove_export(&ExportSelector::Uid(1))
            .expect("remove must succeed");

        // Re-adding uid 1, even with a fresh name and path, must fail —
        // in-flight client handles still encode uid 1 against the old
        // backend.
        let new_dir = TempDir::new().unwrap();
        let cfg = export("/new", 1, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&cfg)
            .expect_err("retired uid reuse must fail")
            .to_string();
        assert!(err.contains("retired"), "got: {err}");
    }

    #[test]
    fn remove_export_errors_when_not_found() {
        let (router, _data, _backup) = build_two_export_router();

        let err = router
            .remove_export(&ExportSelector::Name("/missing".to_string()))
            .expect_err("missing name must fail")
            .to_string();
        assert!(err.contains("No export named"), "got: {err}");

        let err = router
            .remove_export(&ExportSelector::Uid(404))
            .expect_err("missing uid must fail")
            .to_string();
        assert!(err.contains("No export with uid"), "got: {err}");

        // Failed lookups leave the live snapshot untouched.
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn update_export_flips_read_only_round_trip() {
        let (router, _data, _backup) = build_two_export_router();
        let data_root = router.root_handle_for("/data").unwrap();
        let backup_root = router.root_handle_for("/backup").unwrap();

        // Baseline: /data is rw, /backup is ro.
        assert!(!router.is_read_only(&data_root));
        assert!(router.is_read_only(&backup_root));

        // Flip /data to ro.
        router
            .update_export(&ExportSelector::Name("/data".to_string()), Some(true))
            .expect("update must succeed");
        assert!(router.is_read_only(&data_root));
        // The other export is untouched.
        assert!(router.is_read_only(&backup_root));

        // Flip /backup to rw via uid selector.
        router
            .update_export(&ExportSelector::Uid(2), Some(false))
            .expect("update must succeed");
        assert!(!router.is_read_only(&backup_root));

        // And list_exports reflects both mutations.
        let infos = router.list_exports();
        let data_info = infos.iter().find(|i| i.uid == 1).unwrap();
        let backup_info = infos.iter().find(|i| i.uid == 2).unwrap();
        assert!(data_info.read_only);
        assert!(!backup_info.read_only);
    }

    #[test]
    fn update_export_no_change_args_errors() {
        let (router, _data, _backup) = build_two_export_router();
        let err = router
            .update_export(&ExportSelector::Name("/data".to_string()), None)
            .expect_err("no-op update must fail")
            .to_string();
        assert!(err.contains("no fields to mutate"), "got: {err}");
    }

    #[test]
    fn update_export_errors_when_not_found() {
        let (router, _data, _backup) = build_two_export_router();
        let err = router
            .update_export(&ExportSelector::Name("/missing".to_string()), Some(true))
            .expect_err("missing export must fail")
            .to_string();
        assert!(err.contains("No export named"), "got: {err}");
    }

    /// A name retired by `remove_export` must be reusable by a fresh `add_export`
    /// as long as the new uid is also fresh. Only uids are retired for the
    /// lifetime of the daemon — names are not, because no in-flight file handle
    /// references the name (handles carry the uid prefix).
    #[test]
    fn remove_then_readd_same_name_with_fresh_uid_succeeds() {
        let (router, _data, _backup) = build_two_export_router();

        router
            .remove_export(&ExportSelector::Name("/data".to_string()))
            .expect("remove must succeed");

        // Re-add /data with a fresh uid — must succeed.
        let new_dir = TempDir::new().unwrap();
        let cfg = export("/data", 3, new_dir.path().to_path_buf(), false);
        router.add_export(&cfg).expect("readd must succeed");

        let infos = router.list_exports();
        let data_entries: Vec<_> = infos.iter().filter(|i| i.name == "/data").collect();
        assert_eq!(
            data_entries.len(),
            1,
            "exactly one /data entry expected, got: {infos:?}"
        );
        assert_eq!(data_entries[0].uid, 3);
        assert!(!data_entries[0].read_only);

        // And uid retirement is asymmetric: trying to recycle the original
        // uid 1 (now retired) must still fail, even under a brand-new name.
        let bogus_dir = TempDir::new().unwrap();
        let cfg_retired = export("/elsewhere", 1, bogus_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&cfg_retired)
            .expect_err("retired uid reuse must fail")
            .to_string();
        assert!(err.contains("retired"), "got: {err}");
    }

    // ---------------------------------------------------------------
    // Export-name normalization (validate_export_name) tests.
    // ---------------------------------------------------------------

    #[test]
    fn add_export_rejects_name_with_parent_dir_component() {
        let (router, _data, _backup) = build_two_export_router();
        let new_dir = TempDir::new().unwrap();
        let cfg = export("/foo/../bar", 50, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&cfg)
            .expect_err("'..' must be rejected")
            .to_string();
        assert!(err.contains("'..'"), "got: {err}");
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn add_export_rejects_name_with_double_slash() {
        let (router, _data, _backup) = build_two_export_router();
        let new_dir = TempDir::new().unwrap();
        let cfg = export("/foo//bar", 51, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&cfg)
            .expect_err("'//' must be rejected")
            .to_string();
        assert!(err.contains("'//'"), "got: {err}");
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn add_export_rejects_name_with_trailing_slash() {
        let (router, _data, _backup) = build_two_export_router();
        let new_dir = TempDir::new().unwrap();
        let cfg = export("/foo/", 52, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&cfg)
            .expect_err("trailing '/' must be rejected")
            .to_string();
        assert!(err.contains("must not end with '/'"), "got: {err}");
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn add_export_rejects_empty_name() {
        let (router, _data, _backup) = build_two_export_router();
        let new_dir = TempDir::new().unwrap();
        let cfg = export("", 53, new_dir.path().to_path_buf(), false);
        let err = router
            .add_export(&cfg)
            .expect_err("empty name must be rejected")
            .to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
        assert_eq!(router.list_exports().len(), 2);
    }

    #[test]
    fn validate_export_name_accepts_root_and_typical_names() {
        // Mirrors Config::validate() behavior: "/" is accepted because it
        // passes starts_with('/') and our trailing-slash rule special-cases it.
        validate_export_name("/").expect("'/' must be accepted");
        validate_export_name("/data").expect("'/data' must be accepted");
        validate_export_name("/srv/nfs/data").expect("nested path must be accepted");
    }
}
