// NFS COMMIT Procedure (21)
//
// Commits cached write data to stable storage
//
// RFC 1813 Section 3.3.21:
// - Forces any data written with WRITE (stable=UNSTABLE) to be committed to stable storage
// - Client can use this after a series of UNSTABLE writes to ensure data persistence
// - Returns a write verifier that can be used to detect server reboots

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS COMMIT procedure (21)
///
/// Commits data written with UNSTABLE writes to stable storage.
///
/// # Arguments
/// * `xid` - RPC transaction ID
/// * `args_data` - Serialized COMMIT3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized COMMIT3res wrapped in RPC reply
pub async fn handle_commit(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS COMMIT: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_commit3args(args_data)?;

    debug!(
        "  file: {} bytes, offset: {}, count: {}",
        args.file.0.len(),
        args.offset,
        args.count
    );

    // Get file attributes before operation (for wcc_data)
    let file_before = filesystem.getattr(&args.file.0).await.ok();

    // Perform commit operation
    match filesystem.commit(&args.file.0, args.offset, args.count).await {
        Ok(()) => {
            debug!("COMMIT OK");

            // Get file attributes after operation
            let file_after = match filesystem.getattr(&args.file.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get file attributes after commit: {}", e);
                    None
                }
            };

            // Create write verifier (8 bytes)
            // In a production implementation, this should be:
            // - Unique per server boot
            // - Persistent across commits
            // - Changed only when server reboots
            // For now, we use a constant value
            let writeverf: [u8; 8] = [0; 8];

            create_commit_response(xid, nfsstat3::NFS3_OK, file_after, Some(writeverf))
        }
        Err(e) => {
            warn!("COMMIT failed: {}", e);
            let status = map_error_to_status(&e);
            let file_attr = file_before.map(|attr| NfsMessage::fsal_to_fattr3(&attr));
            create_commit_response(xid, status, file_attr, None)
        }
    }
}

/// Create COMMIT3res response
///
/// COMMIT3res structure (RFC 1813):
/// ```text
/// union COMMIT3res switch (nfsstat3 status) {
///     case NFS3_OK:
///         struct {
///             wcc_data file_wcc;
///             writeverf3 verf;
///         } resok;
///     default:
///         struct {
///             wcc_data file_wcc;
///         } resfail;
/// };
/// ```
fn create_commit_response(
    xid: u32,
    status: nfsstat3,
    file_attr: Option<crate::protocol::v3::nfs::fattr3>,
    writeverf: Option<[u8; 8]>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. wcc_data (file weak cache consistency)
    // pre_op_attr (we don't track this, so set to false)
    false.pack(&mut buf)?;

    // post_op_attr (file attributes)
    match &file_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    // 3. For success case, add write verifier
    if status == nfsstat3::NFS3_OK {
        if let Some(verf) = writeverf {
            // Write verifier is opaque[8] - 8 bytes, no length prefix needed
            buf.extend_from_slice(&verf);
        } else {
            return Err(anyhow::anyhow!("Success status but no write verifier provided"));
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
    } else if error_msg.contains("permission denied") || error_msg.contains("access denied") {
        nfsstat3::NFS3ERR_ACCES // 13 - Permission denied
    } else if error_msg.contains("is a directory") {
        nfsstat3::NFS3ERR_ISDIR // 21 - Is a directory
    } else if error_msg.contains("read-only") {
        nfsstat3::NFS3ERR_ROFS // 30 - Read-only filesystem
    } else {
        nfsstat3::NFS3ERR_IO // 5 - I/O error
    }
}
