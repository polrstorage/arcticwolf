//! Shared state for admin request handlers.
//!
//! Phase 2 is the first real consumer: `status` and `version` read the
//! daemon start instant, the resolved listening ports, the startup log
//! level, the filesystem backend (for the export count), and the original
//! configuration. Later phases extend this struct further — Phase 3 swaps
//! `startup_log_level` for a `tracing_subscriber` reload handle, Phase 6
//! adds the audit-log writer.
//!
//! `Clone` and `Default` are deliberately *not* derived: Phase 3's
//! `tracing_subscriber::reload::Handle` does not implement `Clone`, and
//! Phase 6's audit-log writer does not implement `Default`. Consumers share
//! the context via `Arc<AdminContext>` (see [`AdminContext::shared`]).

use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::fsal::NfsBackend;

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
    /// Configured log level at startup. Phase 3 replaces this with a
    /// `tracing_subscriber` reload handle.
    pub startup_log_level: String,
    /// Filesystem backend. Phase 2 uses it for `export_count`.
    pub filesystem: Arc<dyn NfsBackend>,
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
        startup_log_level: String,
        filesystem: Arc<dyn NfsBackend>,
        config: Arc<Config>,
    ) -> Arc<Self> {
        Arc::new(Self {
            start_time,
            server_metadata,
            startup_log_level,
            filesystem,
            config,
        })
    }
}

#[cfg(any(test, feature = "test-util"))]
impl AdminContext {
    /// Construct a minimal `AdminContext` suitable for tests: one
    /// tempdir-backed export, fixed ports, `info` log level. The returned
    /// [`tempfile::TempDir`] must be kept alive for as long as the context
    /// is used.
    ///
    /// Available under `#[cfg(test)]` for in-crate unit tests, or via the
    /// `test-util` feature for integration tests under `tests/`. Never
    /// shipped in release builds.
    pub fn for_test() -> (Arc<Self>, tempfile::TempDir) {
        use crate::config::{BackendConfig, Config, ExportConfig};
        use crate::fsal::MultiExportFilesystem;

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
        let filesystem: Arc<dyn NfsBackend> = Arc::new(
            MultiExportFilesystem::build_from_config(&config.exports)
                .expect("build test filesystem"),
        );
        let metadata = Arc::new(ServerMetadata {
            nfs_port: 2049,
            mount_port: 20048,
            portmap_port: 111,
        });
        let context = Self::shared(
            Instant::now(),
            metadata,
            "info".to_string(),
            filesystem,
            Arc::new(config),
        );
        (context, tmp)
    }
}
