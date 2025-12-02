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
/// for subsequent NFS operations. In this implementation, we generate a simple
/// file handle based on the path.
///
/// Arguments: dirpath (string)
/// Returns: mountres3 (file handle + auth flavors on success)
pub fn handle(call: &rpc_call_msg, args_data: &[u8]) -> Result<BytesMut> {
    debug!(
        "MOUNT MNT: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Deserialize the directory path from the arguments
    let dirpath = MountMessage::deserialize_dirpath(args_data)?;

    info!("MOUNT MNT request for path: '{}'", dirpath);

    // TODO: Validate path exists and is accessible
    // For now, we accept any path and generate a file handle

    // Generate a simple file handle (for now, just hash the path)
    // In a real implementation, this would be a persistent identifier
    let fhandle_bytes = generate_file_handle(&dirpath);

    info!(
        "Generated file handle ({} bytes) for path '{}'",
        fhandle_bytes.len(),
        dirpath
    );

    // Create successful mount response
    let mount_res = MountMessage::create_mount_ok(fhandle_bytes);

    // Create RPC reply header (SUCCESS with no data)
    let rpc_reply = RpcMessage::create_null_reply(call.xid);
    let rpc_header = RpcMessage::serialize_reply(&rpc_reply)?;

    // Serialize MOUNT result
    let mount_data = MountMessage::serialize_mountres3(&mount_res)?;

    // Combine RPC header + MOUNT result
    // RPC wire format: [RPC Reply Header][Procedure Result Data]
    let mut response = BytesMut::with_capacity(rpc_header.len() + mount_data.len());
    response.extend_from_slice(&rpc_header);
    response.extend_from_slice(&mount_data);

    info!("MOUNT MNT response: {} bytes total", response.len());

    Ok(response)
}

/// Generate a file handle for a given path
///
/// This is a simplified implementation. In production, you would:
/// 1. Validate the path exists
/// 2. Check permissions
/// 3. Generate a persistent, secure file handle
/// 4. Store the mapping (path <-> file handle)
fn generate_file_handle(path: &str) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Create a simple hash-based file handle
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();

    // Convert to bytes (8 bytes for u64)
    // Pad to reasonable size (32 bytes is common for NFSv3)
    let mut fhandle = vec![0u8; 32];
    fhandle[0..8].copy_from_slice(&hash.to_be_bytes());

    // Add some metadata (length of path in bytes 8-11)
    let path_len = path.len() as u32;
    fhandle[8..12].copy_from_slice(&path_len.to_be_bytes());

    fhandle
}
