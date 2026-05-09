// File Handle Management
//
// File handles are opaque identifiers used by NFS to reference files/directories.
// This module manages the bidirectional mapping between file handles and paths.
//
// Handle layout (32 bytes):
//   bytes 0..4   : export uid (big-endian u32) — see issue #26
//   bytes 4..32  : random opaque id

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// File handle type (opaque bytes)
pub type FileHandle = Vec<u8>;

/// Total handle size in bytes.
pub const HANDLE_LEN: usize = 32;
/// Length of the export-uid prefix.
pub const EXPORT_UID_LEN: usize = 4;
/// Length of the random opaque tail.
#[allow(dead_code)]
pub const OPAQUE_LEN: usize = HANDLE_LEN - EXPORT_UID_LEN;

/// Extension trait for parsing the export-uid prefix from a [`FileHandle`].
///
/// Used in Phase 3 by the multi-export router to dispatch handles to the
/// owning backend; defined here so handle construction and inspection live
/// next to each other.
#[allow(dead_code)]
pub trait FileHandleExt {
    /// Decode the export uid stored in the first 4 bytes (big-endian).
    ///
    /// Returns `None` for slices shorter than [`EXPORT_UID_LEN`] so the
    /// Phase 3 router can feed untrusted wire bytes through this without
    /// panicking on malformed handles.
    fn export_uid(&self) -> Option<u32>;
}

impl FileHandleExt for [u8] {
    fn export_uid(&self) -> Option<u32> {
        if self.len() < EXPORT_UID_LEN {
            return None;
        }
        let bytes: [u8; EXPORT_UID_LEN] = self[..EXPORT_UID_LEN].try_into().ok()?;
        Some(u32::from_be_bytes(bytes))
    }
}

/// File handle manager
///
/// Maintains the mapping between file handles and filesystem paths.
/// Thread-safe for concurrent access. Each manager is bound to a single
/// export uid; every handle it produces carries that uid in its prefix
/// so the multi-export router (Phase 3) can dispatch by parsing the
/// first 4 bytes of an incoming handle.
#[derive(Clone)]
pub struct HandleManager {
    /// Export uid embedded in every handle produced by this manager.
    export_uid: u32,
    /// Map from file handle to path
    handle_to_path: Arc<RwLock<HashMap<FileHandle, PathBuf>>>,
    /// Map from path to file handle (for quick lookups)
    path_to_handle: Arc<RwLock<HashMap<PathBuf, FileHandle>>>,
}

impl HandleManager {
    /// Create a new handle manager bound to `export_uid`.
    pub fn new(export_uid: u32) -> Self {
        Self {
            export_uid,
            handle_to_path: Arc::new(RwLock::new(HashMap::new())),
            path_to_handle: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Generate a new file handle for a path
    ///
    /// If the path already has a handle, return the existing one.
    /// Otherwise, create a new handle whose first 4 bytes encode the
    /// manager's export uid and whose remaining 28 bytes are random.
    pub fn create_handle(&self, path: PathBuf) -> FileHandle {
        // Check if path already has a handle
        {
            let path_map = self.path_to_handle.read().unwrap();
            if let Some(handle) = path_map.get(&path) {
                return handle.clone();
            }
        }

        let mut handle = vec![0u8; HANDLE_LEN];
        handle[..EXPORT_UID_LEN].copy_from_slice(&self.export_uid.to_be_bytes());
        fill_random(&mut handle[EXPORT_UID_LEN..]);

        // Store mappings
        {
            let mut handle_map = self.handle_to_path.write().unwrap();
            let mut path_map = self.path_to_handle.write().unwrap();

            handle_map.insert(handle.clone(), path.clone());
            path_map.insert(path.clone(), handle.clone());
        }

        tracing::debug!("Created file handle for path: {:?}", path);
        handle
    }

    /// Look up the path for a file handle
    pub fn lookup_path(&self, handle: &FileHandle) -> Option<PathBuf> {
        let handle_map = self.handle_to_path.read().unwrap();
        handle_map.get(handle).cloned()
    }

    /// Check if a file handle exists
    #[allow(dead_code)]
    pub fn is_valid(&self, handle: &FileHandle) -> bool {
        let handle_map = self.handle_to_path.read().unwrap();
        handle_map.contains_key(handle)
    }

    /// Remove a file handle (e.g., when file is deleted)
    #[allow(dead_code)]
    pub fn remove_handle(&self, handle: &FileHandle) -> Option<PathBuf> {
        let mut handle_map = self.handle_to_path.write().unwrap();
        let mut path_map = self.path_to_handle.write().unwrap();

        if let Some(path) = handle_map.remove(handle) {
            path_map.remove(&path);
            tracing::debug!("Removed file handle for path: {:?}", path);
            Some(path)
        } else {
            None
        }
    }

    /// Get total number of handles
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        let handle_map = self.handle_to_path.read().unwrap();
        handle_map.len()
    }
}

/// Fill `dst` with cryptographically random bytes.
///
/// Backed by the `getrandom` crate, which on Linux is the `getrandom(2)`
/// syscall — no per-call file descriptor is allocated, so the NFS hot path
/// does not pay an `open("/dev/urandom")` for every handle creation.
fn fill_random(dst: &mut [u8]) {
    getrandom::fill(dst).expect("getrandom failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_lookup() {
        let manager = HandleManager::new(1);
        let path = PathBuf::from("/test/file.txt");

        let handle = manager.create_handle(path.clone());
        assert_eq!(manager.lookup_path(&handle), Some(path));
    }

    #[test]
    fn test_idempotent_create() {
        let manager = HandleManager::new(1);
        let path = PathBuf::from("/test/file.txt");

        let handle1 = manager.create_handle(path.clone());
        let handle2 = manager.create_handle(path.clone());

        assert_eq!(handle1, handle2);
    }

    #[test]
    fn test_remove_handle() {
        let manager = HandleManager::new(1);
        let path = PathBuf::from("/test/file.txt");

        let handle = manager.create_handle(path.clone());
        assert!(manager.is_valid(&handle));

        let removed_path = manager.remove_handle(&handle);
        assert_eq!(removed_path, Some(path));
        assert!(!manager.is_valid(&handle));
    }

    #[test]
    fn test_handle_layout_round_trip() {
        let manager = HandleManager::new(5);
        let handle = manager.create_handle(PathBuf::from("/x"));
        assert_eq!(handle.len(), HANDLE_LEN);
        assert_eq!(handle.export_uid().unwrap(), 5);
    }

    #[test]
    fn test_distinct_uids_distinct_prefix() {
        let m1 = HandleManager::new(1);
        let m2 = HandleManager::new(2);
        let h1 = m1.create_handle(PathBuf::from("/a"));
        let h2 = m2.create_handle(PathBuf::from("/a"));
        assert_ne!(&h1[..EXPORT_UID_LEN], &h2[..EXPORT_UID_LEN]);
        assert_eq!(h1.export_uid().unwrap(), 1);
        assert_eq!(h2.export_uid().unwrap(), 2);
    }

    #[test]
    fn test_same_uid_shared_prefix_distinct_tail() {
        // Two paths within one export get the same uid prefix but different
        // (random) tails — collision probability is 2^-224.
        let manager = HandleManager::new(7);
        let h1 = manager.create_handle(PathBuf::from("/a"));
        let h2 = manager.create_handle(PathBuf::from("/b"));
        assert_eq!(&h1[..EXPORT_UID_LEN], &h2[..EXPORT_UID_LEN]);
        assert_ne!(&h1[EXPORT_UID_LEN..], &h2[EXPORT_UID_LEN..]);
    }

    #[test]
    fn test_export_uid_extension_on_raw_bytes() {
        let mut bytes = [0u8; HANDLE_LEN];
        bytes[..EXPORT_UID_LEN].copy_from_slice(&42u32.to_be_bytes());
        assert_eq!(bytes.export_uid().unwrap(), 42);
    }

    #[test]
    fn test_export_uid_short_slice_returns_none() {
        let short: &[u8] = &[0u8; 3];
        assert!(short.export_uid().is_none());
        let empty: &[u8] = &[];
        assert!(empty.export_uid().is_none());
    }
}
