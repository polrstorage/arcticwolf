//! Structured audit log for admin requests (issue #25, Phase 6).
//!
//! Every admin request the daemon accepts produces one line of JSON in the
//! audit log file. The wire schema, written one event per line (UTF-8,
//! `\n`-terminated, JSON Lines), is:
//!
//! ```json
//! {
//!   "ts": "2026-06-02T01:33:00.123456Z",
//!   "peer": { "uid": 1000, "gid": 1000, "pid": 12345 },
//!   "command": "exports-add",
//!   "request": { ... wire request minus the "command" tag ... },
//!   "result": "ok",
//!   "duration_ms": 12
//! }
//! ```
//!
//! For an erroring request, `result` is `"err"` and an additional `"error"`
//! field carries the human-readable message; `error` is omitted on success.
//! `peer` is omitted (not `null`) when the kernel-side `SO_PEERCRED` lookup
//! fails — e.g. on a non-Linux host where the syscall is unavailable.
//!
//! ## Synthetic `command` tags
//!
//! Most events carry the real wire command (`"status"`, `"exports-add"`,
//! …). Three angle-bracketed sentinels stand in when there is no usable
//! command to record, so operators can still grep for the failure mode:
//!
//! - `<undecodable>` — the frame bytes were not valid JSON at all; the
//!   `request` field is `null`.
//! - `<unknown>` — the frame was valid JSON but not an object, or had no
//!   string `command` field; `request` carries whatever parsed.
//! - `<frame-error>` — a codec-level failure (oversize frame, EOF
//!   mid-frame) before any JSON was decoded; `request` is `null` and the
//!   connection is closed immediately afterward.
//!
//! Two writer implementations are provided. [`FileAuditWriter`] owns the
//! audit-log file and a dedicated writer task; calls to
//! [`AuditWriter::record`] hand the event off through an unbounded mpsc
//! channel so the admin request path never blocks on disk I/O. The task
//! line-flushes after every event (cheap line buffering — crash-safety
//! beyond the kernel page cache is out of scope for v1). [`NoopAuditWriter`]
//! is the inert variant used when `[audit] enabled = false`.
//!
//! Sensitive-field redaction in the `request` payload is deliberately a
//! follow-up — gated on the S3 backend per the Phase 5 decision — so v1
//! logs every field verbatim. Operators who enable audit today should be
//! aware that future credential-bearing exports will need redaction
//! before they go live.

use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::warn;

/// Peer credentials captured from `SO_PEERCRED` on the admin connection.
///
/// `uid`/`gid`/`pid` mirror the kernel's `struct ucred`. Recorded verbatim
/// in the audit line so operators can answer "who ran this command" and
/// "from which process" without consulting auxiliary logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct PeerCreds {
    /// Effective user id of the connected client process.
    pub uid: u32,
    /// Effective group id of the connected client process.
    pub gid: u32,
    /// Process id of the connected client.
    pub pid: i32,
}

/// Extract `SO_PEERCRED` from an accepted admin connection.
///
/// Returns `None` (and logs a `warn`) on failure rather than propagating
/// an error: a missing peer triple must not fail the admin request. The
/// underlying lookup is Linux-only — on non-Linux test platforms (e.g.
/// developer macOS) the syscall is not implemented and we'd see the same
/// `None`, which is exactly the fallback the audit schema documents.
pub fn peer_creds(stream: &UnixStream) -> Option<PeerCreds> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

    let borrowed = stream.as_fd();
    match getsockopt(&borrowed, PeerCredentials) {
        Ok(creds) => Some(PeerCreds {
            uid: creds.uid(),
            gid: creds.gid(),
            // `UnixCredentials::pid` returns `pid_t` (i32 on Linux). We
            // surface the kernel-supplied value directly so an `unshare(2)`
            // peer's pid in its namespace is what shows up — the audit log
            // is a record of what the kernel told us, not a normalized view.
            pid: creds.pid(),
        }),
        Err(err) => {
            warn!("admin: SO_PEERCRED lookup failed, omitting peer from audit: {err}");
            None
        }
    }
}

/// One JSON-lines audit record.
///
/// Built by the admin server immediately after dispatch and before the
/// response is encoded. Each field maps directly to a top-level key in
/// the wire schema documented at the top of this module.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct AuditEvent {
    /// RFC 3339 / ISO 8601 UTC timestamp with microsecond precision and a
    /// trailing `Z` (e.g. `2026-06-02T01:33:00.123456Z`).
    pub ts: String,
    /// Peer credentials captured from `SO_PEERCRED`. Omitted (the field is
    /// dropped from the serialized JSON, not serialized as `null`) when
    /// the kernel-side lookup failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerCreds>,
    /// Wire command tag (e.g. `"status"`, `"exports-add"`,
    /// `"log-level-set"`), or one of the synthetic sentinels
    /// `"<undecodable>"` / `"<unknown>"` / `"<frame-error>"` documented in
    /// the module-level "Synthetic `command` tags" section.
    pub command: String,
    /// The original request JSON with the `command` discriminator removed.
    /// `null` for undecodable frames.
    pub request: serde_json::Value,
    /// `"ok"` if the handler returned a success response, `"err"` if it
    /// returned an error (or the request never made it past decode).
    pub result: &'static str,
    /// Human-readable error message; present iff `result == "err"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock duration in milliseconds from request decode to the
    /// moment the audit event is built (i.e. just before response encode).
    /// Best-effort — not a load-bearing metric.
    pub duration_ms: u128,
}

/// Type-erased audit destination. Implementations are expected to be
/// fire-and-forget: a `record` call must not block the admin request path
/// on disk I/O. Failures (the writer task panicked, the disk filled, the
/// file is gone) are surfaced through `tracing::warn!` rather than
/// propagated to the caller — losing an audit line is preferable to
/// stalling or failing an admin command on a side-channel.
pub trait AuditWriter: Send + Sync {
    /// Hand `event` to the writer for asynchronous recording.
    fn record(&self, event: AuditEvent);
}

/// Inert audit writer. Used when `[audit] enabled = false` so the request
/// path can `context.audit.record(event)` unconditionally without an
/// `Option` round-trip at the call site.
pub struct NoopAuditWriter;

impl AuditWriter for NoopAuditWriter {
    fn record(&self, _event: AuditEvent) {}
}

/// Audit writer backed by a JSON-lines file and a dedicated writer task.
///
/// The choice of mpsc + task (vs. `Mutex<BufWriter<File>>`) is deliberate:
/// the admin request path must never hold a lock across `.await` on disk
/// I/O, and message-passing makes the contention-free hot path explicit.
/// Backpressure is fine via the unbounded channel: admin traffic is
/// human-scale (operator commands), not RPC-scale.
#[derive(Debug)]
pub struct FileAuditWriter {
    tx: mpsc::UnboundedSender<AuditEvent>,
}

impl FileAuditWriter {
    /// Open the audit-log file and spawn its writer task.
    ///
    /// Fails if the parent directory of `path` does not exist (the daemon
    /// refuses to start with audit misconfigured; `/var/log` is operator
    /// territory and we don't auto-mkdir) or if the file itself cannot be
    /// opened. The file is opened with `append(true).create(true)` and
    /// `mode(0o640)` — never truncated, umask-respecting.
    ///
    /// Must be called inside a Tokio runtime: the writer task is spawned
    /// via [`tokio::spawn`].
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            anyhow::bail!(
                "audit log parent directory {} does not exist (refusing to start with [audit] enabled = true)",
                parent.display(),
            );
        }

        let std_file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o640)
            .open(path)
            .with_context(|| format!("failed to open audit log {}", path.display()))?;
        let file = File::from_std(std_file);

        let (tx, rx) = mpsc::unbounded_channel::<AuditEvent>();
        tokio::spawn(writer_task(file, rx));
        Ok(Self { tx })
    }
}

#[cfg(test)]
impl FileAuditWriter {
    /// Test-only constructor that drops the receiver half immediately, so
    /// the very first [`record`](AuditWriter::record) call hits the
    /// channel-closed `Err` branch. Lets the fail-safe path (warn + return,
    /// never panic) be covered directly instead of relying on writer-task
    /// timing. Spawns no writer task — there is nothing to drain.
    fn with_dropped_receiver() -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<AuditEvent>();
        drop(rx);
        Self { tx }
    }
}

impl AuditWriter for FileAuditWriter {
    fn record(&self, event: AuditEvent) {
        // The receiver lives in the spawned writer task. If that task has
        // exited (panic, runtime shutdown, channel close on drop), there
        // is nothing to do with the event — losing an audit record is
        // strictly preferable to stalling or failing the admin request.
        if let Err(err) = self.tx.send(event) {
            warn!("audit: dropping event, writer channel closed: {err}");
        }
    }
}

/// Drain `rx` into `file`, line-flushing after every event. Exits when the
/// channel is closed (last `Sender` dropped — today, that happens when
/// `Arc<FileAuditWriter>` is dropped at daemon shutdown).
async fn writer_task(file: File, mut rx: mpsc::UnboundedReceiver<AuditEvent>) {
    let mut writer = BufWriter::new(file);
    while let Some(event) = rx.recv().await {
        let line = match serde_json::to_vec(&event) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("audit: serialize failed, dropping event: {err}");
                continue;
            }
        };
        if let Err(err) = writer.write_all(&line).await {
            warn!("audit: write failed, dropping event: {err}");
            continue;
        }
        if let Err(err) = writer.write_all(b"\n").await {
            warn!("audit: newline write failed: {err}");
            continue;
        }
        if let Err(err) = writer.flush().await {
            warn!("audit: flush failed: {err}");
        }
    }
    // Channel closed: drain whatever's buffered before the task exits.
    let _ = writer.flush().await;
}

/// Current wall-clock time as an RFC 3339 string with microsecond
/// precision and a trailing `Z` (UTC). Implemented in terms of
/// [`SystemTime`] + Howard Hinnant's `civil_from_days` algorithm so the
/// audit module doesn't pull in a date/time crate just for one formatter.
pub fn rfc3339_micros_now() -> String {
    format_rfc3339_micros(SystemTime::now())
}

fn format_rfc3339_micros(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let micros = dur.subsec_micros();
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400) as u32;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z",)
}

/// Howard Hinnant's `civil_from_days`: days-since-1970 → (year, month, day)
/// in the proleptic Gregorian calendar. Reference:
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
/// Trusted for the SystemTime range; not exhaustively tested here because
/// the wall-clock domain covers it implicitly.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn sample_event() -> AuditEvent {
        AuditEvent {
            ts: "2026-06-02T01:33:00.123456Z".to_string(),
            peer: Some(PeerCreds {
                uid: 1000,
                gid: 1000,
                pid: 4242,
            }),
            command: "status".to_string(),
            request: json!({}),
            result: "ok",
            error: None,
            duration_ms: 7,
        }
    }

    #[test]
    fn noop_writer_is_silent() {
        // The whole reason `NoopAuditWriter` exists is so the request path
        // can `context.audit.record(event)` without branching on whether
        // audit is enabled. Pin that it doesn't panic and that the trait
        // call is in fact a no-op (no observable side effect at the type
        // level, asserted via successful construction + record).
        let w = NoopAuditWriter;
        w.record(sample_event());
    }

    #[tokio::test]
    async fn file_writer_round_trip_single_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let writer = FileAuditWriter::open(&path).expect("open audit writer");
        writer.record(sample_event());
        // Drop the writer so its mpsc sender closes — the spawned task
        // drains and exits, flushing the BufWriter.
        drop(writer);
        // Give the spawned task a moment to drain. We can't deterministically
        // join it (it lives in tokio::spawn) so we poll the file with a
        // short ceiling — line-buffered writes are fast.
        let contents = wait_for_lines(&path, 1).await;
        assert_eq!(contents.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&contents[0]).expect("valid JSON");
        assert_eq!(value["ts"], "2026-06-02T01:33:00.123456Z");
        assert_eq!(value["command"], "status");
        assert_eq!(value["peer"]["uid"], 1000);
        assert_eq!(value["peer"]["gid"], 1000);
        assert_eq!(value["peer"]["pid"], 4242);
        assert_eq!(value["result"], "ok");
        assert_eq!(value["duration_ms"], 7);
        assert!(
            value.get("error").is_none(),
            "ok result must not carry an error field",
        );
    }

    #[tokio::test]
    async fn file_writer_preserves_record_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let writer = FileAuditWriter::open(&path).expect("open audit writer");
        for i in 0..16 {
            let mut event = sample_event();
            event.duration_ms = i;
            writer.record(event);
        }
        drop(writer);
        let lines = wait_for_lines(&path, 16).await;
        assert_eq!(lines.len(), 16);
        for (i, line) in lines.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(
                value["duration_ms"], i as u128 as i64,
                "lines must be written in the order they were recorded"
            );
        }
    }

    #[tokio::test]
    async fn file_writer_err_event_carries_error_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let writer = FileAuditWriter::open(&path).expect("open audit writer");
        let mut event = sample_event();
        event.result = "err";
        event.error = Some("boom".to_string());
        writer.record(event);
        drop(writer);
        let lines = wait_for_lines(&path, 1).await;
        let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("valid JSON");
        assert_eq!(value["result"], "err");
        assert_eq!(value["error"], "boom");
    }

    #[tokio::test]
    async fn file_writer_omits_peer_when_absent() {
        // Non-Linux test runs or `SO_PEERCRED` failures mean we don't have
        // peer creds for the connection. The schema is "omit, don't null".
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let writer = FileAuditWriter::open(&path).expect("open audit writer");
        let mut event = sample_event();
        event.peer = None;
        writer.record(event);
        drop(writer);
        let lines = wait_for_lines(&path, 1).await;
        let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("valid JSON");
        assert!(
            value.get("peer").is_none(),
            "peer must be omitted (not null) when SO_PEERCRED is unavailable",
        );
    }

    #[test]
    fn file_writer_open_fails_when_parent_dir_missing() {
        // Validates the operator contract: enabling audit with a path under
        // a directory that doesn't exist is a hard startup failure, not a
        // silent fallback. (See also the `validate()` test for the
        // enabled-without-path case.)
        let _rt = tokio::runtime::Runtime::new().expect("runtime");
        let path = std::path::PathBuf::from("/does/not/exist/audit.jsonl");
        let err = FileAuditWriter::open(&path)
            .expect_err("missing parent dir must fail")
            .to_string();
        assert!(
            err.contains("does not exist"),
            "error should mention the missing parent dir; got: {err}",
        );
    }

    #[tokio::test]
    async fn file_audit_writer_sets_requested_mode_on_creation() {
        // The audit file is created `0o640` (owner rw, group r) via
        // `OpenOptions::mode(0o640)`. Pin it the same way
        // `bind_admin_socket_sets_requested_mode` pins the socket mode, so a
        // refactor that drops `.mode(0o640)` fails here loudly. umask is
        // process-global and only *removes* bits; we force it to 0 across
        // the open so the assertion is the exact configured mode rather than
        // a umask-derived subset, then restore it.
        //
        // SAFETY: `umask` mutates process-global state. We save the prior
        // value and restore it immediately after the open. `cargo test` may
        // run other tests on parallel threads, but the other umask-touching
        // tests (`bind_admin_socket_*`) likewise save+restore, so the only
        // cross-test effect is a brief intermediate mask; this test's own
        // assertion reads the mode of a file it alone created.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit-mode.jsonl");
        let saved_umask = unsafe { libc::umask(0o000) };
        let writer = FileAuditWriter::open(&path).expect("open audit writer");
        let mode = std::fs::metadata(&path)
            .expect("stat audit file")
            .permissions()
            .mode()
            & 0o777;
        unsafe {
            libc::umask(saved_umask);
        }
        drop(writer);
        assert_eq!(
            mode, 0o640,
            "audit file must be created with mode 0o640; got {mode:o}",
        );
    }

    #[test]
    fn record_after_receiver_drop_logs_and_does_not_panic() {
        // Documented fail-safe behavior: if the receiver is gone, `record`
        // logs a warning and returns. The caller is the admin request
        // path; losing one audit line is strictly preferable to panicking
        // out of the connection handler. `with_dropped_receiver` closes the
        // channel up front so this directly covers the `Err` branch in
        // `record` rather than racing the writer task's shutdown.
        let writer = FileAuditWriter::with_dropped_receiver();
        writer.record(sample_event());
        // A second record over the same closed channel must also be inert.
        writer.record(sample_event());
    }

    #[test]
    fn rfc3339_micros_known_epoch_round_trip() {
        // Pin a known timestamp against the manual formatter so a future
        // refactor of `format_rfc3339_micros` can't silently break the
        // operator-facing audit log shape (which downstream tooling parses
        // by string).
        let t = UNIX_EPOCH + Duration::from_micros(1_717_286_400_000_001);
        // 2024-06-02T00:00:00.000001Z
        let s = format_rfc3339_micros(t);
        assert_eq!(s, "2024-06-02T00:00:00.000001Z");
    }

    #[test]
    fn rfc3339_micros_now_has_correct_shape() {
        // Don't pin the exact value (it's wall clock), but pin the shape:
        // 4-digit year, `Z` suffix, microsecond precision.
        let s = rfc3339_micros_now();
        assert_eq!(s.len(), 27, "expected YYYY-MM-DDTHH:MM:SS.uuuuuuZ; got {s}");
        assert!(s.ends_with('Z'), "must end with Z; got {s}");
        assert_eq!(s.chars().nth(4), Some('-'));
        assert_eq!(s.chars().nth(10), Some('T'));
        assert_eq!(s.chars().nth(19), Some('.'));
    }

    /// On Linux, an accepted `UnixStream` carries the connecting process's
    /// `SO_PEERCRED` triple. We pair up a listener + client in-process so
    /// the credentials we read back must match this very test process.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn peer_creds_round_trip_on_local_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
        let connect = tokio::net::UnixStream::connect(&socket_path);
        let accept = listener.accept();
        let (_client, accepted) = tokio::join!(connect, accept);
        let (server_side, _) = accepted.expect("accept");
        let creds = peer_creds(&server_side).expect("peer creds must be available on Linux");
        assert_eq!(creds.uid, nix::unistd::geteuid().as_raw());
        assert_eq!(creds.gid, nix::unistd::getegid().as_raw());
        assert_eq!(creds.pid as u32, std::process::id());
    }

    /// Poll `path` until at least `min_lines` non-empty lines have been
    /// flushed. The FileAuditWriter drain runs on a tokio::spawn task we
    /// can't deterministically join, so the test crosses the boundary by
    /// observing the file. Bounded by a 2s ceiling so a regression
    /// (writer task wedged) shows up as a test failure, not a hang.
    async fn wait_for_lines(path: &std::path::Path, min_lines: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
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
            if std::time::Instant::now() >= deadline {
                panic!(
                    "audit writer never produced {min_lines} lines at {}",
                    path.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
