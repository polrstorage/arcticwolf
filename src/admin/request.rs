//! Admin request schema.
//!
//! Phase 2 wired the two read-only commands (`Status`, `Version`); Phase 3
//! adds the log-level commands; Phase 5 adds the exports and config-show
//! commands. Later phases add the rest (Phase 7 metrics, Phase 8 shutdown).
//! The variant set is kept in sync with the v1 surface in issue #25 so
//! later phases can land handlers without touching the protocol again.

use serde::{Deserialize, Serialize};

use crate::config::ExportConfig;
use crate::fsal::multi_export::ExportSelector;

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
    /// `arcticwolfctl log-level get` — report the tracing filter directive
    /// currently in effect.
    LogLevelGet,
    /// `arcticwolfctl log-level set <level>` — swap the live tracing
    /// filter. An unrecognized level is rejected and the previously active
    /// filter is left intact.
    LogLevelSet { level: String },
    /// `arcticwolfctl exports list` — snapshot the live export set
    /// (including retired uids) for operator inspection.
    ExportsList,
    /// `arcticwolfctl exports add` — install a new export at runtime.
    /// With `dry_run = true` the daemon only validates and never mutates
    /// state, so the CLI can flag obvious problems before committing.
    ExportsAdd {
        config: ExportConfig,
        #[serde(default)]
        dry_run: bool,
    },
    /// `arcticwolfctl exports remove` — retire an export. The uid moves
    /// into `retired_uids` so an old client handle can't be silently
    /// reassigned to a freshly-added export.
    ExportsRemove {
        selector: ExportSelector,
        #[serde(default)]
        dry_run: bool,
    },
    /// `arcticwolfctl exports update` — mutate a live export's fields.
    /// v1 only mutates `read_only`; future fields land as additional
    /// optional members on this variant.
    ExportsUpdate {
        selector: ExportSelector,
        read_only: bool,
        #[serde(default)]
        dry_run: bool,
    },
    /// `arcticwolfctl config show` — dump the daemon's startup
    /// configuration as JSON. Phase 5 ships it unredacted; sensitive-field
    /// redaction is a follow-up when non-local FSALs land.
    ConfigShow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig;
    use crate::fsal::multi_export::ExportSelector;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    /// Pin the `command` tag for every variant of `AdminRequest`. The CLI
    /// derives the kebab-case names from the variant identifiers, so a
    /// rename here is a wire-incompatible change — this test catches it.
    #[test]
    fn admin_request_command_tag_format() {
        fn tag(req: AdminRequest) -> String {
            serde_json::to_value(req).unwrap()["command"]
                .as_str()
                .expect("command tag must be a string")
                .to_string()
        }

        assert_eq!(tag(AdminRequest::Status), "status");
        assert_eq!(tag(AdminRequest::Version), "version");
        assert_eq!(tag(AdminRequest::LogLevelGet), "log-level-get");
        assert_eq!(
            tag(AdminRequest::LogLevelSet {
                level: "info".to_string()
            }),
            "log-level-set"
        );
        assert_eq!(tag(AdminRequest::ExportsList), "exports-list");
        assert_eq!(
            tag(AdminRequest::ExportsAdd {
                config: ExportConfig {
                    name: "/data".to_string(),
                    uid: 1,
                    read_only: false,
                    backend: BackendConfig::Local {
                        path: PathBuf::from("/srv/data"),
                    },
                },
                dry_run: false,
            }),
            "exports-add"
        );
        assert_eq!(
            tag(AdminRequest::ExportsRemove {
                selector: ExportSelector::Name("/data".to_string()),
                dry_run: false,
            }),
            "exports-remove"
        );
        assert_eq!(
            tag(AdminRequest::ExportsUpdate {
                selector: ExportSelector::Uid(1),
                read_only: true,
                dry_run: false,
            }),
            "exports-update"
        );
        assert_eq!(tag(AdminRequest::ConfigShow), "config-show");
    }

    /// `ExportsRemove` and `ExportsUpdate` carry `ExportSelector` as a
    /// nested object whose shape the CLI builds explicitly. Lock the
    /// embedded form so a future serde-attribute change can't silently
    /// flatten or rename it.
    #[test]
    fn exports_remove_embeds_selector_as_object() {
        let v: Value = serde_json::to_value(AdminRequest::ExportsRemove {
            selector: ExportSelector::Name("/data".to_string()),
            dry_run: false,
        })
        .unwrap();
        assert_eq!(v["selector"], json!({ "name": "/data" }));
        assert_eq!(v["dry_run"], false);
    }
}
