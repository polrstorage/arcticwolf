// File Handle Management
//
// File handles are opaque identifiers used by NFS to reference files/directories.
// This module manages the bidirectional mapping between file handles and paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// File handle type (opaque bytes)
pub type FileHandle = Vec<u8>;

/// File handle manager
///
/// Maintains the mapping between file handles and filesystem paths.
/// Thread-safe for concurrent access.
#[derive(Clone)]
pub struct HandleManager {
    /// Map from file handle to path
    handle_to_path: Arc<RwLock<HashMap<FileHandle, PathBuf>>>,
    /// Map from path to file handle (for quick lookups)
    path_to_handle: Arc<RwLock<HashMap<PathBuf, FileHandle>>>,
    /// Counter for generating unique handles
    next_id: Arc<RwLock<u64>>,
}

impl HandleManager {
    /// Create a new handle manager
    pub fn new() -> Self {
        Self {
            handle_to_path: Arc::new(RwLock::new(HashMap::new())),
            path_to_handle: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(RwLock::new(1)), // Start from 1 (0 could be reserved)
        }
    }

    /// Generate a new file handle for a path
    ///
    /// If the path already has a handle, return the existing one.
    /// Otherwise, create a new handle.
    pub fn create_handle(&self, path: PathBuf) -> FileHandle {
        // Check if path already has a handle
        {
            let path_map = self.path_to_handle.read().unwrap();
            if let Some(handle) = path_map.get(&path) {
                return handle.clone();
            }
        }

        // Generate new handle
        let id = {
            let mut next_id = self.next_id.write().unwrap();
            let current = *next_id;
            *next_id += 1;
            current
        };

        // Create handle from ID (32 bytes with ID in first 8 bytes)
        let mut handle = vec![0u8; 32];
        handle[0..8].copy_from_slice(&id.to_be_bytes());

        // Store path hash in bytes 8-16 for verification
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let path_hash = hasher.finish();
        handle[8..16].copy_from_slice(&path_hash.to_be_bytes());

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
    pub fn is_valid(&self, handle: &FileHandle) -> bool {
        let handle_map = self.handle_to_path.read().unwrap();
        handle_map.contains_key(handle)
    }

    /// Remove a file handle (e.g., when file is deleted)
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
    pub fn count(&self) -> usize {
        let handle_map = self.handle_to_path.read().unwrap();
        handle_map.len()
    }
}

impl Default for HandleManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_lookup() {
        let manager = HandleManager::new();
        let path = PathBuf::from("/test/file.txt");

        let handle = manager.create_handle(path.clone());
        assert_eq!(manager.lookup_path(&handle), Some(path));
    }

    #[test]
    fn test_idempotent_create() {
        let manager = HandleManager::new();
        let path = PathBuf::from("/test/file.txt");

        let handle1 = manager.create_handle(path.clone());
        let handle2 = manager.create_handle(path.clone());

        assert_eq!(handle1, handle2);
    }

    #[test]
    fn test_remove_handle() {
        let manager = HandleManager::new();
        let path = PathBuf::from("/test/file.txt");

        let handle = manager.create_handle(path.clone());
        assert!(manager.is_valid(&handle));

        let removed_path = manager.remove_handle(&handle);
        assert_eq!(removed_path, Some(path));
        assert!(!manager.is_valid(&handle));
    }
}
