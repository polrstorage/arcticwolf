// MOUNT NULL Procedure Handler
//
// Procedure: 0 (NULL)
// Purpose: Test connectivity, does nothing but return success

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// Handle MOUNT NULL procedure
///
/// This is a simple ping-like operation that verifies the MOUNT service is running.
/// It takes no arguments and returns no data, only an RPC success reply.
pub fn handle(call: &rpc_call_msg) -> Result<BytesMut> {
    debug!(
        "MOUNT NULL: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Create successful reply (same as RPC NULL)
    let reply = RpcMessage::create_null_reply(call.xid);

    // Serialize reply
    RpcMessage::serialize_reply(&reply)
}
