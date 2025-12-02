// MOUNT UMNT Procedure Handler
//
// Procedure: 3 (UMNT)
// Purpose: Unmount a previously mounted directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, info};

use crate::protocol::v3::mount::MountMessage;
use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// Handle MOUNT UMNT procedure
///
/// This procedure unmounts a previously mounted directory path.
/// It takes a directory path as argument and returns void (just RPC success).
///
/// Arguments: dirpath (string)
/// Returns: void (RPC success reply only)
pub fn handle(call: &rpc_call_msg, args_data: &[u8]) -> Result<BytesMut> {
    debug!(
        "MOUNT UMNT: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Deserialize the directory path from the arguments
    let dirpath = match MountMessage::deserialize_dirpath(args_data) {
        Ok(path) => path,
        Err(e) => {
            info!("Failed to deserialize dirpath: {}", e);
            // Even on error, return success (UMNT is idempotent)
            let reply = RpcMessage::create_null_reply(call.xid);
            return RpcMessage::serialize_reply(&reply);
        }
    };

    info!("MOUNT UMNT request for path: '{}'", dirpath);

    // TODO: Remove the mount entry from internal state
    // For now, we just acknowledge the unmount request

    info!("Unmounted path '{}'", dirpath);

    // Return simple success reply (void result)
    let reply = RpcMessage::create_null_reply(call.xid);
    RpcMessage::serialize_reply(&reply)
}
