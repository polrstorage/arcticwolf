//! Integration tests for the admin `exports` and `config-show` commands.
//!
//! Spins up the admin server in-process over a tempdir socket (same code
//! path `arcticwolfctl` uses) and asserts the wire round-trip and live
//! mutations.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arcticwolf::admin::{self, AdminContext, AdminRequest, AdminResponse};
use arcticwolf::config::{BackendConfig, ExportConfig};
use arcticwolf::fsal::multi_export::ExportSelector;
use arcticwolf::shutdown::InFlight;
use tokio_util::sync::CancellationToken;

fn cfg(name: &str, uid: u32, path: PathBuf, read_only: bool) -> ExportConfig {
    ExportConfig {
        name: name.to_string(),
        uid,
        read_only,
        backend: BackendConfig::Local { path },
    }
}

async fn spawn() -> (
    PathBuf,
    tempfile::TempDir,
    tempfile::TempDir,
    admin::context::TestLogReloadGuard,
    CancellationToken,
) {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");

    let (context, export_dir, log_guard) = AdminContext::for_test();
    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    // Detach the serve task; the returned token drives shutdown via
    // `token.cancel()` — the same path production uses (Fix 18).
    let token = CancellationToken::new();
    let _server = tokio::spawn(admin::serve_with_shutdown(
        listener,
        socket_path.clone(),
        context,
        token.clone(),
        Arc::new(InFlight::new()),
    ));
    (socket_path, socket_dir, export_dir, log_guard, token)
}

#[tokio::test]
async fn exports_list_returns_seeded_exports_and_no_retired() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;

    let data = tokio::time::timeout(
        Duration::from_secs(5),
        admin::client::fetch_exports_list(&socket_path),
    )
    .await
    .expect("list did not time out")
    .expect("list succeeds");

    let exports = data["exports"].as_array().expect("exports array");
    assert_eq!(exports.len(), 1);
    assert_eq!(exports[0]["name"], "/data");
    assert_eq!(exports[0]["uid"], 1);
    assert_eq!(exports[0]["fsal"], "local");
    assert_eq!(exports[0]["read_only"], false);
    let retired = data["retired_uids"].as_array().expect("retired_uids array");
    assert!(retired.is_empty());
    token.cancel();
}

#[tokio::test]
async fn exports_add_happy_path_appears_in_list() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    let new_dir = tempfile::tempdir().expect("new export dir");

    let add = admin::client::exports_add(
        &socket_path,
        cfg("/extra", 7, new_dir.path().to_path_buf(), false),
        false,
    )
    .await
    .expect("add succeeds");
    assert_eq!(add["name"], "/extra");
    assert_eq!(add["uid"], 7);

    let list = admin::client::fetch_exports_list(&socket_path)
        .await
        .expect("list succeeds");
    let names: Vec<&str> = list["exports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"/data"), "list missing /data: {list}");
    assert!(names.contains(&"/extra"), "list missing /extra: {list}");
    token.cancel();
}

#[tokio::test]
async fn exports_add_rejects_duplicate_uid_name_and_retired() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    let dup_dir = tempfile::tempdir().expect("dup tempdir");

    // Duplicate uid (1 is taken by /data).
    let resp = admin::client::send_request(
        &socket_path,
        &AdminRequest::ExportsAdd {
            config: cfg("/other", 1, dup_dir.path().to_path_buf(), false),
            dry_run: false,
        },
    )
    .await
    .expect("request completes");
    match resp {
        AdminResponse::Err { error } => assert!(error.contains("already in use"), "got: {error}"),
        AdminResponse::Ok { .. } => panic!("duplicate uid must error"),
    }

    // Duplicate name (/data is taken).
    let resp = admin::client::send_request(
        &socket_path,
        &AdminRequest::ExportsAdd {
            config: cfg("/data", 99, dup_dir.path().to_path_buf(), false),
            dry_run: false,
        },
    )
    .await
    .expect("request completes");
    match resp {
        AdminResponse::Err { error } => assert!(error.contains("already in use"), "got: {error}"),
        AdminResponse::Ok { .. } => panic!("duplicate name must error"),
    }

    // Retire uid 1, then try to readd it (under a fresh name).
    admin::client::exports_remove(&socket_path, ExportSelector::Uid(1), false)
        .await
        .expect("remove succeeds");
    let resp = admin::client::send_request(
        &socket_path,
        &AdminRequest::ExportsAdd {
            config: cfg("/new", 1, dup_dir.path().to_path_buf(), false),
            dry_run: false,
        },
    )
    .await
    .expect("request completes");
    match resp {
        AdminResponse::Err { error } => assert!(error.contains("retired"), "got: {error}"),
        AdminResponse::Ok { .. } => panic!("retired uid must error"),
    }
    token.cancel();
}

#[tokio::test]
async fn exports_remove_by_name_disappears_and_uid_is_retired() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    let resp = admin::client::exports_remove(
        &socket_path,
        ExportSelector::Name("/data".to_string()),
        false,
    )
    .await
    .expect("remove succeeds");
    assert_eq!(resp["uid"], 1);
    assert_eq!(resp["name"], "/data");

    let list = admin::client::fetch_exports_list(&socket_path)
        .await
        .expect("list succeeds");
    assert!(list["exports"].as_array().unwrap().is_empty());
    let retired: Vec<u64> = list["retired_uids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(retired, vec![1]);
    token.cancel();
}

#[tokio::test]
async fn exports_remove_by_uid_succeeds() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    let resp = admin::client::exports_remove(&socket_path, ExportSelector::Uid(1), false)
        .await
        .expect("remove by uid succeeds");
    assert_eq!(resp["uid"], 1);
    assert_eq!(resp["name"], "/data");
    token.cancel();
}

#[tokio::test]
async fn exports_update_flips_read_only() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    admin::client::exports_update(
        &socket_path,
        ExportSelector::Name("/data".to_string()),
        true,
        false,
    )
    .await
    .expect("update succeeds");

    let list = admin::client::fetch_exports_list(&socket_path)
        .await
        .expect("list succeeds");
    let entry = &list["exports"][0];
    assert_eq!(entry["read_only"], true);
    token.cancel();
}

#[tokio::test]
async fn exports_update_missing_export_errors() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    let resp = admin::client::send_request(
        &socket_path,
        &AdminRequest::ExportsUpdate {
            selector: ExportSelector::Uid(999),
            read_only: true,
            dry_run: false,
        },
    )
    .await
    .expect("request completes");
    match resp {
        AdminResponse::Err { error } => {
            assert!(error.contains("No export with uid"), "got: {error}");
        }
        AdminResponse::Ok { .. } => panic!("missing uid must error"),
    }
    token.cancel();
}

#[tokio::test]
async fn dry_run_does_not_mutate_state_across_add_remove_update() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;

    let before = admin::client::fetch_exports_list(&socket_path)
        .await
        .expect("list before");

    // dry-run add
    let new_dir = tempfile::tempdir().expect("new tempdir");
    let resp = admin::client::exports_add(
        &socket_path,
        cfg("/extra", 7, new_dir.path().to_path_buf(), false),
        true,
    )
    .await
    .expect("dry-run add succeeds");
    assert_eq!(resp["dry_run"], true);
    assert_eq!(resp["would_succeed"], true);
    // Symmetric with would_remove/would_update — the dry-run echoes the
    // requested name/uid back to the caller.
    assert_eq!(resp["would_add"]["name"], "/extra");
    assert_eq!(resp["would_add"]["uid"], 7);

    // dry-run remove
    let resp = admin::client::exports_remove(
        &socket_path,
        ExportSelector::Name("/data".to_string()),
        true,
    )
    .await
    .expect("dry-run remove succeeds");
    assert_eq!(resp["dry_run"], true);
    assert_eq!(resp["would_remove"]["uid"], 1);
    assert_eq!(resp["would_remove"]["name"], "/data");

    // dry-run update
    let resp = admin::client::exports_update(
        &socket_path,
        ExportSelector::Name("/data".to_string()),
        true,
        true,
    )
    .await
    .expect("dry-run update succeeds");
    assert_eq!(resp["dry_run"], true);
    assert_eq!(resp["would_update"]["from"]["read_only"], false);
    assert_eq!(resp["would_update"]["to"]["read_only"], true);

    // State is unchanged across all three dry-runs.
    let after = admin::client::fetch_exports_list(&socket_path)
        .await
        .expect("list after");
    assert_eq!(before, after, "state must be unchanged after dry-runs");
    token.cancel();
}

#[tokio::test]
async fn config_show_returns_startup_config() {
    let (socket_path, _sd, _xd, _g, token) = spawn().await;
    let data = admin::client::fetch_config_show(&socket_path)
        .await
        .expect("config-show succeeds");

    // The `for_test` context seeds one export at /data, uid 1.
    assert!(data["server"].is_object());
    assert_eq!(data["exports"][0]["name"], "/data");
    assert_eq!(data["exports"][0]["uid"], 1);
    assert_eq!(data["exports"][0]["backend"], "local");

    // Even after a runtime mutation, config-show keeps returning the
    // startup view — that's the load-bearing contract of the command.
    admin::client::exports_update(
        &socket_path,
        ExportSelector::Name("/data".to_string()),
        true,
        false,
    )
    .await
    .expect("update succeeds");

    let after = admin::client::fetch_config_show(&socket_path)
        .await
        .expect("config-show after update");
    assert_eq!(
        after["exports"][0]["read_only"], false,
        "config-show must reflect the startup config, not live state",
    );
    token.cancel();
}
