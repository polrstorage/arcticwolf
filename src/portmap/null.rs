// Portmapper NULL Procedure Handler
//
// Procedure: 0 (NULL)
// Purpose: Test connectivity

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// Handle Portmapper NULL procedure
///
/// Simple ping test to verify portmapper service is running.
pub fn handle(call: &rpc_call_msg) -> Result<BytesMut> {
    debug!(
        "PORTMAP NULL: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Create successful reply
    let reply = RpcMessage::create_null_reply(call.xid);

    // Serialize reply
    RpcMessage::serialize_reply(&reply)
}
