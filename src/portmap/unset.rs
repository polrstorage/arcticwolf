// Portmapper UNSET Procedure Handler
//
// Procedure: 2 (PMAPPROC_UNSET)
// Purpose: Unregister a service

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, info};

use crate::portmap::registry::Registry;
use crate::protocol::v3::portmap::PortmapMessage;
use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// Handle Portmapper UNSET procedure
///
/// Unregisters a service mapping.
///
/// Arguments: mapping
/// Returns: bool (true if successfully unregistered)
pub fn handle(call: &rpc_call_msg, args_data: &[u8], registry: &Registry) -> Result<BytesMut> {
    debug!(
        "PORTMAP UNSET: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Deserialize mapping argument
    let map = PortmapMessage::deserialize_mapping(args_data)?;

    info!(
        "PORTMAP UNSET: unregistering prog={}, vers={}, prot={}",
        map.prog, map.vers, map.prot
    );

    // Unregister the service
    let success = registry.unset(&map);

    // Create RPC reply header
    let rpc_reply = RpcMessage::create_null_reply(call.xid);
    let rpc_header = RpcMessage::serialize_reply(&rpc_reply)?;

    // Serialize boolean result
    let result_data = PortmapMessage::serialize_bool(success)?;

    // Combine RPC header + result
    let mut response = BytesMut::with_capacity(rpc_header.len() + result_data.len());
    response.extend_from_slice(&rpc_header);
    response.extend_from_slice(&result_data);

    Ok(response)
}
