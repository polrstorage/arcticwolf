// NFS LOOKUP Procedure (Procedure 3)
//
// Looks up a filename in a directory and returns its file handle

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{NfsMessage, nfsstat3};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS LOOKUP procedure (procedure 3)
///
/// Looks up a filename in a directory and returns the file handle and attributes
/// for the named entry.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized LOOKUP3args (directory handle + filename)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with file handle and attributes
pub async fn handle_lookup(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS LOOKUP called (xid={})", xid);

    // Deserialize arguments (directory handle + filename)
    let args = NfsMessage::deserialize_lookup3args(args_data)?;

    // filename3 is a newtype wrapper around String
    let name = &args.name.0;

    debug!(
        "LOOKUP: dir_handle={} bytes, name={}",
        args.what_dir.0.len(),
        name
    );

    // Look up the file in the directory
    let file_handle = match filesystem.lookup(&args.what_dir.0, name).await {
        Ok(handle) => handle,
        Err(e) => {
            debug!("LOOKUP failed: {}", e);
            // Return appropriate NFS error
            let error_status = if e.to_string().contains("not found") {
                nfsstat3::NFS3ERR_NOENT
            } else if e.to_string().contains("Invalid filename") {
                nfsstat3::NFS3ERR_INVAL
            } else if e.to_string().contains("Not a directory") {
                nfsstat3::NFS3ERR_NOTDIR
            } else {
                nfsstat3::NFS3ERR_IO
            };

            let res_data = NfsMessage::create_lookup_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get attributes for the found file
    let obj_attrs = match filesystem.getattr(&file_handle).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("LOOKUP: failed to get attributes for found file: {}", e);
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_lookup_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get attributes for the directory (optional but recommended)
    let dir_attrs = match filesystem.getattr(&args.what_dir.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("LOOKUP: failed to get directory attributes: {}", e);
            // We can still return success even if dir attrs fail
            // For now, use the obj_attrs as a placeholder
            // TODO: Make dir_attributes optional in response
            obj_attrs.clone()
        }
    };

    debug!(
        "LOOKUP success: {} -> handle, type={:?}, size={}",
        name, obj_attrs.ftype, obj_attrs.size
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_obj_attrs = NfsMessage::fsal_to_fattr3(&obj_attrs);
    let nfs_dir_attrs = NfsMessage::fsal_to_fattr3(&dir_attrs);

    // Wrap file_handle in fhandle3 (newtype wrapper)
    use crate::protocol::v3::nfs::fhandle3;
    let nfs_handle = fhandle3(file_handle);

    // Create LOOKUP response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. file handle (fhandle3)
    nfs_handle.pack(&mut buf)?;

    // 3. post_op_attr (obj_attributes)
    true.pack(&mut buf)?;  // attributes_follow = TRUE
    nfs_obj_attrs.pack(&mut buf)?;

    // 4. post_op_attr (dir_attributes)
    true.pack(&mut buf)?;  // attributes_follow = TRUE
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
    async fn test_lookup_existing_file() {
        // Create temp filesystem with a test file
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("testfile.txt");
        fs::write(&test_file, b"hello world").unwrap();

        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Serialize LOOKUP3args
        use crate::protocol::v3::nfs::{LOOKUP3args, filename3, fhandle3};
        use xdr_codec::Pack;

        let args = LOOKUP3args {
            what_dir: fhandle3(root_handle.clone()),
            name: filename3("testfile.txt".to_string()),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call LOOKUP
        let result = handle_lookup(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "LOOKUP should succeed for existing file");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");
    }

    #[tokio::test]
    async fn test_lookup_nonexistent_file() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Serialize LOOKUP3args for non-existent file
        use crate::protocol::v3::nfs::{LOOKUP3args, filename3, fhandle3};
        use xdr_codec::Pack;

        let args = LOOKUP3args {
            what_dir: fhandle3(root_handle.clone()),
            name: filename3("nonexistent.txt".to_string()),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call LOOKUP
        let result = handle_lookup(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "LOOKUP should return error response (not panic)");
    }
}
