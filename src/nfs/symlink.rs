// NFS SYMLINK Procedure (10) Handler
//
// Creates a symbolic link

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle SYMLINK procedure
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `args_data` - Serialized SYMLINK3args
/// * `filesystem` - Filesystem implementation
///
/// # Returns
/// Serialized SYMLINK3res response
pub async fn handle_symlink(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS SYMLINK: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_symlink3args(args_data)?;

    debug!(
        "  dir: {} bytes, name: {}, target: {}",
        args.where_dir.0.len(),
        args.name.0,
        args.symlink.symlink_data.0
    );

    // Get parent directory attributes before operation (for wcc_data)
    let dir_before = filesystem.getattr(&args.where_dir.0).await.ok();

    // Perform symlink operation
    match filesystem.symlink(&args.where_dir.0, &args.name.0, &args.symlink.symlink_data.0).await {
        Ok(new_symlink_handle) => {
            debug!("SYMLINK OK: created symlink '{}'", args.name.0);

            // Get new symlink attributes
            let symlink_attr = match filesystem.getattr(&new_symlink_handle).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get symlink attributes: {}", e);
                    None
                }
            };

            // Get parent directory attributes after operation
            let dir_after = match filesystem.getattr(&args.where_dir.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get directory attributes after symlink: {}", e);
                    None
                }
            };

            create_symlink_response(
                xid,
                nfsstat3::NFS3_OK,
                Some(new_symlink_handle),
                symlink_attr,
                dir_after,
            )
        }
        Err(e) => {
            warn!("SYMLINK failed: {}", e);

            // Map error to NFS status code
            let status = map_error_to_status(&e);

            // Get parent directory attributes for failure case
            let dir_attr = dir_before.map(|attr| NfsMessage::fsal_to_fattr3(&attr));

            create_symlink_response(xid, status, None, None, dir_attr)
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

    if error_str.contains("already exists") {
        return nfsstat3::NFS3ERR_EXIST;
    }

    if error_str.contains("Permission denied") || error_str.contains("Access denied") {
        return nfsstat3::NFS3ERR_ACCES;
    }

    if error_str.contains("Not a directory") {
        return nfsstat3::NFS3ERR_NOTDIR;
    }

    if error_str.contains("Name too long") {
        return nfsstat3::NFS3ERR_NAMETOOLONG;
    }

    if error_str.contains("Read-only") {
        return nfsstat3::NFS3ERR_ROFS;
    }

    if error_str.contains("No space") {
        return nfsstat3::NFS3ERR_NOSPC;
    }

    // Try downcasting to std::io::Error
    if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind;
        return match io_error.kind() {
            ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
            ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
            ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
            _ => nfsstat3::NFS3ERR_IO,
        };
    }

    // Default to IO error
    nfsstat3::NFS3ERR_IO
}

/// Create SYMLINK3res response
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `status` - NFS status code
/// * `symlink_handle` - New symlink file handle (post_op_fh3)
/// * `symlink_attr` - New symlink attributes (post_op_attr)
/// * `dir_attr` - Parent directory attributes (wcc_data)
fn create_symlink_response(
    xid: u32,
    status: nfsstat3,
    symlink_handle: Option<Vec<u8>>,
    symlink_attr: Option<crate::protocol::v3::nfs::fattr3>,
    dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. For success case: post_op_fh3 (new symlink handle) + post_op_attr
    if status == nfsstat3::NFS3_OK {
        // post_op_fh3 (new symlink handle)
        match symlink_handle {
            Some(handle) => {
                true.pack(&mut buf)?;
                // Pack handle as fhandle3 (opaque)
                (handle.len() as u32).pack(&mut buf)?;
                buf.extend_from_slice(&handle);
                // Add padding to 4-byte boundary
                let padding = (4 - (handle.len() % 4)) % 4;
                buf.extend_from_slice(&vec![0u8; padding]);
            }
            None => {
                false.pack(&mut buf)?;
            }
        }

        // post_op_attr (new symlink attributes)
        match &symlink_attr {
            Some(attr) => {
                true.pack(&mut buf)?;
                attr.pack(&mut buf)?;
            }
            None => {
                false.pack(&mut buf)?;
            }
        }
    }

    // 3. wcc_data (parent directory)
    // pre_op_attr (we don't track this, so set to false)
    false.pack(&mut buf)?;

    // post_op_attr (parent directory)
    match &dir_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    let res_data = BytesMut::from(&buf[..]);
    RpcMessage::create_success_reply_with_data(xid, res_data)
}
