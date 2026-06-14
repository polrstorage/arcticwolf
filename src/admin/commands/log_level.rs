//! `log-level` commands — inspect and live-reload the tracing filter.
//!
//! `get` reports the `EnvFilter` directive currently in effect; `set` swaps
//! it through the [`reload::Handle`](tracing_subscriber::reload::Handle) on
//! [`AdminContext`], changing daemon verbosity without a restart.

use std::str::FromStr;

use serde_json::json;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

use crate::admin::{AdminContext, AdminResponse};

/// `log-level get` — report the tracing filter directive in effect.
pub fn handle_get(context: &AdminContext) -> AdminResponse {
    match context.log_reload.with_current(|filter| filter.to_string()) {
        Ok(directive) => AdminResponse::Ok {
            data: json!({ "level": directive }),
        },
        // The handle holds a `Weak`; this only fails if the subscriber
        // owning the filter has been dropped, which cannot happen for the
        // process-global subscriber in a running daemon.
        Err(err) => AdminResponse::error(format!(
            "log-level get failed: reload subscriber gone: {err}"
        )),
    }
}

/// Validate one `EnvFilter` directive against this command's stricter
/// contract: each comma-separated piece is either a bare level
/// (`info`/`debug`/...) or a `target=level` pair whose right-hand side is
/// a recognized level. Target names are intentionally not validated —
/// `EnvFilter` accepts arbitrary target strings, and a user who wrote
/// `=` clearly intended a directive, so we trust the target name even
/// if it doesn't match a real module path.
///
/// This is intentionally stricter than `EnvFilter::try_new` (which would
/// accept a bare `tomato` as the target half of `tomato=trace`) and looser
/// than the previous `LevelFilter::from_str` pre-check (which rejected
/// valid per-module directives like `arcticwolf=debug`).
///
/// **Scope**: intentionally restricted to the v1 directive surface
/// — bare `LevelFilter` and `target=level`. `EnvFilter`'s full grammar
/// (span-field directives like `target[field=value]=level`, range
/// expressions, etc.) is out of scope; a future
/// `LogFilterSet { directive: String }` admin command could bypass this
/// validator and call `EnvFilter::try_new` directly for advanced
/// operators.
fn validate_filter_directive(input: &str) -> Result<(), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Filter directive cannot be empty".to_string());
    }

    for piece in trimmed.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            return Err(format!("Empty directive piece in '{input}'"));
        }
        if let Some(eq_pos) = piece.find('=') {
            let level_str = piece[eq_pos + 1..].trim();
            LevelFilter::from_str(level_str)
                .map_err(|_| format!("Invalid level '{level_str}' in directive '{piece}'"))?;
        } else {
            LevelFilter::from_str(piece).map_err(|_| {
                format!(
                    "Invalid log level '{piece}' (expected one of: error, warn, info, debug, trace, off, or a directive like 'target=level')"
                )
            })?;
        }
    }
    Ok(())
}

/// `log-level set <directive>` — swap the live tracing filter.
///
/// Accepts either a bare log level (`info`) or an `EnvFilter` directive of
/// comma-separated `target=level` pairs (`arcticwolf=debug,hyper=warn`).
/// Bare non-levels (`tomato`) and pieces whose right-hand side isn't a
/// recognized level (`tomato=banana`) are rejected and the active filter
/// is left untouched, per the issue #25 acceptance criteria.
///
/// Parsing the input straight through `EnvFilter` cannot enforce this
/// contract: `EnvFilter` accepts `tomato` as a *target* directive
/// (`tomato=trace`), so it would not reject the very input the AC
/// requires us to reject. [`validate_filter_directive`] applies the
/// per-piece rule before the reload.
pub fn handle_set(context: &AdminContext, level: &str) -> AdminResponse {
    if let Err(err) = validate_filter_directive(level) {
        return AdminResponse::error(err);
    }

    // Defense-in-depth: if `EnvFilter::try_new` rejects input that the
    // strict validator accepted (e.g. whitespace around `=`, or an empty
    // target like `=info`), preserve the previous filter and report the
    // disagreement. The reload is never reached, so the existing state
    // is intact. `to_string()` normalizes the directive (e.g. `DEBUG` ->
    // `debug`) so the echoed value matches a later `get`.
    let filter = match EnvFilter::try_new(level) {
        Ok(filter) => filter,
        Err(err) => {
            return AdminResponse::error(format!(
                "EnvFilter rejected directive '{level}' after validation: {err}"
            ));
        }
    };
    let directive = filter.to_string();

    match context.log_reload.reload(filter) {
        Ok(()) => AdminResponse::Ok {
            data: json!({ "level": directive }),
        },
        Err(err) => AdminResponse::error(format!(
            "log-level set failed: reload subscriber gone: {err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull the `level` string out of an `Ok` response, panicking with the
    /// error message if the response is an `Err`.
    fn ok_level(response: &AdminResponse) -> String {
        match response {
            AdminResponse::Ok { data } => data["level"]
                .as_str()
                .expect("`level` is a string")
                .to_string(),
            AdminResponse::Err { error } => panic!("expected Ok response, got err: {error}"),
        }
    }

    #[test]
    fn set_then_get_round_trips() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();

        let set = handle_set(&context, "debug");
        assert_eq!(ok_level(&set), "debug", "set must echo the new level");

        let get = handle_get(&context);
        assert_eq!(
            ok_level(&get),
            "debug",
            "get must observe the level set by the prior set",
        );
    }

    #[test]
    fn set_invalid_level_errors_and_leaves_filter_unchanged() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        // The `for_test` context starts at `info`.
        assert_eq!(ok_level(&handle_get(&context)), "info");

        match handle_set(&context, "tomato") {
            AdminResponse::Err { error } => assert!(
                error.contains("tomato"),
                "the error should name the rejected input; got: {error}",
            ),
            AdminResponse::Ok { .. } => panic!("`log-level set tomato` must be rejected"),
        }

        // The rejected `set` must not have disturbed the active filter.
        assert_eq!(
            ok_level(&handle_get(&context)),
            "info",
            "a rejected set must leave the filter unchanged",
        );
    }

    #[test]
    fn set_normalizes_case_in_the_echoed_level() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let set = handle_set(&context, "WARN");
        assert_eq!(
            ok_level(&set),
            "warn",
            "an uppercase level must round-trip as its normalized directive",
        );
    }

    /// `error` text must contain the rejected input, panic otherwise.
    fn expect_err_containing(response: AdminResponse, needle: &str) -> String {
        match response {
            AdminResponse::Err { error } => {
                assert!(
                    error.contains(needle),
                    "error should contain {needle:?}; got: {error}",
                );
                error
            }
            AdminResponse::Ok { .. } => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn validates_bare_level() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        assert_eq!(ok_level(&handle_set(&context, "info")), "info");
    }

    #[test]
    fn validates_per_module_directive() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let set = handle_set(&context, "arcticwolf=debug");
        // The echoed directive comes from `EnvFilter::to_string()` after
        // normalization; assert it round-trips through `get`.
        let directive = ok_level(&set);
        assert_eq!(
            ok_level(&handle_get(&context)),
            directive,
            "get must observe the per-module directive set",
        );
    }

    #[test]
    fn validates_compound_directive() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let set = handle_set(&context, "arcticwolf::nfs::read=trace,hyper=warn");
        let directive = ok_level(&set);
        assert_eq!(ok_level(&handle_get(&context)), directive);
    }

    #[test]
    fn rejects_bare_nonsense() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let before = ok_level(&handle_get(&context));
        expect_err_containing(handle_set(&context, "tomato"), "tomato");
        assert_eq!(
            ok_level(&handle_get(&context)),
            before,
            "a rejected bare nonsense input must leave the filter unchanged",
        );
    }

    #[test]
    fn rejects_invalid_level_in_directive() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        let before = ok_level(&handle_get(&context));
        expect_err_containing(handle_set(&context, "tomato=banana"), "banana");
        assert_eq!(
            ok_level(&handle_get(&context)),
            before,
            "a rejected directive must leave the filter unchanged",
        );
    }

    #[test]
    fn accepts_target_with_valid_level() {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        // The target name `tomato` is arbitrary — using `=` signals intent,
        // so we don't try to second-guess what targets are "real".
        let set = handle_set(&context, "tomato=debug");
        let directive = ok_level(&set);
        assert_eq!(ok_level(&handle_get(&context)), directive);
    }

    /// A compound directive must be rejected if *any* piece is invalid,
    /// even when other pieces are well-formed. The error message must
    /// name the offending piece so an operator can fix it without
    /// guessing.
    #[test]
    fn rejects_compound_directive_with_one_invalid_piece() {
        let err = validate_filter_directive("info,tomato")
            .expect_err("compound directive with a bare nonsense piece must be rejected");
        assert!(
            err.contains("tomato"),
            "error '{err}' should name the bad piece 'tomato'",
        );
    }

    #[test]
    fn rejects_compound_directive_with_invalid_level_in_one_directive() {
        let err = validate_filter_directive("arcticwolf=debug,hyper=banana")
            .expect_err("compound directive with one bad level must be rejected");
        assert!(
            err.contains("banana"),
            "error '{err}' should name the bad level 'banana'",
        );
    }

    /// Behavioral assertion for issue #25's acceptance criterion: a
    /// successful `handle_set` must actually change which events the
    /// `EnvFilter` admits, not just the directive string echoed back.
    ///
    /// We can't lean on `AdminContext::for_test` for this — its reload
    /// handle is intentionally never attached to a live subscriber. Instead
    /// we build a one-off `Registry` with the reload layer + an in-memory
    /// capture layer, drive `set_default` for the test's scope, and
    /// construct a minimal `AdminContext` over the same handle so the
    /// real handler runs against the real subscriber.
    #[test]
    fn handle_set_actually_changes_event_filtering() {
        use std::fmt::Write;
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        use tracing::{debug, warn};
        use tracing_subscriber::{Layer, Registry, layer::SubscriberExt, reload};

        use crate::admin::context::{LogReloadHandle, ServerMetadata};
        use crate::config::{BackendConfig, Config, ExportConfig};
        use crate::fsal::MultiExportFilesystem;

        #[derive(Clone, Default)]
        struct CaptureLayer(Arc<Mutex<Vec<String>>>);

        impl<S> Layer<S> for CaptureLayer
        where
            S: tracing::Subscriber,
        {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct Visit<'a>(&'a mut String);
                impl<'a> tracing::field::Visit for Visit<'a> {
                    fn record_str(&mut self, _: &tracing::field::Field, value: &str) {
                        self.0.push_str(value);
                    }
                    fn record_debug(
                        &mut self,
                        _: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        let _ = write!(self.0, "{value:?}");
                    }
                }
                let mut rendered = String::new();
                event.record(&mut Visit(&mut rendered));
                self.0
                    .lock()
                    .expect("capture layer mutex not poisoned")
                    .push(rendered);
            }
        }

        // Subscriber with the reloadable EnvFilter wired into the same
        // capture stack a test can read back from.
        let capture = CaptureLayer::default();
        let (filter_layer, handle) =
            reload::Layer::<EnvFilter, Registry>::new(EnvFilter::new("warn"));
        let subscriber = Registry::default().with(filter_layer).with(capture.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        // Minimal AdminContext that carries the live reload handle. We
        // build it by hand (rather than using `for_test`) so the handler
        // operates on the same subscriber the capture layer observes.
        let handle: LogReloadHandle = handle;
        let tmp = tempfile::tempdir().expect("tempdir for behavioral test");
        let config = Config {
            exports: vec![ExportConfig {
                name: "/data".to_string(),
                uid: 1,
                read_only: false,
                backend: BackendConfig::Local {
                    path: tmp.path().to_path_buf(),
                },
            }],
            ..Config::default()
        };
        let filesystem: Arc<MultiExportFilesystem> = Arc::new(
            MultiExportFilesystem::build_from_config(&config.exports)
                .expect("build test filesystem"),
        );
        let metadata = Arc::new(ServerMetadata {
            nfs_port: 2049,
            mount_port: 20048,
            portmap_port: 111,
        });
        let context = AdminContext::shared(
            Instant::now(),
            metadata,
            handle,
            filesystem,
            Arc::new(config),
            Arc::new(crate::admin::NoopAuditWriter),
            Arc::new(crate::metrics::Metrics::new()),
        );

        // Filter starts at `warn`: debug is suppressed, warn passes.
        debug!("debug-pre-reload");
        warn!("warn-pre-reload");

        // Reload through the public admin handler, exactly as a client would.
        match handle_set(&context, "debug") {
            AdminResponse::Ok { .. } => {}
            AdminResponse::Err { error } => panic!("handle_set('debug') failed: {error}"),
        }

        // Same debug event now passes the relaxed filter.
        debug!("debug-post-reload");

        let events = capture.0.lock().expect("capture mutex not poisoned");
        let combined = events.join("|");
        assert!(
            !combined.contains("debug-pre-reload"),
            "debug-pre-reload should be filtered by the initial `warn` directive; \
             captured events: {combined}",
        );
        assert!(
            combined.contains("warn-pre-reload"),
            "warn-pre-reload should pass the initial `warn` directive; \
             captured events: {combined}",
        );
        assert!(
            combined.contains("debug-post-reload"),
            "debug-post-reload should pass after `handle_set` swapped the filter to `debug`; \
             captured events: {combined}",
        );
    }
}
