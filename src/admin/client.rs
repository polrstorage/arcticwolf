//! In-process admin client.
//!
//! Both the `arcticwolfctl` binary and the integration tests drive the
//! admin protocol through these functions, so the wire round trip is
//! exercised without forking a separate process. The functions speak the
//! Phase 1 length-prefixed JSON codec ([`super::protocol`]).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::UnixStream;

use super::protocol::framed;
use super::request::AdminRequest;
use super::response::AdminResponse;
use crate::config::ExportConfig;
use crate::fsal::multi_export::ExportSelector;

/// Connect to the admin socket, send one request, and return the decoded
/// response. The connection is closed when the returned value is produced.
pub async fn send_request(socket_path: &Path, request: &AdminRequest) -> Result<AdminResponse> {
    let stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to admin socket {}",
            socket_path.display()
        )
    })?;
    let mut connection = framed(stream);

    let payload = serde_json::to_vec(request).context("serializing admin request")?;
    connection
        .send(Bytes::from(payload))
        .await
        .context("sending admin request")?;

    let frame = connection
        .next()
        .await
        .ok_or_else(|| anyhow!("admin connection closed before a response was received"))?
        .context("reading admin response frame")?;
    serde_json::from_slice(&frame).context("decoding admin response")
}

/// Send `request` and unwrap the success payload, turning an
/// `AdminResponse::Err` into an `anyhow` error.
async fn fetch(socket_path: &Path, request: &AdminRequest) -> Result<Value> {
    match send_request(socket_path, request).await? {
        AdminResponse::Ok { data } => Ok(data),
        AdminResponse::Err { error } => bail!("admin error: {error}"),
    }
}

/// Fetch the daemon `status` payload.
pub async fn fetch_status(socket_path: &Path) -> Result<Value> {
    fetch(socket_path, &AdminRequest::Status).await
}

/// Fetch the daemon `version` payload.
pub async fn fetch_version(socket_path: &Path) -> Result<Value> {
    fetch(socket_path, &AdminRequest::Version).await
}

/// Fetch the tracing filter directive currently in effect (`log-level get`).
pub async fn fetch_log_level(socket_path: &Path) -> Result<Value> {
    fetch(socket_path, &AdminRequest::LogLevelGet).await
}

/// Swap the daemon's live tracing filter (`log-level set <level>`).
///
/// The daemon rejects an unrecognized level with an `AdminResponse::Err`,
/// which surfaces here as an `anyhow` error.
pub async fn set_log_level(socket_path: &Path, level: &str) -> Result<Value> {
    fetch(
        socket_path,
        &AdminRequest::LogLevelSet {
            level: level.to_string(),
        },
    )
    .await
}

/// Fetch the live `exports list` payload (active exports + retired uids).
pub async fn fetch_exports_list(socket_path: &Path) -> Result<Value> {
    fetch(socket_path, &AdminRequest::ExportsList).await
}

/// `exports add` — install (or pre-flight) a new export.
pub async fn exports_add(socket_path: &Path, config: ExportConfig, dry_run: bool) -> Result<Value> {
    fetch(socket_path, &AdminRequest::ExportsAdd { config, dry_run }).await
}

/// `exports remove` — retire (or pre-flight) an export.
pub async fn exports_remove(
    socket_path: &Path,
    selector: ExportSelector,
    dry_run: bool,
) -> Result<Value> {
    fetch(
        socket_path,
        &AdminRequest::ExportsRemove { selector, dry_run },
    )
    .await
}

/// `exports update` — mutate (or pre-flight) an export's `read_only`.
pub async fn exports_update(
    socket_path: &Path,
    selector: ExportSelector,
    read_only: bool,
    dry_run: bool,
) -> Result<Value> {
    fetch(
        socket_path,
        &AdminRequest::ExportsUpdate {
            selector,
            read_only,
            dry_run,
        },
    )
    .await
}

/// Fetch the daemon's startup `config show` payload.
pub async fn fetch_config_show(socket_path: &Path) -> Result<Value> {
    fetch(socket_path, &AdminRequest::ConfigShow).await
}

/// Fetch the daemon's operational `metrics` snapshot.
pub async fn fetch_metrics(socket_path: &Path) -> Result<Value> {
    fetch(socket_path, &AdminRequest::Metrics).await
}

/// Render a `status` payload either as pretty JSON or a human summary.
pub fn render_status(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    let mut out = String::new();
    writeln!(out, "Daemon version: {}", field(data, "daemon_version"))?;
    writeln!(out, "Uptime:         {}s", field(data, "uptime_seconds"))?;
    writeln!(out, "Bind address:   {}", field(data, "bind_address"))?;
    writeln!(out, "NFS port:       {}", field(data, "nfs_port"))?;
    writeln!(out, "Mount port:     {}", field(data, "mount_port"))?;
    writeln!(out, "Portmap port:   {}", field(data, "portmap_port"))?;
    writeln!(out, "Log level:      {}", field(data, "log_level"))?;
    write!(out, "Exports:        {}", field(data, "export_count"))?;
    Ok(out)
}

/// Render a daemon `version` payload either as pretty JSON or a human
/// summary.
pub fn render_version(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    let mut out = String::new();
    writeln!(out, "Daemon version: {}", field(data, "daemon_version"))?;
    writeln!(out, "Build commit:   {}", field(data, "build_commit"))?;
    writeln!(out, "Rustc version:  {}", field(data, "rustc_version"))?;
    write!(out, "Build profile:  {}", field(data, "build_profile"))?;
    Ok(out)
}

/// Render a `log-level get` payload either as pretty JSON or a human
/// summary.
pub fn render_log_level_get(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    Ok(format!("Current log level: {}", field(data, "level")))
}

/// Render a `log-level set` payload either as pretty JSON or a human
/// summary.
pub fn render_log_level_set(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    Ok(format!("Log level set to {}", field(data, "level")))
}

/// Render an `exports list` payload.
///
/// `--json` dumps the daemon response verbatim (including `retired_uids`).
/// The default human form prints a column-aligned table of the active
/// exports only; retired uids are deliberately not shown because they're
/// rarely useful at a glance — `--json` covers the diagnostic case.
pub fn render_exports_list(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    let exports = data
        .get("exports")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("exports list response missing `exports` array"))?;

    // Column widths: header sets the floor, contents widen as needed.
    let mut uid_w = "UID".len();
    let mut name_w = "NAME".len();
    let mut fsal_w = "FSAL".len();
    for e in exports {
        uid_w = uid_w.max(e["uid"].to_string().len());
        name_w = name_w.max(
            e["name"]
                .as_str()
                .map(|s| s.len())
                .unwrap_or_else(|| e["name"].to_string().len()),
        );
        fsal_w = fsal_w.max(
            e["fsal"]
                .as_str()
                .map(|s| s.len())
                .unwrap_or_else(|| e["fsal"].to_string().len()),
        );
    }

    let mut out = String::new();
    writeln!(
        out,
        "{:<uid_w$}  {:<name_w$}  {:<fsal_w$}  READ-ONLY",
        "UID",
        "NAME",
        "FSAL",
        uid_w = uid_w,
        name_w = name_w,
        fsal_w = fsal_w,
    )?;
    for (idx, e) in exports.iter().enumerate() {
        let uid = e["uid"].to_string();
        let name = e["name"].as_str().unwrap_or("?");
        let fsal = e["fsal"].as_str().unwrap_or("?");
        let ro = if e["read_only"].as_bool().unwrap_or(false) {
            "true"
        } else {
            "false"
        };
        if idx + 1 == exports.len() {
            write!(
                out,
                "{:<uid_w$}  {:<name_w$}  {:<fsal_w$}  {}",
                uid,
                name,
                fsal,
                ro,
                uid_w = uid_w,
                name_w = name_w,
                fsal_w = fsal_w,
            )?;
        } else {
            writeln!(
                out,
                "{:<uid_w$}  {:<name_w$}  {:<fsal_w$}  {}",
                uid,
                name,
                fsal,
                ro,
                uid_w = uid_w,
                name_w = name_w,
                fsal_w = fsal_w,
            )?;
        }
    }
    Ok(out)
}

/// Render an `exports add` payload.
///
/// `--json` dumps the daemon response verbatim. The default form prints
/// `Added export <name> (uid <uid>)` on success, or — for `--dry-run` —
/// `Dry-run: would add export <name> (uid <uid>)`, symmetric with
/// `would remove` / `would update`.
pub fn render_exports_add(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    if data["dry_run"].as_bool().unwrap_or(false) {
        let would = &data["would_add"];
        return Ok(format!(
            "Dry-run: would add export {} (uid {})",
            field(would, "name"),
            field(would, "uid"),
        ));
    }
    Ok(format!(
        "Added export {} (uid {})",
        field(data, "name"),
        field(data, "uid")
    ))
}

/// Render an `exports remove` payload.
pub fn render_exports_remove(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    if data["dry_run"].as_bool().unwrap_or(false) {
        let would = &data["would_remove"];
        return Ok(format!(
            "Dry-run: would remove export {} (uid {})",
            field(would, "name"),
            field(would, "uid"),
        ));
    }
    Ok(format!(
        "Removed export {} (uid {}) — uid retired until daemon restart",
        field(data, "name"),
        field(data, "uid"),
    ))
}

/// Render an `exports update` payload.
pub fn render_exports_update(data: &Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(data)?);
    }
    if data["dry_run"].as_bool().unwrap_or(false) {
        let would = &data["would_update"];
        return Ok(format!(
            "Dry-run: would update export {} (uid {}): read_only = {} -> {}",
            field(would, "name"),
            field(would, "uid"),
            field(&would["from"], "read_only"),
            field(&would["to"], "read_only"),
        ));
    }
    Ok(format!(
        "Updated export {} (uid {}): read_only = {}",
        field(data, "name"),
        field(data, "uid"),
        field(data, "read_only"),
    ))
}

/// Render a `config show` payload.
///
/// Pretty-prints the config as JSON regardless of `json` — the human form
/// IS the JSON form for v1 (per the issue plan, sensitive-field redaction
/// and a human-curated layout land later). The `--json` flag is accepted
/// to keep the flag surface consistent across commands.
pub fn render_config_show(data: &Value, _json: bool) -> Result<String> {
    Ok(serde_json::to_string_pretty(data)?)
}

/// Render a `metrics` payload.
///
/// Like `config show`, the human form IS the pretty JSON form for v1 — the
/// JSON snapshot is the surface (no table view). `--json` is accepted for
/// flag-surface consistency but does not change the output.
pub fn render_metrics(data: &Value, _json: bool) -> Result<String> {
    Ok(serde_json::to_string_pretty(data)?)
}

/// Render one JSON field for human output. A JSON string is unwrapped so it
/// prints without the surrounding quotes that `Value`'s `Display` would add;
/// other value kinds fall back to their JSON form.
fn field(data: &Value, key: &str) -> String {
    match data.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_status() -> Value {
        json!({
            "daemon_version": "0.1.0",
            "uptime_seconds": 1234,
            "bind_address": "0.0.0.0",
            "nfs_port": 2049,
            "mount_port": 20048,
            "portmap_port": 111,
            "log_level": "info",
            "export_count": 2,
        })
    }

    #[test]
    fn render_status_json_round_trips() {
        let rendered = render_status(&sample_status(), true).expect("render json");
        let parsed: Value = serde_json::from_str(&rendered).expect("json round trip");
        assert_eq!(parsed, sample_status());
    }

    #[test]
    fn render_status_human_is_unquoted_and_labelled() {
        let rendered = render_status(&sample_status(), false).expect("render human");
        assert!(rendered.contains("Daemon version: 0.1.0"));
        assert!(rendered.contains("NFS port:       2049"));
        assert!(rendered.contains("Exports:        2"));
        // The human form must not leak JSON quoting around string values.
        assert!(!rendered.contains("\"0.1.0\""));
    }

    #[test]
    fn render_version_human_lists_build_fields() {
        let data = json!({
            "daemon_version": "0.1.0",
            "build_commit": "abc123",
            "rustc_version": "1.91.0",
            "build_profile": "release",
        });
        let rendered = render_version(&data, false).expect("render human");
        assert!(rendered.contains("Build commit:   abc123"));
        assert!(rendered.contains("Build profile:  release"));
    }

    #[test]
    fn field_falls_back_to_unknown_for_missing_key() {
        assert_eq!(field(&json!({}), "nope"), "unknown");
    }

    #[test]
    fn render_log_level_get_human_is_labelled_and_unquoted() {
        let data = json!({ "level": "debug" });
        let rendered = render_log_level_get(&data, false).expect("render human");
        assert_eq!(rendered, "Current log level: debug");
    }

    #[test]
    fn render_log_level_set_human_is_labelled() {
        let data = json!({ "level": "trace" });
        let rendered = render_log_level_set(&data, false).expect("render human");
        assert_eq!(rendered, "Log level set to trace");
    }

    #[test]
    fn render_log_level_json_round_trips() {
        let data = json!({ "level": "info" });
        let rendered = render_log_level_get(&data, true).expect("render json");
        let parsed: Value = serde_json::from_str(&rendered).expect("json round trip");
        assert_eq!(parsed, data);
    }

    // ----- `exports list` table-rendering tests. ------------------------
    // Cover the 0/1/many input rows so the dynamic column widths
    // (`uid_w`/`name_w`/`fsal_w`) are exercised in each regime: header
    // floor only (zero rows), header floor still dominates (one short
    // row), and content widening (long uid/name/fsal beats header).

    #[test]
    fn render_exports_list_empty_prints_header_only() {
        let data = json!({ "exports": [], "retired_uids": [] });
        let rendered = render_exports_list(&data, false).expect("render");
        // No rows → output is just the header line with header-width pads.
        assert_eq!(rendered.trim_end(), "UID  NAME  FSAL  READ-ONLY");
    }

    #[test]
    fn render_exports_list_one_row_aligned() {
        let data = json!({
            "exports": [
                { "uid": 1, "name": "/data", "fsal": "local", "read_only": false },
            ],
            "retired_uids": [],
        });
        let rendered = render_exports_list(&data, false).expect("render");
        // `/data` is wider than the `NAME` header (4 chars vs 5), so the
        // name column widens to 5; everything else is at the header floor.
        let expected = "UID  NAME   FSAL   READ-ONLY\n1    /data  local  false";
        assert_eq!(rendered.trim_end(), expected);
    }

    #[test]
    fn render_exports_list_many_rows_columns_align_to_widest() {
        // Mix a short row and a long row so the dynamic-width logic
        // actually has to widen each column past the header floor.
        let data = json!({
            "exports": [
                { "uid": 1, "name": "/d", "fsal": "local", "read_only": false },
                { "uid": 999999, "name": "/very/long/export/name", "fsal": "local", "read_only": true },
            ],
            "retired_uids": [],
        });
        let rendered = render_exports_list(&data, false).expect("render");
        let lines: Vec<&str> = rendered.lines().collect();
        // Header + 2 rows.
        assert_eq!(lines.len(), 3, "got: {rendered:?}");
        // Every line is the same length once the dynamic widths apply
        // (READ-ONLY column is the trailing field so no padding follows
        // it — but every line's READ-ONLY value must start at the same
        // column offset).
        let header = lines[0];
        let row1 = lines[1];
        let row2 = lines[2];
        let ro_col = header.find("READ-ONLY").expect("header has READ-ONLY");
        assert!(
            row1.starts_with("1   "),
            "uid column must left-pad to widest: {row1:?}"
        );
        assert_eq!(
            row1.get(ro_col..=ro_col),
            Some("f"),
            "row1 READ-ONLY must start at the header's READ-ONLY column: {row1:?}"
        );
        assert_eq!(
            row2.get(ro_col..=ro_col),
            Some("t"),
            "row2 READ-ONLY must start at the header's READ-ONLY column: {row2:?}"
        );
    }
}
