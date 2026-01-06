// NFS FSINFO Procedure (Procedure 19)
//
// Returns static filesystem information

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

// FSINFO property constants
const FSF3_LINK: u32 = 0x0001; // Server supports hard links
const FSF3_SYMLINK: u32 = 0x0002; // Server supports symbolic links
const FSF3_HOMOGENEOUS: u32 = 0x0008; // PATHCONF is valid for all files
const FSF3_CANSETTIME: u32 = 0x0010; // Server can set time on server

/// Handle NFS FSINFO procedure (procedure 19)
///
/// Returns static filesystem information such as maximum sizes and capabilities.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized FSINFO3args (fsroot handle)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with filesystem information
pub async fn handle_fsinfo(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS FSINFO called (xid={})", xid);

    // Deserialize arguments (fsroot handle)
    debug!("FSINFO: args_data = {} bytes, hex: {:02x?}",
           args_data.len(), &args_data[..args_data.len().min(100)]);

    let args = NfsMessage::deserialize_fsinfo3args(args_data)?;

    debug!("FSINFO: fsroot_handle={} bytes, hex: {:02x?}",
           args.fsroot.0.len(), &args.fsroot.0);

    // Get filesystem attributes
    let obj_attrs = match filesystem.getattr(&args.fsroot.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("FSINFO failed: {}", e);
            let error_status = if e.to_string().contains("not found")
                || e.to_string().contains("Invalid handle")
            {
                nfsstat3::NFS3ERR_STALE
            } else {
                nfsstat3::NFS3ERR_IO
            };

            let res_data = NfsMessage::create_fsinfo_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Define filesystem capabilities and limits
    // These values are based on RFC 1813 recommendations
    let rtmax = 1024 * 1024; // 1 MB - max read request
    let rtpref = 64 * 1024; // 64 KB - preferred read size
    let rtmult = 4096; // 4 KB - suggested read multiple
    let wtmax = 1024 * 1024; // 1 MB - max write request
    let wtpref = 64 * 1024; // 64 KB - preferred write size
    let wtmult = 4096; // 4 KB - suggested write multiple
    let dtpref = 8192; // 8 KB - preferred READDIR size
    let maxfilesize = 0xFFFFFFFFFFFFFFFFu64; // Maximum file size (unlimited)

    // Time precision - 1 nanosecond
    let time_delta_seconds = 0u32;
    let time_delta_nseconds = 1u32;

    // Filesystem properties
    let properties = FSF3_LINK | FSF3_SYMLINK | FSF3_HOMOGENEOUS | FSF3_CANSETTIME;

    debug!(
        "FSINFO success: rtmax={}, wtmax={}, dtpref={}",
        rtmax, wtmax, dtpref
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_attrs = NfsMessage::fsal_to_fattr3(&obj_attrs);

    // Create successful response (manually serialized with proper post_op_attr)
    let res_data = NfsMessage::create_fsinfo_ok(
        nfs_attrs,
        rtmax,
        rtpref,
        rtmult,
        wtmax,
        wtpref,
        wtmult,
        dtpref,
        maxfilesize,
        time_delta_seconds,
        time_delta_nseconds,
        properties,
    )?;

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::BackendConfig;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_fsinfo_root() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Serialize FSINFO3args
        use crate::protocol::v3::nfs::FSINFO3args;
        use xdr_codec::Pack;

        let args = FSINFO3args {
            fsroot: crate::protocol::v3::nfs::fhandle3(root_handle),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call FSINFO
        let result = handle_fsinfo(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "FSINFO should succeed");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");
    }

    #[tokio::test]
    async fn test_fsinfo_invalid_handle() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Use invalid file handle
        use crate::protocol::v3::nfs::FSINFO3args;
        use xdr_codec::Pack;

        let args = FSINFO3args {
            fsroot: crate::protocol::v3::nfs::fhandle3(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call FSINFO
        let result = handle_fsinfo(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "FSINFO should return error response (not panic)");
    }
}
