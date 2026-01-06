// NFS GETATTR Procedure (Procedure 1)
//
// Returns file attributes for a given file handle

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::NfsMessage;
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS GETATTR procedure (procedure 1)
///
/// Returns file attributes for the given file handle.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized GETATTR3args (contains file handle)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with file attributes
pub async fn handle_getattr(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS GETATTR called (xid={})", xid);

    // Deserialize arguments (file handle)
    let args = NfsMessage::deserialize_getattr3args(args_data)?;

    debug!("GETATTR: file handle = {} bytes", args.object.0.len());

    // Get file attributes from FSAL
    let fsal_attrs = match filesystem.getattr(&args.object.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("GETATTR failed: {}", e);
            // Return NFS error - use STALE for invalid handle, IO for other errors
            use crate::protocol::v3::nfs::nfsstat3;
            let error_status = nfsstat3::NFS3ERR_STALE; // File handle error
            let res_data = NfsMessage::create_getattr_error_response(error_status)?;

            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!(
        "GETATTR success: type={:?}, size={}, mode={:o}",
        fsal_attrs.ftype, fsal_attrs.size, fsal_attrs.mode
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_attrs = NfsMessage::fsal_to_fattr3(&fsal_attrs);

    // Create successful response
    let response = NfsMessage::create_getattr_ok(nfs_attrs);

    // Serialize response
    let res_data = NfsMessage::serialize_getattr3res(&response)?;

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::BackendConfig;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_getattr_root() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Serialize the handle as GETATTR3args
        use crate::protocol::v3::nfs::{GETATTR3args, fhandle3};
        use xdr_codec::Pack;

        let args = GETATTR3args {
            object: fhandle3(root_handle.clone()),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call GETATTR
        let result = handle_getattr(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "GETATTR should succeed for root");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");
    }
}
