// Portmapper GETPORT Procedure Handler
//
// Procedure: 3 (PMAPPROC_GETPORT)
// Purpose: Query the port for a service

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::portmap::registry::Registry;
use crate::protocol::v3::portmap::PortmapMessage;
use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// Handle Portmapper GETPORT procedure
///
/// Queries the port number for a registered service.
///
/// Arguments: mapping (only prog, vers, prot are used; port is ignored)
/// Returns: unsigned int (port number, 0 if not found)
pub fn handle(call: &rpc_call_msg, args_data: &[u8], registry: &Registry) -> Result<BytesMut> {
    debug!(
        "PORTMAP GETPORT: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Deserialize mapping argument
    let map = PortmapMessage::deserialize_mapping(args_data)?;

    debug!(
        "PORTMAP GETPORT: query prog={}, vers={}, prot={}",
        map.prog, map.vers, map.prot
    );

    // Query the port
    let port = registry.getport(&map);

    debug!("PORTMAP GETPORT: result port={}", port);

    // Create RPC reply header
    let rpc_reply = RpcMessage::create_null_reply(call.xid);
    let rpc_header = RpcMessage::serialize_reply(&rpc_reply)?;

    // Serialize port result
    let result_data = PortmapMessage::serialize_port(port)?;

    // Combine RPC header + result
    let mut response = BytesMut::with_capacity(rpc_header.len() + result_data.len());
    response.extend_from_slice(&rpc_header);
    response.extend_from_slice(&result_data);

    Ok(response)
}
