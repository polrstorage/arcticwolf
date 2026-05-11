// MOUNT MNT Procedure Handler
//
// Procedure: 1 (MNT)
// Purpose: Mount a directory and return a file handle

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, info, warn};

use crate::fsal::ExportRegistry;
use crate::protocol::v3::mount::{MNT3ERR_NOENT, MountMessage};
use crate::protocol::v3::rpc::{RpcMessage, rpc_call_msg};

/// Handle MOUNT MNT procedure.
///
/// Resolves the client-supplied dirpath against the export registry.
/// Returns the export's root file handle on hit, or
/// `mountstat3::MNT3ERR_NOENT` if no export matches.
///
/// Arguments: dirpath (string)
/// Returns: mountres3 (file handle + auth flavors on success, or status only)
pub async fn handle(
    call: &rpc_call_msg,
    args_data: &[u8],
    registry: &dyn ExportRegistry,
) -> Result<BytesMut> {
    debug!(
        "MOUNT MNT: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    let dirpath = MountMessage::deserialize_dirpath(args_data)?;
    info!("MOUNT MNT request for path: '{}'", dirpath);

    let rpc_reply = RpcMessage::create_null_reply(call.xid);
    let rpc_header = RpcMessage::serialize_reply(&rpc_reply)?;

    let mount_body = match registry.root_handle_for(&dirpath) {
        Some(handle) => {
            info!(
                "Resolved export '{}' to root handle ({} bytes)",
                dirpath,
                handle.len()
            );
            let mount_res = MountMessage::create_mount_ok(handle);
            MountMessage::serialize_mountres3(&mount_res)?
        }
        None => {
            warn!(
                "MOUNT MNT: no export matches dirpath '{}'; returning MNT3ERR_NOENT",
                dirpath
            );
            MountMessage::serialize_mount_status(MNT3ERR_NOENT)
        }
    };

    let mut response = BytesMut::with_capacity(rpc_header.len() + mount_body.len());
    response.extend_from_slice(&rpc_header);
    response.extend_from_slice(&mount_body);
    debug!("MOUNT MNT response: {} bytes total", response.len());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::handle::HANDLE_LEN;
    use crate::fsal::{ExportInfo, FileHandle};
    use std::collections::HashMap;

    /// Test-only `ExportRegistry` that lets each test pin the exact set of
    /// (name → root_handle) pairs it wants to exercise.
    struct FakeRegistry {
        roots: HashMap<String, FileHandle>,
    }

    impl FakeRegistry {
        fn with_exports(exports: &[(&str, u32)]) -> Self {
            let mut roots = HashMap::new();
            for (name, uid) in exports {
                let mut h = vec![0u8; HANDLE_LEN];
                h[..4].copy_from_slice(&uid.to_be_bytes());
                roots.insert((*name).to_string(), h);
            }
            Self { roots }
        }
    }

    impl ExportRegistry for FakeRegistry {
        fn root_handle_for(&self, name: &str) -> Option<FileHandle> {
            self.roots.get(name).cloned()
        }
        fn list_exports(&self) -> Vec<ExportInfo> {
            vec![]
        }
        fn is_read_only(&self, _handle: &FileHandle) -> bool {
            false
        }
        fn export_for_handle(&self, _handle: &FileHandle) -> Option<u32> {
            None
        }
    }

    /// Build a MOUNT MNT RPC call by hand-rolling the wire bytes and then
    /// deserializing — keeps the test independent of xdrgen field naming.
    fn make_mnt_call(xid: u32) -> rpc_call_msg {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&xid.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // mtype = CALL
        buf.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        buf.extend_from_slice(&super::super::MOUNT_PROGRAM.to_be_bytes());
        buf.extend_from_slice(&super::super::MOUNT_V3.to_be_bytes());
        buf.extend_from_slice(&super::super::procedures::MNT.to_be_bytes());
        // cred + verf: both AUTH_NONE with empty body
        for _ in 0..4 {
            buf.extend_from_slice(&0u32.to_be_bytes());
        }
        RpcMessage::deserialize_call(&buf).expect("synthetic call must deserialize")
    }

    /// Encode `path` as an XDR dirpath (4-byte length, bytes, zero padding).
    fn encode_dirpath(path: &str) -> Vec<u8> {
        let bytes = path.as_bytes();
        let mut out = Vec::with_capacity(4 + bytes.len() + 4);
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
        let pad = (4 - (bytes.len() % 4)) % 4;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    /// The MNT body sits right after the RPC reply header. Locate its start
    /// by re-serializing a known-good reply header for the same xid.
    fn mnt_body<'a>(response: &'a BytesMut, xid: u32) -> &'a [u8] {
        let reply = RpcMessage::create_null_reply(xid);
        let header = RpcMessage::serialize_reply(&reply).unwrap();
        assert!(
            response.starts_with(&header[..]),
            "response must begin with the RPC reply header"
        );
        &response[header.len()..]
    }

    #[tokio::test]
    async fn mnt_known_path_returns_root_handle() {
        let registry = FakeRegistry::with_exports(&[("/data", 7)]);
        let call = make_mnt_call(42);
        let args = encode_dirpath("/data");
        let response = super::handle(&call, &args, &registry).await.unwrap();

        let body = mnt_body(&response, 42);
        // MNT3_OK discriminator = 0, followed by fhandle3 (4-byte length + bytes).
        assert_eq!(&body[..4], &[0, 0, 0, 0], "expected MNT3_OK discriminator");
        let fh_len = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
        assert_eq!(fh_len, HANDLE_LEN);
        let fh = &body[8..8 + fh_len];
        assert_eq!(
            &fh[..4],
            &7u32.to_be_bytes(),
            "uid prefix must come through"
        );
    }

    #[tokio::test]
    async fn mnt_unknown_path_returns_noent() {
        let registry = FakeRegistry::with_exports(&[("/data", 1)]);
        let call = make_mnt_call(99);
        let args = encode_dirpath("/missing");
        let response = super::handle(&call, &args, &registry).await.unwrap();

        let body = mnt_body(&response, 99);
        // MNT3ERR_NOENT = 2, void payload → exactly 4 bytes.
        assert_eq!(body, &[0, 0, 0, 2]);
    }

    #[tokio::test]
    async fn mnt_empty_path_returns_noent() {
        // RFC 1813 doesn't define a wildcard path; treat empty input the
        // same as any other unknown name — NOENT.
        let registry = FakeRegistry::with_exports(&[("/data", 1)]);
        let call = make_mnt_call(7);
        let args = encode_dirpath("");
        let response = super::handle(&call, &args, &registry).await.unwrap();

        let body = mnt_body(&response, 7);
        assert_eq!(body, &[0, 0, 0, 2]);
    }
}
