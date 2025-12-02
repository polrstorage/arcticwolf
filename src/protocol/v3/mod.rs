// NFSv3 Protocol Types and Middleware
//
// This module wraps xdrgen-generated types and provides:
// - Unified serialization/deserialization interface
// - Conversion between XDR types and domain types
// - Error handling

pub mod rpc;
pub mod portmap;
pub mod mount;
pub mod nfs;

// Re-export for convenience
pub use rpc::RpcMessage;
pub use portmap::PortmapMessage;
pub use mount::MountMessage;
pub use nfs::NfsMessage;
