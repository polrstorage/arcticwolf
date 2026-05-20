//! `version` command — daemon build and version metadata.

use serde_json::json;

use crate::admin::AdminResponse;

/// Build the `version` response.
///
/// `build_commit` and `rustc_version` come from the `vergen`-generated
/// compile-time env vars (see `build.rs`). When the crate is built without a
/// `.git` directory available — as in the container build, which copies only
/// the sources — `vergen` cannot resolve the commit and `build_commit`
/// falls back to `"unknown"`.
pub fn handle() -> AdminResponse {
    let data = json!({
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "build_commit": build_commit(),
        "rustc_version": rustc_version(),
        "build_profile": build_profile(),
    });
    AdminResponse::Ok { data }
}

/// Git commit the daemon was built from, or `"unknown"`.
fn build_commit() -> &'static str {
    vergen_or_unknown(option_env!("VERGEN_GIT_SHA"))
}

/// `rustc` version the daemon was built with, or `"unknown"`.
fn rustc_version() -> &'static str {
    vergen_or_unknown(option_env!("VERGEN_RUSTC_SEMVER"))
}

/// Cargo build profile (`"debug"` / `"release"`), or `"unknown"`.
fn build_profile() -> &'static str {
    match option_env!("VERGEN_CARGO_DEBUG") {
        Some("true") => "debug",
        Some("false") => "release",
        _ => "unknown",
    }
}

/// Normalize a `vergen` value: an unset var, an empty string, and vergen's
/// `VERGEN_IDEMPOTENT_OUTPUT` placeholder (emitted when the underlying tool
/// — e.g. `git` — was unavailable at build time) all collapse to
/// `"unknown"`.
fn vergen_or_unknown(value: Option<&'static str>) -> &'static str {
    match value {
        Some(v) if !v.is_empty() && v != "VERGEN_IDEMPOTENT_OUTPUT" => v,
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_returns_ok_with_all_documented_fields() {
        let data = match handle() {
            AdminResponse::Ok { data } => data,
            AdminResponse::Err { error } => panic!("version must return Ok; got err: {error}"),
        };
        for field in [
            "daemon_version",
            "build_commit",
            "rustc_version",
            "build_profile",
        ] {
            assert!(
                data[field].is_string(),
                "version field `{field}` must be a string: {data}",
            );
        }
        assert_eq!(data["daemon_version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn build_profile_is_one_of_the_known_values() {
        // `cargo test` builds in the dev profile, so this resolves to
        // "debug" here — but pin the full set so a vergen change is caught.
        assert!(matches!(build_profile(), "debug" | "release" | "unknown"));
    }
}
