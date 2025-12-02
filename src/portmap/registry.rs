// Portmapper Service Registry
//
// Maintains the mapping of (program, version, protocol) -> port

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::protocol::v3::portmap::mapping;

/// Key for service lookups: (program, version, protocol)
type ServiceKey = (u32, u32, u32);

/// Portmapper service registry
///
/// This maintains the mapping between RPC services and their ports.
/// Services register themselves with SET and clients query with GETPORT.
#[derive(Clone)]
pub struct Registry {
    /// Map from (prog, vers, prot) to port
    mappings: Arc<RwLock<HashMap<ServiceKey, u32>>>,
}

impl Registry {
    /// Create a new registry
    pub fn new() -> Self {
        Self {
            mappings: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a service (PMAPPROC_SET)
    ///
    /// Returns true if successful
    pub fn set(&self, map: &mapping) -> bool {
        let key = (map.prog, map.vers, map.prot);

        let mut mappings = self.mappings.write().unwrap();
        mappings.insert(key, map.port);

        tracing::info!(
            "Registered service: prog={}, vers={}, prot={}, port={}",
            map.prog,
            map.vers,
            map.prot,
            map.port
        );

        true
    }

    /// Unregister a service (PMAPPROC_UNSET)
    ///
    /// Returns true if the service was found and removed
    pub fn unset(&self, map: &mapping) -> bool {
        let key = (map.prog, map.vers, map.prot);

        let mut mappings = self.mappings.write().unwrap();
        let existed = mappings.remove(&key).is_some();

        if existed {
            tracing::info!(
                "Unregistered service: prog={}, vers={}, prot={}",
                map.prog,
                map.vers,
                map.prot
            );
        }

        existed
    }

    /// Query the port for a service (PMAPPROC_GETPORT)
    ///
    /// Returns port number (0 if not found)
    pub fn getport(&self, map: &mapping) -> u32 {
        let key = (map.prog, map.vers, map.prot);

        let mappings = self.mappings.read().unwrap();
        let port = mappings.get(&key).copied().unwrap_or(0);

        tracing::debug!(
            "Query service: prog={}, vers={}, prot={} -> port={}",
            map.prog,
            map.vers,
            map.prot,
            port
        );

        port
    }

    /// Get all registered mappings (PMAPPROC_DUMP)
    pub fn dump(&self) -> Vec<mapping> {
        let mappings = self.mappings.read().unwrap();

        mappings
            .iter()
            .map(|((prog, vers, prot), port)| mapping {
                prog: *prog,
                vers: *vers,
                prot: *prot,
                port: *port,
            })
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
