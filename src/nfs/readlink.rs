// NFS READLINK Procedure (5) Handler
//
// Reads the content of a symbolic link

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle READLINK procedure
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `args_data` - Serialized READLINK3args
/// * `filesystem` - Filesystem implementation
///
/// # Returns
/// Serialized READLINK3res response
pub async fn handle_readlink(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS READLINK: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_readlink3args(args_data)?;

    debug!("  symlink: {} bytes", args.symlink.0.len());

    // Get symlink attributes before operation (for post_op_attr)
    let symlink_attr_before = filesystem.getattr(&args.symlink.0).await.ok();

    // Read the symlink target
    match filesystem.readlink(&args.symlink.0).await {
        Ok(target) => {
            debug!("READLINK OK: target = {}", target);

            // Get symlink attributes after operation
            let symlink_attr_after = match filesystem.getattr(&args.symlink.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get symlink attributes after readlink: {}", e);
                    None
                }
            };

            create_readlink_response(xid, nfsstat3::NFS3_OK, symlink_attr_after, Some(target))
        }
        Err(e) => {
            warn!("READLINK failed: {}", e);

            // Map error to NFS status code
            let status = map_error_to_status(&e);

            // Get symlink attributes for failure case
            let symlink_attr = symlink_attr_before.map(|attr| NfsMessage::fsal_to_fattr3(&attr));

            create_readlink_response(xid, status, symlink_attr, None)
        }
    }
}

/// Map filesystem error to NFS status code
fn map_error_to_status(error: &anyhow::Error) -> nfsstat3 {
    let error_str = format!("{:?}", error);

    // Check for specific error patterns
    if error_str.contains("No such file") || error_str.contains("not found") {
        return nfsstat3::NFS3ERR_NOENT;
    }

    if error_str.contains("Permission denied") || error_str.contains("Access denied") {
        return nfsstat3::NFS3ERR_ACCES;
    }

    if error_str.contains("Not a symbolic link") {
        return nfsstat3::NFS3ERR_INVAL;
    }

    if error_str.contains("Invalid argument") {
        return nfsstat3::NFS3ERR_INVAL;
    }

    // Try downcasting to std::io::Error
    if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind;
        return match io_error.kind() {
            ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
            ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
            ErrorKind::InvalidInput => nfsstat3::NFS3ERR_INVAL,
            _ => nfsstat3::NFS3ERR_IO,
        };
    }

    // Default to IO error
    nfsstat3::NFS3ERR_IO
}

/// Create READLINK3res response
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `status` - NFS status code
/// * `symlink_attr` - Symlink attributes (post_op_attr)
/// * `target` - Symlink target path (only for success case)
fn create_readlink_response(
    xid: u32,
    status: nfsstat3,
    symlink_attr: Option<crate::protocol::v3::nfs::fattr3>,
    target: Option<String>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. post_op_attr (symlink_attributes)
    match &symlink_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    // 3. For success case, add target path
    if status == nfsstat3::NFS3_OK {
        if let Some(target_path) = target {
            // Pack as nfspath3 (string)
            let target_bytes = target_path.as_bytes();
            (target_bytes.len() as u32).pack(&mut buf)?;
            buf.extend_from_slice(target_bytes);

            // Add padding to 4-byte boundary
            let padding = (4 - (target_bytes.len() % 4)) % 4;
            buf.extend_from_slice(&vec![0u8; padding]);
        } else {
            return Err(anyhow!("Success status but no target provided"));
        }
    }

    let res_data = BytesMut::from(&buf[..]);
    RpcMessage::create_success_reply_with_data(xid, res_data)
}
