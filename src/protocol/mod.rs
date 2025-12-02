// Protocol middleware layer
//
// This module provides a clean abstraction over XDR-generated types,
// handling serialization/deserialization and version differences.

pub mod v3;

// Re-export commonly used types
pub use v3::{RpcMessage, PortmapMessage, MountMessage, NfsMessage};
