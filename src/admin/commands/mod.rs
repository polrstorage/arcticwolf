//! Daemon-side admin command handlers.
//!
//! Each handler maps one [`AdminRequest`](super::AdminRequest) variant to an
//! [`AdminResponse`](super::AdminResponse). Phase 2 shipped the two
//! read-only commands; Phase 3 adds the log-level commands. Later phases
//! add their handlers alongside these.

pub mod log_level;
pub mod status;
pub mod version;
