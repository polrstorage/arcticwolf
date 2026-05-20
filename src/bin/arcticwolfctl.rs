//! `arcticwolfctl` — operator CLI for the Arctic Wolf admin socket.
//!
//! Speaks the length-prefixed JSON admin protocol (issue #25). Phase 2
//! ships two read-only subcommands, `status` and `version`. A global
//! `--socket` flag overrides the socket path and `--json` swaps the
//! human-readable summary for the raw response payload.

use std::path::PathBuf;
use std::process::ExitCode;

use arcticwolf::admin::{DEFAULT_ADMIN_SOCKET_PATH, client};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "arcticwolfctl", about = "Arctic Wolf admin client", long_about = None)]
struct Cli {
    /// Path to the daemon's admin Unix domain socket.
    #[arg(long, global = true, default_value = DEFAULT_ADMIN_SOCKET_PATH)]
    socket: PathBuf,

    /// Print the raw JSON payload instead of a human-readable summary.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show a daemon health snapshot (uptime, ports, exports).
    Status,
    /// Show client and daemon version information.
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Status => run_status(&cli).await,
        Command::Version => run_version(&cli).await,
    }
}

/// `arcticwolfctl status` — fetch and print the daemon health snapshot.
/// Any failure (connection refused, admin error) exits non-zero.
async fn run_status(cli: &Cli) -> ExitCode {
    let data = match client::fetch_status(&cli.socket).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: status failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_status(&data, cli.json) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("arcticwolfctl: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `arcticwolfctl version` — print the client's own version, then the
/// daemon's. The client version is always shown; if the daemon cannot be
/// reached the command still succeeds and reports it as unreachable.
async fn run_version(cli: &Cli) -> ExitCode {
    let client_version = env!("CARGO_PKG_VERSION");
    let daemon = client::fetch_version(&cli.socket).await;

    if cli.json {
        let daemon_value = match &daemon {
            Ok(data) => {
                // Add an explicit `reachable` discriminator so scripts can
                // branch on `.daemon.reachable` instead of inferring
                // reachability from which keys happen to be present.
                let mut value = data.clone();
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert("reachable".to_string(), serde_json::Value::Bool(true));
                }
                value
            }
            Err(err) => serde_json::json!({
                "reachable": false,
                "error": err.to_string(),
            }),
        };
        let combined = serde_json::json!({
            "client": { "version": client_version },
            "daemon": daemon_value,
        });
        match serde_json::to_string_pretty(&combined) {
            Ok(text) => {
                println!("{text}");
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                eprintln!("arcticwolfctl: {err:#}");
                return ExitCode::FAILURE;
            }
        }
    }

    println!("Client:");
    println!("  arcticwolfctl version: {client_version}");
    println!();
    println!("Daemon:");
    match daemon {
        Ok(data) => match client::render_version(&data, false) {
            Ok(text) => {
                for line in text.lines() {
                    println!("  {line}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("arcticwolfctl: {err:#}");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            println!("  daemon unreachable: {err}");
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use arcticwolf::admin::{AdminResponse, protocol};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    use super::*;

    /// `ExitCode` implements neither `PartialEq` nor a value accessor, so
    /// the contract is asserted by comparing `Debug` output — `SUCCESS`
    /// and `FAILURE` render distinctly, which is all these checks need.
    fn assert_exit(actual: ExitCode, expected: ExitCode) {
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }

    /// Build a `Cli` for the in-process `run_*` entry points, bypassing
    /// `clap` argument parsing.
    fn make_cli(socket: PathBuf, command: Command) -> Cli {
        Cli {
            socket,
            json: false,
            command,
        }
    }

    /// Bind a one-shot fake admin server at `socket_path` that answers the
    /// first request with `response`, then closes. The socket is bound
    /// before this returns so a client can connect without a race; the
    /// accept-and-answer step runs on the returned task.
    fn spawn_fake_server(socket_path: &Path, response: AdminResponse) -> JoinHandle<()> {
        let listener = UnixListener::bind(socket_path).expect("bind fake admin socket");
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let mut connection = protocol::framed(stream);
                // The request content is irrelevant to the exit-code
                // contract under test — just drain the single frame.
                let _ = connection.next().await;
                if let Ok(encoded) = protocol::encode_response(&response) {
                    let _ = connection.send(encoded).await;
                }
            }
        })
    }

    #[tokio::test]
    async fn run_status_exits_failure_when_socket_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = make_cli(dir.path().join("absent.sock"), Command::Status);
        assert_exit(run_status(&cli).await, ExitCode::FAILURE);
    }

    #[tokio::test]
    async fn run_status_exits_failure_when_daemon_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(&socket, AdminResponse::error("simulated admin failure"));

        let code = run_status(&make_cli(socket, Command::Status)).await;
        assert_exit(code, ExitCode::FAILURE);
        server.abort();
    }

    #[tokio::test]
    async fn run_status_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({
                "daemon_version": "0.1.0",
                "uptime_seconds": 1,
                "bind_address": "0.0.0.0",
                "nfs_port": 2049,
                "mount_port": 20048,
                "portmap_port": 111,
                "log_level": "info",
                "export_count": 1,
            }),
        };
        let server = spawn_fake_server(&socket, response);

        let code = run_status(&make_cli(socket, Command::Status)).await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }

    #[tokio::test]
    async fn run_version_exits_success_when_daemon_unreachable() {
        // The client version is always printable, so an unreachable
        // daemon must not turn `version` into a failure — operator
        // scripts rely on `version` succeeding offline.
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = make_cli(dir.path().join("absent.sock"), Command::Version);
        assert_exit(run_version(&cli).await, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn run_version_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({
                "daemon_version": "0.1.0",
                "build_commit": "abc123",
                "rustc_version": "1.91.0",
                "build_profile": "release",
            }),
        };
        let server = spawn_fake_server(&socket, response);

        let code = run_version(&make_cli(socket, Command::Version)).await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }
}
