//! Admin request schema.
//!
//! Phase 2 wires the two read-only commands (`Status`, `Version`); the
//! remaining variants are added by later phases as their handlers land
//! (Phase 3 the log-level commands, Phase 5 the exports/config commands,
//! Phase 7 metrics, Phase 8 shutdown). The variant set is kept in sync with
//! the v1 surface in issue #25 so later phases can land handlers without
//! touching the protocol again.

use serde::{Deserialize, Serialize};

/// An admin command sent by `arcticwolfctl` to the daemon.
///
/// Serialized as a JSON object whose `command` discriminator selects a
/// variant: `{"command": "status"}`, `{"command": "log-level-get"}`, etc.
/// `kebab-case` is used because the CLI itself uses kebab-cased subcommand
/// names — keeping the wire shape and the CLI in lockstep avoids a mental
/// translation when debugging via `arcticwolfctl raw`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum AdminRequest {
    /// `arcticwolfctl status` — daemon health snapshot (uptime, ports,
    /// log level, export count).
    Status,
    /// `arcticwolfctl version` — daemon build/version metadata
    /// (`CARGO_PKG_VERSION`, git commit, rustc version, build profile).
    Version,
}
