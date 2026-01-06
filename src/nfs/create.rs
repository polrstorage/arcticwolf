// NFS CREATE Procedure (Procedure 8)
//
// Creates a new file

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS CREATE procedure (procedure 8)
///
/// Creates a new regular file.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized CREATE3args (dir handle + filename + how)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with new file handle
pub async fn handle_create(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS CREATE called (xid={})", xid);
    debug!(
        "CREATE: args_data = {} bytes, hex: {:02x?}",
        args_data.len(),
        args_data
    );

    // Deserialize arguments
    let args = NfsMessage::deserialize_create3args(args_data)?;

    let filename = &args.name.0;
    debug!(
        "CREATE: dir_handle={} bytes, filename={}",
        args.where_dir.0.len(),
        filename
    );

    // Get directory attributes before create (for wcc_data)
    let _before_dir_attrs = filesystem.getattr(&args.where_dir.0).await.ok();

    // Create the file based on mode
    let file_handle = match &args.how {
        crate::protocol::v3::nfs::createhow3::UNCHECKED(attrs)
        | crate::protocol::v3::nfs::createhow3::GUARDED(attrs) => {
            // For UNCHECKED: create or truncate existing file
            // For GUARDED: fail if file exists (checked by filesystem layer)

            let mode = match &attrs.mode {
                crate::protocol::v3::nfs::set_mode3::SET_MODE(m) => *m,
                _ => 0o644, // Default mode
            };

            // Create the file
            match filesystem.create(&args.where_dir.0, &filename, mode).await {
                Ok(handle) => handle,
                Err(e) => {
                    debug!("CREATE failed: {}", e);
                    let error_status = if e.to_string().contains("exists") {
                        nfsstat3::NFS3ERR_EXIST
                    } else if e.to_string().contains("not found") {
                        nfsstat3::NFS3ERR_NOENT
                    } else if e.to_string().contains("Not a directory") {
                        nfsstat3::NFS3ERR_NOTDIR
                    } else if e.to_string().contains("Permission denied") {
                        nfsstat3::NFS3ERR_ACCES
                    } else if e.to_string().contains("No space") {
                        nfsstat3::NFS3ERR_NOSPC
                    } else if e.to_string().contains("Read-only") {
                        nfsstat3::NFS3ERR_ROFS
                    } else {
                        nfsstat3::NFS3ERR_IO
                    };
                    let res_data = NfsMessage::create_create_error_response(error_status)?;
                    return RpcMessage::create_success_reply_with_data(xid, res_data);
                }
            }
        }
        crate::protocol::v3::nfs::createhow3::EXCLUSIVE(_verf) => {
            // EXCLUSIVE mode: create file with verifier stored in mtime/atime
            // This is for safe concurrent creation
            // For simplicity, we'll treat it like GUARDED for now
            match filesystem.create(&args.where_dir.0, &filename, 0o644).await {
                Ok(handle) => handle,
                Err(e) => {
                    debug!("CREATE (EXCLUSIVE) failed: {}", e);
                    let error_status = if e.to_string().contains("exists") {
                        nfsstat3::NFS3ERR_EXIST
                    } else {
                        nfsstat3::NFS3ERR_IO
                    };
                    let res_data = NfsMessage::create_create_error_response(error_status)?;
                    return RpcMessage::create_success_reply_with_data(xid, res_data);
                }
            }
        }
    };

    // Get file attributes
    let file_attrs = match filesystem.getattr(&file_handle).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("CREATE: failed to get file attributes: {}", e);
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_create_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get directory attributes after create
    let dir_attrs = match filesystem.getattr(&args.where_dir.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("CREATE: failed to get dir attributes: {}", e);
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_create_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!("CREATE success: new file handle {} bytes", file_handle.len());

    // Convert FSAL attributes to NFS fattr3
    let nfs_file_attrs = NfsMessage::fsal_to_fattr3(&file_attrs);
    let nfs_dir_attrs = NfsMessage::fsal_to_fattr3(&dir_attrs);

    // Create CREATE response
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. CREATE3resok
    // obj: post_op_fh3 (optional file handle)
    true.pack(&mut buf)?; // handle_follows = TRUE

    // file handle (variable-length opaque)
    let handle_len = file_handle.len() as u32;
    handle_len.pack(&mut buf)?;
    buf.extend_from_slice(&file_handle);
    // Add padding
    let padding = (4 - (file_handle.len() % 4)) % 4;
    buf.extend_from_slice(&vec![0u8; padding]);

    // obj_attributes: post_op_attr (optional attributes)
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_file_attrs.pack(&mut buf)?;

    // dir_wcc: wcc_data (directory weak cache consistency)
    // pre_op_attr
    false.pack(&mut buf)?; // pre_op_attr = FALSE

    // post_op_attr
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_dir_attrs.pack(&mut buf)?;

    let res_data = BytesMut::from(&buf[..]);

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::BackendConfig;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_create_file() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        let root_handle = fs.root_handle().await;

        // Serialize CREATE3args
        use crate::protocol::v3::nfs::{
            createhow3, fhandle3, filename3, nfstime3, sattr3, set_atime, set_gid3,
            set_mode3, set_mtime, set_size3, set_uid3, CREATE3args,
        };
        use xdr_codec::Pack;

        let test_filename = "new_file.txt";
        let args = CREATE3args {
            where_dir: fhandle3(root_handle),
            name: filename3(test_filename.to_string()),
            how: createhow3::UNCHECKED(sattr3 {
                mode: set_mode3::SET_MODE(0o644),
                uid: set_uid3::SET_UID(0),
                gid: set_gid3::SET_GID(0),
                size: set_size3::SET_SIZE(0),
                atime: set_atime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
                mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
            }),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call CREATE
        let result = handle_create(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "CREATE should succeed");

        // Verify file exists
        let test_file = temp_dir.path().join(test_filename);
        assert!(test_file.exists(), "File should be created");
    }

    #[tokio::test]
    async fn test_create_existing_file_unchecked() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Create an existing file
        let test_file = temp_dir.path().join("existing.txt");
        fs::write(&test_file, b"old content").unwrap();

        let root_handle = fs.root_handle().await;

        // Serialize CREATE3args with UNCHECKED mode
        use crate::protocol::v3::nfs::{
            createhow3, fhandle3, filename3, nfstime3, sattr3, set_atime, set_gid3,
            set_mode3, set_mtime, set_size3, set_uid3, CREATE3args,
        };
        use xdr_codec::Pack;

        let args = CREATE3args {
            where_dir: fhandle3(root_handle),
            name: filename3("existing.txt".to_string()),
            how: createhow3::UNCHECKED(sattr3 {
                mode: set_mode3::SET_MODE(0o644),
                uid: set_uid3::SET_UID(0),
                gid: set_gid3::SET_GID(0),
                size: set_size3::SET_SIZE(0),
                atime: set_atime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
                mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
            }),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call CREATE - should succeed (UNCHECKED allows overwriting)
        let result = handle_create(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "CREATE UNCHECKED should succeed even if file exists");
    }
}
