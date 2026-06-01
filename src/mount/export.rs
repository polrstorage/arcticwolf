// MOUNT EXPORT Procedure Handler
//
// Procedure: 5 (EXPORT)
// Purpose: List every export this server advertises (RFC 1813 §5.2.5).

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, info};

use crate::fsal::ExportRegistry;
use crate::protocol::v3::mount::MountMessage;
use crate::protocol::v3::rpc::{RpcMessage, rpc_call_msg};

/// Handle MOUNT EXPORT procedure.
///
/// Takes no arguments, returns an XDR linked list of `exportnode` describing
/// every configured export. `showmount -e <server>` is the primary client.
pub fn handle(call: &rpc_call_msg, registry: &dyn ExportRegistry) -> Result<BytesMut> {
    debug!(
        "MOUNT EXPORT: xid={}, prog={}, vers={}, proc={}",
        call.xid, call.prog, call.vers, call.proc_
    );

    let exports = registry.list_exports();
    info!("MOUNT EXPORT response: {} export(s)", exports.len());

    let rpc_reply = RpcMessage::create_null_reply(call.xid);
    let rpc_header = RpcMessage::serialize_reply(&rpc_reply)?;
    let body = MountMessage::serialize_exports(&exports);

    let mut response = BytesMut::with_capacity(rpc_header.len() + body.len());
    response.extend_from_slice(&rpc_header);
    response.extend_from_slice(&body);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::{ExportInfo, FileHandle};

    /// Minimal `ExportRegistry` for tests — backed by a static list so we
    /// don't need the full multi-export filesystem plumbing here.
    struct FakeRegistry(Vec<ExportInfo>);

    impl ExportRegistry for FakeRegistry {
        fn root_handle_for(&self, _name: &str) -> Option<FileHandle> {
            None
        }
        fn list_exports(&self) -> Vec<ExportInfo> {
            self.0.clone()
        }
        fn is_read_only(&self, _handle: &FileHandle) -> bool {
            false
        }
        fn export_for_handle(&self, _handle: &FileHandle) -> Option<u32> {
            None
        }
    }

    /// Build a real MOUNT EXPORT RPC call by hand-rolling the wire bytes,
    /// then deserialize them through `RpcMessage::deserialize_call`. This
    /// avoids depending on the xdrgen-generated struct's field names/order.
    fn make_export_call(xid: u32) -> rpc_call_msg {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&xid.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // mtype = CALL
        buf.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        buf.extend_from_slice(&super::super::MOUNT_PROGRAM.to_be_bytes());
        buf.extend_from_slice(&super::super::MOUNT_V3.to_be_bytes());
        buf.extend_from_slice(&super::super::procedures::EXPORT.to_be_bytes());
        // cred: flavor=AUTH_NONE, body length 0
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        // verf: same
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        RpcMessage::deserialize_call(&buf).expect("synthetic call must deserialize")
    }

    #[test]
    fn export_handler_serializes_all_configured_exports() {
        let registry = FakeRegistry(vec![
            ExportInfo {
                name: "/data".to_string(),
                uid: 1,
                read_only: false,
                fsal: "local".to_string(),
            },
            ExportInfo {
                name: "/backup".to_string(),
                uid: 2,
                read_only: true,
                fsal: "local".to_string(),
            },
        ]);
        let call = make_export_call(0xdeadbeef);
        let response = handle(&call, &registry).expect("export handler must succeed");

        // The response is [RPC reply header][exports XDR]. The body is
        // deterministic so we can assert that the response ends with it.
        let body = MountMessage::serialize_exports(&registry.list_exports());
        assert!(
            response.ends_with(&body[..]),
            "response must end with the exports XDR body"
        );
        assert!(body.len() > 4, "two exports must serialize to >4 bytes");
    }

    #[test]
    fn export_handler_emits_terminator_when_no_exports() {
        let registry = FakeRegistry(vec![]);
        let call = make_export_call(1);
        let response = handle(&call, &registry).expect("export handler must succeed");

        // Last four bytes are the list terminator (0).
        let tail = &response[response.len() - 4..];
        assert_eq!(tail, &[0, 0, 0, 0]);
    }
}
