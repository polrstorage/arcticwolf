// NFS ACCESS Procedure (Procedure 4)
//
// Checks file access permissions

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

// Access mode bits (from RFC 1813)
const ACCESS3_READ: u32 = 0x0001;
const ACCESS3_LOOKUP: u32 = 0x0002;
const ACCESS3_MODIFY: u32 = 0x0004;
const ACCESS3_EXTEND: u32 = 0x0008;
const ACCESS3_DELETE: u32 = 0x0010;
const ACCESS3_EXECUTE: u32 = 0x0020;

/// Handle NFS ACCESS procedure (procedure 4)
///
/// Determines the access rights that a user has for a file system object.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized ACCESS3args (file handle + access bits)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with granted access rights
pub async fn handle_access(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS ACCESS called (xid={})", xid);

    // Deserialize arguments (file handle + requested access)
    let args = NfsMessage::deserialize_access3args(args_data)?;

    debug!(
        "ACCESS: file_handle={} bytes, requested_access={:#06x}",
        args.object.0.len(),
        args.access
    );

    // Get file attributes to check type and permissions
    let file_attrs = match filesystem.getattr(&args.object.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("ACCESS failed: {}", e);
            // Return appropriate NFS error
            let error_status = if e.to_string().contains("not found")
                || e.to_string().contains("Invalid handle")
            {
                nfsstat3::NFS3ERR_STALE
            } else {
                nfsstat3::NFS3ERR_IO
            };

            // Create ACCESS error response with post_op_attr format
            use xdr_codec::Pack;
            let mut buf = Vec::new();
            (error_status as i32).pack(&mut buf)?;
            false.pack(&mut buf)?;  // attributes_follow = FALSE
            let res_data = BytesMut::from(&buf[..]);
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // For simplicity, grant all requested permissions
    // In a production implementation, this would check actual file permissions
    // against the user's UID/GID from the RPC credentials
    let mut granted_access = 0u32;

    // Check each requested access bit
    if args.access & ACCESS3_READ != 0 {
        granted_access |= ACCESS3_READ;
    }
    if args.access & ACCESS3_LOOKUP != 0 {
        // LOOKUP is only valid for directories
        use crate::fsal::FileType;
        if file_attrs.ftype == FileType::Directory {
            granted_access |= ACCESS3_LOOKUP;
        }
    }
    if args.access & ACCESS3_MODIFY != 0 {
        granted_access |= ACCESS3_MODIFY;
    }
    if args.access & ACCESS3_EXTEND != 0 {
        granted_access |= ACCESS3_EXTEND;
    }
    if args.access & ACCESS3_DELETE != 0 {
        granted_access |= ACCESS3_DELETE;
    }
    if args.access & ACCESS3_EXECUTE != 0 {
        granted_access |= ACCESS3_EXECUTE;
    }

    debug!(
        "ACCESS success: requested={:#06x}, granted={:#06x}",
        args.access, granted_access
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_attrs = NfsMessage::fsal_to_fattr3(&file_attrs);

    // Create successful ACCESS response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. post_op_attr (obj_attributes)
    // post_op_attr = bool (1 = present) + fattr3 (if present)
    true.pack(&mut buf)?;  // attributes_follow = TRUE
    nfs_attrs.pack(&mut buf)?;

    // 3. access (granted permissions)
    granted_access.pack(&mut buf)?;

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
    async fn test_access_file() {
        // Create temp filesystem with a test file
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("access_test.txt");
        fs::write(&test_file, b"test content").unwrap();

        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle and lookup the file
        let root_handle = fs.root_handle().await;
        let file_handle = fs.lookup(&root_handle, "access_test.txt").await.unwrap();

        // Serialize ACCESS3args
        use crate::protocol::v3::nfs::ACCESS3args;
        use xdr_codec::Pack;

        let args = ACCESS3args {
            object: crate::protocol::v3::nfs::fhandle3(file_handle),
            access: ACCESS3_READ | ACCESS3_MODIFY,
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call ACCESS
        let result = handle_access(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "ACCESS should succeed for existing file");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");
    }

    #[tokio::test]
    async fn test_access_directory() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Get root handle (which is a directory)
        let root_handle = fs.root_handle().await;

        // Serialize ACCESS3args with LOOKUP permission
        use crate::protocol::v3::nfs::ACCESS3args;
        use xdr_codec::Pack;

        let args = ACCESS3args {
            object: crate::protocol::v3::nfs::fhandle3(root_handle),
            access: ACCESS3_READ | ACCESS3_LOOKUP,
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call ACCESS
        let result = handle_access(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "ACCESS should succeed for directory");
    }

    #[tokio::test]
    async fn test_access_invalid_handle() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Use invalid file handle
        use crate::protocol::v3::nfs::ACCESS3args;
        use xdr_codec::Pack;

        let args = ACCESS3args {
            object: crate::protocol::v3::nfs::fhandle3(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            access: ACCESS3_READ,
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call ACCESS
        let result = handle_access(12345, &args_buf, fs.as_ref()).await;

        assert!(result.is_ok(), "ACCESS should return error response (not panic)");
    }
}
