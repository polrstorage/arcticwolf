// NFS RMDIR Procedure (13)
//
// Remove a directory from a parent directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS RMDIR request
///
/// Removes a directory from the specified parent directory. The directory
/// must be empty to be removed successfully.
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized RMDIR3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with RMDIR3res
pub async fn handle_rmdir(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS RMDIR: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_rmdir3args(args_data)?;

    debug!(
        "  parent dir handle: {} bytes, dirname: {}",
        args.dir.0.len(),
        args.name.0
    );

    // Get parent directory attributes before removal (for wcc_data)
    let dir_before = filesystem.getattr(&args.dir.0).await.ok();

    // Perform rmdir operation
    match filesystem.rmdir(&args.dir.0, &args.name.0).await {
        Ok(()) => {
            debug!("RMDIR OK: removed directory '{}'", args.name.0);

            // Get parent directory attributes after removal
            let dir_after = match filesystem.getattr(&args.dir.0).await {
                Ok(attr) => NfsMessage::fsal_to_fattr3(&attr),
                Err(e) => {
                    warn!("Failed to get parent dir attributes after rmdir: {}", e);
                    // Continue anyway, removal succeeded
                    return create_rmdir_response(xid, nfsstat3::NFS3_OK, None);
                }
            };

            create_rmdir_response(xid, nfsstat3::NFS3_OK, Some(dir_after))
        }
        Err(e) => {
            warn!("RMDIR failed for '{}': {}", args.name.0, e);

            // Determine appropriate error code
            let error_string = e.to_string();
            let status = if error_string.contains("not found") || error_string.contains("No such") {
                nfsstat3::NFS3ERR_NOENT
            } else if error_string.contains("permission") || error_string.contains("Permission") {
                nfsstat3::NFS3ERR_ACCES
            } else if error_string.contains("not empty") || error_string.contains("Directory not empty") {
                nfsstat3::NFS3ERR_NOTEMPTY
            } else if error_string.contains("not a directory") || error_string.contains("Not a directory") {
                nfsstat3::NFS3ERR_NOTDIR
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

            // Try to get current parent directory attributes for wcc_data
            let dir_after = filesystem.getattr(&args.dir.0).await.ok().map(|attr| NfsMessage::fsal_to_fattr3(&attr));

            create_rmdir_response(xid, status, dir_after)
        }
    }
}

/// Create RMDIR response
fn create_rmdir_response(
    xid: u32,
    status: nfsstat3,
    dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. wcc_data (parent directory)
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
        "RMDIR response: status={:?}, response size: {} bytes",
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
    async fn test_rmdir() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rmdir");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create an empty directory to remove
        let target_dir = test_dir.join("emptydir");
        fs::create_dir(&target_dir).unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_rmdir".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create RMDIR3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // name (filename3)
        let dirname = crate::protocol::v3::nfs::filename3("emptydir".to_string());
        dirname.pack(&mut args_buf).unwrap();

        // Verify directory exists before removal
        assert!(target_dir.exists());

        // Call RMDIR
        let result = handle_rmdir(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "RMDIR should succeed");

        // Verify directory was removed
        assert!(!target_dir.exists(), "Directory should be removed");

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_rmdir_nonexistent() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rmdir_nonexistent");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create filesystem (directory does NOT exist)
        let fs = LocalFilesystem::new("/tmp/nfs_test_rmdir_nonexistent".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create RMDIR3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        let dirname = crate::protocol::v3::nfs::filename3("does_not_exist".to_string());
        dirname.pack(&mut args_buf).unwrap();

        // Call RMDIR - should fail with NOENT
        let result = handle_rmdir(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "RMDIR should return response (not crash)");

        // TODO: Parse response and verify status is NFS3ERR_NOENT

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_rmdir_not_empty() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rmdir_notempty");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create a non-empty directory
        let target_dir = test_dir.join("nonemptydir");
        fs::create_dir(&target_dir).unwrap();
        fs::write(target_dir.join("somefile.txt"), "data").unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_rmdir_notempty".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create RMDIR3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        let dirname = crate::protocol::v3::nfs::filename3("nonemptydir".to_string());
        dirname.pack(&mut args_buf).unwrap();

        // Call RMDIR - should fail with NOTEMPTY
        let result = handle_rmdir(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "RMDIR should return response (not crash)");

        // Verify directory still exists
        assert!(target_dir.exists(), "Directory should still exist");

        // TODO: Parse response and verify status is NFS3ERR_NOTEMPTY

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }
}
