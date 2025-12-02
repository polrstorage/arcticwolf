// Portmapper Protocol Handlers
//
// Program: 100000 (PORTMAP)
// Version: 2
//
// The portmapper is a service discovery mechanism for RPC services.
// Services register themselves (SET) and clients query for service ports (GETPORT).

pub mod getport;
pub mod null;
pub mod registry;
pub mod set;
pub mod unset;

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::protocol::v3::rpc::rpc_call_msg;
pub use registry::Registry;

/// Portmapper program number (RFC 1833)
pub const PORTMAP_PROGRAM: u32 = 100000;

/// Portmapper version 2
pub const PORTMAP_V2: u32 = 2;

/// Portmapper procedure numbers
pub mod procedures {
    pub const NULL: u32 = 0;
    pub const SET: u32 = 1;
    pub const UNSET: u32 = 2;
    pub const GETPORT: u32 = 3;
    pub const DUMP: u32 = 4;
    pub const CALLIT: u32 = 5;
}

/// Dispatch Portmapper procedure call to appropriate handler
pub fn handle_portmap_call(
    call: &rpc_call_msg,
    args_data: &[u8],
    registry: &Registry,
) -> Result<BytesMut> {
    debug!(
        "Dispatching PORTMAP call: proc={}, prog={}, vers={}",
        call.proc_, call.prog, call.vers
    );

    // Verify this is actually a PORTMAP call
    if call.prog != PORTMAP_PROGRAM {
        warn!(
            "Expected PORTMAP program {}, got {}",
            PORTMAP_PROGRAM, call.prog
        );
        return Err(anyhow!(
            "Wrong program number: expected {}, got {}",
            PORTMAP_PROGRAM,
            call.prog
        ));
    }

    // Verify version 2
    if call.vers != PORTMAP_V2 {
        warn!(
            "Expected PORTMAP version {}, got {}",
            PORTMAP_V2, call.vers
        );
        return Err(anyhow!(
            "Unsupported PORTMAP version: expected {}, got {}",
            PORTMAP_V2,
            call.vers
        ));
    }

    // Dispatch to handler based on procedure number
    match call.proc_ {
        procedures::NULL => {
            debug!("Routing to PORTMAP NULL handler");
            null::handle(call)
        }
        procedures::SET => {
            debug!("Routing to PORTMAP SET handler");
            set::handle(call, args_data, registry)
        }
        procedures::UNSET => {
            debug!("Routing to PORTMAP UNSET handler");
            unset::handle(call, args_data, registry)
        }
        procedures::GETPORT => {
            debug!("Routing to PORTMAP GETPORT handler");
            getport::handle(call, args_data, registry)
        }
        procedures::DUMP => {
            warn!("PORTMAP DUMP not yet implemented");
            Err(anyhow!("PORTMAP DUMP procedure not implemented"))
        }
        procedures::CALLIT => {
            warn!("PORTMAP CALLIT not supported");
            Err(anyhow!("PORTMAP CALLIT procedure not supported"))
        }
        _ => {
            warn!("Unknown PORTMAP procedure: {}", call.proc_);
            Err(anyhow!("Unknown PORTMAP procedure: {}", call.proc_))
        }
    }
}
