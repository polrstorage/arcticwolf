// NFS PATHCONF Procedure (20)
//
// Get filesystem path configuration information

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;
use xdr_codec::Pack;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{fattr3, nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS PATHCONF request
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized PATHCONF3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with PATHCONF3res
pub async fn handle_pathconf(xid: u32, args_data: &[u8], filesystem: &dyn Filesystem) -> Result<BytesMut> {
    debug!("NFS PATHCONF: xid={}", xid);

    // Parse arguments - just a file handle
    // PATHCONF3args { fhandle3 object; }
    let mut cursor = std::io::Cursor::new(args_data);
    use xdr_codec::Unpack;
    let (object, _) = crate::protocol::v3::nfs::fhandle3::unpack(&mut cursor)?;

    debug!("  object handle: {} bytes", object.0.len());

    // Get file attributes
    let obj_attrs = match filesystem.getattr(&object.0).await {
        Ok(attr) => NfsMessage::fsal_to_fattr3(&attr),
        Err(e) => {
            debug!("PATHCONF failed: {}", e);
            return create_pathconf_error(xid, nfsstat3::NFS3ERR_STALE);
        }
    };

    // Create PATHCONF response with typical Unix values
    let response = create_pathconf_ok(
        obj_attrs,
        255,    // linkmax - maximum number of hard links
        255,    // name_max - maximum filename length
        true,   // no_trunc - server will reject names longer than name_max
        true,   // chown_restricted - only privileged user can change file ownership
        false,  // case_insensitive - filenames are case-sensitive
        true,   // case_preserving - filenames preserve case
    )?;

    debug!("PATHCONF OK: response size: {} bytes", response.len());

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, response)
}

/// Create PATHCONF OK response
fn create_pathconf_ok(
    obj_attributes: fattr3,
    linkmax: u32,
    name_max: u32,
    no_trunc: bool,
    chown_restricted: bool,
    case_insensitive: bool,
    case_preserving: bool,
) -> Result<BytesMut> {
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. post_op_attr (obj_attributes)
    // post_op_attr = bool (1 = present) + fattr3 (if present)
    true.pack(&mut buf)?;  // attributes_follow = TRUE
    obj_attributes.pack(&mut buf)?;

    // 3. PATHCONF fields
    linkmax.pack(&mut buf)?;
    name_max.pack(&mut buf)?;
    no_trunc.pack(&mut buf)?;
    chown_restricted.pack(&mut buf)?;
    case_insensitive.pack(&mut buf)?;
    case_preserving.pack(&mut buf)?;

    Ok(BytesMut::from(&buf[..]))
}

/// Create PATHCONF error response
fn create_pathconf_error(xid: u32, status: nfsstat3) -> Result<BytesMut> {
    let mut buf = Vec::new();

    // Status code
    (status as i32).pack(&mut buf)?;

    // post_op_attr with no attributes (bool = false)
    false.pack(&mut buf)?;  // attributes_follow = FALSE

    let res_data = BytesMut::from(&buf[..]);
    RpcMessage::create_success_reply_with_data(xid, res_data)
}
