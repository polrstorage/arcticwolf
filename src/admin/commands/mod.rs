//! Daemon-side admin command handlers.
//!
//! Each handler maps one [`AdminRequest`](super::AdminRequest) variant to an
//! [`AdminResponse`](super::AdminResponse). Phase 2 ships the two read-only
//! commands; later phases add their handlers alongside these.

pub mod status;
pub mod version;
