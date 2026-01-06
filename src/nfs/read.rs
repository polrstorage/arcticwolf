// NFS READ Procedure (Procedure 6)
//
// Reads data from a file

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS READ procedure (procedure 6)
///
/// Reads data from a file at a specified offset.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized READ3args (file handle + offset + count)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with file data
pub async fn handle_read(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS READ called (xid={})", xid);

    // Deserialize arguments (file handle + offset + count)
    let args = NfsMessage::deserialize_read3args(args_data)?;

    debug!(
        "READ: file_handle={} bytes, offset={}, count={}",
        args.file.0.len(),
        args.offset,
        args.count
    );

    // Read data from the file
    let data = match filesystem.read(&args.file.0, args.offset, args.count).await {
        Ok(data) => data,
        Err(e) => {
            debug!("READ failed: {}", e);
            // Return appropriate NFS error
            let error_status = if e.to_string().contains("not found")
                || e.to_string().contains("Invalid handle")
            {
                nfsstat3::NFS3ERR_STALE
            } else if e.to_string().contains("Not a file") {
                nfsstat3::NFS3ERR_ISDIR
            } else if e.to_string().contains("Permission denied") {
                nfsstat3::NFS3ERR_ACCES
            } else {
                nfsstat3::NFS3ERR_IO
            };

            let res_data = NfsMessage::create_read_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get file attributes (for the response)
    let file_attrs = match filesystem.getattr(&args.file.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("READ: failed to get file attributes: {}", e);
            // Still return error even if we read successfully but can't get attrs
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_read_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Determine if we've reached end of file
    let bytes_read = data.len() as u32;
    let eof = (args.offset + bytes_read as u64) >= file_attrs.size;

    debug!(
        "READ success: read {} bytes, eof={}",
        bytes_read, eof
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_attrs = NfsMessage::fsal_to_fattr3(&file_attrs);

    // Create READ response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. post_op_attr (file_attributes)
    true.pack(&mut buf)?;  // attributes_follow = TRUE
    nfs_attrs.pack(&mut buf)?;

    // 3. count (bytes read)
    bytes_read.pack(&mut buf)?;

    // 4. eof (end of file)
    eof.pack(&mut buf)?;

    // 5. data (opaque<>) - pack as variable-length opaque data
    // XDR opaque format: length (u32) + data + padding to 4-byte boundary
    let data_len = data.len() as u32;
    data_len.pack(&mut buf)?;
    buf.extend_from_slice(&data);

    // Add padding to align to 4-byte boundary
    let padding = (4 - (data.len() % 4)) % 4;
    for _ in 0..padding {
        buf.push(0);
    }

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
    async fn test_read_file() {
        // Create temp filesystem with a test file
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("readtest.txt");
        let test_content = b"Hello, NFS World! This is a test file.";
        fs::write(&test_file, test_content).unwrap();

        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle and lookup the file
        let root_handle = fs.root_handle().await;
        let file_handle = fs.lookup(&root_handle, "readtest.txt").await.unwrap();

        // Serialize READ3args
        use crate::protocol::v3::nfs::READ3args;
        use xdr_codec::Pack;

        let args = READ3args {
            file: crate::protocol::v3::nfs::fhandle3(file_handle),
            offset: 0,
            count: 100, // Read more than file size to test
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call READ
        let result = handle_read(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "READ should succeed");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");
    }

    #[tokio::test]
    async fn test_read_partial() {
        // Create temp filesystem with a test file
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("partial.txt");
        let test_content = b"0123456789ABCDEFGHIJ"; // 20 bytes
        fs::write(&test_file, test_content).unwrap();

        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get file handle
        let root_handle = fs.root_handle().await;
        let file_handle = fs.lookup(&root_handle, "partial.txt").await.unwrap();

        // Read middle section (offset 5, count 10)
        use crate::protocol::v3::nfs::READ3args;
        use xdr_codec::Pack;

        let args = READ3args {
            file: crate::protocol::v3::nfs::fhandle3(file_handle),
            offset: 5,
            count: 10,
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call READ
        let result = handle_read(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "Partial READ should succeed");
    }

    #[tokio::test]
    async fn test_read_nonexistent_handle() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Use invalid file handle
        use crate::protocol::v3::nfs::READ3args;
        use xdr_codec::Pack;

        let args = READ3args {
            file: crate::protocol::v3::nfs::fhandle3(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            offset: 0,
            count: 100,
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call READ
        let result = handle_read(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "READ should return error response (not panic)");
    }
}
