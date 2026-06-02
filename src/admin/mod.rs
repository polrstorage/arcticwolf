//! Admin server (issue #25).
//!
//! Phase 1 introduced the transport scaffolding: a length-prefixed JSON
//! codec over a Unix domain socket, the request/response schema, and a
//! dispatcher. Phase 2 wires the first two read-only commands — `status`
//! and `version` — end to end, including the operator-facing
//! `arcticwolfctl` client (see [`client`]). Later phases add the remaining
//! commands (log-level, exports/config, metrics, shutdown).

pub mod audit;
pub mod client;
pub mod commands;
pub mod context;
pub mod protocol;
pub mod request;
pub mod response;
pub mod server;

pub use audit::{AuditEvent, AuditWriter, FileAuditWriter, NoopAuditWriter, PeerCreds};
pub use context::{AdminContext, ServerMetadata};
pub use request::AdminRequest;
pub use response::AdminResponse;
pub use server::serve;

/// Default filesystem path for the admin Unix domain socket.
///
/// Shared by [`AdminConfig::default`](crate::config::AdminConfig) and the
/// `arcticwolfctl --socket` flag so the daemon and the operator CLI agree
/// on the socket location without writing the literal path in two places.
pub const DEFAULT_ADMIN_SOCKET_PATH: &str = "/run/arcticwolf/admin.sock";
