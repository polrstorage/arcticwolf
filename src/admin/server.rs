//! Unix-domain-socket admin server.
//!
//! `serve()` is the entry point invoked from `main.rs`. It owns the
//! listener (already bound and chmod'd by `bind_admin_socket()`) and
//! spawns one task per accepted connection. Each connection is a framed
//! JSON request/response stream; for Phase 1 every request is answered
//! with a not-implemented error.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use super::audit::{AuditEvent, PeerCreds, peer_creds, rfc3339_micros_now};
use super::commands;
use super::context::AdminContext;
use super::protocol::{decode_request, encode_response, framed};
use super::request::AdminRequest;
use super::response::AdminResponse;

/// Upper bound for the post-codec-error courtesy response. After a frame
/// decode failure the socket is in an unknown state (e.g. oversize payload
/// left half-read), and the client may not be draining its read half — we
/// MUST not let a blocked `send` keep the connection task alive forever.
const ERROR_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bind the admin Unix domain socket described by `socket_path` and apply
/// `socket_mode` via `chmod(2)`. Fails fast if the parent directory does
/// not exist or is not writable so the operator sees a clear startup
/// error rather than a vague "bind failed" deep in the listener.
pub fn bind_admin_socket(socket_path: &Path, socket_mode: u32) -> Result<UnixListener> {
    let parent = socket_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "admin socket_path {} has no parent directory",
            socket_path.display()
        )
    })?;

    if !parent.as_os_str().is_empty() && !parent.exists() {
        anyhow::bail!(
            "admin socket parent directory {} does not exist (refusing to start with [admin] enabled = true)",
            parent.display(),
        );
    }

    // Probe writability by attempting to create a tempfile-like path next
    // to the target socket. This catches the common "directory exists but
    // daemon user lacks write permission" case before bind() returns a
    // less informative EACCES.
    if !parent.as_os_str().is_empty() {
        let writability_probe =
            parent.join(format!(".arcticwolf-admin-probe-{}", std::process::id()));
        match std::fs::File::create(&writability_probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&writability_probe);
            }
            Err(err) => {
                anyhow::bail!(
                    "admin socket parent directory {} is not writable: {} \
                     (refusing to start with [admin] enabled = true)",
                    parent.display(),
                    err,
                );
            }
        }
    }

    // Remove a stale socket file from a previous run, if any. We only
    // unlink a path of file-type socket; refusing to remove regular files
    // protects an operator who accidentally configured `socket_path` to
    // an existing data file.
    if socket_path.exists() {
        let metadata = std::fs::symlink_metadata(socket_path).with_context(|| {
            format!(
                "failed to stat existing admin socket path {}",
                socket_path.display()
            )
        })?;
        use std::os::unix::fs::FileTypeExt;
        if metadata.file_type().is_socket() {
            std::fs::remove_file(socket_path).with_context(|| {
                format!(
                    "failed to remove stale admin socket {}",
                    socket_path.display()
                )
            })?;
        } else {
            anyhow::bail!(
                "admin socket_path {} already exists and is not a socket; refusing to overwrite",
                socket_path.display(),
            );
        }
    }

    // Close the bind→chmod race window. `UnixListener::bind` creates the
    // socket file using the process umask, which on a typical system leaves
    // a brief window where the socket is reachable by other local users
    // before `set_permissions` tightens it down to `socket_mode`. Saving the
    // umask, narrowing it to 0o077 for just the bind, and restoring it
    // afterwards bounds that window to a no-op for non-owner principals.
    // The subsequent `set_permissions` is still required: umask only
    // *removes* bits, so any non-default `socket_mode` (e.g. 0o640 with
    // group access) still needs an explicit chmod to set the exact mode.
    //
    // SAFETY: `umask(2)` is a process-global syscall. We capture the prior
    // value and restore it immediately after `bind` returns, so any
    // concurrent thread observes the relaxed mask only for the duration of
    // a single `bind` call. There is no other safe wrapper in `libc`/`nix`
    // for this; the operation is inherently `unsafe` because it mutates
    // process state visible to all threads.
    let saved_umask = unsafe { libc::umask(0o077) };
    let listener_result = UnixListener::bind(socket_path);
    unsafe {
        libc::umask(saved_umask);
    }
    let listener = listener_result
        .with_context(|| format!("failed to bind admin socket at {}", socket_path.display()))?;

    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(socket_mode);
    std::fs::set_permissions(socket_path, permissions).with_context(|| {
        format!(
            "failed to chmod admin socket {} to {:o}",
            socket_path.display(),
            socket_mode,
        )
    })?;

    info!(
        "admin socket bound at {} (mode {:o})",
        socket_path.display(),
        socket_mode,
    );
    Ok(listener)
}

/// Run the admin server until the listener returns a fatal error.
///
/// `socket_path` is held only so the file can be unlinked on a clean
/// shutdown — graceful shutdown lands in Phase 8, so today we just log
/// the socket path on accept loop termination.
pub async fn serve(
    listener: UnixListener,
    socket_path: PathBuf,
    context: Arc<AdminContext>,
) -> Result<()> {
    info!("admin server accepting on {}", socket_path.display());
    loop {
        let (socket, _) = listener
            .accept()
            .await
            .with_context(|| format!("admin accept failed on {}", socket_path.display()))?;
        debug!("admin connection accepted");

        // Extract `SO_PEERCRED` once per connection. The triple is fixed
        // for the lifetime of the socket (the kernel snapshots it at
        // connect time), so we don't re-query on every request.
        let peer = peer_creds(&socket);

        let context = context.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(socket, context, peer).await {
                warn!("admin connection error: {err:#}");
            }
        });
    }
}

async fn handle_connection(
    socket: UnixStream,
    context: Arc<AdminContext>,
    peer: Option<PeerCreds>,
) -> Result<()> {
    let mut framed = framed(socket);

    while let Some(frame) = framed.next().await {
        let bytes = match frame {
            Ok(bytes) => bytes,
            Err(err) => {
                // Codec-level errors (oversize frame, eof mid-frame). Record
                // a synthetic audit event *before* anything else, so an
                // operator who triggers a frame error (e.g. an oversize
                // payload) still leaves an audit trail even though the
                // request never decoded. `duration_ms` is 0: there is no
                // per-request decode→dispatch span to measure here.
                context.audit.record(AuditEvent {
                    ts: rfc3339_micros_now(),
                    peer,
                    command: "<frame-error>".to_string(),
                    request: serde_json::Value::Null,
                    result: "err",
                    error: Some(format!("{err:#}")),
                    duration_ms: 0,
                });

                // Try to send a structured error back, then close. The write
                // half may be wedged if the client isn't reading (e.g. it
                // sent an oversize frame and then walked away), so bound
                // the courtesy send by `ERROR_RESPONSE_TIMEOUT` rather
                // than letting the spawned connection task hang forever.
                let response = AdminResponse::error(format!("frame error: {err}"));
                match tokio::time::timeout(
                    ERROR_RESPONSE_TIMEOUT,
                    send_response(&mut framed, &response),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(send_err)) => {
                        debug!("admin: failed to deliver frame-error response: {send_err:#}");
                    }
                    Err(_) => {
                        warn!(
                            "admin: timed out after {:?} sending frame-error response; closing connection",
                            ERROR_RESPONSE_TIMEOUT,
                        );
                    }
                }
                return Err(err.into());
            }
        };

        // Phase 6: build the audit event around dispatch and hand it to
        // the writer *before* the response is encoded. `record` is
        // fire-and-forget — the call returns immediately.
        let start = Instant::now();
        let (command_tag, request_payload) = extract_command_and_request(&bytes);
        let response = dispatch(&context, &bytes);
        let duration = start.elapsed();
        let event = build_audit_event(
            command_tag,
            request_payload,
            &response,
            peer,
            duration.as_millis(),
        );
        context.audit.record(event);

        send_response(&mut framed, &response).await?;
    }
    Ok(())
}

/// Peel the `command` discriminator out of the wire frame and return both
/// the tag (`"exports-add"`, `"<undecodable>"`, …) and the request body
/// minus the tag — that's what the audit `request` field carries.
///
/// We don't reuse [`crate::admin::protocol::decode_request`] for this: the
/// audit schema wants the payload "verbatim", and parsing into the typed
/// `AdminRequest` enum would drop omitted-default fields and renormalize
/// member order. Parsing as a free-form `Value` keeps the wire shape.
fn extract_command_and_request(bytes: &[u8]) -> (String, serde_json::Value) {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(mut map)) => {
            let tag = map
                .remove("command")
                .and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    _ => None,
                })
                .unwrap_or_else(|| "<unknown>".to_string());
            (tag, serde_json::Value::Object(map))
        }
        Ok(other) => ("<unknown>".to_string(), other),
        Err(_) => ("<undecodable>".to_string(), serde_json::Value::Null),
    }
}

fn build_audit_event(
    command: String,
    request: serde_json::Value,
    response: &AdminResponse,
    peer: Option<PeerCreds>,
    duration_ms: u128,
) -> AuditEvent {
    let (result, error) = match response {
        AdminResponse::Ok { .. } => ("ok", None),
        AdminResponse::Err { error } => ("err", Some(error.clone())),
    };
    AuditEvent {
        ts: rfc3339_micros_now(),
        peer,
        command,
        request,
        result,
        error,
        duration_ms,
    }
}

async fn send_response<S>(
    framed: &mut tokio_util::codec::Framed<S, tokio_util::codec::LengthDelimitedCodec>,
    response: &AdminResponse,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let encoded: Bytes = encode_response(response).context("encoding admin response")?;
    framed.send(encoded).await.context("sending admin response")
}

/// Decode an admin request frame and route it to its handler.
///
/// Handlers are synchronous — Phase 2's `status`/`version` only read
/// already-resolved context state, so no `.await` is needed on this path.
fn dispatch(context: &AdminContext, frame: &[u8]) -> AdminResponse {
    let request = match decode_request(frame) {
        Ok(request) => request,
        // Includes the "Unknown command: ..." prefix the CLI keys on for
        // its user-facing error.
        Err(err) => return AdminResponse::error(err),
    };
    debug!("admin: dispatching {request:?}");
    match request {
        AdminRequest::Status => commands::status::handle(context),
        AdminRequest::Version => commands::version::handle(),
        AdminRequest::LogLevelGet => commands::log_level::handle_get(context),
        AdminRequest::LogLevelSet { level } => commands::log_level::handle_set(context, &level),
        AdminRequest::ExportsList => commands::exports_list::handle(context),
        AdminRequest::ExportsAdd { config, dry_run } => {
            commands::exports_add::handle(context, &config, dry_run)
        }
        AdminRequest::ExportsRemove { selector, dry_run } => {
            commands::exports_remove::handle(context, &selector, dry_run)
        }
        AdminRequest::ExportsUpdate {
            selector,
            read_only,
            dry_run,
        } => commands::exports_update::handle(context, &selector, read_only, dry_run),
        AdminRequest::ConfigShow => commands::config_show::handle(context),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    fn check_dispatch(frame: &[u8]) -> AdminResponse {
        let (context, _tmp, _log_guard) = AdminContext::for_test();
        dispatch(&context, frame)
    }

    fn client_codec() -> LengthDelimitedCodec {
        // Match the server's framing: 4-byte BE length prefix, 1 MiB cap.
        // Kept inline (rather than calling into the protocol helper) so a
        // future change to either side of the wire fails this test loudly.
        LengthDelimitedCodec::builder()
            .length_field_length(4)
            .max_frame_length(1024 * 1024)
            .new_codec()
    }

    #[test]
    fn dispatch_status_returns_ok_snapshot() {
        // Phase 2: `status` is wired end to end and answers with an Ok
        // carrier whose `data` is the health snapshot.
        let payload = serde_json::to_vec(&AdminRequest::Status).unwrap();
        match check_dispatch(&payload) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["daemon_version"], env!("CARGO_PKG_VERSION"));
                assert!(data.get("export_count").is_some());
            }
            AdminResponse::Err { error } => panic!("status must return Ok; got: {error}"),
        }
    }

    #[test]
    fn dispatch_version_returns_ok_build_info() {
        let payload = serde_json::to_vec(&AdminRequest::Version).unwrap();
        match check_dispatch(&payload) {
            AdminResponse::Ok { data } => {
                assert_eq!(data["daemon_version"], env!("CARGO_PKG_VERSION"));
                assert!(data["build_profile"].is_string());
            }
            AdminResponse::Err { error } => panic!("version must return Ok; got: {error}"),
        }
    }

    #[test]
    fn dispatch_unknown_command_returns_unknown_command_error() {
        // The server layer is responsible for surfacing "Unknown command:
        // foo" through `AdminResponse::Err` (not for example by closing
        // the socket). Pins the AC from issue #25's "unknown command error
        // shape" test bullet.
        let frame = br#"{"command":"banana"}"#;
        let response = check_dispatch(frame);
        match response {
            AdminResponse::Err { error } => {
                assert!(
                    error.contains("Unknown command"),
                    "unknown commands must surface as 'Unknown command: ...'; got: {error}",
                );
            }
            AdminResponse::Ok { .. } => panic!("unknown command must not return Ok"),
        }
    }

    #[test]
    fn bind_admin_socket_fails_fast_on_missing_parent_dir() {
        // Acceptance criterion from issue #25: the daemon must refuse to
        // start with `[admin] enabled = true` if the socket's parent
        // directory does not exist. `bind_admin_socket` is the choke
        // point — `main.rs` propagates this error via `?` so verifying
        // the failure here pins the AC end-to-end without needing to
        // boot the full daemon.
        let socket_path = std::path::PathBuf::from("/does/not/exist/admin.sock");
        let err = bind_admin_socket(&socket_path, 0o600)
            .expect_err("missing parent dir must error")
            .to_string();
        assert!(
            err.contains("does not exist"),
            "error should mention the missing parent dir; got: {err}",
        );
    }

    #[tokio::test]
    async fn bind_admin_socket_applies_mode_even_under_permissive_umask() {
        // The bind→chmod sequence has a microsecond race window: between
        // `UnixListener::bind` (which creates the socket file using the
        // process umask) and `set_permissions` (which tightens it down),
        // any other local process could `connect(2)` to it. To shrink that
        // window we tighten the umask to 0o077 across the bind. This test
        // pins the operator-facing invariant: with an artificially
        // permissive umask (0o000 — every bit allowed) the resulting
        // socket must still end up at the requested mode rather than the
        // umask-derived `0o777`.
        //
        // SAFETY: `umask` is process-global; `cargo test` may run other
        // tests in parallel threads. We save the prior value and restore
        // it on the way out. Concurrent calls in `bind_admin_socket` from
        // other test threads also save+restore around their own bind, so
        // the only observable effect on this test is at most a brief
        // intermediate change; the post-bind `chmod` ensures the final
        // mode is precise regardless.
        use std::os::unix::fs::PermissionsExt;
        let saved_umask = unsafe { libc::umask(0o000) };
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("admin.sock");
        let bind_result = bind_admin_socket(&socket_path, 0o600);
        let mode = std::fs::metadata(&socket_path)
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;
        unsafe {
            libc::umask(saved_umask);
        }
        let _listener = bind_result.expect("bind admin sock");
        assert_eq!(
            mode, 0o600,
            "socket mode must match `socket_mode`, not be derived from the relaxed process umask; got {mode:o}",
        );
    }

    #[tokio::test]
    async fn bind_admin_socket_sets_requested_mode() {
        // The chmod step is what gives operators the `0o600` permission
        // they configured. Round-trip the mode through `stat(2)` so a
        // future refactor (eg. moving to `bind_with_mode`) can't silently
        // drop the chmod. `bind_admin_socket` is itself synchronous but
        // it calls `tokio::net::UnixListener::bind` which requires an
        // active Tokio reactor — `#[tokio::test]` supplies one.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("admin.sock");
        let _listener = bind_admin_socket(&socket_path, 0o600).expect("bind admin sock");
        let mode = std::fs::metadata(&socket_path)
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "bind_admin_socket must apply the requested mode; got {mode:o}",
        );
    }

    #[tokio::test]
    async fn bind_admin_socket_unlinks_stale_socket() {
        // A previous daemon process may have exited without unlinking its
        // socket file (kill -9, container OOM, etc.). The next start must
        // recover by removing the stale entry and binding fresh, otherwise
        // operators would have to manually `rm` the socket after every
        // unclean shutdown.
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("admin.sock");
        let stale = UnixListener::bind(&socket_path).expect("create stale socket");
        // Drop the listener without `remove_file` — same shape as a previous
        // run that died before its Drop ran.
        drop(stale);
        assert!(
            socket_path.exists(),
            "test precondition: stale socket file must remain after drop",
        );
        let _listener = bind_admin_socket(&socket_path, 0o600)
            .expect("bind_admin_socket must recover from a stale socket file");
    }

    #[tokio::test]
    async fn bind_admin_socket_refuses_non_socket_path() {
        // If the operator typos `socket_path` to point at an existing data
        // file (e.g. `/etc/passwd`, or some innocent file under
        // `/run/arcticwolf/`), the daemon MUST refuse rather than unlink
        // and overwrite. The branch in `bind_admin_socket` that gates the
        // unlink on `file_type().is_socket()` is the guard for this; this
        // test pins it.
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("not-actually-a-socket");
        std::fs::write(&socket_path, b"hello").expect("create regular file");
        let err = bind_admin_socket(&socket_path, 0o600)
            .expect_err("regular file at socket_path must error")
            .to_string();
        assert!(
            err.contains("not a socket"),
            "error should explain why the path can't be reused; got: {err}",
        );
        // Critical: the daemon must NOT have removed the operator's file.
        let contents = std::fs::read(&socket_path).expect("file must still exist");
        assert_eq!(
            contents, b"hello",
            "daemon must leave the existing non-socket file untouched",
        );
    }

    /// End-to-end wire test: bind a socket in a tempdir, connect a client,
    /// send a length-prefixed `AdminRequest::Status` frame, and assert the
    /// server replies with an Ok `AdminResponse` carrying the snapshot.
    /// Exercises the same code path `main.rs` uses, just over a tempdir
    /// socket instead of `/run/arcticwolf/admin.sock`.
    #[tokio::test]
    async fn end_to_end_round_trip_through_unix_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("admin.sock");

        let listener = bind_admin_socket(&socket_path, 0o600).expect("bind admin sock");
        let (context, _export_tmp, _log_guard) = AdminContext::for_test();
        let socket_path_for_serve = socket_path.clone();
        let server =
            tokio::spawn(async move { serve(listener, socket_path_for_serve, context).await });

        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect to admin socket");
        let mut framed: Framed<tokio::net::UnixStream, LengthDelimitedCodec> =
            Framed::new(stream, client_codec());

        let payload = serde_json::to_vec(&AdminRequest::Status).expect("serialize");
        framed
            .send(bytes::Bytes::from(payload))
            .await
            .expect("send request");

        let frame = tokio::time::timeout(Duration::from_secs(2), framed.next())
            .await
            .expect("server replied within 2s")
            .expect("server produced a frame")
            .expect("frame is well-formed");
        let response: AdminResponse = serde_json::from_slice(&frame).expect("response decodes");
        match response {
            AdminResponse::Ok { data } => {
                assert_eq!(data["daemon_version"], env!("CARGO_PKG_VERSION"));
            }
            AdminResponse::Err { error } => {
                panic!("phase 2 server must answer status with Ok; got: {error}")
            }
        }

        server.abort();
    }

    /// A codec-level frame error must still leave an audit trail. We point
    /// the context at a `FileAuditWriter` over a tempfile, connect a raw
    /// client, and write a 4-byte length prefix declaring a ~4 GiB frame —
    /// far past the server codec's 1 MiB cap, so the decoder errors on the
    /// length field alone (without waiting for the body). The connection
    /// then dies, but the synthetic `<frame-error>` event must have been
    /// recorded first.
    #[tokio::test]
    async fn frame_error_records_synthetic_audit_event() {
        use crate::admin::audit::FileAuditWriter;
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("admin.sock");
        let audit_path = dir.path().join("audit.jsonl");

        let listener = bind_admin_socket(&socket_path, 0o600).expect("bind admin sock");
        let audit = Arc::new(FileAuditWriter::open(&audit_path).expect("open audit writer"));
        let (context, _export_tmp, _log_guard) = AdminContext::for_test_with_audit(audit);
        let socket_path_for_serve = socket_path.clone();
        let server =
            tokio::spawn(async move { serve(listener, socket_path_for_serve, context).await });

        let mut stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect to admin socket");
        stream
            .write_all(&[0xFF, 0xFF, 0xFF, 0xFF])
            .await
            .expect("write oversize length prefix");
        stream.flush().await.expect("flush");

        let lines = wait_for_audit_lines(&audit_path, 1).await;
        let value: serde_json::Value =
            serde_json::from_str(&lines[0]).expect("audit line is valid JSON");
        assert_eq!(value["command"], "<frame-error>");
        assert_eq!(value["result"], "err");
        assert!(
            value["error"].as_str().is_some(),
            "frame-error event must carry an error string; got: {value}",
        );
        assert!(
            value["request"].is_null(),
            "frame-error event request must be null; got: {value}",
        );

        server.abort();
    }

    /// Poll `path` until at least `min_lines` non-empty lines are present.
    /// The `FileAuditWriter` drains on a `tokio::spawn` task we can't join,
    /// so the test observes the file. Bounded by a 2s ceiling so a wedged
    /// writer surfaces as a failure, not a hang.
    async fn wait_for_audit_lines(path: &std::path::Path, min_lines: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(path).await {
                let lines: Vec<String> = contents
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect();
                if lines.len() >= min_lines {
                    return lines;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "audit writer never produced {min_lines} lines at {}",
                    path.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
