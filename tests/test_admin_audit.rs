//! Integration tests for the Phase 6 admin audit log.
//!
//! Spins up an in-process admin server bound to a tempdir socket, points
//! its [`AdminContext`] at a `FileAuditWriter` over a tempfile, sends a
//! batch of admin requests, and asserts that each one produced exactly
//! one well-formed JSONL audit record.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arcticwolf::admin::{self, AdminContext, AdminRequest, AdminResponse, FileAuditWriter};
use arcticwolf::config::{BackendConfig, ExportConfig};
use arcticwolf::fsal::multi_export::ExportSelector;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

fn cfg(name: &str, uid: u32, path: PathBuf, read_only: bool) -> ExportConfig {
    ExportConfig {
        name: name.to_string(),
        uid,
        read_only,
        backend: BackendConfig::Local { path },
    }
}

/// Mirrors the daemon's `LengthDelimitedCodec` configuration so the test's
/// raw-frame path (used for the malformed-JSON case) writes bytes the
/// server actually decodes. Inlined here rather than reaching into a
/// crate-private helper so a wire-format drift fails the test loudly.
fn client_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .max_frame_length(1024 * 1024)
        .new_codec()
}

/// Set up an in-process admin server backed by a `FileAuditWriter` over a
/// tempfile. The returned tuple keeps every borrowed resource alive for
/// the duration of the test:
///   - `socket_path`        — what the client connects to
///   - `audit_path`         — what the test reads back to assert events
///   - `_socket_dir`        — owns the socket tempdir
///   - `_audit_dir`         — owns the audit-log tempdir
///   - `_export_dir`        — owns the for_test() export tempdir
///   - `_log_guard`         — serializes shared tracing reload handle
///   - `server`             — the spawned serve() task; aborted by caller
async fn spawn_with_audit() -> (
    PathBuf,
    PathBuf,
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    admin::context::TestLogReloadGuard,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");

    let audit_dir = tempfile::tempdir().expect("audit tempdir");
    let audit_path = audit_dir.path().join("audit.jsonl");
    let audit_writer = Arc::new(FileAuditWriter::open(&audit_path).expect("open audit writer"));

    let (context, export_dir, log_guard) = AdminContext::for_test_with_audit(audit_writer);
    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    let server = tokio::spawn(admin::serve(listener, socket_path.clone(), context));
    (
        socket_path,
        audit_path,
        socket_dir,
        audit_dir,
        export_dir,
        log_guard,
        server,
    )
}

/// Read the audit file repeatedly until it has at least `min_lines`
/// non-empty lines or the 2s deadline expires. The writer task lives on a
/// `tokio::spawn` we can't join from here, so we cross the boundary by
/// observing the on-disk file.
async fn wait_for_audit_lines(path: &std::path::Path, min_lines: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(contents) = tokio::fs::read_to_string(path).await {
            let lines: Vec<Value> = contents
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).expect("each audit line must be valid JSON"))
                .collect();
            if lines.len() >= min_lines {
                return lines;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "audit log at {} never reached {min_lines} lines",
                path.display(),
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Open a raw framed connection and send a single user-supplied byte
/// payload as the body of one length-prefixed frame. Used to drive the
/// malformed-JSON path that the typed client helpers can't exercise.
async fn send_raw_frame(socket_path: &std::path::Path, body: &[u8]) -> AdminResponse {
    let stream = UnixStream::connect(socket_path)
        .await
        .expect("connect to admin socket");
    let mut framed: Framed<UnixStream, LengthDelimitedCodec> = Framed::new(stream, client_codec());
    framed
        .send(Bytes::copy_from_slice(body))
        .await
        .expect("send malformed frame");
    let frame = tokio::time::timeout(Duration::from_secs(2), framed.next())
        .await
        .expect("server replied in time")
        .expect("server produced a frame")
        .expect("frame well-formed");
    serde_json::from_slice(&frame).expect("response decodes")
}

#[tokio::test]
async fn audit_records_status_request_with_peer_uid() {
    let (socket_path, audit_path, _sd, _ad, _xd, _g, server) = spawn_with_audit().await;

    admin::client::fetch_status(&socket_path)
        .await
        .expect("status succeeds");

    let lines = wait_for_audit_lines(&audit_path, 1).await;
    assert_eq!(lines.len(), 1);
    let event = &lines[0];
    assert_eq!(event["command"], "status");
    assert_eq!(event["result"], "ok");
    assert!(
        event.get("error").is_none(),
        "ok result must not carry an error field: {event}",
    );
    assert!(
        event["request"].is_object(),
        "status request body must be an empty object (the command tag is stripped): {event}",
    );
    // The for_test context wires no extra fields into Status; with the
    // discriminator removed the request body is `{}`.
    assert_eq!(event["request"], serde_json::json!({}));

    // On Linux the test process and the server share an address space, so
    // SO_PEERCRED reports our own uid/pid. The audit file is the cleanest
    // place to assert that wiring without exposing internals.
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            event["peer"]["uid"].as_u64().expect("peer.uid"),
            u64::from(nix::unistd::geteuid().as_raw()),
        );
        assert_eq!(
            event["peer"]["pid"].as_u64().expect("peer.pid"),
            u64::from(std::process::id()),
        );
    }
    server.abort();
}

#[tokio::test]
async fn audit_records_ok_and_err_outcomes_with_correct_fields() {
    let (socket_path, audit_path, _sd, _ad, _xd, _g, server) = spawn_with_audit().await;

    // OK: add a new export.
    let new_dir = tempfile::tempdir().expect("new export tempdir");
    admin::client::exports_add(
        &socket_path,
        cfg("/extra", 7, new_dir.path().to_path_buf(), false),
        false,
    )
    .await
    .expect("add ok");

    // ERR: try to add the same uid again — duplicate.
    let dup_resp = admin::client::send_request(
        &socket_path,
        &AdminRequest::ExportsAdd {
            config: cfg("/extra2", 7, new_dir.path().to_path_buf(), false),
            dry_run: false,
        },
    )
    .await
    .expect("request completes");
    match dup_resp {
        AdminResponse::Err { .. } => {}
        AdminResponse::Ok { .. } => panic!("duplicate uid must produce err"),
    }

    // OK: remove the export so the next assertion has clean state.
    admin::client::exports_remove(&socket_path, ExportSelector::Uid(7), false)
        .await
        .expect("remove ok");

    let lines = wait_for_audit_lines(&audit_path, 3).await;
    assert_eq!(
        lines.len(),
        3,
        "three commands → three audit lines: {lines:?}"
    );

    // First line: exports-add OK.
    assert_eq!(lines[0]["command"], "exports-add");
    assert_eq!(lines[0]["result"], "ok");
    assert!(lines[0].get("error").is_none());
    assert_eq!(lines[0]["request"]["config"]["name"], "/extra");
    assert_eq!(lines[0]["request"]["config"]["uid"], 7);
    assert_eq!(lines[0]["request"]["dry_run"], false);

    // Second line: exports-add ERR (duplicate uid).
    assert_eq!(lines[1]["command"], "exports-add");
    assert_eq!(lines[1]["result"], "err");
    let err = lines[1]["error"]
        .as_str()
        .expect("err must carry error string");
    assert!(err.contains("already in use"), "got: {err}");
    assert_eq!(lines[1]["request"]["config"]["uid"], 7);

    // Third line: exports-remove OK.
    assert_eq!(lines[2]["command"], "exports-remove");
    assert_eq!(lines[2]["result"], "ok");
    assert!(lines[2].get("error").is_none());
    assert_eq!(lines[2]["request"]["selector"]["uid"], 7);

    server.abort();
}

#[tokio::test]
async fn audit_records_malformed_frame_with_undecodable_marker() {
    let (socket_path, audit_path, _sd, _ad, _xd, _g, server) = spawn_with_audit().await;

    // Bytes that aren't valid JSON at all — the audit line must still get
    // emitted, with the documented `<undecodable>` command tag and a
    // `null` request body.
    let resp = send_raw_frame(&socket_path, b"this is not json at all").await;
    match resp {
        AdminResponse::Err { .. } => {}
        AdminResponse::Ok { .. } => panic!("garbage payload must produce err"),
    }

    let lines = wait_for_audit_lines(&audit_path, 1).await;
    assert_eq!(lines.len(), 1);
    let event = &lines[0];
    assert_eq!(event["command"], "<undecodable>");
    assert!(
        event["request"].is_null(),
        "undecodable frame must record null request: {event}",
    );
    assert_eq!(event["result"], "err");
    assert!(
        event["error"].is_string(),
        "undecodable frame must carry the decode error: {event}",
    );

    server.abort();
}

#[tokio::test]
async fn audit_records_every_request_in_order_including_repeats() {
    // Pin the "every request gets one line" invariant across a mixed
    // batch — operators rely on a 1:1 ratio so log volume matches
    // request count exactly.
    let (socket_path, audit_path, _sd, _ad, _xd, _g, server) = spawn_with_audit().await;

    let _ = admin::client::fetch_status(&socket_path).await.unwrap();
    let _ = admin::client::fetch_version(&socket_path).await.unwrap();
    let _ = admin::client::fetch_exports_list(&socket_path)
        .await
        .unwrap();
    let _ = admin::client::fetch_status(&socket_path).await.unwrap();

    let lines = wait_for_audit_lines(&audit_path, 4).await;
    assert_eq!(lines.len(), 4);
    let tags: Vec<&str> = lines
        .iter()
        .map(|l| l["command"].as_str().unwrap())
        .collect();
    assert_eq!(tags, vec!["status", "version", "exports-list", "status"]);

    // Every line must carry a non-empty RFC 3339 timestamp.
    for (i, line) in lines.iter().enumerate() {
        let ts = line["ts"].as_str().unwrap_or_default();
        assert!(
            ts.ends_with('Z') && ts.contains('T'),
            "line {i} has malformed ts: {ts}",
        );
    }

    server.abort();
}
