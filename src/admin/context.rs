//! Shared state for admin request handlers.
//!
//! Handlers read the daemon start instant (for `uptime`), the resolved
//! listening ports, the live tracing reload handle (for `log-level`), the
//! filesystem backend (for `export_count`), and the original configuration.
//! Later phases extend this struct further (Phase 6 adds the audit-log
//! writer).
//!
//! `Clone` and `Default` are deliberately *not* derived: there is no
//! meaningful `Default` for the filesystem backend or the reload handle,
//! and the context is shared by reference rather than copied. Consumers
//! share it via `Arc<AdminContext>` (see [`AdminContext::shared`]).

use std::sync::Arc;
use std::time::Instant;

use tracing_subscriber::{EnvFilter, Registry, reload};

use crate::config::Config;
use crate::fsal::MultiExportFilesystem;

/// Live reload handle for the daemon's tracing `EnvFilter`.
///
/// The type parameters mirror the layer stack assembled in `main.rs`: the
/// reloadable layer is the `EnvFilter` itself, layered onto a `Registry`
/// subscriber. `log-level get` reads the current directive through it and
/// `log-level set` swaps it, so a single handle is shared (cheaply — it is
/// a `Weak` internally) across every admin connection.
pub type LogReloadHandle = reload::Handle<EnvFilter, Registry>;

/// Actual TCP ports the RPC services ended up listening on.
///
/// Resolved *after* `bind(2)` so that a configured port of `0` ("ask the OS
/// for any free port") is reported as the concrete port the daemon is
/// actually reachable on, not the literal `0` from the config file.
#[derive(Debug, Clone, Copy)]
pub struct ServerMetadata {
    /// Port the NFS (program 100003) service is listening on.
    pub nfs_port: u16,
    /// Port the MOUNT (program 100005) service is listening on.
    pub mount_port: u16,
    /// Port the PORTMAP (program 100000) service is listening on.
    pub portmap_port: u16,
}

/// Shared, read-only state handed to every admin command handler.
#[non_exhaustive]
pub struct AdminContext {
    /// Daemon process start instant. Used to compute `uptime_seconds`.
    pub start_time: Instant,
    /// Actual listening ports, resolved after bind.
    pub server_metadata: Arc<ServerMetadata>,
    /// Live reload handle for the tracing `EnvFilter`. `log-level get`
    /// reads the directive currently in effect; `log-level set` swaps it,
    /// retuning daemon verbosity without a restart. `status` also reads it
    /// so the reported `log_level` always reflects the live filter.
    pub log_reload: LogReloadHandle,
    /// Filesystem backend. Phase 2 uses it for `export_count`; Phase 5
    /// also calls the inherent admin mutation methods
    /// (`add_export`/`remove_export`/`update_export`) on it, so the
    /// context stores the concrete `MultiExportFilesystem` rather than
    /// `Arc<dyn NfsBackend>`.
    pub filesystem: Arc<MultiExportFilesystem>,
    /// Original daemon configuration (e.g. for `bind_address`).
    pub config: Arc<Config>,
}

impl AdminContext {
    /// Assemble the context from the daemon's already-resolved startup
    /// state. `main.rs` shares the result across per-connection tasks
    /// behind the returned `Arc` (see the type-level note on why `Clone`
    /// is deliberately not derived).
    pub fn shared(
        start_time: Instant,
        server_metadata: Arc<ServerMetadata>,
        log_reload: LogReloadHandle,
        filesystem: Arc<MultiExportFilesystem>,
        config: Arc<Config>,
    ) -> Arc<Self> {
        Arc::new(Self {
            start_time,
            server_metadata,
            log_reload,
            filesystem,
            config,
        })
    }
}

/// Per-process reload handle used by [`AdminContext::for_test`].
///
/// The backing layer is leaked exactly once via `OnceLock` so the handle
/// remains usable for the lifetime of the test process without piling up
/// one leaked layer per `for_test` call.
#[cfg(any(test, feature = "test-util"))]
static SHARED_TEST_LOG_RELOAD: std::sync::OnceLock<LogReloadHandle> = std::sync::OnceLock::new();

/// Serializes access to the shared test reload handle.
///
/// `for_test` returns a guard against this mutex so tests in the same
/// binary that observe or mutate the filter state (e.g. `handle_get`
/// after `handle_set`, or `status` asserting `log_level == "info"`)
/// don't race on the shared `OnceLock` handle. Tests in different test
/// binaries get separate copies of this mutex and are isolated by
/// construction.
#[cfg(any(test, feature = "test-util"))]
static TEST_LOG_RELOAD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Guard returned by [`AdminContext::for_test`] that serializes test
/// access to the shared tracing reload handle.
///
/// Keep it alive for the entire test body — dropping it releases the
/// lock and lets another test mutate the shared filter state.
#[cfg(any(test, feature = "test-util"))]
pub type TestLogReloadGuard = std::sync::MutexGuard<'static, ()>;

#[cfg(any(test, feature = "test-util"))]
impl AdminContext {
    /// Construct a minimal `AdminContext` suitable for tests: one
    /// tempdir-backed export, fixed ports, `info` log level. The returned
    /// [`tempfile::TempDir`] and [`TestLogReloadGuard`] must both be kept
    /// alive for as long as the context is used — the tempdir backs the
    /// export, and the guard serializes access to the shared tracing
    /// reload handle (see `TEST_LOG_RELOAD_LOCK`).
    ///
    /// The reload handle itself is shared across all `for_test` callers
    /// via a process-wide `OnceLock`; the underlying layer is leaked once
    /// per test process. The filter is reset to `info` on every call so
    /// each test sees a known starting state.
    ///
    /// Available under `#[cfg(test)]` for in-crate unit tests, or via the
    /// `test-util` feature for integration tests under `tests/`. Never
    /// shipped in release builds.
    pub fn for_test() -> (Arc<Self>, tempfile::TempDir, TestLogReloadGuard) {
        use crate::config::{BackendConfig, Config, ExportConfig};

        let tmp = tempfile::tempdir().expect("create tempdir for admin test context");
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

        // Acquire the shared-handle lock for the duration of the test.
        // Recover from poisoning so a panic in an earlier test doesn't
        // also poison every subsequent test in the same binary.
        let guard = TEST_LOG_RELOAD_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        // Build a standalone reload handle starting at `info`. The handle
        // only stays usable while the layer that owns its backing `Arc` is
        // alive; in the daemon that layer lives inside the process-global
        // subscriber installed by `main.rs`. Tests install no global
        // subscriber, so the layer is leaked *once per test process* via
        // `OnceLock` to give the handle the same process-lifetime backing
        // — without touching global tracing state. Subsequent `for_test`
        // callers receive a clone of the same handle (it's a `Weak`
        // internally, so cloning is cheap).
        let log_reload = SHARED_TEST_LOG_RELOAD
            .get_or_init(|| {
                let (filter_layer, handle) =
                    reload::Layer::<EnvFilter, Registry>::new(EnvFilter::new("info"));
                std::mem::forget(filter_layer);
                handle
            })
            .clone();

        // Reset the filter to `info` so each test sees a known starting
        // state regardless of what a previous test left behind. The
        // returned guard then keeps the next `for_test` caller out until
        // this test drops its tuple.
        log_reload
            .reload(EnvFilter::new("info"))
            .expect("reset shared test reload handle");

        let context = Self::shared(
            Instant::now(),
            metadata,
            log_reload,
            filesystem,
            Arc::new(config),
        );
        (context, tmp, guard)
    }
}
