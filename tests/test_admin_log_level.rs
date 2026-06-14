//! Integration test for the admin `log-level` commands.
//!
//! Drives the admin server in-process over a tempdir socket (the same code
//! path `arcticwolfctl` uses) and exercises the `log-level get`/`set` round
//! trip plus the rejected-input contract from issue #25: an unrecognized
//! level must error and leave the active filter unchanged.

use std::sync::Arc;
use std::time::Duration;

use arcticwolf::admin::{self, AdminContext, AdminRequest, AdminResponse};
use arcticwolf::shutdown::InFlight;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn log_level_set_then_get_round_trips_over_the_socket() {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");

    let (context, _export_dir, _log_guard) = AdminContext::for_test();
    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    let token = CancellationToken::new();
    let _server = tokio::spawn(admin::serve_with_shutdown(
        listener,
        socket_path.clone(),
        context,
        token.clone(),
        Arc::new(InFlight::new()),
    ));

    // The `for_test` context starts at `info`.
    let initial = tokio::time::timeout(
        Duration::from_secs(5),
        admin::client::fetch_log_level(&socket_path),
    )
    .await
    .expect("get did not time out")
    .expect("log-level get succeeds");
    assert_eq!(initial["level"], "info");

    // `set debug` echoes the new directive...
    let set = tokio::time::timeout(
        Duration::from_secs(5),
        admin::client::set_log_level(&socket_path, "debug"),
    )
    .await
    .expect("set did not time out")
    .expect("log-level set succeeds");
    assert_eq!(set["level"], "debug");

    // ...and a subsequent `get` observes it: the live filter was swapped.
    let after = admin::client::fetch_log_level(&socket_path)
        .await
        .expect("log-level get after set succeeds");
    assert_eq!(after["level"], "debug");

    token.cancel();
}

#[tokio::test]
async fn log_level_set_rejects_invalid_level_and_leaves_filter_unchanged() {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");

    let (context, _export_dir, _log_guard) = AdminContext::for_test();
    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    let token = CancellationToken::new();
    let _server = tokio::spawn(admin::serve_with_shutdown(
        listener,
        socket_path.clone(),
        context,
        token.clone(),
        Arc::new(InFlight::new()),
    ));

    // An unrecognized level must come back as an error response...
    let response = admin::client::send_request(
        &socket_path,
        &AdminRequest::LogLevelSet {
            level: "tomato".to_string(),
        },
    )
    .await
    .expect("the request itself completes");
    match response {
        AdminResponse::Err { error } => assert!(
            error.contains("tomato"),
            "the error should name the rejected input; got: {error}",
        ),
        AdminResponse::Ok { .. } => panic!("`log-level set tomato` must be rejected"),
    }

    // ...and must not have disturbed the active filter, which stays `info`.
    let still = admin::client::fetch_log_level(&socket_path)
        .await
        .expect("log-level get after rejected set succeeds");
    assert_eq!(
        still["level"], "info",
        "a rejected `set` must leave the filter unchanged",
    );

    token.cancel();
}
