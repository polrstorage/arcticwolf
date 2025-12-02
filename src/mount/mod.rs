// MOUNT Protocol Handlers
//
// Program: 100005 (MOUNT)
// Version: 3 (MOUNTv3)
//
// This module implements the MOUNT protocol, which is a prerequisite for NFS.
// Clients must first mount a directory path to obtain a file handle before
// they can perform NFS operations.

pub mod mnt;
pub mod null;
pub mod umnt;

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::protocol::v3::rpc::rpc_call_msg;

/// MOUNT program number (RFC 1813)
pub const MOUNT_PROGRAM: u32 = 100005;

/// MOUNT version 3
pub const MOUNT_V3: u32 = 3;

/// MOUNT procedure numbers
pub mod procedures {
    pub const NULL: u32 = 0;
    pub const MNT: u32 = 1;
    pub const DUMP: u32 = 2;
    pub const UMNT: u32 = 3;
    pub const UMNTALL: u32 = 4;
    pub const EXPORT: u32 = 5;
}

/// Dispatch MOUNT procedure call to appropriate handler
///
/// This function routes the RPC call to the correct MOUNT procedure handler
/// based on the procedure number.
pub fn handle_mount_call(call: &rpc_call_msg, args_data: &[u8]) -> Result<BytesMut> {
    debug!(
        "Dispatching MOUNT call: proc={}, prog={}, vers={}",
        call.proc_, call.prog, call.vers
    );

    // Verify this is actually a MOUNT call
    if call.prog != MOUNT_PROGRAM {
        warn!(
            "Expected MOUNT program {}, got {}",
            MOUNT_PROGRAM, call.prog
        );
        return Err(anyhow!(
            "Wrong program number: expected {}, got {}",
            MOUNT_PROGRAM,
            call.prog
        ));
    }

    // Verify version 3
    if call.vers != MOUNT_V3 {
        warn!("Expected MOUNT version {}, got {}", MOUNT_V3, call.vers);
        return Err(anyhow!(
            "Unsupported MOUNT version: expected {}, got {}",
            MOUNT_V3,
            call.vers
        ));
    }

    // Dispatch to handler based on procedure number
    match call.proc_ {
        procedures::NULL => {
            debug!("Routing to MOUNT NULL handler");
            null::handle(call)
        }
        procedures::MNT => {
            debug!("Routing to MOUNT MNT handler");
            mnt::handle(call, args_data)
        }
        procedures::UMNT => {
            debug!("Routing to MOUNT UMNT handler");
            umnt::handle(call, args_data)
        }
        procedures::DUMP => {
            warn!("MOUNT DUMP not yet implemented");
            Err(anyhow!("MOUNT DUMP procedure not implemented"))
        }
        procedures::UMNTALL => {
            warn!("MOUNT UMNTALL not yet implemented");
            Err(anyhow!("MOUNT UMNTALL procedure not implemented"))
        }
        procedures::EXPORT => {
            warn!("MOUNT EXPORT not yet implemented");
            Err(anyhow!("MOUNT EXPORT procedure not implemented"))
        }
        _ => {
            warn!("Unknown MOUNT procedure: {}", call.proc_);
            Err(anyhow!("Unknown MOUNT procedure: {}", call.proc_))
        }
    }
}
