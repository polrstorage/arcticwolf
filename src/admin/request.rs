//! Admin request schema.
//!
//! Phase 1 only declares the variants the rest of the v1 CLI will use; their
//! handlers all return `"Command not implemented in phase 1"` today and are
//! filled in by later phases (Phase 2 wires `Status` and `Version`,
//! Phase 3 the log-level commands, Phase 5 the exports/config commands,
//! Phase 7 metrics, Phase 8 shutdown). The variant set is kept in sync with
//! the v1 surface in issue #25 so phase 2+ can land handlers without
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
    /// `arcticwolfctl status` — daemon health snapshot. Phase 2 will implement.
    Status,
}
