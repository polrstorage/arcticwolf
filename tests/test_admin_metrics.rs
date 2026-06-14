//! Integration tests for the Phase 7 admin `metrics` command.
//!
//! Spins up an in-process admin server backed by `AdminContext::for_test`,
//! sends a batch of admin requests (including `metrics` itself, which
//! counts), exercises the shared `MultiExportFilesystem` directly to move
//! the per-export I/O counters, then asserts the second `metrics` snapshot
//! reflects both surfaces.

use std::path::PathBuf;
use std::sync::Arc;

use arcticwolf::admin::{self, AdminContext};
use arcticwolf::fsal::{ExportRegistry, Filesystem, MultiExportFilesystem};

/// Bring up an admin server on a tempdir socket and hand back a clone of
/// the shared filesystem so the test can drive per-export counters. Every
/// returned resource must be kept alive for the test's duration.
async fn spawn() -> (
    PathBuf,
    Arc<MultiExportFilesystem>,
    tempfile::TempDir,
    tempfile::TempDir,
    admin::context::TestLogReloadGuard,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");

    let (context, export_dir, log_guard) = AdminContext::for_test();
    let fs = context.filesystem.clone();

    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    let server = tokio::spawn(admin::serve(listener, socket_path.clone(), context));
    (socket_path, fs, socket_dir, export_dir, log_guard, server)
}

#[tokio::test]
async fn metrics_counts_admin_commands_and_per_export_io() {
    let (socket_path, fs, _socket_dir, _export_dir, _log_guard, server) = spawn().await;

    // A handful of admin commands. `metrics` itself is counted.
    admin::client::fetch_status(&socket_path)
        .await
        .expect("status succeeds");
    admin::client::fetch_exports_list(&socket_path)
        .await
        .expect("exports-list succeeds");
    let _first = admin::client::fetch_metrics(&socket_path)
        .await
        .expect("metrics succeeds");

    // Exercise per-export counters directly against the shared filesystem:
    // create + write + read on /data (uid 1) → 3 requests, 11 bytes each way.
    let root = fs.root_handle_for("/data").expect("/data root handle");
    let file = fs
        .create(&root, "probe.bin", 0o644)
        .await
        .expect("create probe file");
    let written = fs.write(&file, 0, b"hello world").await.expect("write");
    assert_eq!(written, 11, "write must report the byte count");
    let read_back = fs.read(&file, 0, 11).await.expect("read");
    assert_eq!(read_back, b"hello world");

    // Second snapshot, taken after the I/O.
    let data = admin::client::fetch_metrics(&socket_path)
        .await
        .expect("metrics succeeds");

    // Admin counters: status, exports-list, metrics, metrics → >= 4.
    let total = data["admin"]["commands_total"]
        .as_u64()
        .expect("commands_total is a number");
    assert!(
        total >= 4,
        "expected at least 4 admin commands, got {total}: {data}",
    );
    assert_eq!(data["admin"]["by_command"]["status"], 1);
    assert_eq!(data["admin"]["by_command"]["exports-list"], 1);
    assert!(
        data["admin"]["by_command"]["metrics"]
            .as_u64()
            .expect("metrics command count")
            >= 1,
        "metrics command must be counted: {data}",
    );

    // Per-export I/O: /data is uid 1.
    let exports = data["exports"].as_array().expect("exports array");
    let data_export = exports
        .iter()
        .find(|e| e["uid"] == 1)
        .expect("uid 1 export present");
    assert_eq!(data_export["name"], "/data");
    assert!(
        data_export["requests"].as_u64().unwrap() >= 3,
        "create + write + read must each bump requests: {data_export}",
    );
    assert_eq!(data_export["bytes_written"], 11);
    assert_eq!(data_export["bytes_read"], 11);
    assert_eq!(data_export["errors"], 0);

    // Server-wide RPC counters are present and zero on this admin-only path
    // (no NFS/portmap/mount RPCs were issued through the RPC server).
    assert_eq!(data["server"]["rpc_requests_total"], 0);
    assert_eq!(data["server"]["rpc_errors_total"], 0);

    server.abort();
}
