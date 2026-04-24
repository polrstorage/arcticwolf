// NFS FSSTAT Procedure (Procedure 18)
//
// Returns dynamic filesystem statistics

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{NfsMessage, nfsstat3};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS FSSTAT procedure (procedure 18)
///
/// Returns dynamic filesystem information such as total/free space.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized FSSTAT3args (fsroot handle)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with filesystem statistics
pub async fn handle_fsstat(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS FSSTAT called (xid={})", xid);

    // Deserialize arguments (fsroot handle)
    let args = NfsMessage::deserialize_fsstat3args(args_data)?;

    debug!("FSSTAT: fsroot_handle={} bytes", args.fsroot.0.len());

    // Get filesystem attributes
    let obj_attrs = match filesystem.getattr(&args.fsroot.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("FSSTAT failed: {}", e);
            let error_status = if e.to_string().contains("not found")
                || e.to_string().contains("Invalid handle")
            {
                nfsstat3::NFS3ERR_STALE
            } else {
                nfsstat3::NFS3ERR_IO
            };

            let res_data = NfsMessage::create_fsstat_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Query filesystem statistics via the FSAL. The backend returns real
    // statvfs values; a later stage folds quota-aware byte counts into this
    // same path.
    let stats = match filesystem.statvfs(&args.fsroot.0).await {
        Ok(s) => s,
        Err(e) => {
            debug!("FSSTAT statvfs failed: {}", e);
            let res_data = NfsMessage::create_fsstat_error_response(nfsstat3::NFS3ERR_IO)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    let tbytes = stats.total_bytes;
    let fbytes = stats.free_bytes;
    let abytes = stats.avail_bytes;
    let tfiles = stats.total_files;
    let ffiles = stats.free_files;
    let afiles = stats.avail_files;
    let invarsec = 0u32; // filesystem not expected to change without client intervention

    debug!(
        "FSSTAT success: tbytes={}, fbytes={}, tfiles={}",
        tbytes, fbytes, tfiles
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_attrs = NfsMessage::fsal_to_fattr3(&obj_attrs);

    // Create FSSTAT response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. post_op_attr (obj_attributes)
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_attrs.pack(&mut buf)?;

    // 3. FSSTAT fields
    tbytes.pack(&mut buf)?;
    fbytes.pack(&mut buf)?;
    abytes.pack(&mut buf)?;
    tfiles.pack(&mut buf)?;
    ffiles.pack(&mut buf)?;
    afiles.pack(&mut buf)?;
    invarsec.pack(&mut buf)?;

    let res_data = BytesMut::from(&buf[..]);

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::BackendConfig;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_fsstat_root() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Serialize FSSTAT3args
        use crate::protocol::v3::nfs::FSSTAT3args;
        use xdr_codec::Pack;

        let args = FSSTAT3args {
            fsroot: crate::protocol::v3::nfs::fhandle3(root_handle),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call FSSTAT
        let result = handle_fsstat(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "FSSTAT should succeed");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");
    }

    #[tokio::test]
    async fn test_fsstat_invalid_handle() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Use invalid file handle
        use crate::protocol::v3::nfs::FSSTAT3args;
        use xdr_codec::Pack;

        let args = FSSTAT3args {
            fsroot: crate::protocol::v3::nfs::fhandle3(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call FSSTAT
        let result = handle_fsstat(12345, &args_buf, fs.as_ref()).await;

        assert!(
            result.is_ok(),
            "FSSTAT should return error response (not panic)"
        );
    }
}
