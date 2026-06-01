//! `exports add` command — install a new export at runtime.
//!
//! Delegates the heavy lifting to [`MultiExportFilesystem::add_export`],
//! which handles input validation, duplicate detection (active + retired
//! uids), and atomic snapshot rebuild. `dry_run = true` re-uses
//! [`MultiExportFilesystem::dry_run_add`] for the same checks without
//! touching the live snapshot, so operators can pre-flight an `add`
//! before committing.

use serde_json::json;

use crate::admin::{AdminContext, AdminResponse};
use crate::config::ExportConfig;

/// `exports add <config>` — add or pre-flight an export.
pub fn handle(context: &AdminContext, config: &ExportConfig, dry_run: bool) -> AdminResponse {
    if dry_run {
        return match context.filesystem.dry_run_add(config) {
            // Echo the requested name/uid back in a `would_add` block so
            // the dry-run shape is symmetric with `would_remove` /
            // `would_update`. The values come straight from the request
            // — `dry_run_add` validated them as a precondition for `Ok`.
            Ok(()) => AdminResponse::Ok {
                data: json!({
                    "dry_run": true,
                    "would_succeed": true,
                    "would_add": { "name": config.name, "uid": config.uid },
                }),
            },
            Err(err) => AdminResponse::error(format!("exports add (dry-run): {err:#}")),
        };
    }

    match context.filesystem.add_export(config) {
        Ok(()) => AdminResponse::Ok {
            data: json!({
                "name": config.name,
                "uid": config.uid,
            }),
        },
        Err(err) => AdminResponse::error(format!("exports add: {err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig;
    use crate::fsal::ExportRegistry;

    fn cfg(name: &str, uid: u32, path: std::path::PathBuf, read_only: bool) -> ExportConfig {
        ExportConfig {
            name: name.to_string(),
            uid,
            read_only,
            backend: BackendConfig::Local { path },
        }
    }

    #[test]
    fn dry_run_returns_would_succeed_without_mutating() {
        let (context, tmp, _log_guard) = AdminContext::for_test();
        // The for_test snapshot has one export, uid 1, name "/data".
        let new_dir = tempfile::tempdir().expect("tempdir");
        let new = cfg("/extra", 7, new_dir.path().to_path_buf(), false);
        let resp = handle(&context, &new, /*dry_run=*/ true);
        match resp {
            AdminResponse::Ok { data } => {
                assert_eq!(data["dry_run"], true);
                assert_eq!(data["would_succeed"], true);
                // Symmetric with would_remove/would_update.
                assert_eq!(data["would_add"]["name"], "/extra");
                assert_eq!(data["would_add"]["uid"], 7);
            }
            AdminResponse::Err { error } => panic!("dry-run must succeed; got: {error}"),
        }
        // State unchanged: still exactly one export.
        assert_eq!(context.filesystem.list_exports().len(), 1);
        // Keep tmp alive.
        drop(tmp);
    }

    #[test]
    fn dry_run_rejects_duplicate_uid() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let new_dir = tempfile::tempdir().expect("tempdir");
        // uid 1 already taken by the seeded /data export.
        let dup = cfg("/other", 1, new_dir.path().to_path_buf(), false);
        match handle(&context, &dup, /*dry_run=*/ true) {
            AdminResponse::Err { error } => {
                assert!(error.contains("already in use"), "got: {error}")
            }
            AdminResponse::Ok { .. } => panic!("duplicate uid must be rejected"),
        }
    }

    #[test]
    fn real_add_mutates_snapshot() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let new_dir = tempfile::tempdir().expect("tempdir");
        let new = cfg("/extra", 8, new_dir.path().to_path_buf(), false);
        match handle(&context, &new, /*dry_run=*/ false) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["name"], "/extra");
                assert_eq!(data["uid"], 8);
            }
            AdminResponse::Err { error } => panic!("add must succeed; got: {error}"),
        }
        assert_eq!(context.filesystem.list_exports().len(), 2);
    }
}
