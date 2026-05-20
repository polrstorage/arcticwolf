//! `status` command — a daemon health snapshot.

use serde_json::json;

use crate::admin::{AdminContext, AdminResponse};

/// Build the `status` response from the live daemon context.
///
/// The reported ports come from [`AdminContext::server_metadata`], which is
/// resolved after `bind(2)` — so a configured port of `0` (dynamic
/// allocation) is reported as the concrete port in use.
pub fn handle(context: &AdminContext) -> AdminResponse {
    let data = json!({
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": context.start_time.elapsed().as_secs(),
        "bind_address": context.config.server.bind_address,
        "nfs_port": context.server_metadata.nfs_port,
        "mount_port": context.server_metadata.mount_port,
        "portmap_port": context.server_metadata.portmap_port,
        "log_level": context.startup_log_level,
        "export_count": context.filesystem.list_exports().len(),
    });
    AdminResponse::Ok { data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_returns_ok_with_all_documented_fields() {
        let (context, _tmp) = AdminContext::for_test();
        let data = match handle(&context) {
            AdminResponse::Ok { data } => data,
            AdminResponse::Err { error } => panic!("status must return Ok; got err: {error}"),
        };
        for field in [
            "daemon_version",
            "uptime_seconds",
            "bind_address",
            "nfs_port",
            "mount_port",
            "portmap_port",
            "log_level",
            "export_count",
        ] {
            assert!(
                data.get(field).is_some(),
                "status response missing `{field}`: {data}",
            );
        }
        assert_eq!(data["daemon_version"], env!("CARGO_PKG_VERSION"));
        assert!(data["uptime_seconds"].is_u64());
        assert_eq!(data["log_level"], "info");
        assert_eq!(
            data["export_count"], 1,
            "the for_test context defines exactly one export",
        );
    }
}
