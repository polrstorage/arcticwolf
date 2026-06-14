//! Daemon-side admin command handlers.
//!
//! Each handler maps one [`AdminRequest`](super::AdminRequest) variant to an
//! [`AdminResponse`](super::AdminResponse). Phase 2 shipped the two
//! read-only commands; Phase 3 added log-level; Phase 5 adds the exports
//! and config-show commands. Later phases add their handlers alongside
//! these.

pub mod config_show;
pub mod exports_add;
pub mod exports_list;
pub mod exports_remove;
pub mod exports_update;
pub mod log_level;
pub mod metrics;
pub mod status;
pub mod version;
