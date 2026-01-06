// NFS NULL Procedure (Procedure 0)
//
// This is the simplest NFS procedure - it does nothing but return success.
// Used to test connectivity and verify the NFS service is responding.

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::protocol::v3::rpc::RpcMessage;

/// Handle NFS NULL procedure (procedure 0)
///
/// This procedure does no work. It is used for server response testing
/// and timing.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
///
/// # Returns
/// Serialized RPC reply message (success with no data)
pub async fn handle_null(xid: u32) -> Result<BytesMut> {
    debug!("NFS NULL called (xid={})", xid);

    // Create successful reply (same as RPC/MOUNT NULL)
    let reply = RpcMessage::create_null_reply(xid);

    // Serialize reply
    RpcMessage::serialize_reply(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_null_procedure() {
        let xid = 12345;
        let result = handle_null(xid).await;

        assert!(result.is_ok(), "NULL procedure should succeed");

        let reply = result.unwrap();
        assert!(!reply.is_empty(), "Reply should contain data");

        // Reply should be at least 24 bytes (RPC header minimum)
        assert!(reply.len() >= 24, "Reply should have RPC header");
    }
}
