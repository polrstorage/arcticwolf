// NFS RENAME Procedure (14)
//
// Rename a file or directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS RENAME request
///
/// Renames or moves a file/directory from one location to another.
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized RENAME3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with RENAME3res
pub async fn handle_rename(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS RENAME: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_rename3args(args_data)?;

    debug!(
        "  from: dir {} bytes, name {}",
        args.from_dir.0.len(),
        args.from_name.0
    );
    debug!(
        "  to: dir {} bytes, name {}",
        args.to_dir.0.len(),
        args.to_name.0
    );

    // Get source directory attributes before operation (for wcc_data)
    let fromdir_before = filesystem.getattr(&args.from_dir.0).await.ok();

    // Get target directory attributes before operation (for wcc_data)
    // Only if different from source directory
    let todir_before = if args.from_dir.0 == args.to_dir.0 {
        None  // Same directory, use fromdir_before
    } else {
        filesystem.getattr(&args.to_dir.0).await.ok()
    };

    // Perform rename operation
    match filesystem.rename(
        &args.from_dir.0,
        &args.from_name.0,
        &args.to_dir.0,
        &args.to_name.0,
    ).await {
        Ok(()) => {
            debug!(
                "RENAME OK: '{}' -> '{}'",
                args.from_name.0, args.to_name.0
            );

            // Get source directory attributes after operation
            let fromdir_after = match filesystem.getattr(&args.from_dir.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get source dir attributes after rename: {}", e);
                    None
                }
            };

            // Get target directory attributes after operation
            let todir_after = if args.from_dir.0 == args.to_dir.0 {
                fromdir_after.clone()  // Same directory
            } else {
                match filesystem.getattr(&args.to_dir.0).await {
                    Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                    Err(e) => {
                        warn!("Failed to get target dir attributes after rename: {}", e);
                        None
                    }
                }
            };

            create_rename_response(xid, nfsstat3::NFS3_OK, fromdir_after, todir_after)
        }
        Err(e) => {
            warn!("RENAME failed for '{}': {}", args.from_name.0, e);

            // Determine appropriate error code
            let error_string = e.to_string();
            let status = if error_string.contains("not found") || error_string.contains("No such") {
                nfsstat3::NFS3ERR_NOENT
            } else if error_string.contains("already exists") || error_string.contains("File exists") {
                nfsstat3::NFS3ERR_EXIST
            } else if error_string.contains("permission") || error_string.contains("Permission") {
                nfsstat3::NFS3ERR_ACCES
            } else if error_string.contains("not a directory") || error_string.contains("Not a directory") {
                nfsstat3::NFS3ERR_NOTDIR
            } else if error_string.contains("is a directory") || error_string.contains("Is a directory") {
                nfsstat3::NFS3ERR_ISDIR
            } else if error_string.contains("not empty") || error_string.contains("Directory not empty") {
                nfsstat3::NFS3ERR_NOTEMPTY
            } else if error_string.contains("cross-device") || error_string.contains("Invalid cross-device") {
                nfsstat3::NFS3ERR_XDEV
            } else {
                // Try to get std::io::Error from anyhow::Error
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    match io_err.kind() {
                        std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
                        std::io::ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
                        std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
                        _ => nfsstat3::NFS3ERR_IO,
                    }
                } else {
                    nfsstat3::NFS3ERR_IO
                }
            };

            // Try to get current directory attributes for wcc_data
            let fromdir_after = filesystem.getattr(&args.from_dir.0).await.ok().map(|attr| NfsMessage::fsal_to_fattr3(&attr));
            let todir_after = if args.from_dir.0 == args.to_dir.0 {
                fromdir_after.clone()
            } else {
                filesystem.getattr(&args.to_dir.0).await.ok().map(|attr| NfsMessage::fsal_to_fattr3(&attr))
            };

            create_rename_response(xid, status, fromdir_after, todir_after)
        }
    }
}

/// Create RENAME response
fn create_rename_response(
    xid: u32,
    status: nfsstat3,
    fromdir_attr: Option<crate::protocol::v3::nfs::fattr3>,
    todir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. wcc_data for source directory (fromdir_wcc)
    // wcc_data = pre_op_attr + post_op_attr

    // 2.1 pre_op_attr (before the operation) - we don't track this, so send FALSE
    false.pack(&mut buf)?;

    // 2.2 post_op_attr (after the operation)
    match &fromdir_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    // 3. wcc_data for target directory (todir_wcc)
    // wcc_data = pre_op_attr + post_op_attr

    // 3.1 pre_op_attr (before the operation) - we don't track this, so send FALSE
    false.pack(&mut buf)?;

    // 3.2 post_op_attr (after the operation)
    match &todir_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    let res_data = BytesMut::from(&buf[..]);

    debug!(
        "RENAME response: status={:?}, response size: {} bytes",
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
    use std::io::Write;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_rename_file() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rename");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create a test file
        let mut file = fs::File::create(test_dir.join("oldname.txt")).unwrap();
        file.write_all(b"test content").unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_rename".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create RENAME3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // from_dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // from_name (filename3)
        let from_name = crate::protocol::v3::nfs::filename3("oldname.txt".to_string());
        from_name.pack(&mut args_buf).unwrap();

        // to_dir (fhandle3) - same directory
        fhandle.pack(&mut args_buf).unwrap();

        // to_name (filename3)
        let to_name = crate::protocol::v3::nfs::filename3("newname.txt".to_string());
        to_name.pack(&mut args_buf).unwrap();

        // Call RENAME
        let result = handle_rename(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "RENAME should succeed");

        // Verify file was renamed
        assert!(!test_dir.join("oldname.txt").exists(), "Old file should not exist");
        assert!(test_dir.join("newname.txt").exists(), "New file should exist");

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_rename_directory() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rename_dir");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create a test subdirectory
        fs::create_dir(test_dir.join("olddir")).unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_rename_dir".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create RENAME3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        let from_name = crate::protocol::v3::nfs::filename3("olddir".to_string());
        from_name.pack(&mut args_buf).unwrap();

        fhandle.pack(&mut args_buf).unwrap();

        let to_name = crate::protocol::v3::nfs::filename3("newdir".to_string());
        to_name.pack(&mut args_buf).unwrap();

        // Call RENAME
        let result = handle_rename(12346, &args_buf, &fs).await;
        assert!(result.is_ok(), "RENAME should succeed");

        // Verify directory was renamed
        assert!(!test_dir.join("olddir").exists(), "Old directory should not exist");
        assert!(test_dir.join("newdir").exists(), "New directory should exist");

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }
}
