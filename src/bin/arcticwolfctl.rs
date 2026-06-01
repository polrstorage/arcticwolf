//! `arcticwolfctl` — operator CLI for the Arctic Wolf admin socket.
//!
//! Speaks the length-prefixed JSON admin protocol (issue #25). Phase 2
//! shipped two read-only subcommands (`status`, `version`); Phase 3 added
//! the nested `log-level get` / `log-level set`; Phase 5 adds the
//! `exports list/add/remove/update` and `config show` subcommands. A
//! global `--socket` flag overrides the socket path and `--json` swaps
//! the human-readable summary for the raw response payload.

use std::path::PathBuf;
use std::process::ExitCode;

use arcticwolf::admin::{DEFAULT_ADMIN_SOCKET_PATH, client};
use arcticwolf::config::{BackendConfig, ExportConfig};
use arcticwolf::fsal::multi_export::ExportSelector;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

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
    /// Inspect or change the daemon's live log level.
    LogLevel {
        #[command(subcommand)]
        action: LogLevelAction,
    },
    /// Inspect or mutate the live NFS export set.
    Exports {
        #[command(subcommand)]
        action: ExportsAction,
    },
    /// Inspect daemon configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

/// Sub-actions of the `log-level` command.
#[derive(Subcommand, Debug)]
enum LogLevelAction {
    /// Print the tracing filter currently in effect.
    Get,
    /// Set the daemon's log level (error/warn/info/debug/trace/off).
    Set {
        /// The log level to apply.
        level: String,
    },
}

/// Sub-actions of the `exports` command.
#[derive(Subcommand, Debug)]
enum ExportsAction {
    /// List the active exports (and retired uids in `--json`).
    List,
    /// Add a new export to the daemon. `--dry-run` validates only.
    Add(ExportsAddArgs),
    /// Remove an export. The uid is retired until daemon restart.
    Remove(ExportsSelectorArgs),
    /// Update an existing export. v1 only flips `read_only`.
    Update(ExportsUpdateArgs),
}

/// Sub-actions of the `config` command.
#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Dump the daemon's startup configuration as JSON.
    Show,
}

/// FSAL backend kind accepted by `exports add`. v1 only ships `local`;
/// `clap::ValueEnum` rejects anything else at parse-time with
/// `possible values: local`, so the daemon never sees an unknown variant.
#[derive(Clone, Debug, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum FsalKind {
    Local,
}

#[derive(Args, Debug)]
struct ExportsAddArgs {
    /// Export path advertised to clients (e.g. `/data`).
    #[arg(long)]
    name: String,
    /// Non-zero export uid (encoded in every file handle for this export).
    #[arg(long)]
    uid: u32,
    /// FSAL backend kind. v1 only supports `local`; clap rejects others.
    #[arg(long, value_enum)]
    fsal: FsalKind,
    /// For `--fsal local`: absolute path on the server's filesystem.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Mark the new export read-only.
    #[arg(long, default_value_t = false)]
    read_only: bool,
    /// Validate inputs against the live snapshot without mutating it.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

/// Selector arg pair shared by `remove`: exactly one of `--name`/`--uid`.
#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("selector")
        .required(true)
        .multiple(false)
        .args(["name", "uid"]),
))]
struct ExportsSelectorArgs {
    /// Select the export by its advertised path (e.g. `/data`).
    #[arg(long)]
    name: Option<String>,
    /// Select the export by its uid.
    #[arg(long)]
    uid: Option<u32>,
    /// Validate the selector against the live snapshot without mutating it.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

/// Selector + the v1-only `--read-only <bool>` flag for `update`.
#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("selector")
        .required(true)
        .multiple(false)
        .args(["name", "uid"]),
))]
struct ExportsUpdateArgs {
    /// Select the export by its advertised path (e.g. `/data`).
    #[arg(long)]
    name: Option<String>,
    /// Select the export by its uid.
    #[arg(long)]
    uid: Option<u32>,
    /// New `read_only` value. v1 requires this — `update_export` rejects a
    /// no-field-change call. Use `action = Set` so the flag takes an
    /// explicit `true`/`false` value rather than acting as a presence
    /// switch.
    #[arg(long, action = clap::ArgAction::Set, required = true)]
    read_only: bool,
    /// Validate against the live snapshot without mutating it.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Match by reference so the nested `LogLevel { action }` bindings do not
    // partially move `cli` out from under the `&cli` the run_* helpers take.
    match &cli.command {
        Command::Status => run_status(&cli).await,
        Command::Version => run_version(&cli).await,
        Command::LogLevel { action } => match action {
            LogLevelAction::Get => run_log_level_get(&cli).await,
            LogLevelAction::Set { level } => run_log_level_set(&cli, level).await,
        },
        Command::Exports { action } => match action {
            ExportsAction::List => run_exports_list(&cli).await,
            ExportsAction::Add(args) => run_exports_add(&cli, args).await,
            ExportsAction::Remove(args) => run_exports_remove(&cli, args).await,
            ExportsAction::Update(args) => run_exports_update(&cli, args).await,
        },
        Command::Config { action } => match action {
            ConfigAction::Show => run_config_show(&cli).await,
        },
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

/// `arcticwolfctl log-level get` — print the daemon's active log filter.
/// Any failure (connection refused, admin error) exits non-zero.
async fn run_log_level_get(cli: &Cli) -> ExitCode {
    let data = match client::fetch_log_level(&cli.socket).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: log-level get failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_log_level_get(&data, cli.json) {
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

/// `arcticwolfctl log-level set <level>` — change the daemon's log level.
/// An unrecognized level is rejected by the daemon, which surfaces here as
/// a stderr error and a non-zero exit (the issue #25 acceptance criterion).
async fn run_log_level_set(cli: &Cli, level: &str) -> ExitCode {
    let data = match client::set_log_level(&cli.socket, level).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: log-level set failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_log_level_set(&data, cli.json) {
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

/// Translate the CLI's `--name`/`--uid` pair into the wire `ExportSelector`.
///
/// Clap's `ArgGroup(required=true, multiple=false)` guarantees that exactly
/// one of the two is `Some`, so the caller never sees a `None`/`None` pair.
/// The `unreachable!` branch documents that invariant for the reader and
/// pins a regression test if the group config ever gets weakened.
fn selector_from(name: Option<&str>, uid: Option<u32>) -> ExportSelector {
    match (name, uid) {
        (Some(name), None) => ExportSelector::Name(name.to_string()),
        (None, Some(uid)) => ExportSelector::Uid(uid),
        _ => unreachable!("clap ArgGroup guarantees exactly one selector arg"),
    }
}

/// Build the wire-level [`ExportConfig`] from the CLI's `exports add` args.
///
/// `--fsal` is validated by clap as a `ValueEnum`, so this function only
/// handles the dispatch from `FsalKind` to `BackendConfig` and the
/// per-backend required-flag check (`--path` is mandatory for `local`).
fn export_config_from(args: &ExportsAddArgs) -> Result<ExportConfig, String> {
    match args.fsal {
        FsalKind::Local => {
            let path = args
                .path
                .clone()
                .ok_or_else(|| "exports add --fsal local requires --path".to_string())?;
            Ok(ExportConfig {
                name: args.name.clone(),
                uid: args.uid,
                read_only: args.read_only,
                backend: BackendConfig::Local { path },
            })
        }
    }
}

/// `arcticwolfctl exports list` — print the snapshot.
async fn run_exports_list(cli: &Cli) -> ExitCode {
    let data = match client::fetch_exports_list(&cli.socket).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: exports list failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_exports_list(&data, cli.json) {
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

/// `arcticwolfctl exports add` — add or pre-flight an export.
async fn run_exports_add(cli: &Cli, args: &ExportsAddArgs) -> ExitCode {
    let config = match export_config_from(args) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("arcticwolfctl: {err}");
            return ExitCode::FAILURE;
        }
    };
    let data = match client::exports_add(&cli.socket, config, args.dry_run).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: exports add failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_exports_add(&data, cli.json) {
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

/// `arcticwolfctl exports remove` — retire or pre-flight an export.
async fn run_exports_remove(cli: &Cli, args: &ExportsSelectorArgs) -> ExitCode {
    let selector = selector_from(args.name.as_deref(), args.uid);
    let data = match client::exports_remove(&cli.socket, selector, args.dry_run).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: exports remove failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_exports_remove(&data, cli.json) {
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

/// `arcticwolfctl exports update` — update or pre-flight an export.
async fn run_exports_update(cli: &Cli, args: &ExportsUpdateArgs) -> ExitCode {
    let selector = selector_from(args.name.as_deref(), args.uid);
    let data =
        match client::exports_update(&cli.socket, selector, args.read_only, args.dry_run).await {
            Ok(data) => data,
            Err(err) => {
                eprintln!("arcticwolfctl: exports update failed: {err:#}");
                return ExitCode::FAILURE;
            }
        };
    match client::render_exports_update(&data, cli.json) {
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

/// `arcticwolfctl config show` — dump the startup config as JSON.
async fn run_config_show(cli: &Cli) -> ExitCode {
    let data = match client::fetch_config_show(&cli.socket).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("arcticwolfctl: config show failed: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match client::render_config_show(&data, cli.json) {
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

    #[tokio::test]
    async fn run_log_level_get_exits_failure_when_socket_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = make_cli(
            dir.path().join("absent.sock"),
            Command::LogLevel {
                action: LogLevelAction::Get,
            },
        );
        assert_exit(run_log_level_get(&cli).await, ExitCode::FAILURE);
    }

    #[tokio::test]
    async fn run_log_level_get_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({ "level": "info" }),
        };
        let server = spawn_fake_server(&socket, response);

        let cli = make_cli(
            socket,
            Command::LogLevel {
                action: LogLevelAction::Get,
            },
        );
        assert_exit(run_log_level_get(&cli).await, ExitCode::SUCCESS);
        server.abort();
    }

    #[tokio::test]
    async fn run_log_level_set_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({ "level": "debug" }),
        };
        let server = spawn_fake_server(&socket, response);

        let cli = make_cli(
            socket,
            Command::LogLevel {
                action: LogLevelAction::Set {
                    level: "debug".to_string(),
                },
            },
        );
        assert_exit(run_log_level_set(&cli, "debug").await, ExitCode::SUCCESS);
        server.abort();
    }

    #[tokio::test]
    async fn run_log_level_set_exits_failure_when_daemon_returns_err() {
        // Pins the issue #25 acceptance criterion: `log-level set tomato`
        // (an unrecognized level the daemon rejects with an error) must
        // exit non-zero so operator scripts can detect the failure.
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(
            &socket,
            AdminResponse::error("Invalid log level 'tomato': expected one of ..."),
        );

        let cli = make_cli(
            socket,
            Command::LogLevel {
                action: LogLevelAction::Set {
                    level: "tomato".to_string(),
                },
            },
        );
        assert_exit(run_log_level_set(&cli, "tomato").await, ExitCode::FAILURE);
        server.abort();
    }

    // ---------------------------------------------------------------
    // Phase 5: exports list/add/remove/update and config show
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn run_exports_list_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({
                "exports": [
                    { "uid": 1, "name": "/data", "fsal": "local", "read_only": false }
                ],
                "retired_uids": [],
            }),
        };
        let server = spawn_fake_server(&socket, response);

        let code = run_exports_list(&make_cli(
            socket,
            Command::Exports {
                action: ExportsAction::List,
            },
        ))
        .await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }

    #[tokio::test]
    async fn run_exports_list_exits_failure_when_socket_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = make_cli(
            dir.path().join("absent.sock"),
            Command::Exports {
                action: ExportsAction::List,
            },
        );
        assert_exit(run_exports_list(&cli).await, ExitCode::FAILURE);
    }

    #[tokio::test]
    async fn run_exports_add_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({ "name": "/extra", "uid": 7 }),
        };
        let server = spawn_fake_server(&socket, response);

        let args = ExportsAddArgs {
            name: "/extra".to_string(),
            uid: 7,
            fsal: FsalKind::Local,
            path: Some(PathBuf::from("/srv/extra")),
            read_only: false,
            dry_run: false,
        };
        let code = run_exports_add(
            &make_cli(
                socket,
                Command::Exports {
                    action: ExportsAction::Add(ExportsAddArgs {
                        name: args.name.clone(),
                        uid: args.uid,
                        fsal: args.fsal.clone(),
                        path: args.path.clone(),
                        read_only: args.read_only,
                        dry_run: args.dry_run,
                    }),
                },
            ),
            &args,
        )
        .await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }

    /// Reject `--fsal s3` at clap parse time: with `FsalKind` as a
    /// `ValueEnum`, the rejection moves from `export_config_from`'s
    /// `String` match to clap's "possible values" validation, and the
    /// failure surfaces as a non-zero exit code (whatever message clap
    /// renders, which we don't pin further to keep the test robust).
    #[test]
    fn run_exports_add_rejects_unsupported_fsal_before_dialing_socket() {
        let result = Cli::try_parse_from([
            "arcticwolfctl",
            "exports",
            "add",
            "--name",
            "/extra",
            "--uid",
            "7",
            "--fsal",
            "s3",
            "--path",
            "/srv/extra",
        ]);
        let err = result.expect_err("unsupported --fsal must fail at parse time");
        // Don't pin the exact wording — only that clap is the source and
        // that it mentioned the one supported value.
        let rendered = err.to_string();
        assert!(
            rendered.contains("local"),
            "clap error should mention 'local' as a possible value; got: {rendered}"
        );
    }

    #[tokio::test]
    async fn run_exports_remove_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({ "uid": 1, "name": "/data" }),
        };
        let server = spawn_fake_server(&socket, response);

        let args = ExportsSelectorArgs {
            name: Some("/data".to_string()),
            uid: None,
            dry_run: false,
        };
        let code = run_exports_remove(
            &make_cli(
                socket,
                Command::Exports {
                    action: ExportsAction::Remove(ExportsSelectorArgs {
                        name: args.name.clone(),
                        uid: args.uid,
                        dry_run: args.dry_run,
                    }),
                },
            ),
            &args,
        )
        .await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }

    #[tokio::test]
    async fn run_exports_update_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({ "uid": 1, "name": "/data", "read_only": true }),
        };
        let server = spawn_fake_server(&socket, response);

        let args = ExportsUpdateArgs {
            name: Some("/data".to_string()),
            uid: None,
            read_only: true,
            dry_run: false,
        };
        let code = run_exports_update(
            &make_cli(
                socket,
                Command::Exports {
                    action: ExportsAction::Update(ExportsUpdateArgs {
                        name: args.name.clone(),
                        uid: args.uid,
                        read_only: args.read_only,
                        dry_run: args.dry_run,
                    }),
                },
            ),
            &args,
        )
        .await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }

    // ----- Daemon-Err exit-code coverage for Phase 5 commands. ----------
    // Mirrors `run_log_level_set_exits_failure_when_daemon_returns_err`: if
    // the daemon returns `AdminResponse::Err`, the CLI must surface a
    // non-zero exit so operator scripts notice. One test per command —
    // combinatorial coverage adds no value over the underlying contract.

    #[tokio::test]
    async fn run_exports_list_exits_failure_when_daemon_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(&socket, AdminResponse::error("simulated"));
        let cli = make_cli(
            socket,
            Command::Exports {
                action: ExportsAction::List,
            },
        );
        assert_exit(run_exports_list(&cli).await, ExitCode::FAILURE);
        server.abort();
    }

    #[tokio::test]
    async fn run_exports_add_exits_failure_when_daemon_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(&socket, AdminResponse::error("simulated"));
        let args = ExportsAddArgs {
            name: "/extra".to_string(),
            uid: 7,
            fsal: FsalKind::Local,
            path: Some(PathBuf::from("/srv/extra")),
            read_only: false,
            dry_run: false,
        };
        let code = run_exports_add(
            &make_cli(
                socket,
                Command::Exports {
                    action: ExportsAction::Add(ExportsAddArgs {
                        name: args.name.clone(),
                        uid: args.uid,
                        fsal: args.fsal.clone(),
                        path: args.path.clone(),
                        read_only: args.read_only,
                        dry_run: args.dry_run,
                    }),
                },
            ),
            &args,
        )
        .await;
        assert_exit(code, ExitCode::FAILURE);
        server.abort();
    }

    #[tokio::test]
    async fn run_exports_remove_exits_failure_when_daemon_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(&socket, AdminResponse::error("simulated"));
        let args = ExportsSelectorArgs {
            name: Some("/data".to_string()),
            uid: None,
            dry_run: false,
        };
        let code = run_exports_remove(
            &make_cli(
                socket,
                Command::Exports {
                    action: ExportsAction::Remove(ExportsSelectorArgs {
                        name: args.name.clone(),
                        uid: args.uid,
                        dry_run: args.dry_run,
                    }),
                },
            ),
            &args,
        )
        .await;
        assert_exit(code, ExitCode::FAILURE);
        server.abort();
    }

    #[tokio::test]
    async fn run_exports_update_exits_failure_when_daemon_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(&socket, AdminResponse::error("simulated"));
        let args = ExportsUpdateArgs {
            name: Some("/data".to_string()),
            uid: None,
            read_only: true,
            dry_run: false,
        };
        let code = run_exports_update(
            &make_cli(
                socket,
                Command::Exports {
                    action: ExportsAction::Update(ExportsUpdateArgs {
                        name: args.name.clone(),
                        uid: args.uid,
                        read_only: args.read_only,
                        dry_run: args.dry_run,
                    }),
                },
            ),
            &args,
        )
        .await;
        assert_exit(code, ExitCode::FAILURE);
        server.abort();
    }

    #[tokio::test]
    async fn run_config_show_exits_failure_when_daemon_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let server = spawn_fake_server(&socket, AdminResponse::error("simulated"));
        let cli = make_cli(
            socket,
            Command::Config {
                action: ConfigAction::Show,
            },
        );
        assert_exit(run_config_show(&cli).await, ExitCode::FAILURE);
        server.abort();
    }

    #[tokio::test]
    async fn run_config_show_exits_success_on_ok_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let response = AdminResponse::Ok {
            data: json!({
                "server": { "bind_address": "0.0.0.0", "nfs_port": 2049, "mount_port": 0 },
                "exports": [],
                "logging": { "level": null },
                "admin": {
                    "enabled": false,
                    "socket_path": "/run/arcticwolf/admin.sock",
                    "socket_mode": 384,
                },
            }),
        };
        let server = spawn_fake_server(&socket, response);

        let code = run_config_show(&make_cli(
            socket,
            Command::Config {
                action: ConfigAction::Show,
            },
        ))
        .await;
        assert_exit(code, ExitCode::SUCCESS);
        server.abort();
    }

    /// Parse-level smoke checks for the new clap subtrees: the
    /// mutually-exclusive selector group on `exports remove` and the
    /// required `--read-only` flag on `exports update`.
    #[test]
    fn cli_parse_exports_remove_requires_exactly_one_selector() {
        // Neither --name nor --uid → ArgGroup error.
        assert!(
            Cli::try_parse_from(["arcticwolfctl", "exports", "remove"]).is_err(),
            "missing selector must fail"
        );
        // Both --name and --uid → ArgGroup error.
        assert!(
            Cli::try_parse_from([
                "arcticwolfctl",
                "exports",
                "remove",
                "--name",
                "/data",
                "--uid",
                "1",
            ])
            .is_err(),
            "two selectors must fail"
        );
        // --name alone → ok.
        Cli::try_parse_from(["arcticwolfctl", "exports", "remove", "--name", "/data"])
            .expect("--name alone must parse");
        // --uid alone → ok.
        Cli::try_parse_from(["arcticwolfctl", "exports", "remove", "--uid", "5"])
            .expect("--uid alone must parse");
    }

    #[test]
    fn cli_parse_exports_update_requires_read_only_flag() {
        // Missing --read-only → clap error.
        assert!(
            Cli::try_parse_from(["arcticwolfctl", "exports", "update", "--name", "/data"]).is_err(),
            "missing --read-only must fail"
        );
        // With --read-only → ok.
        Cli::try_parse_from([
            "arcticwolfctl",
            "exports",
            "update",
            "--name",
            "/data",
            "--read-only",
            "true",
        ])
        .expect("update --read-only=true must parse");
    }

    /// `--read-only false` must parse exactly like `--read-only true`.
    /// Locking this avoids a regression where clap's `ArgAction::Set` on
    /// a `bool` field silently turns into a presence switch (which would
    /// make `false` a synonym for `true`).
    #[test]
    fn cli_parse_exports_update_accepts_read_only_false() {
        Cli::try_parse_from([
            "arcticwolfctl",
            "exports",
            "update",
            "--name",
            "/data",
            "--read-only",
            "false",
        ])
        .expect("update --read-only=false must parse");
    }

    #[test]
    fn cli_parse_exports_add_accepts_local_fsal() {
        Cli::try_parse_from([
            "arcticwolfctl",
            "exports",
            "add",
            "--name",
            "/extra",
            "--uid",
            "9",
            "--fsal",
            "local",
            "--path",
            "/srv/extra",
        ])
        .expect("typical exports add must parse");
    }
}
