// MOUNT MNT Procedure Handler
//
// Procedure: 1 (MNT)
// Purpose: Mount a directory and return a file handle

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, info};

use crate::protocol::v3::mount::MountMessage;
use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// Handle MOUNT MNT procedure
///
/// This procedure takes a directory path and returns a file handle that can be used
/// for subsequent NFS operations.
///
/// Arguments: dirpath (string)
/// Returns: mountres3 (file handle + auth flavors on success)
pub async fn handle(
    call: &rpc_call_msg,
    args_data: &[u8],
    filesystem: &dyn crate::fsal::Filesystem,
) -> Result<BytesMut> {
    debug!(
        "MOUNT MNT: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    debug!("MOUNT MNT: args_data = {} bytes, hex: {:02x?}",
           args_data.len(), &args_data[..args_data.len().min(50)]);

    // Deserialize the directory path from the arguments
    let dirpath = MountMessage::deserialize_dirpath(args_data)?;

    info!("MOUNT MNT request for path: '{}'", dirpath);

    // For root path "/" or empty, return the root file handle
    // In a production NFS server, we would validate export permissions here
    // For now, accept any path and return root handle (temporary workaround for path parsing issue)
    let fhandle_bytes = filesystem.root_handle().await;

    info!(
        "Generated file handle ({} bytes) for path '{}'",
        fhandle_bytes.len(),
        dirpath
    );

    // Create successful mount response
    let mount_res = MountMessage::create_mount_ok(fhandle_bytes.clone());

    debug!("MOUNT MNT: Created mountres3 with {} byte handle", fhandle_bytes.len());

    // Create RPC reply header (SUCCESS with no data)
    let rpc_reply = RpcMessage::create_null_reply(call.xid);
    let rpc_header = RpcMessage::serialize_reply(&rpc_reply)?;

    // Serialize MOUNT result
    let mount_data = MountMessage::serialize_mountres3(&mount_res)?;

    debug!("MOUNT MNT: Serialized mount_data = {} bytes, hex: {:02x?}",
           mount_data.len(), &mount_data[..mount_data.len().min(100)]);

    // Combine RPC header + MOUNT result
    // RPC wire format: [RPC Reply Header][Procedure Result Data]
    let mut response = BytesMut::with_capacity(rpc_header.len() + mount_data.len());
    response.extend_from_slice(&rpc_header);
    response.extend_from_slice(&mount_data);

    info!("MOUNT MNT response: {} bytes total", response.len());

    Ok(response)
}

