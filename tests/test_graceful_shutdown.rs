//! Integration tests for Phase 8 graceful shutdown (issue #25).
//!
//! These exercise the admin server's cancellation + drain path end to end
//! over a real Unix socket, plus the whole-daemon drain-timeout helper:
//!
//! - `graceful_drain_completes_inflight_and_removes_socket` proves that a
//!   request in flight when the shutdown token is cancelled still completes,
//!   that new connections after shutdown are refused, that the socket file
//!   is unlinked once the drain finishes, and that the in-flight requests
//!   reached the audit log.
//! - `drain_timeout_fires_and_warns_when_handler_is_stuck` proves the
//!   whole-daemon drain ceiling fires (and logs a `warn` carrying the
//!   aggregate in-flight count) when a handler refuses to finish, rather than
//!   hanging forever.
//!
//! Coverage gap (Fix 3): the daemon installs its signal handlers immediately
//! after tracing init — before the FSAL build, the port binds, and the
//! admin-socket bind — so a SIGTERM that lands *during* init triggers a clean
//! shutdown rather than the default kill. That early-init path is not
//! exercised here: it would require spawning the real `main()` (which reads
//! `/etc/arcticwolf/config.toml` and binds privileged ports 111/2049) under a
//! test harness and racing a SIGTERM into the init window. It is left to the
//! VM-based `make nfstest` suite rather than these in-process unit tests.

use std::sync::Arc;
use std::time::Duration;

use arcticwolf::admin::{
    self, AdminContext, AdminRequest, AdminResponse, AuditWriter, FileAuditWriter,
};
use arcticwolf::shutdown::{DrainOutcome, InFlight, InFlightGuard, drain_with_timeout};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

/// Mirror the daemon's `LengthDelimitedCodec` configuration so the client's
/// frames are exactly what the server decodes.
fn client_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .max_frame_length(1024 * 1024)
        .new_codec()
}

/// Open a persistent framed admin connection. Persistence matters: the
/// graceful-drain test reuses the *same* connection across the cancel so it
/// can prove an accepted connection keeps being serviced after shutdown
/// begins (the per-call `admin::client` helpers open a fresh socket each
/// time, which wouldn't exercise the in-flight path).
async fn connect(path: &std::path::Path) -> Framed<UnixStream, LengthDelimitedCodec> {
    let stream = UnixStream::connect(path)
        .await
        .expect("connect to admin socket");
    Framed::new(stream, client_codec())
}

/// Send one request on an existing framed connection and await the reply.
async fn round_trip(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    request: &AdminRequest,
) -> AdminResponse {
    let payload = serde_json::to_vec(request).expect("serialize request");
    framed
        .send(Bytes::from(payload))
        .await
        .expect("send request");
    let frame = tokio::time::timeout(Duration::from_secs(2), framed.next())
        .await
        .expect("server replied within 2s")
        .expect("server produced a frame")
        .expect("frame is well-formed");
    serde_json::from_slice(&frame).expect("response decodes")
}

#[tokio::test]
async fn graceful_drain_completes_inflight_and_removes_socket() {
    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("admin.sock");
    let audit_dir = tempfile::tempdir().expect("audit tempdir");
    let audit_path = audit_dir.path().join("audit.jsonl");

    // Concrete handle kept for the post-drain audit flush; the context gets
    // a `dyn` clone.
    let audit = Arc::new(FileAuditWriter::open(&audit_path).expect("open audit writer"));
    let audit_dyn: Arc<dyn AuditWriter> = audit.clone();
    let (context, _export_dir, _log_guard) = AdminContext::for_test_with_audit(audit_dyn);

    let listener =
        admin::server::bind_admin_socket(&socket_path, 0o600).expect("bind admin socket");
    let token = CancellationToken::new();
    let server = tokio::spawn(admin::serve_with_shutdown(
        listener,
        socket_path.clone(),
        context,
        token.clone(),
        Arc::new(InFlight::new()),
    ));

    // Open a connection and complete a request. Once the reply lands, the
    // connection has been accepted and its handler task is tracked in the
    // server's JoinSet — so the cancel below cannot abort it.
    let mut client = connect(&socket_path).await;
    match round_trip(&mut client, &AdminRequest::Status).await {
        AdminResponse::Ok { .. } => {}
        AdminResponse::Err { error } => panic!("first status must succeed; got: {error}"),
    }

    // Begin shutdown: the accept loop stops taking new connections.
    token.cancel();

    // The already-accepted connection must STILL be serviced after cancel —
    // this is the core "drain in-flight, don't abort" guarantee.
    match round_trip(&mut client, &AdminRequest::Status).await {
        AdminResponse::Ok { .. } => {}
        AdminResponse::Err { error } => {
            panic!("in-flight connection must complete after cancel; got: {error}")
        }
    }

    // Close the connection so its handler task ends and the drain finishes.
    drop(client);

    // The serve task drains and returns within the bound; assert it returned
    // Ok rather than panicking or hanging.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve must drain and return within 5s")
        .expect("serve task joins cleanly")
        .expect("serve returns Ok");

    // The socket file is unlinked once the drain completes.
    assert!(
        !socket_path.exists(),
        "admin socket file must be removed after the drain completes",
    );

    // New connections after shutdown are refused (the path is gone).
    assert!(
        UnixStream::connect(&socket_path).await.is_err(),
        "a connection attempt after shutdown must be refused",
    );

    // Flush the audit writer and confirm both in-flight requests were logged.
    audit.shutdown().await;
    let contents = std::fs::read_to_string(&audit_path).expect("read audit log");
    let status_lines = contents
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["command"] == "status")
        .count();
    assert!(
        status_lines >= 2,
        "audit log must contain the in-flight status requests; found {status_lines} in:\n{contents}",
    );
}

#[tokio::test(start_paused = true)]
async fn drain_timeout_fires_and_warns_when_handler_is_stuck() {
    use std::io::Write;
    use std::sync::Mutex;
    use tracing_subscriber::fmt::MakeWriter;

    // A `MakeWriter` that appends every formatted log line into a shared
    // buffer, so the test can assert the drain-timeout `warn!` was emitted.
    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(buf.clone()))
        .with_ansi(false)
        .finish();
    // `#[tokio::test]` runs on a current-thread runtime, so the drain future
    // is polled on this very thread and inherits the thread-local default
    // subscriber set here.
    let _guard = tracing::subscriber::set_default(subscriber);

    // Model one stuck in-flight handler: hold an `InFlightGuard` so the shared
    // counter reads 1, and stand in for the wedged handler with a 60s sleep
    // bounded by a 1s ceiling. `#[tokio::test(start_paused = true)]` runs the
    // clock in virtual time and auto-advances to the next timer whenever the
    // runtime is otherwise idle, so the 1s ceiling fires deterministically with
    // no real wall-clock wait — removing the slow-CI flake risk the old
    // `elapsed < 1500ms` assertion guarded against. (Fix 23)
    let in_flight = Arc::new(InFlight::new());
    let _guard = InFlightGuard::new(in_flight.clone());
    assert_eq!(in_flight.load(), 1, "one live guard → in-flight count 1");

    let outcome = drain_with_timeout(
        tokio::time::sleep(Duration::from_secs(60)),
        Duration::from_secs(1),
        &in_flight,
    )
    .await;

    assert!(
        matches!(outcome, DrainOutcome::TimedOut(_)),
        "a 60s handler under a 1s ceiling must time out",
    );

    let logs = String::from_utf8(buf.lock().unwrap_or_else(|p| p.into_inner()).clone())
        .expect("captured logs are UTF-8");
    assert!(
        logs.contains("drain timeout exceeded"),
        "the drain-timeout warn must be emitted; captured logs:\n{logs}",
    );
    // The warn must carry the aggregate in-flight count so operators see how
    // much work was abandoned, not just that the ceiling fired. (Fix 9)
    assert!(
        logs.contains("in_flight=1"),
        "the drain-timeout warn must report the non-zero in-flight count; captured logs:\n{logs}",
    );
}
