//! Admin server (issue #25).
//!
//! Phase 1 introduces the transport scaffolding only: a length-prefixed
//! JSON codec over a Unix domain socket, a placeholder request/response
//! schema, and a dispatcher that responds `not_implemented` to every
//! command. The actual admin commands (`status`, `exports list`, ...) are
//! wired up in later phases on top of this skeleton.

pub mod context;
pub mod protocol;
pub mod request;
pub mod response;
pub mod server;

pub use context::AdminContext;
pub use request::AdminRequest;
pub use response::AdminResponse;
pub use server::serve;
