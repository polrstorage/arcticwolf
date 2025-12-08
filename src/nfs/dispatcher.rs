// NFS Procedure Dispatcher
//
// Routes incoming NFS RPC calls to the appropriate procedure handler

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::Filesystem;
use crate::protocol::v3::rpc::rpc_call_msg;

use super::{access, commit, create, fsinfo, fsstat, getattr, link, lookup, mkdir, null, pathconf, read, readdir, readdirplus, readlink, remove, rename, rmdir, setattr, symlink, write};

/// Dispatch NFS procedure call to appropriate handler
///
/// # Arguments
/// * `call` - Parsed RPC call message
/// * `args_data` - Procedure arguments data
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message
pub fn dispatch(
    call: &rpc_call_msg,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    let procedure = call.proc_;
    let xid = call.xid;

    debug!(
        "NFS dispatcher: procedure={}, xid={}, version={}",
        procedure, xid, call.vers
    );

    // Verify NFS version
    if call.vers != 3 {
        warn!("Unsupported NFS version: {}", call.vers);
        return Err(anyhow!("NFS version {} not supported", call.vers));
    }

    // Dispatch based on procedure number
    match procedure {
        0 => {
            // NULL - test procedure
            null::handle_null(xid)
        }
        1 => {
            // GETATTR - get file attributes
            getattr::handle_getattr(xid, args_data, filesystem)
        }
        2 => {
            // SETATTR - set file attributes
            setattr::handle_setattr(xid, args_data, filesystem)
        }
        3 => {
            // LOOKUP - lookup filename
            lookup::handle_lookup(xid, args_data, filesystem)
        }
        4 => {
            // ACCESS - check file access permissions
            access::handle_access(xid, args_data, filesystem)
        }
        5 => {
            // READLINK - read symbolic link
            readlink::handle_readlink(xid, args_data, filesystem)
        }
        6 => {
            // READ - read from file
            read::handle_read(xid, args_data, filesystem)
        }
        16 => {
            // READDIR - read directory entries
            readdir::handle_readdir(xid, args_data, filesystem)
        }
        18 => {
            // FSSTAT - get filesystem statistics
            fsstat::handle_fsstat(xid, args_data, filesystem)
        }
        19 => {
            // FSINFO - get filesystem information
            fsinfo::handle_fsinfo(xid, args_data, filesystem)
        }
        20 => {
            // PATHCONF - get filesystem path configuration
            pathconf::handle_pathconf(xid, args_data, filesystem)
        }
        17 => {
            // READDIRPLUS - read directory entries with attributes
            readdirplus::handle_readdirplus(xid, args_data, filesystem)
        }
        7 => {
            // WRITE - write to file
            write::handle_write(xid, args_data, filesystem)
        }
        8 => {
            // CREATE - create file
            create::handle_create(xid, args_data, filesystem)
        }
        9 => {
            // MKDIR - create directory
            mkdir::handle_mkdir(xid, args_data, filesystem)
        }
        10 => {
            // SYMLINK - create symbolic link
            symlink::handle_symlink(xid, args_data, filesystem)
        }
        12 => {
            // REMOVE - remove file
            remove::handle_remove(xid, args_data, filesystem)
        }
        13 => {
            // RMDIR - remove directory
            rmdir::handle_rmdir(xid, args_data, filesystem)
        }
        14 => {
            // RENAME - rename file or directory
            rename::handle_rename(xid, args_data, filesystem)
        }
        15 => {
            // LINK - create hard link
            link::handle_link(xid, args_data, filesystem)
        }
        21 => {
            // COMMIT - commit cached writes to stable storage
            commit::handle_commit(xid, args_data, filesystem)
        }
        _ => {
            warn!("Unknown NFS procedure: {}", procedure);
            create_notsupp_response(xid)
        }
    }
}

/// Create a NFS3ERR_NOTSUPP error response
fn create_notsupp_response(xid: u32) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();
    (crate::protocol::v3::nfs::nfsstat3::NFS3ERR_NOTSUPP as i32).pack(&mut buf)?;
    let res_data = BytesMut::from(&buf[..]);
    crate::protocol::v3::rpc::RpcMessage::create_success_reply_with_data(xid, res_data)
}
