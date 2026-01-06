// NFS READDIRPLUS Procedure (17)
//
// Read directory entries with attributes and file handles
// More efficient than READDIR + multiple LOOKUP/GETATTR calls

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{cookieverf3, nfsstat3, NfsMessage, COOKIEVERFSIZE};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS READDIRPLUS request
///
/// READDIRPLUS is an enhanced version of READDIR that returns:
/// - Directory entries (fileid, name, cookie)
/// - File attributes for each entry (post_op_attr)
/// - File handle for each entry (post_op_fh3)
///
/// This reduces round-trips compared to READDIR + multiple LOOKUP/GETATTR.
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized READDIRPLUS3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with READDIRPLUS3res
pub async fn handle_readdirplus(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS READDIRPLUS: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_readdirplus3args(args_data)?;

    debug!(
        "  dir handle: {} bytes, cookie: {}, dircount: {}, maxcount: {}",
        args.dir.0.len(),
        args.cookie,
        args.dircount,
        args.maxcount
    );

    // Get directory attributes
    let dir_attr = match filesystem.getattr(&args.dir.0).await {
        Ok(attr) => NfsMessage::fsal_to_fattr3(&attr),
        Err(e) => {
            warn!("READDIRPLUS failed: getattr error: {}", e);
            let res_data = NfsMessage::create_readdirplus_error_response(nfsstat3::NFS3ERR_IO)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Read directory entries
    // Use dircount as the count parameter (RFC 1813 says dircount is for entry names)
    let (entries, eof) = match filesystem.readdir(&args.dir.0, args.cookie, args.dircount).await {
        Ok(result) => result,
        Err(e) => {
            warn!("READDIRPLUS failed: {}", e);
            let res_data = NfsMessage::create_readdirplus_error_response(nfsstat3::NFS3ERR_IO)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!("  Found {} entries, eof={}", entries.len(), eof);

    // Create READDIRPLUS response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. post_op_attr (dir_attributes)
    // post_op_attr = bool (1 = present) + fattr3 (if present)
    true.pack(&mut buf)?; // attributes_follow = TRUE
    dir_attr.pack(&mut buf)?;

    // 3. cookieverf
    let cookieverf = cookieverf3([0u8; COOKIEVERFSIZE as usize]);
    cookieverf.pack(&mut buf)?;

    // 4. dirlistplus3 (entry list with attributes and handles)
    // Serialize each entry with boolean discriminator pattern:
    // For each entry: true + entryplus3 data
    // entryplus3 = fileid + name + cookie + post_op_attr + post_op_fh3
    // End of list: false
    let mut cookie_counter = args.cookie;
    for dir_entry in entries.iter() {
        cookie_counter += 1;

        // Boolean discriminator: true = entry follows
        true.pack(&mut buf)?;

        // Serialize entryplus3 fields
        let fileid = dir_entry.fileid;
        fileid.pack(&mut buf)?;

        let name = crate::protocol::v3::nfs::filename3(dir_entry.name.clone());
        name.pack(&mut buf)?;

        cookie_counter.pack(&mut buf)?;

        // post_op_attr: Get attributes for this entry
        // We need to lookup the file handle first
        match filesystem.lookup(&args.dir.0, &dir_entry.name).await {
            Ok(entry_handle) => {
                // Get attributes for this entry
                match filesystem.getattr(&entry_handle).await {
                    Ok(entry_attr) => {
                        // post_op_attr: true + fattr3
                        true.pack(&mut buf)?;
                        let fattr = NfsMessage::fsal_to_fattr3(&entry_attr);
                        fattr.pack(&mut buf)?;

                        // post_op_fh3: true + fhandle3
                        true.pack(&mut buf)?;
                        let fhandle = crate::protocol::v3::nfs::fhandle3(entry_handle);
                        fhandle.pack(&mut buf)?;
                    }
                    Err(e) => {
                        // Failed to get attributes - return empty post_op_attr and post_op_fh3
                        warn!("READDIRPLUS: failed to get attributes for {}: {}", dir_entry.name, e);
                        false.pack(&mut buf)?; // post_op_attr: no attributes
                        false.pack(&mut buf)?; // post_op_fh3: no handle
                    }
                }
            }
            Err(e) => {
                // Failed to lookup - return empty post_op_attr and post_op_fh3
                warn!("READDIRPLUS: failed to lookup {}: {}", dir_entry.name, e);
                false.pack(&mut buf)?; // post_op_attr: no attributes
                false.pack(&mut buf)?; // post_op_fh3: no handle
            }
        }
    }

    // End of list: false = no more entries
    false.pack(&mut buf)?;

    // 5. eof
    eof.pack(&mut buf)?;

    let res_data = BytesMut::from(&buf[..]);

    debug!(
        "READDIRPLUS OK: {} entries, eof={}, response size: {} bytes",
        entries.len(),
        eof,
        res_data.len()
    );

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::local::LocalFilesystem;
    use std::fs;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_readdirplus_basic() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_readdirplus");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create some test files
        fs::write(test_dir.join("file1.txt"), "content1").unwrap();
        fs::write(test_dir.join("file2.txt"), "content2").unwrap();
        fs::create_dir(test_dir.join("subdir")).unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_readdirplus".to_string()).unwrap();

        // Get root handle
        let root_handle = fs.root_handle().await;

        // Create READDIRPLUS3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // cookie
        0u64.pack(&mut args_buf).unwrap();

        // cookieverf
        let cookieverf = cookieverf3([0u8; COOKIEVERFSIZE as usize]);
        cookieverf.pack(&mut args_buf).unwrap();

        // dircount
        8192u32.pack(&mut args_buf).unwrap();

        // maxcount
        32768u32.pack(&mut args_buf).unwrap();

        // Call handler
        let result = handle_readdirplus(1, &args_buf, &fs).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        assert!(response.len() > 100); // Should contain multiple entries with attributes

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }
}
