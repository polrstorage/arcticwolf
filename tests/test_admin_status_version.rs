//! Integration test for the admin `status` and `version` commands.
//!
//! Spins up the admin server in-process over a tempdir socket, drives it
//! with the in-process client (the same code path `arcticwolfctl` uses),
//! and asserts the response JSON shape for both commands as well as the
//! `--json` / human render paths.

use std::time::Duration;

use arcticwolf::admin::{self, AdminContext};

#[tokio::test]
async fn status_and_version_round_trip_over_the_socket() {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");

    let (context, _export_dir) = AdminContext::for_test();
    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    let server = tokio::spawn(admin::serve(listener, socket_path.clone(), context));

    // --- status ---
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        admin::client::fetch_status(&socket_path),
    )
    .await
    .expect("status did not time out")
    .expect("status command succeeds");

    for field in [
        "daemon_version",
        "uptime_seconds",
        "bind_address",
        "nfs_port",
        "mount_port",
        "portmap_port",
        "log_level",
        "export_count",
    ] {
        assert!(
            status.get(field).is_some(),
            "status response missing `{field}`: {status}",
        );
    }
    assert_eq!(status["daemon_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["export_count"], 1);
    assert_eq!(status["nfs_port"], 2049);
    assert_eq!(status["mount_port"], 20048);
    assert_eq!(status["portmap_port"], 111);
    assert_eq!(status["log_level"], "info");
    assert!(status["uptime_seconds"].is_u64());

    // --- version ---
    let version = tokio::time::timeout(
        Duration::from_secs(5),
        admin::client::fetch_version(&socket_path),
    )
    .await
    .expect("version did not time out")
    .expect("version command succeeds");

    for field in [
        "daemon_version",
        "build_commit",
        "rustc_version",
        "build_profile",
    ] {
        assert!(
            version[field].is_string(),
            "version response missing string `{field}`: {version}",
        );
    }
    assert_eq!(version["daemon_version"], env!("CARGO_PKG_VERSION"));

    // --- both render modes ---
    let human_status = admin::client::render_status(&status, false).expect("render human status");
    assert!(human_status.contains("Daemon version:"));
    assert!(human_status.contains("Exports:"));
    // Human output must not leak JSON quoting around string values.
    assert!(!human_status.contains("\"daemon_version\""));

    let json_status = admin::client::render_status(&status, true).expect("render json status");
    let reparsed: serde_json::Value =
        serde_json::from_str(&json_status).expect("json status reparses");
    assert_eq!(reparsed, status);

    let human_version =
        admin::client::render_version(&version, false).expect("render human version");
    assert!(human_version.contains("Build commit:"));
    let json_version = admin::client::render_version(&version, true).expect("render json version");
    assert!(json_version.contains("\"rustc_version\""));

    server.abort();
}

#[tokio::test]
async fn fetch_status_errors_when_socket_is_absent() {
    // The client must surface a connection failure as an error rather than
    // hang — `arcticwolfctl status` relies on this to exit non-zero.
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.sock");
    let result = admin::client::fetch_status(&missing).await;
    assert!(result.is_err(), "connecting to a missing socket must error");
}
