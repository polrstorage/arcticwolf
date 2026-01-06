// NFS MKNOD Procedure (11)
//
// Creates special files (device files, FIFOs, sockets)
//
// RFC 1813 Section 3.3.11:
// - Creates character devices, block devices, FIFOs (named pipes), or sockets
// - Requires appropriate permissions (typically root for device files)
// - Returns file handle and attributes of the created special file

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::{FileType, Filesystem};
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS MKNOD procedure (11)
///
/// Creates a special file (device, FIFO, socket).
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `args_data` - Serialized MKNOD3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized MKNOD3res wrapped in RPC reply
pub async fn handle_mknod(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS MKNOD: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_mknod3args(args_data)?;

    debug!(
        "  dir: {} bytes, name: {}, type: {:?}",
        args.where_dir.0.len(),
        args.name.0,
        args.what
    );

    // Get directory attributes before operation (for wcc_data)
    let dir_before = filesystem.getattr(&args.where_dir.0).await.ok();

    // Extract file type, mode, and device numbers from union
    let (file_type, mode, rdev) = match &args.what {
        crate::protocol::v3::nfs::mknoddata3::NF3CHR(dev) => {
            debug!("  Creating character device: major={}, minor={}", dev.major, dev.minor);
            let mode = extract_mode(&dev.dev_attributes);
            (FileType::CharDevice, mode, (dev.major, dev.minor))
        }
        crate::protocol::v3::nfs::mknoddata3::NF3BLK(dev) => {
            debug!("  Creating block device: major={}, minor={}", dev.major, dev.minor);
            let mode = extract_mode(&dev.dev_attributes);
            (FileType::BlockDevice, mode, (dev.major, dev.minor))
        }
        crate::protocol::v3::nfs::mknoddata3::NF3SOCK(attrs) => {
            debug!("  Creating socket");
            let mode = extract_mode(attrs);
            (FileType::Socket, mode, (0, 0))
        }
        crate::protocol::v3::nfs::mknoddata3::NF3FIFO(attrs) => {
            debug!("  Creating FIFO (named pipe)");
            let mode = extract_mode(attrs);
            (FileType::NamedPipe, mode, (0, 0))
        }
    };

    let name = &args.name.0;

    // Perform mknod operation
    match filesystem.mknod(&args.where_dir.0, &name, file_type, mode, rdev).await {
        Ok(handle) => {
            debug!("MKNOD OK: created {:?}", name);

            // Get attributes of the created special file
            let obj_attr = match filesystem.getattr(&handle).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get attributes after mknod: {}", e);
                    None
                }
            };

            // Get directory attributes after operation
            let dir_after = match filesystem.getattr(&args.where_dir.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get dir attributes after mknod: {}", e);
                    None
                }
            };

            create_mknod_response(xid, nfsstat3::NFS3_OK, Some(handle), obj_attr, dir_after)
        }
        Err(e) => {
            warn!("MKNOD failed: {}", e);
            let status = map_error_to_status(&e);
            let dir_attr = dir_before.map(|attr| NfsMessage::fsal_to_fattr3(&attr));
            create_mknod_response(xid, status, None, None, dir_attr)
        }
    }
}

/// Extract mode from sattr3
fn extract_mode(sattr: &crate::protocol::v3::nfs::sattr3) -> u32 {
    match &sattr.mode {
        crate::protocol::v3::nfs::set_mode3::SET_MODE(mode) => *mode,
        crate::protocol::v3::nfs::set_mode3::default => 0o666, // Default mode
    }
}

/// Create MKNOD3res response
///
/// MKNOD3res structure (RFC 1813):
/// ```text
/// union MKNOD3res switch (nfsstat3 status) {
///     case NFS3_OK:
///         struct {
///             post_op_fh3   obj;
///             post_op_attr  obj_attributes;
///             wcc_data      dir_wcc;
///         } resok;
///     default:
///         struct {
///             wcc_data      dir_wcc;
///         } resfail;
/// };
/// ```
fn create_mknod_response(
    xid: u32,
    status: nfsstat3,
    obj_handle: Option<Vec<u8>>,
    obj_attr: Option<crate::protocol::v3::nfs::fattr3>,
    dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    if status == nfsstat3::NFS3_OK {
        // Success case: obj + obj_attributes + dir_wcc

        // post_op_fh3 obj (new special file handle)
        match obj_handle {
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

        // post_op_attr obj_attributes
        match &obj_attr {
            Some(attr) => {
                true.pack(&mut buf)?;
                attr.pack(&mut buf)?;
            }
            None => {
                false.pack(&mut buf)?;
            }
        }
    }

    // dir_wcc (for both success and failure)
    // wcc_data: pre_op_attr + post_op_attr

    // pre_op_attr (we don't track this, so set to false)
    false.pack(&mut buf)?;

    // post_op_attr (directory attributes)
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

/// Map filesystem errors to NFS status codes
fn map_error_to_status(error: &anyhow::Error) -> nfsstat3 {
    let error_msg = error.to_string().to_lowercase();

    if error_msg.contains("not found") || error_msg.contains("no such file") {
        nfsstat3::NFS3ERR_NOENT // 2 - No such file or directory
    } else if error_msg.contains("permission denied") || error_msg.contains("access denied") || error_msg.contains("operation not permitted") {
        nfsstat3::NFS3ERR_ACCES // 13 - Permission denied
    } else if error_msg.contains("exists") || error_msg.contains("already") {
        nfsstat3::NFS3ERR_EXIST // 17 - File exists
    } else if error_msg.contains("not a directory") {
        nfsstat3::NFS3ERR_NOTDIR // 20 - Not a directory
    } else if error_msg.contains("read-only") {
        nfsstat3::NFS3ERR_ROFS // 30 - Read-only filesystem
    } else if error_msg.contains("not supported") || error_msg.contains("not fully supported") {
        nfsstat3::NFS3ERR_NOTSUPP // 10004 - Operation not supported
    } else {
        nfsstat3::NFS3ERR_IO // 5 - I/O error
    }
}
