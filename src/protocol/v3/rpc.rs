// RPC Protocol Middleware
//
// Wraps xdrgen-generated RPC types and provides serialization helpers

use anyhow::Result;
use bytes::BytesMut;
use std::io::Cursor;
use xdr_codec::{Pack, Unpack};

// Include xdrgen-generated RPC types
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::all
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/rpc_generated.rs"));
}

// Re-export generated types
pub use generated::*;

/// Wrapper for RPC messages providing serialization helpers
pub struct RpcMessage;

impl RpcMessage {
    /// Deserialize RPC call from bytes
    pub fn deserialize_call(data: &[u8]) -> Result<rpc_call_msg> {
        let mut cursor = Cursor::new(data);
        let (msg, _bytes_read) = rpc_call_msg::unpack(&mut cursor)?;
        Ok(msg)
    }

    /// Serialize RPC reply to bytes
    pub fn serialize_reply(reply: &rpc_reply_msg) -> Result<BytesMut> {
        let mut buf = Vec::new();
        reply.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful NULL reply
    pub fn create_null_reply(xid: u32) -> rpc_reply_msg {
        rpc_reply_msg {
            xid,
            mtype: msg_type::REPLY,
            stat: reply_stat::MSG_ACCEPTED,
            verf: opaque_auth {
                flavor: auth_flavor::AUTH_NONE,
                body: vec![],
            },
            accept_stat: accept_stat::SUCCESS,
        }
    }

    /// Create a successful reply with procedure result data
    ///
    /// Combines RPC reply header with procedure-specific result data
    pub fn create_success_reply_with_data(xid: u32, proc_data: BytesMut) -> Result<BytesMut> {
        // Create RPC reply header
        let rpc_reply = Self::create_null_reply(xid);
        let rpc_header = Self::serialize_reply(&rpc_reply)?;

        // Combine RPC header + procedure result data
        let mut response = BytesMut::with_capacity(rpc_header.len() + proc_data.len());
        response.extend_from_slice(&rpc_header);
        response.extend_from_slice(&proc_data);

        Ok(response)
    }

    /// Create a `MSG_DENIED` / `AUTH_ERROR` reply (RFC 5531 §9).
    ///
    /// The generated `rpc_reply_msg` is flattened around the accepted-reply
    /// shape and has no room for `reject_stat` / `auth_stat`, so the
    /// rejected form is packed by hand:
    /// `xid, REPLY, MSG_DENIED, AUTH_ERROR, auth_stat`.
    pub fn create_auth_error_reply(xid: u32, stat: auth_stat) -> Result<BytesMut> {
        let mut buf = Vec::with_capacity(20);
        xid.pack(&mut buf)?;
        msg_type::REPLY.pack(&mut buf)?;
        reply_stat::MSG_DENIED.pack(&mut buf)?;
        reject_stat::AUTH_ERROR.pack(&mut buf)?;
        stat.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create an RPC error reply for unsupported programs
    pub fn create_prog_unavail_reply(xid: u32) -> Result<BytesMut> {
        let rpc_reply = rpc_reply_msg {
            xid,
            mtype: msg_type::REPLY,
            stat: reply_stat::MSG_ACCEPTED,
            verf: opaque_auth {
                flavor: auth_flavor::AUTH_NONE,
                body: vec![],
            },
            accept_stat: accept_stat::PROG_UNAVAIL,
        };
        Self::serialize_reply(&rpc_reply)
    }
}
