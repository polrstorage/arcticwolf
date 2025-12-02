// Portmapper Protocol Middleware
//
// Wraps xdrgen-generated Portmapper types and provides serialization helpers

use anyhow::Result;
use bytes::BytesMut;
use std::io::Cursor;
use xdr_codec::{Pack, Unpack};

// Include xdrgen-generated Portmapper types
#[allow(dead_code, non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/portmap_generated.rs"));
}

// Re-export generated types
pub use generated::*;

/// Wrapper for Portmapper messages providing serialization helpers
pub struct PortmapMessage;

impl PortmapMessage {
    /// Deserialize mapping argument
    pub fn deserialize_mapping(data: &[u8]) -> Result<mapping> {
        let mut cursor = Cursor::new(data);
        let (map, _bytes_read) = mapping::unpack(&mut cursor)?;
        Ok(map)
    }

    /// Serialize boolean result
    pub fn serialize_bool(result: bool) -> Result<BytesMut> {
        let mut buf = Vec::new();
        let bool_val = if result { 1u32 } else { 0u32 };
        bool_val.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Serialize port result
    pub fn serialize_port(port: u32) -> Result<BytesMut> {
        let mut buf = Vec::new();
        port.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a mapping entry
    pub fn create_mapping(prog: u32, vers: u32, prot: u32, port: u32) -> mapping {
        mapping {
            prog,
            vers,
            prot,
            port,
        }
    }
}
