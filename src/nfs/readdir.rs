// NFS READDIR Procedure (16)
//
// Read directory entries

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{cookieverf3, fileid3, nfsstat3, NfsMessage, COOKIEVERFSIZE};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS READDIR request
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized READDIR3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with READDIR3res
pub async fn handle_readdir(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS READDIR: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_readdir3args(args_data)?;

    debug!(
        "  dir handle: {} bytes, cookie: {}, count: {}",
        args.dir.0.len(),
        args.cookie,
        args.count
    );

    // Get directory attributes
    let dir_attr = match filesystem.getattr(&args.dir.0).await {
        Ok(attr) => NfsMessage::fsal_to_fattr3(&attr),
        Err(e) => {
            warn!("READDIR failed: getattr error: {}", e);
            let res_data = NfsMessage::create_readdir_error_response(nfsstat3::NFS3ERR_IO)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Read directory entries
    let (entries, eof) = match filesystem.readdir(&args.dir.0, args.cookie, args.count).await {
        Ok(result) => result,
        Err(e) => {
            warn!("READDIR failed: {}", e);
            let res_data = NfsMessage::create_readdir_error_response(nfsstat3::NFS3ERR_IO)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!("  Found {} entries, eof={}", entries.len(), eof);

    // Create READDIR response manually with post_op_attr format
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. post_op_attr (dir_attributes)
    // post_op_attr = bool (1 = present) + fattr3 (if present)
    true.pack(&mut buf)?;  // attributes_follow = TRUE
    dir_attr.pack(&mut buf)?;

    // 3. cookieverf
    let cookieverf = cookieverf3([0u8; COOKIEVERFSIZE as usize]);
    cookieverf.pack(&mut buf)?;

    // 4. dirlist3 (entry list)
    // Serialize each entry with boolean discriminator pattern:
    // For each entry: true + entry3 data (fileid + name + cookie)
    // End of list: false
    let mut cookie_counter = args.cookie;
    for dir_entry in entries.iter() {
        cookie_counter += 1;

        // Boolean discriminator: true = entry follows
        true.pack(&mut buf)?;

        // Serialize entry3 fields directly (without nextentry pointer)
        let fileid = dir_entry.fileid as fileid3;
        fileid.pack(&mut buf)?;

        let name = crate::protocol::v3::nfs::filename3(dir_entry.name.clone());
        name.pack(&mut buf)?;

        cookie_counter.pack(&mut buf)?;
    }

    // End of list: false = no more entries
    false.pack(&mut buf)?;

    // 5. eof
    eof.pack(&mut buf)?;

    let res_data = BytesMut::from(&buf[..]);

    debug!(
        "READDIR OK: {} entries, eof={}, response size: {} bytes",
        entries.len(),
        eof,
        res_data.len()
    );

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}
