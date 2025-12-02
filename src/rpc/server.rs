// RPC TCP Server with Record Marking
//
// Implements Sun RPC over TCP with record marking protocol (RFC 5531)

use anyhow::{anyhow, Result};
use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use crate::portmap::Registry;
use crate::protocol::v3::rpc::{rpc_call_msg, RpcMessage};

/// RPC server handling TCP connections with record marking
pub struct RpcServer {
    addr: String,
    registry: Registry,
}

impl RpcServer {
    pub fn new(addr: String, registry: Registry) -> Self {
        Self { addr, registry }
    }

    pub async fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.addr).await?;
        info!("RPC server listening on {}", self.addr);

        loop {
            let (socket, peer_addr) = listener.accept().await?;
            info!("New connection from {}", peer_addr);

            let registry = self.registry.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, registry).await {
                    error!("Connection error from {}: {}", peer_addr, e);
                }
            });
        }
    }
}

/// Handle a single TCP connection
async fn handle_connection(mut socket: TcpStream, registry: Registry) -> Result<()> {
    let mut buffer = BytesMut::with_capacity(8192);

    loop {
        // Read record marking fragment header (4 bytes)
        let mut header = [0u8; 4];
        if socket.read_exact(&mut header).await.is_err() {
            debug!("Connection closed by peer");
            break;
        }

        // Parse record marking header
        // Bit 31: last fragment (1 = last, 0 = more fragments)
        // Bits 0-30: fragment length
        let header_u32 = u32::from_be_bytes(header);
        let is_last = (header_u32 & 0x80000000) != 0;
        let fragment_len = (header_u32 & 0x7FFFFFFF) as usize;

        debug!(
            "Record marking: last={}, length={}",
            is_last, fragment_len
        );

        // Read fragment data
        let mut fragment = vec![0u8; fragment_len];
        socket.read_exact(&mut fragment).await?;
        buffer.put_slice(&fragment);

        // If this is the last fragment, process the complete RPC message
        if is_last {
            debug!("Complete RPC message received ({} bytes)", buffer.len());

            match handle_rpc_message(&buffer, &registry).await {
                Ok(response) => {
                    // Send response with record marking
                    let response_len = response.len() as u32;
                    let record_header = response_len | 0x80000000; // Set last fragment bit

                    socket.write_u32(record_header).await?;
                    socket.write_all(&response).await?;
                    socket.flush().await?;

                    debug!("Sent response ({} bytes)", response.len());
                }
                Err(e) => {
                    error!("Failed to handle RPC message: {}", e);
                    // TODO: Send error response
                }
            }

            // Clear buffer for next message
            buffer.clear();
        }
    }

    Ok(())
}

/// Handle a complete RPC message
async fn handle_rpc_message(data: &[u8], registry: &Registry) -> Result<BytesMut> {
    // Deserialize RPC call header
    let call = RpcMessage::deserialize_call(data)?;

    debug!(
        "RPC call: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Calculate where procedure arguments start (after RPC call header)
    // RPC call header size: xid(4) + rpcvers(4) + prog(4) + vers(4) + proc(4) + cred + verf
    // For AUTH_NONE: cred(4+4) + verf(4+4) = 16 bytes
    // Total: 20 + 16 = 36 bytes for NULL auth
    let args_offset = 36; // TODO: Parse cred/verf length dynamically
    let args_data = if data.len() > args_offset {
        &data[args_offset..]
    } else {
        &[]
    };

    // Route to appropriate handler based on program number
    match call.prog {
        100000 => {
            // Portmapper protocol (program 100000)
            debug!("Routing to PORTMAP protocol handler");
            crate::portmap::handle_portmap_call(&call, args_data, registry)
        }
        100005 => {
            // MOUNT protocol (program 100005)
            debug!("Routing to MOUNT protocol handler");
            crate::mount::handle_mount_call(&call, args_data)
        }
        100003 => {
            // NFS protocol (program 100003)
            debug!("Routing to NFS protocol handler");
            // TODO: Implement NFS handler
            warn!("NFS protocol not yet implemented");
            Err(anyhow!("NFS protocol not yet implemented"))
        }
        _ => {
            warn!("Unknown program number: {}", call.prog);
            Err(anyhow!("Unknown program number: {}", call.prog))
        }
    }
}
