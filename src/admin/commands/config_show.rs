//! `config show` command — dump the daemon's startup configuration.
//!
//! Returns the original `Config` loaded at boot, not the live mutated
//! export set (admin mutations affect [`crate::fsal::MultiExportFilesystem`]
//! state; `config-show` is intentionally the *startup* view so operators
//! can diff what they configured against what's now live).
//!
//! Phase 5 returns the config unredacted. Sensitive-field redaction is a
//! follow-up when non-local FSALs land (S3 access keys, Ceph secrets,
//! etc.) — see the v1 plan in issue #25.

use crate::admin::{AdminContext, AdminResponse};

/// `config show` — return the startup config as a JSON value.
pub fn handle(context: &AdminContext) -> AdminResponse {
    match serde_json::to_value(context.config.as_ref()) {
        Ok(data) => AdminResponse::Ok { data },
        Err(err) => AdminResponse::error(format!("config show: failed to serialize config: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_startup_config_keyed_by_top_level_sections() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let data = match handle(&context) {
            AdminResponse::Ok { data } => data,
            AdminResponse::Err { error } => panic!("config-show must return Ok; got: {error}"),
        };
        // Top-level shape mirrors `Config`'s struct: server / exports /
        // logging / admin. We don't lock the exact values here (the test
        // helper builds an ephemeral config); just pin the shape.
        assert!(data.get("server").is_some(), "missing `server`: {data}");
        assert!(data.get("exports").is_some(), "missing `exports`: {data}");
        assert!(data.get("logging").is_some(), "missing `logging`: {data}");
        assert!(data.get("admin").is_some(), "missing `admin`: {data}");
        // The one seeded export must round-trip.
        let exports = data["exports"].as_array().expect("exports array");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0]["name"], "/data");
        assert_eq!(exports[0]["uid"], 1);
    }
}
