//! `exports remove` command — retire an export.
//!
//! The uid moves into `retired_uids` so an old client handle can't be
//! silently rerouted to a freshly-added export with the same uid.

use serde_json::json;

use crate::admin::{AdminContext, AdminResponse};
use crate::fsal::multi_export::ExportSelector;

/// `exports remove <selector>` — remove or pre-flight a removal.
pub fn handle(context: &AdminContext, selector: &ExportSelector, dry_run: bool) -> AdminResponse {
    if dry_run {
        return match context.filesystem.dry_run_remove(selector) {
            Ok((uid, name)) => AdminResponse::Ok {
                data: json!({
                    "dry_run": true,
                    "would_succeed": true,
                    "would_remove": { "uid": uid, "name": name },
                }),
            },
            Err(err) => AdminResponse::error(format!("exports remove (dry-run): {err:#}")),
        };
    }

    // `remove_export` returns (uid, name) derived from the same locked
    // snapshot that performs the swap, so there is no TOCTOU window
    // between a prefetch and the real mutation.
    match context.filesystem.remove_export(selector) {
        Ok(removed) => AdminResponse::Ok {
            data: json!({
                "uid": removed.uid,
                "name": removed.name,
            }),
        },
        Err(err) => AdminResponse::error(format!("exports remove: {err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::ExportRegistry;

    #[test]
    fn dry_run_returns_would_remove_without_mutating() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let sel = ExportSelector::Name("/data".to_string());
        match handle(&context, &sel, /*dry_run=*/ true) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["dry_run"], true);
                assert_eq!(data["would_remove"]["uid"], 1);
                assert_eq!(data["would_remove"]["name"], "/data");
            }
            AdminResponse::Err { error } => panic!("dry-run must succeed; got: {error}"),
        }
        // Still active.
        assert_eq!(context.filesystem.list_exports().len(), 1);
        assert!(context.filesystem.retired_uids().is_empty());
    }

    #[test]
    fn real_remove_retires_uid() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let sel = ExportSelector::Uid(1);
        match handle(&context, &sel, /*dry_run=*/ false) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["uid"], 1);
                assert_eq!(data["name"], "/data");
            }
            AdminResponse::Err { error } => panic!("remove must succeed; got: {error}"),
        }
        assert_eq!(context.filesystem.list_exports().len(), 0);
        assert_eq!(context.filesystem.retired_uids(), vec![1]);
    }

    #[test]
    fn remove_missing_export_errors() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let sel = ExportSelector::Name("/missing".to_string());
        match handle(&context, &sel, /*dry_run=*/ false) {
            AdminResponse::Err { error } => {
                assert!(error.contains("No export named"), "got: {error}");
            }
            AdminResponse::Ok { .. } => panic!("missing export must error"),
        }
    }
}
