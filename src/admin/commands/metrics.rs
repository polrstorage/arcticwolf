//! `metrics` command — a JSON snapshot of operational counters (Phase 7).
//!
//! Returns the lock-free counter snapshot assembled by
//! [`crate::metrics::Metrics::snapshot_json`]: server-wide RPC totals,
//! per-NFS-op counts, per-export I/O counters, and per-admin-command
//! counts. `uptime_seconds` is derived from [`AdminContext::start_time`]
//! rather than duplicated in the registry; the per-export rows come from the
//! filesystem router. The snapshot is NOT atomic across counters — see the
//! [`crate::metrics`] module doc.

use crate::admin::{AdminContext, AdminResponse};

/// `metrics` — snapshot the daemon's operational counters as JSON.
pub fn handle(context: &AdminContext) -> AdminResponse {
    let uptime_seconds = context.start_time.elapsed().as_secs();
    let exports = context.filesystem.export_metrics();
    let data = context.metrics.snapshot_json(uptime_seconds, exports);
    AdminResponse::Ok { data }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::AdminRequest;

    #[test]
    fn metrics_returns_ok_with_documented_shape() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let data = match handle(&context) {
            AdminResponse::Ok { data } => data,
            AdminResponse::Err { error } => panic!("metrics must return Ok; got: {error}"),
        };
        // Top-level sections.
        for section in ["server", "nfs_ops", "exports", "admin"] {
            assert!(data.get(section).is_some(), "missing `{section}`: {data}");
        }
        assert!(data["server"]["uptime_seconds"].is_u64());
        assert!(data["server"]["rpc_requests_total"].is_u64());
        assert!(data["nfs_ops"]["getattr"].is_u64());
        // The for_test context defines exactly one export (/data, uid 1).
        let exports = data["exports"].as_array().expect("exports array");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0]["uid"], 1);
        assert_eq!(exports[0]["name"], "/data");
        assert_eq!(exports[0]["requests"], 0);
        assert!(data["admin"]["by_command"].is_object());
    }

    #[test]
    fn metrics_reflects_admin_command_counts() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        // Simulate two accepted admin requests being recorded the way the
        // server's dispatch path does.
        context.metrics.admin.record("status");
        context.metrics.admin.record("metrics");
        let data = match handle(&context) {
            AdminResponse::Ok { data } => data,
            AdminResponse::Err { error } => panic!("metrics must return Ok; got: {error}"),
        };
        assert_eq!(data["admin"]["commands_total"], 2);
        assert_eq!(data["admin"]["by_command"]["status"], 1);
        assert_eq!(data["admin"]["by_command"]["metrics"], 1);
    }

    /// The `metrics` request tag is `"metrics"`, matching the audit/command
    /// tag the server records for this command.
    #[test]
    fn metrics_request_tag_is_metrics() {
        let tag = serde_json::to_value(AdminRequest::Metrics).unwrap()["command"]
            .as_str()
            .expect("command tag")
            .to_string();
        assert_eq!(tag, "metrics");
    }
}
