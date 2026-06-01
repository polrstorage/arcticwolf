//! `exports list` command — snapshot the live export set.
//!
//! Returns every active export (uid, name, fsal, read_only) along with the
//! list of retired uids — uids that were live during this daemon's run and
//! have since been removed. The retirement set is useful for operators
//! debugging "I removed this and it didn't come back" or wondering why
//! `exports add --uid <retired>` was rejected.

use serde_json::json;

use crate::admin::{AdminContext, AdminResponse};
use crate::fsal::ExportRegistry;

/// `exports list` — return the active exports + retired uids.
pub fn handle(context: &AdminContext) -> AdminResponse {
    let exports = context.filesystem.list_exports();
    let retired = context.filesystem.retired_uids();
    AdminResponse::Ok {
        data: json!({
            "exports": exports,
            "retired_uids": retired,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_returns_seeded_export_and_no_retired() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let data = match handle(&context) {
            AdminResponse::Ok { data } => data,
            AdminResponse::Err { error } => panic!("list must return Ok; got: {error}"),
        };
        let exports = data["exports"].as_array().expect("exports is an array");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0]["name"], "/data");
        assert_eq!(exports[0]["uid"], 1);
        assert_eq!(exports[0]["read_only"], false);
        assert_eq!(exports[0]["fsal"], "local");
        let retired = data["retired_uids"]
            .as_array()
            .expect("retired_uids is an array");
        assert!(retired.is_empty());
    }
}
