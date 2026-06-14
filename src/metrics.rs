//! In-memory operational metrics (issue #25, Phase 7).
//!
//! A single [`Metrics`] value, constructed once at startup and shared as an
//! `Arc<Metrics>`, holds every operational counter the daemon exposes via
//! the admin `metrics` command. There is no HTTP scrape endpoint and no
//! external metrics crate — counters are plain [`AtomicU64`]s bumped with
//! [`Ordering::Relaxed`].
//!
//! ## Semantics
//!
//! - **Monotonic within a daemon lifetime.** Every counter only ever
//!   increases while the process runs; nothing decrements. Errors bump an
//!   error counter *in addition to* the op/request counter they belong to,
//!   they never roll one back.
//! - **Reset on restart.** Counters live only in memory. A daemon restart
//!   starts every counter back at zero; there is no persistence.
//! - **Lock-free, non-atomic snapshots.** [`Metrics::snapshot_json`] loads
//!   each counter independently with `Relaxed`. The resulting JSON is NOT a
//!   globally consistent point-in-time view: a request counted in
//!   `rpc_requests_total` may not yet be reflected in the per-op or
//!   per-export counter it also touches, and vice versa. This is
//!   deliberate — taking a global lock to make the snapshot atomic would
//!   serialize the hot NFS path against the rare admin read. Operators may
//!   observe `rpc_requests_total < sum(nfs_ops)` (or vice versa) when
//!   reading mid-request — counters are loaded in sequence and an in-flight
//!   RPC can advance one before the other is observed.
//! - **Per-export metrics are dropped on remove.** Each export's
//!   [`ExportMetrics`] lives behind an `Arc` on its export entry (see
//!   [`crate::fsal::multi_export`]). The same `Arc` is preserved across
//!   `update_export` snapshot rebuilds, so unrelated admin mutations don't
//!   reset an export's counters. But when an export is *removed*, its entry
//!   — and therefore its `Arc<ExportMetrics>` — is dropped once no in-flight
//!   request still holds a clone. v1 keeps no historical metrics for removed
//!   exports.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Value, json};

use crate::admin::request::{KNOWN_ADMIN_COMMAND_TAGS, UNKNOWN_ADMIN_COMMAND_TAG};

/// Top-level metrics registry, constructed once and shared via `Arc`.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Metrics {
    /// Server-wide RPC counters (across portmap, mount, and NFS).
    pub server: ServerMetrics,
    /// Per-NFSv3-procedure invocation counters.
    pub nfs_ops: NfsOpsMetrics,
    /// Admin-command counters.
    pub admin: AdminMetrics,
}

impl Metrics {
    /// Construct a fresh registry with every counter initialized to zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the JSON snapshot returned by the admin `metrics` command.
    ///
    /// Shape:
    /// ```json
    /// {
    ///   "server":  { "uptime_seconds": .., "rpc_requests_total": .., "rpc_errors_total": .., "rpc_decode_errors_total": .. },
    ///   "nfs_ops": { "getattr": .., "read": .., ... },
    ///   "exports": [ { "uid": .., "name": .., "requests": .., "bytes_read": .., "bytes_written": .., "errors": .. } ],
    ///   "admin":   { "commands_total": .., "by_command": { "status": .., ... } }
    /// }
    /// ```
    ///
    /// `uptime_seconds` and `exports` are supplied by the caller (the
    /// uptime comes from `AdminContext::start_time`, the per-export
    /// snapshots from the filesystem) because they live outside this
    /// registry. Every counter is loaded with `Relaxed`; see the module
    /// doc on why the result is not cross-counter atomic.
    pub fn snapshot_json(&self, uptime_seconds: u64, exports: Vec<ExportMetricsSnapshot>) -> Value {
        json!({
            "server": {
                "uptime_seconds": uptime_seconds,
                "rpc_requests_total": self.server.rpc_requests_total.load(Ordering::Relaxed),
                "rpc_errors_total": self.server.rpc_errors_total.load(Ordering::Relaxed),
                "rpc_decode_errors_total": self.server.rpc_decode_errors_total.load(Ordering::Relaxed),
            },
            "nfs_ops": self.nfs_ops.to_json(),
            "exports": exports,
            "admin": self.admin.to_json(),
        })
    }
}

/// Server-wide RPC counters, bumped in [`crate::rpc::server`] for every
/// program the daemon serves.
///
/// Three counters partition the lifecycle of an inbound RPC:
///
/// - `rpc_requests_total` — RPCs whose call header *decoded successfully*.
///   This is the denominator for "requests the server understood enough to
///   route", regardless of whether routing or the handler then succeeded.
/// - `rpc_errors_total` — decoded RPCs that were *accepted but failed*
///   downstream (unknown program, malformed auth, handler `Err`). A request
///   counted here was always first counted in `rpc_requests_total`.
/// - `rpc_decode_errors_total` — frames dropped *before* decode, i.e. the
///   call header itself was undecodable. These never reach
///   `rpc_requests_total` (we never learned program/proc) and are therefore
///   tracked separately rather than folded into `rpc_errors_total`.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ServerMetrics {
    /// Every RPC request whose call header decoded successfully (any
    /// program). Malformed frames that fail to decode are NOT counted here;
    /// they bump [`rpc_decode_errors_total`](Self::rpc_decode_errors_total)
    /// instead.
    pub rpc_requests_total: AtomicU64,
    /// Every decoded RPC whose routing or handler returned an error to the
    /// client. Does NOT include logical NFS errors returned in-band as
    /// `nfsstat3` codes (NFS3ERR_PERM, NFS3ERR_NOENT, etc.); only RPC-layer
    /// errors (unknown program, malformed auth, handler `Err`). The
    /// pre-decode case is covered by
    /// [`rpc_decode_errors_total`](Self::rpc_decode_errors_total), which
    /// applies before this counter could.
    pub rpc_errors_total: AtomicU64,
    /// Every inbound frame whose RPC call header failed to deserialize, so
    /// the server never learned which program/procedure it targeted (and
    /// replied PROG_UNAVAIL). Disjoint from `rpc_requests_total` (which only
    /// counts successful decodes) and from `rpc_errors_total` (which counts
    /// decoded-but-failed requests).
    pub rpc_decode_errors_total: AtomicU64,
}

/// One [`AtomicU64`] per NFSv3 procedure the dispatcher routes on.
///
/// A named-field struct rather than a `HashMap`. [`record`](Self::record)
/// dispatches on the NFSv3 procedure number to bump the matching counter.
/// The proc-number → field mapping is convention-enforced via the `match`
/// arm in `record`: adding a new procedure to the dispatcher requires *both*
/// adding a field to this struct *and* adding an arm to `record` (and a key
/// to [`to_json`](Self::to_json)). Adding only one and not the other will
/// silently miscount — there is no compile-time check that the three stay in
/// lockstep.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct NfsOpsMetrics {
    pub null: AtomicU64,
    pub getattr: AtomicU64,
    pub setattr: AtomicU64,
    pub lookup: AtomicU64,
    pub access: AtomicU64,
    pub readlink: AtomicU64,
    pub read: AtomicU64,
    pub write: AtomicU64,
    pub create: AtomicU64,
    pub mkdir: AtomicU64,
    pub symlink: AtomicU64,
    pub mknod: AtomicU64,
    pub remove: AtomicU64,
    pub rmdir: AtomicU64,
    pub rename: AtomicU64,
    pub link: AtomicU64,
    pub readdir: AtomicU64,
    pub readdirplus: AtomicU64,
    pub fsstat: AtomicU64,
    pub fsinfo: AtomicU64,
    pub pathconf: AtomicU64,
    pub commit: AtomicU64,
}

impl NfsOpsMetrics {
    /// Bump the counter for NFSv3 procedure number `proc_`. Procedure
    /// numbers outside the dispatched set (RFC 1813 §3) are ignored — the
    /// dispatcher answers those with `NFS3ERR_NOTSUPP` and they have no
    /// counter of their own.
    pub fn record(&self, proc_: u32) {
        let counter = match proc_ {
            0 => &self.null,
            1 => &self.getattr,
            2 => &self.setattr,
            3 => &self.lookup,
            4 => &self.access,
            5 => &self.readlink,
            6 => &self.read,
            7 => &self.write,
            8 => &self.create,
            9 => &self.mkdir,
            10 => &self.symlink,
            11 => &self.mknod,
            12 => &self.remove,
            13 => &self.rmdir,
            14 => &self.rename,
            15 => &self.link,
            16 => &self.readdir,
            17 => &self.readdirplus,
            18 => &self.fsstat,
            19 => &self.fsinfo,
            20 => &self.pathconf,
            21 => &self.commit,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot every procedure counter into a JSON object keyed by the
    /// lowercase procedure name.
    fn to_json(&self) -> Value {
        json!({
            "null": self.null.load(Ordering::Relaxed),
            "getattr": self.getattr.load(Ordering::Relaxed),
            "setattr": self.setattr.load(Ordering::Relaxed),
            "lookup": self.lookup.load(Ordering::Relaxed),
            "access": self.access.load(Ordering::Relaxed),
            "readlink": self.readlink.load(Ordering::Relaxed),
            "read": self.read.load(Ordering::Relaxed),
            "write": self.write.load(Ordering::Relaxed),
            "create": self.create.load(Ordering::Relaxed),
            "mkdir": self.mkdir.load(Ordering::Relaxed),
            "symlink": self.symlink.load(Ordering::Relaxed),
            "mknod": self.mknod.load(Ordering::Relaxed),
            "remove": self.remove.load(Ordering::Relaxed),
            "rmdir": self.rmdir.load(Ordering::Relaxed),
            "rename": self.rename.load(Ordering::Relaxed),
            "link": self.link.load(Ordering::Relaxed),
            "readdir": self.readdir.load(Ordering::Relaxed),
            "readdirplus": self.readdirplus.load(Ordering::Relaxed),
            "fsstat": self.fsstat.load(Ordering::Relaxed),
            "fsinfo": self.fsinfo.load(Ordering::Relaxed),
            "pathconf": self.pathconf.load(Ordering::Relaxed),
            "commit": self.commit.load(Ordering::Relaxed),
        })
    }
}

/// Per-export counters. Lives behind an `Arc` on each export entry so it
/// survives `update_export` snapshot rebuilds (the same `Arc` is cloned
/// into the new snapshot) and is dropped on `remove_export`.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ExportMetrics {
    /// Filesystem operations routed to this export (bumped before the call).
    pub requests: AtomicU64,
    /// Bytes returned by successful `read` calls.
    pub bytes_read: AtomicU64,
    /// Bytes accepted by successful `write` calls.
    pub bytes_written: AtomicU64,
    /// Operations that returned an error from the backend.
    pub errors: AtomicU64,
}

impl ExportMetrics {
    /// Bump [`errors`](Self::errors) if `result` is `Err`. Convenience for
    /// the router's trait-method impls, which all share this tail.
    pub fn record_result<T, E>(&self, result: &Result<T, E>) {
        if result.is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Flattened, owned snapshot of one export's counters, tagged with its
/// identity. Built by the filesystem and embedded verbatim in the metrics
/// JSON `exports` array.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ExportMetricsSnapshot {
    pub uid: u32,
    pub name: String,
    pub requests: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub errors: u64,
}

/// Admin-command counters.
///
/// `by_command` is a `HashMap` rather than named fields because the tag set
/// includes runtime synthetic tags (`<undecodable>`, `<frame-error>`, …)
/// that don't correspond to a typed `AdminRequest` variant. `parking_lot` is
/// not a dependency, so this uses [`std::sync::RwLock`]; the map structure is
/// guarded by the lock, the values are `Arc<AtomicU64>` bumped lock-free.
///
/// The map is *bounded and fixed*: it is pre-populated at construction with
/// every entry in [`KNOWN_ADMIN_COMMAND_TAGS`] plus the
/// [`UNKNOWN_ADMIN_COMMAND_TAG`] catch-all bucket, all starting at 0, and no
/// keys are ever inserted afterwards. A client-supplied tag that isn't in the
/// allowlist is counted under `<unknown-command>` rather than allocating a
/// fresh entry, so a hostile client can't grow the map unboundedly. A
/// pleasant side effect: the snapshot always lists every known command, so an
/// operator can see at-a-glance which commands have never been used.
#[derive(Debug)]
#[non_exhaustive]
pub struct AdminMetrics {
    /// Every admin request the server accepts, including malformed ones.
    pub commands_total: AtomicU64,
    /// Per-command-tag counters, keyed by the same tag the audit log uses.
    /// Pre-populated with all known tags; never gains new keys at runtime.
    pub by_command: RwLock<HashMap<String, Arc<AtomicU64>>>,
}

impl Default for AdminMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminMetrics {
    /// Construct with `commands_total` at 0 and `by_command` pre-populated:
    /// one zeroed counter for every [`KNOWN_ADMIN_COMMAND_TAGS`] entry plus
    /// the [`UNKNOWN_ADMIN_COMMAND_TAG`] catch-all. After this, the map's key
    /// set is fixed for the lifetime of the value.
    pub fn new() -> Self {
        let mut map: HashMap<String, Arc<AtomicU64>> =
            HashMap::with_capacity(KNOWN_ADMIN_COMMAND_TAGS.len() + 1);
        for tag in KNOWN_ADMIN_COMMAND_TAGS {
            map.insert((*tag).to_string(), Arc::new(AtomicU64::new(0)));
        }
        map.insert(
            UNKNOWN_ADMIN_COMMAND_TAG.to_string(),
            Arc::new(AtomicU64::new(0)),
        );
        Self {
            commands_total: AtomicU64::new(0),
            by_command: RwLock::new(map),
        }
    }

    /// Record one admin request tagged `tag`. Always bumps
    /// [`commands_total`](Self::commands_total); also bumps the matching
    /// per-tag counter, or the [`UNKNOWN_ADMIN_COMMAND_TAG`] bucket if `tag`
    /// is not a known command.
    ///
    /// This only ever takes the *read* lock: every counter the map can hold
    /// was inserted at construction, so there is no first-observation write
    /// path and no way for a client-supplied tag to allocate a new entry.
    pub fn record(&self, tag: &str) {
        self.commands_total.fetch_add(1, Ordering::Relaxed);

        let map = self
            .by_command
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(counter) = map.get(tag) {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            // `UNKNOWN_ADMIN_COMMAND_TAG` is always inserted by `new`, so this
            // lookup cannot miss.
            map.get(UNKNOWN_ADMIN_COMMAND_TAG)
                .expect("unknown-command bucket is pre-populated by AdminMetrics::new")
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot the admin counters into a JSON object.
    fn to_json(&self) -> Value {
        let map = self
            .by_command
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        let by_command: serde_json::Map<String, Value> = map
            .iter()
            .map(|(tag, counter)| (tag.clone(), Value::from(counter.load(Ordering::Relaxed))))
            .collect();
        json!({
            "commands_total": self.commands_total.load(Ordering::Relaxed),
            "by_command": by_command,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_every_counter_to_zero() {
        let m = Metrics::new();
        let snap = m.snapshot_json(0, Vec::new());
        assert_eq!(snap["server"]["rpc_requests_total"], 0);
        assert_eq!(snap["server"]["rpc_errors_total"], 0);
        assert_eq!(snap["server"]["rpc_decode_errors_total"], 0);
        assert_eq!(snap["server"]["uptime_seconds"], 0);
        // Every nfs op present and zero.
        for op in [
            "null",
            "getattr",
            "setattr",
            "lookup",
            "access",
            "readlink",
            "read",
            "write",
            "create",
            "mkdir",
            "symlink",
            "mknod",
            "remove",
            "rmdir",
            "rename",
            "link",
            "readdir",
            "readdirplus",
            "fsstat",
            "fsinfo",
            "pathconf",
            "commit",
        ] {
            assert_eq!(snap["nfs_ops"][op], 0, "nfs_ops.{op} must start at 0");
        }
        assert_eq!(snap["admin"]["commands_total"], 0);
        // `by_command` is pre-populated with every known tag at 0 (finding
        // 10): no key is created at runtime, so all of them are present from
        // construction.
        for tag in KNOWN_ADMIN_COMMAND_TAGS {
            assert_eq!(
                snap["admin"]["by_command"][tag], 0,
                "known tag {tag} must be present at 0",
            );
        }
        assert_eq!(snap["admin"]["by_command"][UNKNOWN_ADMIN_COMMAND_TAG], 0);
        assert_eq!(
            snap["admin"]["by_command"]
                .as_object()
                .expect("by_command is an object")
                .len(),
            KNOWN_ADMIN_COMMAND_TAGS.len() + 1,
            "by_command must hold exactly the known tags plus the catch-all",
        );
        assert_eq!(snap["exports"], json!([]));
    }

    #[test]
    fn bumping_counters_reflects_in_snapshot() {
        let m = Metrics::new();
        m.server.rpc_requests_total.fetch_add(10, Ordering::Relaxed);
        m.server.rpc_errors_total.fetch_add(2, Ordering::Relaxed);
        m.nfs_ops.record(1); // getattr
        m.nfs_ops.record(1);
        m.nfs_ops.record(6); // read

        let exports = vec![ExportMetricsSnapshot {
            uid: 1,
            name: "/data".to_string(),
            requests: 3,
            bytes_read: 100,
            bytes_written: 50,
            errors: 1,
        }];
        let snap = m.snapshot_json(42, exports);

        assert_eq!(snap["server"]["uptime_seconds"], 42);
        assert_eq!(snap["server"]["rpc_requests_total"], 10);
        assert_eq!(snap["server"]["rpc_errors_total"], 2);
        assert_eq!(snap["nfs_ops"]["getattr"], 2);
        assert_eq!(snap["nfs_ops"]["read"], 1);
        assert_eq!(snap["nfs_ops"]["write"], 0);
        assert_eq!(snap["exports"][0]["uid"], 1);
        assert_eq!(snap["exports"][0]["name"], "/data");
        assert_eq!(snap["exports"][0]["requests"], 3);
        assert_eq!(snap["exports"][0]["bytes_read"], 100);
        assert_eq!(snap["exports"][0]["bytes_written"], 50);
        assert_eq!(snap["exports"][0]["errors"], 1);
    }

    #[test]
    fn record_ignores_unknown_procedure_numbers() {
        let m = Metrics::new();
        m.nfs_ops.record(999);
        let snap = m.nfs_ops.to_json();
        // Nothing bumped; spot-check a couple of fields stay zero.
        assert_eq!(snap["getattr"], 0);
        assert_eq!(snap["commit"], 0);
    }

    #[test]
    fn by_command_bumps_known_tags_in_prepopulated_map() {
        let admin = AdminMetrics::default();
        admin.record("status");
        admin.record("status");
        admin.record("exports-add");
        admin.record("<undecodable>");

        assert_eq!(admin.commands_total.load(Ordering::Relaxed), 4);
        let snap = admin.to_json();
        assert_eq!(snap["commands_total"], 4);
        assert_eq!(snap["by_command"]["status"], 2);
        assert_eq!(snap["by_command"]["exports-add"], 1);
        assert_eq!(snap["by_command"]["<undecodable>"], 1);
        // A never-seen but known tag is present at 0 (pre-populated), not
        // absent — finding 10 changed this from the old insert-on-first-use
        // behavior.
        assert_eq!(snap["by_command"]["version"], 0);
    }

    #[test]
    fn admin_metrics_buckets_unknown_tag_under_unknown_command() {
        let admin = AdminMetrics::new();
        admin.record("definitely-not-a-command");
        admin.record("also-bogus");

        let snap = admin.to_json();
        assert_eq!(snap["commands_total"], 2);
        // Both unknown tags collapse into the catch-all bucket...
        assert_eq!(snap["by_command"][UNKNOWN_ADMIN_COMMAND_TAG], 2);
        // ...and no key was created for either of them.
        assert!(snap["by_command"].get("definitely-not-a-command").is_none());
        assert!(snap["by_command"].get("also-bogus").is_none());
        // `<unknown-command>` is distinct from `<unknown>`.
        assert_eq!(snap["by_command"]["<unknown>"], 0);
    }

    #[test]
    fn admin_metrics_record_never_takes_write_lock() {
        let admin = AdminMetrics::new();
        let key_count_before = admin.by_command.read().unwrap().len();

        // The old behavior inserted a fresh key on first observation (a write
        // lock). With finding 10's pre-populated, fixed map, recording an
        // unknown tag must instead route to the catch-all bucket and leave the
        // key set untouched — observable proof the write path is gone.
        admin.record("brand-new-unknown-tag");
        admin.record("another-unknown");

        let map = admin.by_command.read().unwrap();
        assert_eq!(
            map.len(),
            key_count_before,
            "record must not insert new keys (no write-lock path)",
        );
        assert!(
            map.get("brand-new-unknown-tag").is_none(),
            "the unknown tag must not have been inserted as its own key",
        );
        assert_eq!(
            map.get(UNKNOWN_ADMIN_COMMAND_TAG)
                .unwrap()
                .load(Ordering::Relaxed),
            2,
            "both unknown tags must have advanced the catch-all bucket",
        );
    }

    #[test]
    fn record_result_only_bumps_errors_on_err() {
        let em = ExportMetrics::default();
        em.record_result::<(), ()>(&Ok(()));
        assert_eq!(em.errors.load(Ordering::Relaxed), 0);
        em.record_result::<(), ()>(&Err(()));
        assert_eq!(em.errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn export_metrics_snapshot_serialization_shape() {
        let snap = ExportMetricsSnapshot {
            uid: 1,
            name: "/data".to_string(),
            requests: 0,
            bytes_read: 0,
            bytes_written: 0,
            errors: 0,
        };
        assert_eq!(
            serde_json::to_value(&snap).unwrap(),
            json!({
                "uid": 1,
                "name": "/data",
                "requests": 0,
                "bytes_read": 0,
                "bytes_written": 0,
                "errors": 0
            })
        );
    }

    /// Golden shape for the full snapshot of a fresh registry: every section
    /// present, all server / nfs_ops counters at 0, exports empty, and the
    /// admin `by_command` map carrying exactly the pre-populated known tags
    /// (finding 10) at 0. Pinning the literal locks both the JSON shape and
    /// the "all known keys present from construction" guarantee.
    #[test]
    fn fresh_metrics_snapshot_golden_shape() {
        let snap = Metrics::new().snapshot_json(0, Vec::new());
        assert_eq!(
            snap,
            json!({
                "server": {
                    "uptime_seconds": 0,
                    "rpc_requests_total": 0,
                    "rpc_errors_total": 0,
                    "rpc_decode_errors_total": 0,
                },
                "nfs_ops": {
                    "null": 0,
                    "getattr": 0,
                    "setattr": 0,
                    "lookup": 0,
                    "access": 0,
                    "readlink": 0,
                    "read": 0,
                    "write": 0,
                    "create": 0,
                    "mkdir": 0,
                    "symlink": 0,
                    "mknod": 0,
                    "remove": 0,
                    "rmdir": 0,
                    "rename": 0,
                    "link": 0,
                    "readdir": 0,
                    "readdirplus": 0,
                    "fsstat": 0,
                    "fsinfo": 0,
                    "pathconf": 0,
                    "commit": 0,
                },
                "exports": [],
                "admin": {
                    "commands_total": 0,
                    "by_command": {
                        "status": 0,
                        "version": 0,
                        "log-level-get": 0,
                        "log-level-set": 0,
                        "exports-list": 0,
                        "exports-add": 0,
                        "exports-remove": 0,
                        "exports-update": 0,
                        "config-show": 0,
                        "metrics": 0,
                        "<undecodable>": 0,
                        "<unknown>": 0,
                        "<frame-error>": 0,
                        "<unknown-command>": 0,
                    },
                },
            }),
        );
    }
}
