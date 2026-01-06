// NFS WRITE Procedure (Procedure 7)
//
// Writes data to a file

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS WRITE procedure (procedure 7)
///
/// Writes data to a file at a specified offset.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized WRITE3args (file handle + offset + count + stable + data)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with write status
pub async fn handle_write(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS WRITE called (xid={})", xid);

    // Deserialize arguments
    let args = NfsMessage::deserialize_write3args(args_data)?;

    debug!(
        "WRITE: file_handle={} bytes, offset={}, count={}, stable={:?}",
        args.file.0.len(),
        args.offset,
        args.count,
        args.stable
    );

    // Get file attributes before write (for wcc_data)
    let before_attrs = filesystem.getattr(&args.file.0).await.ok();

    // Write data to the file
    let bytes_written = match filesystem.write(&args.file.0, args.offset, &args.data).await {
        Ok(count) => count,
        Err(e) => {
            debug!("WRITE failed: {}", e);
            // Return appropriate NFS error
            let error_status = if e.to_string().contains("not found")
                || e.to_string().contains("Invalid handle")
            {
                nfsstat3::NFS3ERR_STALE
            } else if e.to_string().contains("Not a file") {
                nfsstat3::NFS3ERR_ISDIR
            } else if e.to_string().contains("Permission denied") {
                nfsstat3::NFS3ERR_ACCES
            } else if e.to_string().contains("No space") {
                nfsstat3::NFS3ERR_NOSPC
            } else if e.to_string().contains("Read-only") {
                nfsstat3::NFS3ERR_ROFS
            } else {
                nfsstat3::NFS3ERR_IO
            };

            let res_data = NfsMessage::create_write_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get file attributes after write (for wcc_data)
    let after_attrs = match filesystem.getattr(&args.file.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("WRITE: failed to get file attributes after write: {}", e);
            // Still return error even if write succeeded but can't get attrs
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_write_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!(
        "WRITE success: wrote {} bytes (requested {})",
        bytes_written, args.count
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_after_attrs = NfsMessage::fsal_to_fattr3(&after_attrs);

    // Create WRITE response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. file_wcc: wcc_data (weak cache consistency data)
    // For simplicity, we only provide post_op_attr (after)
    // pre_op_attr (before) is optional and set to FALSE
    false.pack(&mut buf)?; // pre_op_attr = FALSE (no before attributes)

    // post_op_attr (after attributes)
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_after_attrs.pack(&mut buf)?;

    // 3. count (bytes written)
    bytes_written.pack(&mut buf)?;

    // 4. committed (stable_how) - return same as requested
    // For simplicity, always return FILE_SYNC (2) to indicate data is committed
    let committed = 2i32; // FILE_SYNC
    committed.pack(&mut buf)?;

    // 5. writeverf3 (write verifier) - 8 bytes
    // This is used to detect server reboots between unstable writes and COMMIT
    // For now, use a constant verifier (in production, use server boot time)
    let verf: [u8; 8] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
    buf.extend_from_slice(&verf);

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
    async fn test_write_file() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("writetest.txt");
        fs::write(&test_file, b"").unwrap();

        // Get file handle
        let root_handle = fs.root_handle().await;
        let file_handle = fs.lookup(&root_handle, "writetest.txt").await.unwrap();

        // Serialize WRITE3args
        use crate::protocol::v3::nfs::{fhandle3, stable_how, WRITE3args};
        use xdr_codec::Pack;

        let test_data = b"Hello, NFS World!";
        let args = WRITE3args {
            file: fhandle3(file_handle),
            offset: 0,
            count: test_data.len() as u32,
            stable: stable_how::FILE_SYNC,
            data: test_data.to_vec(),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call WRITE
        let result = handle_write(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "WRITE should succeed");

        // Verify file content
        let content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "Hello, NFS World!");
    }

    #[tokio::test]
    async fn test_write_with_offset() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Create a test file with initial content
        let test_file = temp_dir.path().join("offset.txt");
        fs::write(&test_file, b"0123456789").unwrap();

        // Get file handle
        let root_handle = fs.root_handle().await;
        let file_handle = fs.lookup(&root_handle, "offset.txt").await.unwrap();

        // Write at offset 5
        use crate::protocol::v3::nfs::{fhandle3, stable_how, WRITE3args};
        use xdr_codec::Pack;

        let test_data = b"ABCDE";
        let args = WRITE3args {
            file: fhandle3(file_handle),
            offset: 5,
            count: test_data.len() as u32,
            stable: stable_how::FILE_SYNC,
            data: test_data.to_vec(),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call WRITE
        let result = handle_write(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "WRITE with offset should succeed");

        // Verify file content
        let content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "01234ABCDE");
    }

    #[tokio::test]
    async fn test_write_nonexistent_handle() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Use invalid file handle
        use crate::protocol::v3::nfs::{fhandle3, stable_how, WRITE3args};
        use xdr_codec::Pack;

        let test_data = b"test";
        let args = WRITE3args {
            file: fhandle3(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            offset: 0,
            count: test_data.len() as u32,
            stable: stable_how::FILE_SYNC,
            data: test_data.to_vec(),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call WRITE
        let result = handle_write(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "WRITE should return error response (not panic)");
    }
}
