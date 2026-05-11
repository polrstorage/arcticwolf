// NFS FSSTAT Procedure (Procedure 18)
//
// Returns dynamic filesystem statistics

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::NfsBackend;
use crate::nfs::error::classify_handle_error;
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
    filesystem: &dyn NfsBackend,
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
            let error_status = classify_handle_error(&e);
            let res_data = NfsMessage::create_fsstat_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Query the export's underlying filesystem for live capacity / inode
    // counts. The FsStats are per-export: a multi-export wrapper routes by
    // the uid prefix in `args.fsroot`, so two exports backed by different
    // mount points report different numbers from the same server.
    let stats = match filesystem.fs_stats(&args.fsroot.0).await {
        Ok(s) => s,
        Err(e) => {
            debug!("FSSTAT failed (fs_stats): {}", e);
            let error_status = classify_handle_error(&e);
            let res_data = NfsMessage::create_fsstat_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    let tbytes = stats.total_bytes;
    let fbytes = stats.free_bytes;
    let abytes = stats.avail_bytes;
    let tfiles = stats.total_files;
    let ffiles = stats.free_files;
    let afiles = stats.avail_files;
    // invarsec=0 advertises "filesystem may change at any time"; we have no
    // better estimate for a live export, so leave it at the spec-suggested
    // safe default.
    let invarsec = 0u32;

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
        let config = BackendConfig::Local {
            path: temp_dir.path().to_path_buf(),
        };
        let fs = config.create_filesystem(1).unwrap();

        // Get root handle
        let root_handle = fs.root_file_handle();

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
        let config = BackendConfig::Local {
            path: temp_dir.path().to_path_buf(),
        };
        let fs = config.create_filesystem(1).unwrap();

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
