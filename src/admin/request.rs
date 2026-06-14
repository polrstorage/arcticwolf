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
    /// `arcticwolfctl metrics` — a JSON snapshot of operational counters
    /// (server-wide RPC totals, per-NFS-op counts, per-export I/O, per-admin-
    /// command counts). In-memory only; resets on daemon restart. No HTTP
    /// scrape endpoint (deferred per the Phase 7 spec).
    Metrics,
}

/// Every command tag the daemon recognizes, used to bound the admin
/// `by_command` metrics map (see [`crate::metrics::AdminMetrics`]).
///
/// This is the wire tag of every [`AdminRequest`] variant *plus* the three
/// synthetic tags the admin server emits for non-dispatchable frames
/// (`<undecodable>`, `<unknown>`, `<frame-error>`, documented in
/// [`crate::admin::audit`]). Anything a client sends that is not in this list
/// is bucketed under [`UNKNOWN_ADMIN_COMMAND_TAG`] rather than allocating a
/// fresh map entry.
///
/// Convention burden: this array must be kept in sync with the variants of
/// [`AdminRequest`] by hand — there is no compile-time link. A missed entry
/// is not a panic, it only means that command lands in the
/// `<unknown-command>` bucket instead of its own. The
/// `known_admin_command_tags_cover_every_variant` test guards the wire-tag
/// half of the list against drift.
pub const KNOWN_ADMIN_COMMAND_TAGS: &[&str] = &[
    // Wire tags — one per `AdminRequest` variant (kebab-case of the variant).
    "status",
    "version",
    "log-level-get",
    "log-level-set",
    "exports-list",
    "exports-add",
    "exports-remove",
    "exports-update",
    "config-show",
    "metrics",
    // Synthetic tags emitted by the admin server (see `crate::admin::audit`).
    "<undecodable>",
    "<unknown>",
    "<frame-error>",
];

/// Catch-all bucket for any client-supplied tag not in
/// [`KNOWN_ADMIN_COMMAND_TAGS`]. Distinct from the synthetic `<unknown>`
/// (valid JSON object, no `command` field): `<unknown-command>` means the
/// frame *named* a command the daemon doesn't recognize — an operator typo or
/// version skew — so the two can be told apart in the metrics snapshot.
pub const UNKNOWN_ADMIN_COMMAND_TAG: &str = "<unknown-command>";

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
        assert_eq!(tag(AdminRequest::Metrics), "metrics");
    }

    /// Every `AdminRequest` wire tag must appear in
    /// [`KNOWN_ADMIN_COMMAND_TAGS`], otherwise that command would be counted
    /// under the `<unknown-command>` bucket instead of its own. This guards
    /// the hand-maintained list against a new variant being added without a
    /// matching entry.
    #[test]
    fn known_admin_command_tags_cover_every_variant() {
        fn tag(req: AdminRequest) -> String {
            serde_json::to_value(req).unwrap()["command"]
                .as_str()
                .expect("command tag must be a string")
                .to_string()
        }

        // One representative value per variant. Adding a variant without
        // extending this match is a compile error (no `_` arm), which in turn
        // forces the author past this test.
        let variants = [
            AdminRequest::Status,
            AdminRequest::Version,
            AdminRequest::LogLevelGet,
            AdminRequest::LogLevelSet {
                level: "info".to_string(),
            },
            AdminRequest::ExportsList,
            AdminRequest::ExportsAdd {
                config: ExportConfig {
                    name: "/data".to_string(),
                    uid: 1,
                    read_only: false,
                    backend: BackendConfig::Local {
                        path: PathBuf::from("/srv/data"),
                    },
                },
                dry_run: false,
            },
            AdminRequest::ExportsRemove {
                selector: ExportSelector::Uid(1),
                dry_run: false,
            },
            AdminRequest::ExportsUpdate {
                selector: ExportSelector::Uid(1),
                read_only: true,
                dry_run: false,
            },
            AdminRequest::ConfigShow,
            AdminRequest::Metrics,
        ];

        for variant in variants {
            let t = tag(variant);
            assert!(
                KNOWN_ADMIN_COMMAND_TAGS.contains(&t.as_str()),
                "wire tag {t:?} is missing from KNOWN_ADMIN_COMMAND_TAGS",
            );
        }

        // The exhaustive match below makes the omission of a variant a
        // compile error; this asserts the catch-all isn't accidentally listed
        // as a known tag (it must stay distinct).
        assert!(!KNOWN_ADMIN_COMMAND_TAGS.contains(&UNKNOWN_ADMIN_COMMAND_TAG));
    }

    /// Exhaustive match: adding an `AdminRequest` variant fails to compile
    /// here until the author accounts for it, nudging them to update
    /// [`KNOWN_ADMIN_COMMAND_TAGS`] and the variant list above.
    #[test]
    fn admin_request_variants_are_accounted_for() {
        fn assert_accounted(req: &AdminRequest) {
            match req {
                AdminRequest::Status
                | AdminRequest::Version
                | AdminRequest::LogLevelGet
                | AdminRequest::LogLevelSet { .. }
                | AdminRequest::ExportsList
                | AdminRequest::ExportsAdd { .. }
                | AdminRequest::ExportsRemove { .. }
                | AdminRequest::ExportsUpdate { .. }
                | AdminRequest::ConfigShow
                | AdminRequest::Metrics => {}
            }
        }
        assert_accounted(&AdminRequest::Status);
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
