// NFS REMOVE Procedure (12)
//
// Remove a file from a directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS REMOVE request
///
/// Removes a file from a directory. This operation is atomic - either the file
/// is removed successfully or the directory is unchanged.
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized REMOVE3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with REMOVE3res
pub async fn handle_remove(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS REMOVE: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_remove3args(args_data)?;

    debug!(
        "  dir handle: {} bytes, filename: {}",
        args.dir.0.len(),
        args.name.0
    );

    // Get directory attributes before removal (for wcc_data)
    let dir_before = filesystem.getattr(&args.dir.0).await.ok();

    // Perform remove operation
    match filesystem.remove(&args.dir.0, &args.name.0).await {
        Ok(()) => {
            debug!("REMOVE OK: removed file '{}'", args.name.0);

            // Get directory attributes after removal
            let dir_after = match filesystem.getattr(&args.dir.0).await {
                Ok(attr) => NfsMessage::fsal_to_fattr3(&attr),
                Err(e) => {
                    warn!("Failed to get dir attributes after remove: {}", e);
                    // Continue anyway, removal succeeded
                    return create_remove_response(xid, nfsstat3::NFS3_OK, None);
                }
            };

            create_remove_response(xid, nfsstat3::NFS3_OK, Some(dir_after))
        }
        Err(e) => {
            warn!("REMOVE failed for '{}': {}", args.name.0, e);

            // Determine appropriate error code based on error message and IO error kind
            let error_string = e.to_string();
            let status = if error_string.contains("not found") || error_string.contains("No such") {
                nfsstat3::NFS3ERR_NOENT
            } else if error_string.contains("permission") || error_string.contains("Permission") {
                nfsstat3::NFS3ERR_ACCES
            } else if error_string.contains("directory") || error_string.contains("Is a directory") {
                nfsstat3::NFS3ERR_ISDIR
            } else {
                // Try to get std::io::Error from anyhow::Error
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    match io_err.kind() {
                        std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
                        std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
                        _ => nfsstat3::NFS3ERR_IO,
                    }
                } else {
                    nfsstat3::NFS3ERR_IO
                }
            };

            // Try to get current directory attributes for wcc_data
            let dir_after = filesystem.getattr(&args.dir.0).await.ok().map(|attr| NfsMessage::fsal_to_fattr3(&attr));

            create_remove_response(xid, status, dir_after)
        }
    }
}

/// Create REMOVE response
fn create_remove_response(
    xid: u32,
    status: nfsstat3,
    dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. wcc_data (dir_wcc)
    // wcc_data = pre_op_attr + post_op_attr

    // 2.1 pre_op_attr (before the operation) - we don't track this, so send FALSE
    false.pack(&mut buf)?; // pre_op_attr: attributes_follow = FALSE

    // 2.2 post_op_attr (after the operation)
    match dir_attr {
        Some(attr) => {
            true.pack(&mut buf)?; // post_op_attr: attributes_follow = TRUE
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?; // post_op_attr: attributes_follow = FALSE
        }
    }

    let res_data = BytesMut::from(&buf[..]);

    debug!(
        "REMOVE response: status={:?}, response size: {} bytes",
        status,
        res_data.len()
    );

    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::local::LocalFilesystem;
    use std::fs;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_remove_file() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_remove");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create test file
        let test_file = test_dir.join("test_file.txt");
        fs::write(&test_file, "test content").unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_remove".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create REMOVE3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // name (filename3)
        let filename = crate::protocol::v3::nfs::filename3("test_file.txt".to_string());
        filename.pack(&mut args_buf).unwrap();

        // Verify file exists before removal
        assert!(test_file.exists());

        // Call REMOVE
        let result = handle_remove(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "REMOVE should succeed");

        // Verify file was removed
        assert!(!test_file.exists(), "File should be removed");

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_remove_nonexistent_file() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_remove_nonexistent");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create filesystem (file does NOT exist)
        let fs = LocalFilesystem::new("/tmp/nfs_test_remove_nonexistent".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create REMOVE3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // name (filename3) - nonexistent file
        let filename = crate::protocol::v3::nfs::filename3("does_not_exist.txt".to_string());
        filename.pack(&mut args_buf).unwrap();

        // Call REMOVE - should fail with NOENT
        let result = handle_remove(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "REMOVE should return response (not crash)");

        // TODO: Parse response and verify status is NFS3ERR_NOENT

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }
}
