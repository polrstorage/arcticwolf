// NFS MKDIR Procedure (9)
//
// Create a directory in a parent directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS MKDIR request
///
/// Creates a new directory in the specified parent directory.
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized MKDIR3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with MKDIR3res
pub async fn handle_mkdir(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS MKDIR: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_mkdir3args(args_data)?;

    debug!(
        "  parent dir handle: {} bytes, dirname: {}",
        args.where_dir.0.len(),
        args.name.0
    );

    // Get parent directory attributes before operation (for wcc_data)
    let dir_before = filesystem.getattr(&args.where_dir.0).await.ok();

    // Extract mode from sattr3, default to 0755
    let mode = match args.attributes.mode {
        crate::protocol::v3::nfs::set_mode3::SET_MODE(m) => m,
        crate::protocol::v3::nfs::set_mode3::default => 0o755,
    };

    // Perform mkdir operation
    match filesystem.mkdir(&args.where_dir.0, &args.name.0, mode).await {
        Ok(new_dir_handle) => {
            debug!("MKDIR OK: created directory '{}'", args.name.0);

            // Get new directory attributes
            let new_dir_attr = match filesystem.getattr(&new_dir_handle).await {
                Ok(attr) => NfsMessage::fsal_to_fattr3(&attr),
                Err(e) => {
                    warn!("Failed to get new directory attributes: {}", e);
                    return create_mkdir_response(xid, nfsstat3::NFS3_OK, None, None, None);
                }
            };

            // Get parent directory attributes after operation
            let dir_after = match filesystem.getattr(&args.where_dir.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get parent dir attributes after mkdir: {}", e);
                    None
                }
            };

            create_mkdir_response(
                xid,
                nfsstat3::NFS3_OK,
                Some(new_dir_handle),
                Some(new_dir_attr),
                dir_after,
            )
        }
        Err(e) => {
            warn!("MKDIR failed for '{}': {}", args.name.0, e);

            // Determine appropriate error code
            let error_string = e.to_string();
            let status = if error_string.contains("already exists") || error_string.contains("File exists") {
                nfsstat3::NFS3ERR_EXIST
            } else if error_string.contains("not found") || error_string.contains("No such") {
                nfsstat3::NFS3ERR_NOENT
            } else if error_string.contains("permission") || error_string.contains("Permission") {
                nfsstat3::NFS3ERR_ACCES
            } else {
                // Try to get std::io::Error from anyhow::Error
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    match io_err.kind() {
                        std::io::ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
                        std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
                        std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
                        _ => nfsstat3::NFS3ERR_IO,
                    }
                } else {
                    nfsstat3::NFS3ERR_IO
                }
            };

            // Try to get current parent directory attributes for wcc_data
            let dir_after = filesystem.getattr(&args.where_dir.0).await.ok().map(|attr| NfsMessage::fsal_to_fattr3(&attr));

            create_mkdir_response(xid, status, None, None, dir_after)
        }
    }
}

/// Create MKDIR response
fn create_mkdir_response(
    xid: u32,
    status: nfsstat3,
    new_dir_handle: Option<Vec<u8>>,
    new_dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
    parent_dir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    if status == nfsstat3::NFS3_OK {
        // Success case: post_op_fh3 + post_op_attr + wcc_data

        // 2. post_op_fh3 (new directory handle)
        match new_dir_handle {
            Some(handle) => {
                true.pack(&mut buf)?;  // handle follows
                (handle.len() as u32).pack(&mut buf)?;
                buf.extend_from_slice(&handle);
                // Add padding
                let padding = (4 - (handle.len() % 4)) % 4;
                buf.extend_from_slice(&vec![0u8; padding]);
            }
            None => {
                false.pack(&mut buf)?;  // no handle
            }
        }

        // 3. post_op_attr (new directory attributes)
        match new_dir_attr {
            Some(attr) => {
                true.pack(&mut buf)?;  // attributes follow
                attr.pack(&mut buf)?;
            }
            None => {
                false.pack(&mut buf)?;  // no attributes
            }
        }
    }

    // 4. wcc_data (parent directory)
    // wcc_data = pre_op_attr + post_op_attr

    // 4.1 pre_op_attr (before the operation) - we don't track this, so send FALSE
    false.pack(&mut buf)?;

    // 4.2 post_op_attr (after the operation)
    match parent_dir_attr {
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
        "MKDIR response: status={:?}, response size: {} bytes",
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
    async fn test_mkdir() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_mkdir");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_mkdir".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create MKDIR3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // where_dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // name (filename3)
        let dirname = crate::protocol::v3::nfs::filename3("testdir".to_string());
        dirname.pack(&mut args_buf).unwrap();

        // attributes (sattr3)
        let sattr = crate::protocol::v3::nfs::sattr3 {
            mode: crate::protocol::v3::nfs::set_mode3::SET_MODE(0o755),
            uid: crate::protocol::v3::nfs::set_uid3::SET_UID(0),
            gid: crate::protocol::v3::nfs::set_gid3::SET_GID(0),
            size: crate::protocol::v3::nfs::set_size3::SET_SIZE(0),
            atime: crate::protocol::v3::nfs::set_atime::SET_TO_CLIENT_TIME(
                crate::protocol::v3::nfs::nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                },
            ),
            mtime: crate::protocol::v3::nfs::set_mtime::SET_TO_CLIENT_TIME(
                crate::protocol::v3::nfs::nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                },
            ),
        };
        sattr.pack(&mut args_buf).unwrap();

        // Call MKDIR
        let result = handle_mkdir(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "MKDIR should succeed");

        // Verify directory was created
        let new_dir = test_dir.join("testdir");
        assert!(new_dir.exists(), "Directory should be created");
        assert!(new_dir.is_dir(), "Should be a directory");

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_mkdir_already_exists() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_mkdir_exists");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create the directory beforehand
        fs::create_dir(test_dir.join("existingdir")).unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_mkdir_exists".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create MKDIR3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        let dirname = crate::protocol::v3::nfs::filename3("existingdir".to_string());
        dirname.pack(&mut args_buf).unwrap();

        let sattr = crate::protocol::v3::nfs::sattr3 {
            mode: crate::protocol::v3::nfs::set_mode3::SET_MODE(0o755),
            uid: crate::protocol::v3::nfs::set_uid3::SET_UID(0),
            gid: crate::protocol::v3::nfs::set_gid3::SET_GID(0),
            size: crate::protocol::v3::nfs::set_size3::SET_SIZE(0),
            atime: crate::protocol::v3::nfs::set_atime::SET_TO_CLIENT_TIME(
                crate::protocol::v3::nfs::nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                },
            ),
            mtime: crate::protocol::v3::nfs::set_mtime::SET_TO_CLIENT_TIME(
                crate::protocol::v3::nfs::nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                },
            ),
        };
        sattr.pack(&mut args_buf).unwrap();

        // Call MKDIR
        let result = handle_mkdir(12345, &args_buf, &fs).await;
        assert!(result.is_ok(), "MKDIR should return response (not crash)");

        // TODO: Parse response and verify status is NFS3ERR_EXIST

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }
}
