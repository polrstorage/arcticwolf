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
}
