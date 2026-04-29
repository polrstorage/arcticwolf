// Folder Quota Manager
//
// Provides per-subdirectory byte quotas persisted to a redb database, with
// an in-memory cache for fast quota checks on the hot path.
//
// Quotas are keyed by the first-level subdirectory name under the export
// root (e.g. a PVC folder). Operations that cross into a quota directory
// consult this manager before and after the underlying filesystem call.

use anyhow::{Context, Result, anyhow};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

/// redb table: first-level directory name -> (limit_bytes, usage_bytes)
const QUOTA_TABLE: TableDefinition<&str, (u64, u64)> = TableDefinition::new("quotas");

/// In-memory view of a single quota entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaEntry {
    pub limit: u64,
    pub usage: u64,
}

/// Manages folder quotas with redb persistence and an in-memory cache.
///
/// Reads go to the cache for low latency. Writes update the cache first,
/// then synchronously persist to redb via `spawn_blocking` so the tokio
/// runtime is not blocked on disk I/O.
pub struct QuotaManager {
    /// Canonical path to the export root.
    root_path: PathBuf,
    /// In-memory cache keyed by first-level subdirectory name.
    entries: Arc<RwLock<HashMap<String, QuotaEntry>>>,
    /// Persistent redb handle.
    db: Arc<Database>,
}

impl QuotaManager {
    /// Open or create the redb database at `db_path` and load all quota
    /// entries into the in-memory cache.
    ///
    /// Creates the parent directory of `db_path` if it does not exist.
    pub fn new(db_path: impl AsRef<Path>, root_path: PathBuf) -> Result<Self> {
        let db_path = db_path.as_ref();

        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .context(format!("Failed to create quota DB directory: {:?}", parent))?;
        }

        let db =
            Database::create(db_path).context(format!("Failed to open quota DB: {:?}", db_path))?;

        // Ensure the table exists.
        {
            let txn = db.begin_write().context("Failed to begin write txn")?;
            {
                let _ = txn
                    .open_table(QUOTA_TABLE)
                    .context("Failed to open quota table")?;
            }
            txn.commit().context("Failed to commit table init")?;
        }

        // Populate the in-memory cache from existing entries. Stored keys
        // are also re-validated here as a defence in depth: a bad key
        // (manual edit, file corruption, older buggy version) must not
        // sneak into the cache, because reconciliation would later join
        // it onto `root_path` and walk a path outside the export tree.
        let mut entries: HashMap<String, QuotaEntry> = HashMap::new();
        let mut skipped: u64 = 0;
        {
            let txn = db.begin_read().context("Failed to begin read txn")?;
            let table = txn
                .open_table(QUOTA_TABLE)
                .context("Failed to open quota table")?;
            for item in table.iter().context("Failed to iterate quota table")? {
                let (key, value) = item.context("Failed to read quota entry")?;
                let key_str = key.value().to_string();
                if let Err(e) = validate_quota_dir(&key_str) {
                    tracing::warn!("Quota: skipping unsafe stored key {:?}: {}", key_str, e);
                    skipped += 1;
                    continue;
                }
                let (limit, usage) = value.value();
                entries.insert(key_str, QuotaEntry { limit, usage });
            }
        }

        debug!(
            "Quota: loaded {} entries from {:?} (root={:?}, skipped={})",
            entries.len(),
            db_path,
            root_path,
            skipped
        );

        Ok(Self {
            root_path,
            entries: Arc::new(RwLock::new(entries)),
            db: Arc::new(db),
        })
    }

    /// Given an absolute path inside the export, return the first-level
    /// subdirectory name. Returns `None` if the path is the root itself
    /// or does not live under the export root.
    pub fn resolve_quota_dir(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root_path).ok()?;
        let first = relative.components().next()?;
        Some(first.as_os_str().to_string_lossy().into_owned())
    }

    /// Check whether adding `additional_bytes` to `quota_dir` would exceed
    /// its configured limit. Returns `Ok(())` when the directory has no
    /// quota (not tracked).
    ///
    /// Concurrency note: this is an **advisory** pre-check. The read lock
    /// is dropped before the caller actually adds usage, so two writers
    /// can both pass `check_quota` against the same usage snapshot and
    /// then both call [`add_usage`], briefly exceeding the limit by up
    /// to `(N - 1) * max_write_size` for `N` concurrent writers. For the
    /// PVC use case this is acceptable — a single client typically owns
    /// each PVC and writes are bounded to ~1 MiB chunks; reconciliation
    /// (or a subsequent over-limit write) repairs the drift. Strict
    /// atomicity would require a check-and-reserve API that holds the
    /// write lock across both steps, at the cost of more contention.
    pub async fn check_quota(&self, quota_dir: &str, additional_bytes: u64) -> Result<()> {
        let entries = self.entries.read().await;
        if let Some(entry) = entries.get(quota_dir) {
            let projected = entry.usage.saturating_add(additional_bytes);
            if projected > entry.limit {
                return Err(anyhow!(
                    "Quota exceeded for '{}': {} + {} > {}",
                    quota_dir,
                    entry.usage,
                    additional_bytes,
                    entry.limit
                ));
            }
        }
        Ok(())
    }

    /// Increase the tracked usage of `quota_dir` by `bytes` and persist
    /// the new value. No-op for directories without a quota.
    ///
    /// The write lock is held across the redb commit so the in-memory
    /// cache is never observable in a state that has not also been
    /// persisted; if persistence fails the cache is rolled back before
    /// the lock is released.
    pub async fn add_usage(&self, quota_dir: &str, bytes: u64) -> Result<()> {
        let mut entries = self.entries.write().await;

        let (limit, old_usage, new_usage) = match entries.get_mut(quota_dir) {
            Some(entry) => {
                let old = entry.usage;
                let new = old.saturating_add(bytes);
                entry.usage = new;
                (entry.limit, old, new)
            }
            None => return Ok(()),
        };

        if let Err(e) = self
            .persist_entry(quota_dir.to_string(), limit, new_usage)
            .await
        {
            if let Some(entry) = entries.get_mut(quota_dir) {
                entry.usage = old_usage;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Decrease the tracked usage of `quota_dir` by `bytes` (saturating)
    /// and persist. No-op for directories without a quota.
    ///
    /// The write lock is held across the redb commit; see [`add_usage`]
    /// for the consistency rationale.
    pub async fn sub_usage(&self, quota_dir: &str, bytes: u64) -> Result<()> {
        let mut entries = self.entries.write().await;

        let (limit, old_usage, new_usage) = match entries.get_mut(quota_dir) {
            Some(entry) => {
                let old = entry.usage;
                let new = old.saturating_sub(bytes);
                entry.usage = new;
                (entry.limit, old, new)
            }
            None => return Ok(()),
        };

        if let Err(e) = self
            .persist_entry(quota_dir.to_string(), limit, new_usage)
            .await
        {
            if let Some(entry) = entries.get_mut(quota_dir) {
                entry.usage = old_usage;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Set or update the quota limit for a directory. Preserves existing
    /// usage if the entry already exists; initializes to zero otherwise.
    ///
    /// On persistence failure the cache is restored to its previous
    /// state (either the old limit, or removal of a freshly inserted
    /// entry).
    pub async fn set_quota(&self, quota_dir: &str, limit: u64) -> Result<()> {
        validate_quota_dir(quota_dir)?;

        let mut entries = self.entries.write().await;

        let previous_limit = entries.get(quota_dir).map(|e| e.limit);
        let entry = entries
            .entry(quota_dir.to_string())
            .or_insert(QuotaEntry { limit: 0, usage: 0 });
        entry.limit = limit;
        let usage = entry.usage;

        if let Err(e) = self
            .persist_entry(quota_dir.to_string(), limit, usage)
            .await
        {
            match previous_limit {
                Some(old) => {
                    if let Some(entry) = entries.get_mut(quota_dir) {
                        entry.limit = old;
                    }
                }
                None => {
                    entries.remove(quota_dir);
                }
            }
            return Err(e);
        }
        Ok(())
    }

    /// Remove a quota entry entirely.
    ///
    /// The cache entry is reinstated if the redb removal fails so the
    /// in-memory and on-disk views stay in sync.
    ///
    /// Currently only exercised by unit tests — no runtime code path
    /// removes a quota — but the API is kept ready for an admin tool
    /// that needs to retire a PVC entry without restarting the server.
    #[allow(dead_code)]
    pub async fn remove_quota(&self, quota_dir: &str) -> Result<()> {
        let mut entries = self.entries.write().await;

        let removed = entries.remove(quota_dir);

        let db = self.db.clone();
        let key = quota_dir.to_string();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().context("Failed to begin write txn")?;
            {
                let mut table = txn
                    .open_table(QUOTA_TABLE)
                    .context("Failed to open quota table")?;
                table
                    .remove(key.as_str())
                    .context("Failed to remove quota entry")?;
            }
            txn.commit().context("Failed to commit quota removal")?;
            Ok(())
        })
        .await
        .context("Failed to run DB task")?;

        if let Err(e) = result {
            if let Some(entry) = removed {
                entries.insert(quota_dir.to_string(), entry);
            }
            return Err(e);
        }

        Ok(())
    }

    /// Return `(limit, usage)` for the directory, or `None` if not tracked.
    pub async fn get_quota_info(&self, quota_dir: &str) -> Option<(u64, u64)> {
        let entries = self.entries.read().await;
        entries.get(quota_dir).map(|e| (e.limit, e.usage))
    }

    /// Return the list of tracked quota directory names.
    pub async fn tracked_dirs(&self) -> Vec<String> {
        let entries = self.entries.read().await;
        entries.keys().cloned().collect()
    }

    /// Apply a declarative bootstrap map at startup.
    ///
    /// For each `(dir, size_str)` entry, installs a quota limit on `dir`
    /// only when that directory does not already have a quota recorded.
    /// This makes the bootstrap idempotent across restarts: changing the
    /// config string after the first boot will not silently overwrite the
    /// usage counter that is already being tracked.
    pub async fn apply_bootstrap(&self, bootstrap: &HashMap<String, String>) -> Result<()> {
        for (dir, size_str) in bootstrap {
            // Validate up front so a bad config is reported as a startup
            // error rather than silently creating an orphan redb entry
            // that reconciliation might later try to scan with a path
            // like `<root>/../escape`.
            validate_quota_dir(dir)
                .with_context(|| format!("Invalid bootstrap quota directory '{}'", dir))?;

            if self.get_quota_info(dir).await.is_some() {
                tracing::debug!("Quota bootstrap '{}': already present, skipped", dir);
                continue;
            }
            let limit = crate::config::parse_size(size_str)
                .with_context(|| format!("Invalid bootstrap size for '{}'", dir))?;
            self.set_quota(dir, limit).await?;
            tracing::info!("Quota bootstrap: installed '{}' = {} bytes", dir, limit);
        }
        Ok(())
    }

    /// Walk the named quota directory on disk, recompute its true byte
    /// footprint, and reconcile the tracked usage with it. Non-existent
    /// directories reconcile to zero usage.
    ///
    /// Returns `Some((before, after))` when the entry existed (useful for
    /// logging drift) or `None` if the directory has no quota configured.
    pub async fn scan_and_reconcile(&self, quota_dir: &str) -> Result<Option<(u64, u64)>> {
        // Snapshot the entry under a short-lived read lock so we can return
        // early when there is nothing to reconcile.
        if self.entries.read().await.get(quota_dir).is_none() {
            return Ok(None);
        }

        // Walk the directory without holding any quota lock — the scan is
        // I/O-bound and we don't want to stall live writes.
        let dir_path = self.root_path.join(quota_dir);
        let scanned: u64 = tokio::task::spawn_blocking(move || allocated_path_size(&dir_path))
            .await
            .context("spawn_blocking failed for scan")??;

        // Take the write lock and hold it across the redb commit. This
        // prevents two races at once:
        //   * an interleaving add_usage/sub_usage from being clobbered by
        //     this reconciliation (lost-update);
        //   * the cache showing the new scanned value while persistence
        //     has actually failed (cache/DB drift).
        let mut entries = self.entries.write().await;

        let (old_usage, new_usage, limit) = match entries.get_mut(quota_dir) {
            Some(entry) => {
                let old = entry.usage;
                entry.usage = scanned;
                (old, entry.usage, entry.limit)
            }
            // The entry was removed while we were scanning — nothing to do.
            None => return Ok(None),
        };

        if let Err(e) = self
            .persist_entry(quota_dir.to_string(), limit, new_usage)
            .await
        {
            if let Some(entry) = entries.get_mut(quota_dir) {
                entry.usage = old_usage;
            }
            return Err(e);
        }

        Ok(Some((old_usage, new_usage)))
    }

    /// Reconcile every tracked quota directory. Errors on individual
    /// directories are logged and skipped so one missing PVC does not
    /// prevent others from being scanned.
    pub async fn reconcile_all(&self) {
        let dirs = self.tracked_dirs().await;
        for dir in dirs {
            match self.scan_and_reconcile(&dir).await {
                Ok(Some((before, after))) if before != after => {
                    tracing::info!(
                        "Quota reconcile '{}': usage {} -> {} ({:+})",
                        dir,
                        before,
                        after,
                        after as i128 - before as i128
                    );
                }
                Ok(_) => {
                    tracing::debug!("Quota reconcile '{}': unchanged", dir);
                }
                Err(e) => {
                    tracing::warn!("Quota reconcile '{}' failed: {}", dir, e);
                }
            }
        }
    }

    /// Persist a single entry to redb in a blocking task.
    async fn persist_entry(&self, key: String, limit: u64, usage: u64) -> Result<()> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().context("Failed to begin write txn")?;
            {
                let mut table = txn
                    .open_table(QUOTA_TABLE)
                    .context("Failed to open quota table")?;
                table
                    .insert(key.as_str(), &(limit, usage))
                    .context("Failed to insert quota entry")?;
            }
            txn.commit().context("Failed to commit quota update")?;
            Ok(())
        })
        .await
        .context("Failed to run DB task")??;
        Ok(())
    }
}

/// Reject quota directory keys that are not a single safe path component.
///
/// `resolve_quota_dir()` only ever returns the first component of a path
/// relative to the export root, so a stored key with a path separator,
/// `..`, or an empty value can never match any live filesystem path —
/// it would just sit in redb as a phantom entry. Worse, reconciliation
/// joins the key onto the export root and walks it, which would let a
/// malformed config like `"../escape"` direct the scanner outside the
/// export tree. Validate at the ingress points (bootstrap, set_quota)
/// and again on load (defence in depth) to fail fast or skip cleanly.
fn validate_quota_dir(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("Quota directory name must not be empty"));
    }
    if name == "." || name == ".." {
        return Err(anyhow!(
            "Quota directory name must not be '.' or '..': got '{}'",
            name
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(anyhow!(
            "Quota directory name must be a single path component (no '/' or '\\\\'): got '{}'",
            name
        ));
    }
    if name.contains('\0') {
        return Err(anyhow!(
            "Quota directory name must not contain NUL bytes: got '{}'",
            name.escape_default()
        ));
    }
    Ok(())
}

/// Recursively sum the on-disk byte footprint (st_blocks * 512) of all
/// regular files rooted at `path`. The unit matches what `write()` and
/// `remove()` charge against the quota, so callers — reconciliation
/// (scanning a quota directory) and cross-quota rename (computing the
/// usage to transfer) — get consistent numbers.
///
/// Behaviour:
///   * Missing path → returns `0` (e.g. a PVC directory was deleted
///     out of band before reconciliation runs).
///   * Plain file → returns its allocated bytes.
///   * Directory → recursive walk. `symlink_metadata` is used per entry
///     so a malicious symlink cannot escape the subtree or trap the
///     walk in a cycle.
///   * Other types (symlink, fifo, …) → not followed, contribute 0.
pub(crate) fn allocated_path_size(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;

    if !path.exists() {
        return Ok(0);
    }

    let meta =
        std::fs::symlink_metadata(path).context(format!("Failed to stat path: {:?}", path))?;
    if meta.is_file() {
        return Ok(meta.blocks().saturating_mul(512));
    }
    if !meta.is_dir() {
        return Ok(0);
    }

    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let rd = std::fs::read_dir(&current).context(format!(
            "Failed to read dir while summing size: {:?}",
            current
        ))?;
        for entry in rd {
            let entry = entry?;
            let entry_path = entry.path();
            let m = std::fs::symlink_metadata(&entry_path).context(format!(
                "Failed to stat path while summing size: {:?}",
                entry_path
            ))?;
            if m.is_dir() {
                stack.push(entry_path);
            } else if m.is_file() {
                total = total.saturating_add(m.blocks().saturating_mul(512));
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_manager(db_dir: &TempDir, root: &Path) -> QuotaManager {
        let db_path = db_dir.path().join("quota.db");
        QuotaManager::new(db_path, root.to_path_buf()).expect("create quota manager")
    }

    #[tokio::test]
    async fn test_new_empty_db() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        assert!(qm.get_quota_info("anything").await.is_none());
    }

    #[tokio::test]
    async fn test_set_and_get_quota() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 10 * 1024 * 1024).await.unwrap();

        let info = qm.get_quota_info("pvc-a").await;
        assert_eq!(info, Some((10 * 1024 * 1024, 0)));
    }

    #[tokio::test]
    async fn test_add_and_sub_usage() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 1000).await.unwrap();
        qm.add_usage("pvc-a", 400).await.unwrap();
        qm.add_usage("pvc-a", 200).await.unwrap();
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((1000, 600)));

        qm.sub_usage("pvc-a", 100).await.unwrap();
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((1000, 500)));
    }

    #[tokio::test]
    async fn test_sub_usage_saturating() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 1000).await.unwrap();
        qm.add_usage("pvc-a", 100).await.unwrap();
        qm.sub_usage("pvc-a", 500).await.unwrap();
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((1000, 0)));
    }

    #[tokio::test]
    async fn test_check_quota_under_limit() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 1000).await.unwrap();
        qm.add_usage("pvc-a", 400).await.unwrap();

        qm.check_quota("pvc-a", 500).await.unwrap(); // 400 + 500 = 900 <= 1000
        qm.check_quota("pvc-a", 601).await.unwrap_err(); // 400 + 601 > 1000
    }

    #[tokio::test]
    async fn test_check_quota_at_boundary() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 1000).await.unwrap();
        qm.add_usage("pvc-a", 600).await.unwrap();

        qm.check_quota("pvc-a", 400).await.unwrap(); // exactly at limit
        qm.check_quota("pvc-a", 401).await.unwrap_err();
    }

    #[tokio::test]
    async fn test_check_quota_untracked_dir_allows() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        // No quota configured, any write is allowed.
        qm.check_quota("untracked", u64::MAX).await.unwrap();
    }

    #[tokio::test]
    async fn test_check_quota_error_message_contains_quota_exceeded() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 100).await.unwrap();
        let err = qm.check_quota("pvc-a", 1000).await.unwrap_err();
        // NFS handlers rely on this substring to map to NFS3ERR_DQUOT.
        assert!(err.to_string().contains("Quota exceeded"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_remove_quota() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 1000).await.unwrap();
        qm.add_usage("pvc-a", 500).await.unwrap();
        assert!(qm.get_quota_info("pvc-a").await.is_some());

        qm.remove_quota("pvc-a").await.unwrap();
        assert!(qm.get_quota_info("pvc-a").await.is_none());
    }

    #[tokio::test]
    async fn test_update_preserves_usage() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.set_quota("pvc-a", 1000).await.unwrap();
        qm.add_usage("pvc-a", 500).await.unwrap();
        // Raise the limit; usage should stay.
        qm.set_quota("pvc-a", 2000).await.unwrap();
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((2000, 500)));
    }

    #[tokio::test]
    async fn test_persistence_across_reopen() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let db_path = tmp.path().join("quota.db");

        {
            let qm = QuotaManager::new(&db_path, root.path().to_path_buf()).unwrap();
            qm.set_quota("pvc-a", 1000).await.unwrap();
            qm.add_usage("pvc-a", 750).await.unwrap();
            qm.set_quota("pvc-b", 2000).await.unwrap();
        }

        // Reopen: entries should be loaded from disk.
        let qm = QuotaManager::new(&db_path, root.path().to_path_buf()).unwrap();
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((1000, 750)));
        assert_eq!(qm.get_quota_info("pvc-b").await, Some((2000, 0)));
    }

    #[tokio::test]
    async fn test_resolve_quota_dir_first_level() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let qm = QuotaManager::new(tmp.path().join("quota.db"), root_path.clone()).unwrap();

        assert_eq!(
            qm.resolve_quota_dir(&root_path.join("pvc-a")),
            Some("pvc-a".to_string())
        );
        assert_eq!(
            qm.resolve_quota_dir(&root_path.join("pvc-a/file.txt")),
            Some("pvc-a".to_string())
        );
        assert_eq!(
            qm.resolve_quota_dir(&root_path.join("pvc-a/sub/deep/file")),
            Some("pvc-a".to_string())
        );
    }

    #[tokio::test]
    async fn test_resolve_quota_dir_root_itself_returns_none() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let qm = QuotaManager::new(tmp.path().join("quota.db"), root_path.clone()).unwrap();

        assert_eq!(qm.resolve_quota_dir(&root_path), None);
    }

    #[tokio::test]
    async fn test_resolve_quota_dir_outside_root() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let qm = QuotaManager::new(tmp.path().join("quota.db"), root_path).unwrap();

        assert_eq!(qm.resolve_quota_dir(Path::new("/nowhere/else")), None);
    }

    #[tokio::test]
    async fn test_add_usage_on_untracked_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        qm.add_usage("untracked", 500).await.unwrap();
        assert!(qm.get_quota_info("untracked").await.is_none());
    }

    #[test]
    fn test_validate_quota_dir() {
        assert!(validate_quota_dir("pvc-a").is_ok());
        assert!(validate_quota_dir("with spaces").is_ok());

        assert!(validate_quota_dir("").is_err());
        assert!(validate_quota_dir(".").is_err());
        assert!(validate_quota_dir("..").is_err());
        assert!(validate_quota_dir("a/b").is_err());
        assert!(validate_quota_dir("../escape").is_err());
        assert!(validate_quota_dir("/abs").is_err());
        assert!(validate_quota_dir("a\\b").is_err());
        assert!(validate_quota_dir("with\0nul").is_err());
    }

    #[tokio::test]
    async fn test_new_skips_unsafe_stored_keys() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let db_path = tmp.path().join("quota.db");

        // Plant a mix of safe and unsafe entries directly in redb,
        // simulating a corrupted DB or a manual edit.
        {
            let db = Database::create(&db_path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table(QUOTA_TABLE).unwrap();
                table.insert("pvc-good", &(1024u64, 0u64)).unwrap();
                table.insert("../escape", &(2048u64, 0u64)).unwrap();
                table.insert("a/b", &(4096u64, 0u64)).unwrap();
                table.insert("", &(8192u64, 0u64)).unwrap();
            }
            txn.commit().unwrap();
        }

        let qm = QuotaManager::new(&db_path, root.path().to_path_buf()).unwrap();

        // Only the safe key survives the load.
        assert_eq!(qm.get_quota_info("pvc-good").await, Some((1024, 0)));
        assert!(qm.get_quota_info("../escape").await.is_none());
        assert!(qm.get_quota_info("a/b").await.is_none());
        assert!(qm.get_quota_info("").await.is_none());
    }

    #[tokio::test]
    async fn test_scan_and_reconcile_corrects_drift() {
        // Reconciliation accounts in allocated bytes (st_blocks * 512) so
        // it agrees with what write()/remove() charge against the quota.
        // 4 KiB writes line up with the typical filesystem page size,
        // making the expected value exact across tmpfs/ext4.
        const BLOCK: u64 = 4096;
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let qm = QuotaManager::new(tmp.path().join("quota.db"), root_path.clone()).unwrap();

        // Configure quota; tracked usage starts wrong (stale from a crash).
        qm.set_quota("pvc-a", 1024 * 1024).await.unwrap();
        qm.add_usage("pvc-a", 9999).await.unwrap();

        // Create real files under the PVC directory totaling 3 blocks.
        let pvc_dir = root_path.join("pvc-a");
        std::fs::create_dir_all(&pvc_dir).unwrap();
        std::fs::write(pvc_dir.join("a.bin"), vec![0u8; BLOCK as usize]).unwrap();
        std::fs::write(pvc_dir.join("b.bin"), vec![0u8; (2 * BLOCK) as usize]).unwrap();

        let result = qm.scan_and_reconcile("pvc-a").await.unwrap();
        assert_eq!(result, Some((9999, 3 * BLOCK)));
        assert_eq!(
            qm.get_quota_info("pvc-a").await,
            Some((1024 * 1024, 3 * BLOCK))
        );
    }

    #[tokio::test]
    async fn test_scan_and_reconcile_returns_none_for_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = QuotaManager::new(
            tmp.path().join("quota.db"),
            root.path().canonicalize().unwrap(),
        )
        .unwrap();

        let result = qm.scan_and_reconcile("nonexistent").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_scan_and_reconcile_missing_directory_is_zero() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = QuotaManager::new(
            tmp.path().join("quota.db"),
            root.path().canonicalize().unwrap(),
        )
        .unwrap();

        qm.set_quota("pvc-missing", 1000).await.unwrap();
        qm.add_usage("pvc-missing", 500).await.unwrap();

        let result = qm.scan_and_reconcile("pvc-missing").await.unwrap();
        assert_eq!(result, Some((500, 0)));
        assert_eq!(qm.get_quota_info("pvc-missing").await, Some((1000, 0)));
    }

    #[tokio::test]
    async fn test_reconcile_all_processes_every_tracked_dir() {
        const BLOCK: u64 = 4096;
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let qm = QuotaManager::new(tmp.path().join("quota.db"), root_path.clone()).unwrap();

        qm.set_quota("pvc-a", 16 * BLOCK).await.unwrap();
        qm.set_quota("pvc-b", 16 * BLOCK).await.unwrap();
        qm.add_usage("pvc-a", 9999).await.unwrap();
        qm.add_usage("pvc-b", 9999).await.unwrap();

        std::fs::create_dir_all(root_path.join("pvc-a")).unwrap();
        std::fs::write(root_path.join("pvc-a/f.bin"), vec![0u8; BLOCK as usize]).unwrap();
        std::fs::create_dir_all(root_path.join("pvc-b")).unwrap();
        std::fs::write(
            root_path.join("pvc-b/f.bin"),
            vec![0u8; (2 * BLOCK) as usize],
        )
        .unwrap();

        qm.reconcile_all().await;

        assert_eq!(qm.get_quota_info("pvc-a").await, Some((16 * BLOCK, BLOCK)));
        assert_eq!(
            qm.get_quota_info("pvc-b").await,
            Some((16 * BLOCK, 2 * BLOCK))
        );
    }

    #[tokio::test]
    async fn test_apply_bootstrap_installs_new_entries() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        let mut bootstrap = HashMap::new();
        bootstrap.insert("pvc-a".to_string(), "1MB".to_string());
        bootstrap.insert("pvc-b".to_string(), "500KB".to_string());

        qm.apply_bootstrap(&bootstrap).await.unwrap();

        assert_eq!(qm.get_quota_info("pvc-a").await, Some((1024 * 1024, 0)));
        assert_eq!(qm.get_quota_info("pvc-b").await, Some((500 * 1024, 0)));
    }

    #[tokio::test]
    async fn test_apply_bootstrap_skips_existing_entries() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        // Pre-existing entry with different limit and some usage.
        qm.set_quota("pvc-a", 2048).await.unwrap();
        qm.add_usage("pvc-a", 512).await.unwrap();

        let mut bootstrap = HashMap::new();
        bootstrap.insert("pvc-a".to_string(), "1MB".to_string());
        qm.apply_bootstrap(&bootstrap).await.unwrap();

        // Limit and usage must be preserved (bootstrap is no-op).
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((2048, 512)));
    }

    #[tokio::test]
    async fn test_apply_bootstrap_rejects_invalid_size() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        let mut bootstrap = HashMap::new();
        bootstrap.insert("pvc-a".to_string(), "garbage".to_string());

        let err = qm.apply_bootstrap(&bootstrap).await.unwrap_err();
        assert!(
            err.to_string().contains("Invalid bootstrap size"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_apply_bootstrap_rejects_unsafe_keys() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        for bad in ["", ".", "..", "a/b", "../escape", "/abs", "a\\b"] {
            let mut bootstrap = HashMap::new();
            bootstrap.insert(bad.to_string(), "1MB".to_string());
            let err = qm
                .apply_bootstrap(&bootstrap)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("Invalid bootstrap quota directory"),
                "key '{}' should be rejected, got: {}",
                bad,
                err
            );
        }
    }

    #[tokio::test]
    async fn test_set_quota_rejects_unsafe_key() {
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let qm = make_manager(&tmp, root.path());

        let err = qm.set_quota("../escape", 1024).await.unwrap_err();
        assert!(
            err.to_string().contains("must be a single path component"),
            "got: {}",
            err
        );
        assert!(qm.get_quota_info("../escape").await.is_none());
    }

    #[tokio::test]
    async fn test_reconciled_values_survive_reopen() {
        const BLOCK: u64 = 4096;
        let tmp = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let db_path = tmp.path().join("quota.db");

        {
            let qm = QuotaManager::new(&db_path, root_path.clone()).unwrap();
            qm.set_quota("pvc-a", 8 * BLOCK).await.unwrap();
            qm.add_usage("pvc-a", 4_000).await.unwrap();

            std::fs::create_dir_all(root_path.join("pvc-a")).unwrap();
            std::fs::write(root_path.join("pvc-a/f.bin"), vec![0u8; BLOCK as usize]).unwrap();

            qm.scan_and_reconcile("pvc-a").await.unwrap();
        }

        let qm = QuotaManager::new(&db_path, root_path).unwrap();
        assert_eq!(qm.get_quota_info("pvc-a").await, Some((8 * BLOCK, BLOCK)));
    }
}
