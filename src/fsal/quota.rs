// Folder Quota Manager
//
// Provides per-subdirectory byte quotas persisted to a redb database, with
// an in-memory cache for fast quota checks on the hot path.
//
// Quotas are keyed by the first-level subdirectory name under the export
// root (e.g. a PVC folder). Operations that cross into a quota directory
// consult this manager before and after the underlying filesystem call.

// API is exercised by unit tests; wiring into LocalFilesystem lands in a
// later stage of the folder-quota rollout.
#![allow(dead_code)]

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
}
