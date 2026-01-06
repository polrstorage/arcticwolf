// RPC TCP Server with Record Marking
//
// Implements Sun RPC over TCP with record marking protocol (RFC 5531)

use anyhow::{anyhow, Result};
use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use crate::fsal::Filesystem;
use crate::portmap::Registry;
use crate::protocol::v3::rpc::RpcMessage;

/// RPC server handling TCP connections with record marking
pub struct RpcServer {
    addr: String,
    registry: Registry,
    filesystem: Arc<dyn Filesystem>,
}

impl RpcServer {
    pub fn new(addr: String, registry: Registry, filesystem: Arc<dyn Filesystem>) -> Self {
        Self {
            addr,
            registry,
            filesystem,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.addr).await?;
        info!("RPC server listening on {}", self.addr);

        loop {
            let (socket, peer_addr) = listener.accept().await?;
            info!("New connection from {}", peer_addr);

            let registry = self.registry.clone();
            let filesystem = self.filesystem.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, registry, filesystem).await {
                    error!("Connection error from {}: {}", peer_addr, e);
                }
            });
        }
    }
}

/// Handle a single TCP connection
async fn handle_connection(
    mut socket: TcpStream,
    registry: Registry,
    filesystem: Arc<dyn Filesystem>,
) -> Result<()> {
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

            let response = match handle_rpc_message(&buffer, &registry, filesystem.as_ref()).await {
                Ok(response) => response,
                Err(e) => {
                    error!("Failed to handle RPC message: {}", e);

                    // Try to parse XID from buffer to send proper error response
                    if buffer.len() >= 4 {
                        let xid = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);

                        // Send PROG_UNAVAIL error response
                        match RpcMessage::create_prog_unavail_reply(xid) {
                            Ok(error_response) => {
                                warn!("Sending PROG_UNAVAIL error response for xid={}", xid);
                                error_response
                            }
                            Err(serialize_err) => {
                                error!("Failed to create error response: {}", serialize_err);
                                continue; // Skip this message and wait for next one
                            }
                        }
                    } else {
                        error!("Buffer too short to extract XID");
                        continue; // Skip this message and wait for next one
                    }
                }
            };

            // Send response with record marking
            // IMPORTANT: Record mark and payload must be sent in a single write()
            // to avoid TCP fragmentation causing client parsing issues
            let response_len = response.len() as u32;
            let record_header = response_len | 0x80000000; // Set last fragment bit

            // Combine record mark + payload into single buffer
            let mut full_response = Vec::with_capacity(4 + response.len());
            full_response.extend_from_slice(&record_header.to_be_bytes());
            full_response.extend_from_slice(&response);

            socket.write_all(&full_response).await?;
            socket.flush().await?;

            debug!("Sent response ({} bytes)", response.len());

            // Clear buffer for next message
            buffer.clear();
        }
    }

    Ok(())
}

/// Handle a complete RPC message
async fn handle_rpc_message(
    data: &[u8],
    registry: &Registry,
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    // Debug: dump complete RPC message
    debug!(
        "Complete RPC message ({} bytes): {:02x?}",
        data.len(),
        &data[..data.len().min(100)]
    );

    // Deserialize RPC call header
    let call = RpcMessage::deserialize_call(data)?;

    debug!(
        "RPC call: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // Calculate where procedure arguments start (after RPC call header)
    // RPC call header: xid(4) + mtype(4) + rpcvers(4) + prog(4) + vers(4) + proc(4) = 24 bytes
    // Then: opaque_auth cred + opaque_auth verf (variable length)
    // opaque_auth = flavor(4) + length(4) + body(length bytes, padded to 4-byte boundary)

    let mut offset = 24; // After fixed RPC header fields

    // Parse credential (opaque_auth)
    if data.len() < offset + 8 {
        return Err(anyhow!("RPC message too short for credential header"));
    }
    let cred_length = u32::from_be_bytes([
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]) as usize;
    let cred_padded = (cred_length + 3) & !3; // Round up to multiple of 4
    offset += 8 + cred_padded; // flavor(4) + length(4) + body(padded)

    debug!("Credential length: {} bytes (padded: {}), offset now: {}", cred_length, cred_padded, offset);

    // Parse verifier (opaque_auth)
    if data.len() < offset + 8 {
        return Err(anyhow!("RPC message too short for verifier header"));
    }
    let verf_length = u32::from_be_bytes([
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]) as usize;
    let verf_padded = (verf_length + 3) & !3; // Round up to multiple of 4
    offset += 8 + verf_padded; // flavor(4) + length(4) + body(padded)

    debug!("Verifier length: {} bytes (padded: {}), offset now: {}", verf_length, verf_padded, offset);

    // Now offset points to the procedure arguments
    let args_offset = offset;
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
            crate::mount::handle_mount_call(&call, args_data, filesystem).await
        }
        100003 => {
            // NFS protocol (program 100003)
            debug!("Routing to NFS protocol handler");
            crate::nfs::dispatch(&call, args_data, filesystem).await
        }
        _ => {
            warn!("Unknown program number: {}", call.prog);
            Err(anyhow!("Unknown program number: {}", call.prog))
        }
    }
}
