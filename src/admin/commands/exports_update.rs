//! `exports update` command — mutate a live export.
//!
//! v1 only mutates `read_only`. Future fields land as additional optional
//! members on the wire variant and additional arguments here.

use serde_json::json;

use crate::admin::{AdminContext, AdminResponse};
use crate::fsal::multi_export::ExportSelector;

/// `exports update <selector> --read-only <bool>` — update or pre-flight.
pub fn handle(
    context: &AdminContext,
    selector: &ExportSelector,
    new_read_only: bool,
    dry_run: bool,
) -> AdminResponse {
    if dry_run {
        return match context.filesystem.dry_run_update(selector) {
            Ok((uid, name, current_ro)) => AdminResponse::Ok {
                data: json!({
                    "dry_run": true,
                    "would_succeed": true,
                    "would_update": {
                        "uid": uid,
                        "name": name,
                        "from": { "read_only": current_ro },
                        "to": { "read_only": new_read_only },
                    },
                }),
            },
            Err(err) => AdminResponse::error(format!("exports update (dry-run): {err:#}")),
        };
    }

    // `update_export` returns the (uid, name, prev_ro, new_ro) tuple
    // derived from the snapshot that was just swapped in. No second
    // `load()`, no TOCTOU window between the prefetch and the mutation.
    match context
        .filesystem
        .update_export(selector, Some(new_read_only))
    {
        Ok(updated) => AdminResponse::Ok {
            data: json!({
                "uid": updated.uid,
                "name": updated.name,
                "read_only": updated.new_read_only,
            }),
        },
        Err(err) => AdminResponse::error(format!("exports update: {err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::ExportRegistry;

    #[test]
    fn dry_run_returns_from_to_diff_without_mutating() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let sel = ExportSelector::Name("/data".to_string());
        match handle(&context, &sel, /*new=*/ true, /*dry_run=*/ true) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["dry_run"], true);
                assert_eq!(data["would_update"]["uid"], 1);
                assert_eq!(data["would_update"]["from"]["read_only"], false);
                assert_eq!(data["would_update"]["to"]["read_only"], true);
            }
            AdminResponse::Err { error } => panic!("dry-run must succeed; got: {error}"),
        }
        // State unchanged.
        let infos = context.filesystem.list_exports();
        assert!(!infos[0].read_only);
    }

    #[test]
    fn real_update_flips_read_only() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let sel = ExportSelector::Name("/data".to_string());
        match handle(&context, &sel, /*new=*/ true, /*dry_run=*/ false) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["read_only"], true);
            }
            AdminResponse::Err { error } => panic!("update must succeed; got: {error}"),
        }
        let infos = context.filesystem.list_exports();
        assert!(infos[0].read_only);
    }

    #[test]
    fn update_missing_export_errors() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let sel = ExportSelector::Uid(999);
        match handle(&context, &sel, true, false) {
            AdminResponse::Err { error } => {
                assert!(error.contains("No export with uid"), "got: {error}")
            }
            AdminResponse::Ok { .. } => panic!("missing export must error"),
        }
    }
}
