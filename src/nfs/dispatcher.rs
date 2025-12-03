// NFS Procedure Dispatcher
//
// Routes incoming NFS RPC calls to the appropriate procedure handler

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::protocol::v3::rpc::rpc_call_msg;

use super::null;

/// Dispatch NFS procedure call to appropriate handler
///
/// # Arguments
/// * `call` - Parsed RPC call message
///
/// # Returns
/// Serialized RPC reply message
pub fn dispatch(call: &rpc_call_msg) -> Result<BytesMut> {
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
            warn!("NFS GETATTR not yet implemented");
            Err(anyhow!("GETATTR not implemented"))
        }
        3 => {
            // LOOKUP - lookup filename
            warn!("NFS LOOKUP not yet implemented");
            Err(anyhow!("LOOKUP not implemented"))
        }
        6 => {
            // READ - read from file
            warn!("NFS READ not yet implemented");
            Err(anyhow!("READ not implemented"))
        }
        7 => {
            // WRITE - write to file
            warn!("NFS WRITE not yet implemented");
            Err(anyhow!("WRITE not implemented"))
        }
        8 => {
            // CREATE - create file
            warn!("NFS CREATE not yet implemented");
            Err(anyhow!("CREATE not implemented"))
        }
        9 => {
            // MKDIR - create directory
            warn!("NFS MKDIR not yet implemented");
            Err(anyhow!("MKDIR not implemented"))
        }
        _ => {
            warn!("Unknown NFS procedure: {}", procedure);
            Err(anyhow!("Unknown procedure: {}", procedure))
        }
    }
}
