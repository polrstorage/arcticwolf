// RPC TCP Server with Record Marking
//
// Implements Sun RPC over TCP with record marking protocol (RFC 5531)

use anyhow::{Result, anyhow};
use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use crate::fsal::{ExportRegistry, NfsBackend};
use crate::metrics::Metrics;
use crate::portmap::Registry;
use crate::protocol::v3::rpc::{RpcMessage, rpc_call_msg};

/// RPC server handling TCP connections with record marking
///
/// Each server instance listens on a single port and only handles
/// requests for the specified RPC programs (allowed_programs).
pub struct RpcServer {
    listener: TcpListener,
    registry: Registry,
    filesystem: Arc<dyn NfsBackend>,
    allowed_programs: Vec<u32>,
    metrics: Arc<Metrics>,
}

impl RpcServer {
    /// Bind to the given address and create an RPC server.
    ///
    /// The server only accepts RPC calls for programs in `allowed_programs`.
    /// Use `local_port()` after binding to discover the actual port (useful when binding to port 0).
    pub async fn bind(
        addr: &str,
        registry: Registry,
        filesystem: Arc<dyn NfsBackend>,
        allowed_programs: Vec<u32>,
        metrics: Arc<Metrics>,
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        info!("RPC server bound to {}", listener.local_addr()?);
        Ok(Self {
            listener,
            registry,
            filesystem,
            allowed_programs,
            metrics,
        })
    }

    /// Get the actual local port this server is bound to.
    ///
    /// Useful when binding to port 0 (dynamic allocation).
    pub fn local_port(&self) -> Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    /// Start accepting connections and serving RPC requests.
    pub async fn serve(&self) -> Result<()> {
        info!(
            "RPC server listening on {} for programs {:?}",
            self.listener.local_addr()?,
            self.allowed_programs
        );

        loop {
            let (socket, peer_addr) = self.listener.accept().await?;
            info!("New connection from {}", peer_addr);

            let registry = self.registry.clone();
            let filesystem = self.filesystem.clone();
            let allowed_programs = self.allowed_programs.clone();
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(socket, registry, filesystem, &allowed_programs, metrics)
                        .await
                {
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
    filesystem: Arc<dyn NfsBackend>,
    allowed_programs: &[u32],
    metrics: Arc<Metrics>,
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

        debug!("Record marking: last={}, length={}", is_last, fragment_len);

        // Read fragment data
        let mut fragment = vec![0u8; fragment_len];
        socket.read_exact(&mut fragment).await?;
        buffer.put_slice(&fragment);

        // If this is the last fragment, process the complete RPC message
        if is_last {
            debug!("Complete RPC message received ({} bytes)", buffer.len());

            let response = match handle_rpc_message(
                &buffer,
                &registry,
                filesystem.as_ref(),
                allowed_programs,
                &metrics,
            )
            .await
            {
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

/// Handle a complete RPC message.
///
/// Takes `&dyn NfsBackend` so each dispatcher can be handed the trait view
/// it actually needs: MOUNT only touches `ExportRegistry`, while NFS still
/// needs the full `Filesystem`. Trait upcasting is stable since Rust 1.86.
async fn handle_rpc_message(
    data: &[u8],
    registry: &Registry,
    filesystem: &dyn NfsBackend,
    allowed_programs: &[u32],
    metrics: &Metrics,
) -> Result<BytesMut> {
    use std::sync::atomic::Ordering::Relaxed;

    // Debug: dump complete RPC message
    debug!(
        "Complete RPC message ({} bytes): {:02x?}",
        data.len(),
        &data[..data.len().min(100)]
    );

    // Deserialize RPC call header. A failure here means the frame's RPC call
    // header was undecodable, so we never learned which program/procedure it
    // targeted — the caller replies PROG_UNAVAIL. Count it under
    // `rpc_decode_errors_total` *before* the early return, so this dropped
    // fraction stays visible and disjoint from both `rpc_requests_total`
    // (decoded RPCs) and `rpc_errors_total` (decoded-but-failed RPCs).
    let call = match RpcMessage::deserialize_call(data) {
        Ok(call) => call,
        Err(e) => {
            metrics.server.rpc_decode_errors_total.fetch_add(1, Relaxed);
            return Err(e);
        }
    };

    debug!(
        "RPC call: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    // A decoded RPC call is a request the server has accepted, regardless of
    // which program it targets. Count it here, once, for portmap/mount/NFS
    // alike; the per-program error counter is bumped below if routing fails.
    metrics.server.rpc_requests_total.fetch_add(1, Relaxed);

    let result = route_rpc_call(&call, data, registry, filesystem, allowed_programs, metrics).await;
    if result.is_err() {
        metrics.server.rpc_errors_total.fetch_add(1, Relaxed);
    }
    result
}

/// Route an already-decoded RPC call to its program handler.
///
/// Split out of [`handle_rpc_message`] so the request/error counters wrap a
/// single call site: every `Err` this returns (unknown program, malformed
/// auth, downstream handler failure) is counted once as an RPC error.
async fn route_rpc_call(
    call: &rpc_call_msg,
    data: &[u8],
    registry: &Registry,
    filesystem: &dyn NfsBackend,
    allowed_programs: &[u32],
    metrics: &Metrics,
) -> Result<BytesMut> {
    // Check if this program is allowed on this server instance
    if !allowed_programs.contains(&call.prog) {
        warn!(
            "Program {} not allowed on this port (allowed: {:?})",
            call.prog, allowed_programs
        );
        return Err(anyhow!("Program {} not served on this port", call.prog));
    }

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

    debug!(
        "Credential length: {} bytes (padded: {}), offset now: {}",
        cred_length, cred_padded, offset
    );

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

    debug!(
        "Verifier length: {} bytes (padded: {}), offset now: {}",
        verf_length, verf_padded, offset
    );

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
            crate::portmap::handle_portmap_call(call, args_data, registry)
        }
        100005 => {
            // MOUNT protocol (program 100005)
            debug!("Routing to MOUNT protocol handler");
            crate::mount::handle_mount_call(call, args_data, filesystem as &dyn ExportRegistry)
                .await
        }
        100003 => {
            // NFS protocol (program 100003)
            debug!("Routing to NFS protocol handler");
            crate::nfs::dispatch(call, args_data, filesystem, metrics).await
        }
        _ => {
            warn!("Unknown program number: {}", call.prog);
            Err(anyhow!("Unknown program number: {}", call.prog))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig, ExportConfig};
    use crate::fsal::MultiExportFilesystem;
    use std::sync::atomic::Ordering::Relaxed;

    /// A frame whose RPC call header can't be decoded must bump
    /// `rpc_decode_errors_total` while leaving `rpc_requests_total` (decoded
    /// RPCs) and `rpc_errors_total` (decoded-but-failed RPCs) untouched —
    /// finding 3.
    #[tokio::test]
    async fn malformed_rpc_header_bumps_decode_errors_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exports = vec![ExportConfig {
            name: "/data".to_string(),
            uid: 1,
            read_only: false,
            backend: BackendConfig::Local {
                path: tmp.path().to_path_buf(),
            },
        }];
        let filesystem: Arc<dyn NfsBackend> = Arc::new(
            MultiExportFilesystem::build_from_config(&exports).expect("build_from_config"),
        );
        let registry = Registry::new();
        let metrics = Metrics::new();

        // Three bytes — far too short to be a valid RPC call header, so
        // `deserialize_call` fails before any routing happens.
        let truncated = [0x00u8, 0x01, 0x02];
        let result = handle_rpc_message(
            &truncated,
            &registry,
            filesystem.as_ref(),
            &[100003],
            &metrics,
        )
        .await;

        assert!(result.is_err(), "a truncated header must fail to decode");
        assert_eq!(metrics.server.rpc_decode_errors_total.load(Relaxed), 1);
        assert_eq!(metrics.server.rpc_requests_total.load(Relaxed), 0);
        assert_eq!(metrics.server.rpc_errors_total.load(Relaxed), 0);
    }
}
