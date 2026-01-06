// NFS LINK Procedure (15)
//
// Creates a hard link to an existing file.
//
// RFC 1813 Section 3.3.15:
// - Creates a new directory entry (link) pointing to the same inode
// - Returns updated file attributes (link count increases)
// - Returns wcc_data for the target directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS LINK procedure (15)
///
/// Creates a hard link from `link_dir/name` pointing to `file`.
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `args_data` - Serialized LINK3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized LINK3res wrapped in RPC reply
pub async fn handle_link(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS LINK: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_link3args(args_data)?;

    debug!(
        "  file: {} bytes, link_dir: {} bytes, name: {}",
        args.file.0.len(),
        args.link_dir.0.len(),
        args.name.0
    );

    // Get source file attributes before operation (for post_op_attr)
    let file_before = filesystem.getattr(&args.file.0).await.ok();

    // Get target directory attributes before operation (for wcc_data)
    let dir_before = filesystem.getattr(&args.link_dir.0).await.ok();

    // Perform link operation
    match filesystem.link(&args.file.0, &args.link_dir.0, &args.name.0).await {
        Ok(_file_handle) => {
            debug!("LINK OK: created hard link '{}'", args.name.0);

            // Get source file attributes after operation (link count should increase)
            let file_after = match filesystem.getattr(&args.file.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get file attributes after link: {}", e);
                    None
                }
            };

            // Get target directory attributes after operation
            let dir_after = match filesystem.getattr(&args.link_dir.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get directory attributes after link: {}", e);
                    None
                }
            };

            create_link_response(xid, nfsstat3::NFS3_OK, file_after, dir_after)
        }
        Err(e) => {
            warn!("LINK failed: {}", e);
            let status = map_error_to_status(&e);
            let file_attr = file_before.map(|attr| NfsMessage::fsal_to_fattr3(&attr));
            let dir_attr = dir_before.map(|attr| NfsMessage::fsal_to_fattr3(&attr));
            create_link_response(xid, status, file_attr, dir_attr)
        }
    }
}

/// Create LINK3res response
///
/// LINK3res structure (RFC 1813):
/// ```text
/// union LINK3res switch (nfsstat3 status) {
///     case NFS3_OK:
///         struct {
///             post_op_attr file_attributes;
///             wcc_data linkdir_wcc;
///         } resok;
///     default:
///         struct {
///             post_op_attr file_attributes;
///             wcc_data linkdir_wcc;
///         } resfail;
/// };
/// ```
fn create_link_response(
    xid: u32,
    status: nfsstat3,
    file_attr: Option<crate::protocol::v3::nfs::fattr3>,
    dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. post_op_attr (source file attributes)
    match &file_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    // 3. wcc_data (target directory)
    // pre_op_attr (we don't track this, so set to false)
    false.pack(&mut buf)?;

    // post_op_attr (target directory)
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
    } else if error_msg.contains("already exists") || error_msg.contains("file exists") {
        nfsstat3::NFS3ERR_EXIST // 17 - File exists
    } else if error_msg.contains("permission denied") || error_msg.contains("access denied") {
        nfsstat3::NFS3ERR_ACCES // 13 - Permission denied
    } else if error_msg.contains("not a directory") {
        nfsstat3::NFS3ERR_NOTDIR // 20 - Not a directory
    } else if error_msg.contains("is a directory") || error_msg.contains("cannot create hard link to directory") {
        nfsstat3::NFS3ERR_ISDIR // 21 - Is a directory
    } else if error_msg.contains("cross-device") || error_msg.contains("different filesystem") {
        nfsstat3::NFS3ERR_XDEV // 18 - Cross-device link
    } else if error_msg.contains("invalid") {
        nfsstat3::NFS3ERR_INVAL // 22 - Invalid argument
    } else {
        nfsstat3::NFS3ERR_IO // 5 - I/O error
    }
}
