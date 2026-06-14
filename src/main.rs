#[cfg(not(target_os = "linux"))]
compile_error!("Arctic Wolf NFS server only supports Linux");

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, reload, util::SubscriberInitExt};

use arcticwolf::admin;
use arcticwolf::config::{self, Config};
use arcticwolf::fsal;
use arcticwolf::metrics::Metrics;
use arcticwolf::portmap;
use arcticwolf::protocol::v3::portmap::mapping;
use arcticwolf::rpc;
use arcticwolf::shutdown::{self, Signal};

/// Portmapper port is fixed at 111 per RFC 1833
const PORTMAP_PORT: u16 = 111;

/// Register all RPC services in the portmapper registry
///
/// This makes services discoverable via PMAPPROC_GETPORT queries.
fn register_services(
    registry: &portmap::Registry,
    portmap_port: u32,
    mount_port: u32,
    nfs_port: u32,
) {
    const IPPROTO_TCP: u32 = 6;

    println!("Registering services:");

    // Register Portmapper itself (program 100000)
    let portmap_tcp = mapping {
        prog: 100000, // PORTMAP
        vers: 2,      // Version 2
        prot: IPPROTO_TCP,
        port: portmap_port,
    };
    registry.set(&portmap_tcp);
    println!("  Portmapper v2 (TCP) on port {}", portmap_port);

    // Register MOUNT protocol (program 100005)
    let mount_tcp = mapping {
        prog: 100005, // MOUNT
        vers: 3,      // MOUNTv3
        prot: IPPROTO_TCP,
        port: mount_port,
    };
    registry.set(&mount_tcp);
    println!("  MOUNT v3 (TCP) on port {}", mount_port);

    // Register NFS protocol (program 100003)
    let nfs_tcp = mapping {
        prog: 100003, // NFS
        vers: 3,      // NFSv3
        prot: IPPROTO_TCP,
        port: nfs_port,
    };
    registry.set(&nfs_tcp);
    println!("  NFS v3 (TCP) on port {}", nfs_port);

    println!();
}

/// Build the admin server future based on `[admin]` config.
///
/// When `enabled = false`, returns a future that simply waits for the
/// shutdown token and then resolves `Ok(())`. (A `pending()` here would
/// never resolve, forcing the whole-daemon drain to always hit its timeout
/// when admin is disabled.) When enabled, binds the socket eagerly so any
/// setup failure (missing parent dir, EACCES, stale non-socket at the path)
/// returns from `main()` with a clear error instead of being deferred, then
/// serves until the token is cancelled.
///
/// The future is boxed `+ Send` so the caller can `tokio::spawn` it
/// alongside the RPC server tasks.
fn build_admin_future(
    admin: &config::AdminConfig,
    context: Arc<admin::AdminContext>,
    shutdown: CancellationToken,
    in_flight: Arc<shutdown::InFlight>,
) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>> {
    if !admin.enabled {
        return Ok(Box::pin(async move {
            shutdown.cancelled().await;
            Ok(())
        }));
    }

    println!("Admin server:");
    println!("  Socket: {}", admin.socket_path.display());
    println!("  Mode: {:o}", admin.socket_mode);
    println!();

    let listener = admin::server::bind_admin_socket(&admin.socket_path, admin.socket_mode)?;
    let socket_path = admin.socket_path.clone();
    Ok(Box::pin(admin::serve_with_shutdown(
        listener,
        socket_path,
        context,
        shutdown,
        in_flight,
    )))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Capture the process start instant up front so the admin `status`
    // command can report an accurate uptime.
    let process_start_instant = std::time::Instant::now();

    // Load configuration first (before tracing init). Wrapped in an `Arc`
    // so it can be shared with the admin context without a deep copy.
    let config = Arc::new(Config::load()?);

    // Initialize tracing with the configured log filter.
    // Priority: config file -> RUST_LOG env -> "info".
    //
    // The filter is an `EnvFilter` wrapped in a `reload::Layer` so the admin
    // `log-level` commands can swap it at runtime; `log_reload` is threaded
    // into the `AdminContext` below. The `EnvFilter` layer filters globally
    // for the registry, so the `fmt` layer only sees events that pass it.
    let log_filter_str = config.logging.effective_level();
    let env_filter = match EnvFilter::try_new(&log_filter_str) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!(
                "Warning: invalid log filter '{log_filter_str}': {err}; falling back to 'info'"
            );
            EnvFilter::new("info")
        }
    };
    // The directive that actually took effect — captured here because
    // `env_filter` is moved into the reload layer on the next line. The
    // startup banner surfaces it to operators; the admin `status` command
    // reads the live value through the reload handle instead.
    let effective_log_filter = env_filter.to_string();
    let (filter_layer, log_reload) = reload::Layer::new(env_filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    // === Graceful shutdown wiring (issue #25 Phase 8) ===
    //
    // Install the signal handlers as early as possible — immediately after
    // tracing init (so the watch task's `tracing::info!` isn't dropped, per
    // the logging-order rule) and *before* any side-effecting init (FSAL
    // build, port binds, audit-file open, admin-socket bind). A SIGTERM that
    // arrives during init must be caught and turned into a clean shutdown
    // rather than killing the process with the default action — which would
    // leave half-bound listeners and possibly an orphaned admin socket file
    // behind. (Fix 3)
    //
    // We deliberately do *not* try to abort init when a signal lands mid-init:
    // init runs to completion, the four server tasks are spawned as usual, and
    // each one observes the already-cancelled token on its first select! and
    // breaks out of its accept loop before taking a single connection. The
    // drain then joins them immediately (0 in-flight), and the normal
    // audit-flush + socket-cleanup tail runs. Because that tail only runs after
    // init has finished, `audit` always exists by the time we touch it — there
    // is no early-exit path that could reach cleanup before `audit` is built,
    // so no guarding is required.
    //
    // A single `CancellationToken` fans the signal out to all four accept
    // loops. We chose `tokio_util::sync::CancellationToken` over a
    // `tokio::sync::watch`: it is already on the dependency tree, its
    // `cancelled()` future is exactly the shape each accept loop's
    // `tokio::select!` arm wants, and `cancel()` is idempotent and broadcast
    // to every clone — no `*borrow()`/`changed()` dance.
    //
    // Flow: the first SIGTERM/SIGINT cancels the token, so every server stops
    // accepting and drains its in-flight tasks (bounded by `[shutdown]
    // drain_timeout_seconds`); then the audit writer is flushed and the admin
    // socket file removed; finally the process exits 0. A *second* signal while
    // draining escalates to an immediate `std::process::exit` with the
    // conventional `128 + signum` code (130 for SIGINT, 143 for SIGTERM).
    let shutdown_token = CancellationToken::new();

    // Install the signal handlers up front so a `signal()` failure is a clear
    // startup error (propagated via `?`) rather than a silent drop inside a
    // spawned task.
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    {
        let shutdown_token = shutdown_token.clone();
        tokio::spawn(async move {
            // First signal: begin graceful shutdown.
            tokio::select! {
                _ = sigterm.recv() => tracing::info!("received SIGTERM, beginning graceful shutdown"),
                _ = sigint.recv() => tracing::info!("received SIGINT, beginning graceful shutdown"),
            }
            shutdown_token.cancel();
            // Second signal: abandon the drain and exit immediately. This
            // `process::exit` is why the escalation path can't be unit-tested
            // in-process (see `arcticwolf::shutdown` tests, which cover the
            // pure exit-code mapping instead).
            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::warn!("second signal (SIGTERM) during drain, exiting immediately");
                    std::process::exit(Signal::Terminate.escalation_exit_code());
                }
                _ = sigint.recv() => {
                    tracing::warn!("second signal (SIGINT) during drain, exiting immediately");
                    std::process::exit(Signal::Interrupt.escalation_exit_code());
                }
            }
        });
    }

    // Process-wide in-flight handler counter, shared by all four servers so
    // the drain-timeout warn can report the aggregate still-running count.
    // (Fix 9)
    let in_flight = Arc::new(shutdown::InFlight::new());

    println!("Arctic Wolf NFS Server");
    println!("======================");
    println!("Configuration:");
    println!("  Bind address: {}", config.server.bind_address);
    println!("  Portmap port: {}", PORTMAP_PORT);
    println!(
        "  Mount port: {}",
        if config.server.mount_port == 0 {
            "dynamic".to_string()
        } else {
            config.server.mount_port.to_string()
        }
    );
    println!("  NFS port: {}", config.server.nfs_port);
    println!("  Log level: {}", effective_log_filter);
    println!();

    // Initialize FSAL (File System Abstraction Layer).
    //
    // MultiExportFilesystem owns one backend per configured export and
    // routes operations by the uid prefix in each file handle. MOUNT MNT
    // resolves the client-supplied dirpath against `ExportRegistry`; NFS
    // still consumes the wrapper as `&dyn Filesystem`.
    println!("Initializing FSAL:");

    // The concrete `MultiExportFilesystem` is shared with both the RPC
    // servers (as `Arc<dyn NfsBackend>`) and the admin context (as the
    // concrete type, so admin handlers can call the inherent mutators).
    let multi_export = Arc::new(fsal::MultiExportFilesystem::build_from_config(
        &config.exports,
    )?);
    let filesystem: Arc<dyn fsal::NfsBackend> = multi_export.clone();

    // Phase 7: single in-memory metrics registry, constructed once and
    // shared (by `Arc` clone) with every RPC server and the admin context.
    // Per-export counters live on each `ExportEntry` inside the filesystem
    // router, so they are not threaded through here.
    let metrics = Arc::new(Metrics::new());

    let exports = filesystem.list_exports();
    for export in &exports {
        // Backend type/path live in `config.exports` (the FSAL view via
        // `ExportInfo` is intentionally backend-agnostic). Cross-reference
        // by uid so the banner stays accurate as we add backends.
        let source = config
            .exports
            .iter()
            .find(|e| e.uid == export.uid)
            .expect("every ExportInfo originates from a config entry");
        let backend_path = match &source.backend {
            config::BackendConfig::Local { path } => path.display().to_string(),
        };
        println!(
            "  Export: {} (uid {}, {}, backend={}, path={})",
            export.name,
            export.uid,
            if export.read_only { "ro" } else { "rw" },
            source.backend.name(),
            backend_path,
        );
    }
    println!();

    // Create portmapper registry
    let registry = portmap::Registry::new();

    // Bind each service to its own port
    let portmap_server = rpc::server::RpcServer::bind(
        &config.bind_addr_for(PORTMAP_PORT),
        registry.clone(),
        filesystem.clone(),
        vec![100000], // PORTMAP only
        metrics.clone(),
    )
    .await?;

    let mount_server = rpc::server::RpcServer::bind(
        &config.bind_addr_for(config.server.mount_port),
        registry.clone(),
        filesystem.clone(),
        vec![100005], // MOUNT only
        metrics.clone(),
    )
    .await?;

    // Get the actual mount port (may be dynamically assigned)
    let actual_mount_port = mount_server.local_port()? as u32;

    let nfs_server = rpc::server::RpcServer::bind(
        &config.bind_addr_for(config.server.nfs_port),
        registry.clone(),
        filesystem.clone(),
        vec![100003], // NFS only
        metrics.clone(),
    )
    .await?;

    // Register services with their actual ports
    register_services(
        &registry,
        PORTMAP_PORT as u32,
        actual_mount_port,
        config.server.nfs_port as u32,
    );

    // Admin Unix-domain-socket server (issue #25).
    //
    // When `[admin] enabled = false` (the default) we deliberately do not
    // bind a socket or spawn any task — `admin_future` becomes a
    // never-resolving sentinel so it's selectable with the other servers
    // but never wakes the select!.
    //
    // The admin `status` command reports the *actual* listening ports, so
    // they are read back from the bound listeners here — a configured port
    // of 0 means the OS picked one.
    let server_metadata = Arc::new(admin::ServerMetadata {
        nfs_port: nfs_server.local_port()?,
        mount_port: mount_server.local_port()?,
        portmap_port: portmap_server.local_port()?,
    });

    // Phase 6: build the audit writer based on `[audit]` config. When
    // disabled (the default) we get a `NoopAuditWriter` so the dispatch
    // path can `record(event)` without branching on whether audit is on.
    // When enabled, opening the file fails fast — the daemon refuses to
    // start with audit configured but unwritable.
    let audit_writer: Arc<dyn admin::AuditWriter> = if config.audit.enabled {
        let path = config
            .audit
            .path
            .as_ref()
            .expect("validate() guarantees audit.path is set when audit.enabled is true");
        println!("Audit log:");
        println!("  Path: {}", path.display());
        println!();
        Arc::new(admin::FileAuditWriter::open(path)?)
    } else {
        Arc::new(admin::NoopAuditWriter)
    };
    // Keep a clone for the shutdown flush. The other clone is moved into the
    // admin context below; on shutdown `shutdown()` closes the channel and
    // joins the writer task regardless of how many `Arc` clones remain (see
    // `FileAuditWriter::shutdown`).
    let audit_for_shutdown = audit_writer.clone();

    // The admin context shares the daemon's single `MultiExportFilesystem`
    // instance with the RPC servers, so the admin `status` command and the
    // NFS path always observe the same set of exports.
    let admin_context = admin::AdminContext::shared(
        process_start_instant,
        server_metadata,
        log_reload,
        multi_export.clone(),
        config.clone(),
        audit_writer,
        metrics.clone(),
    );

    // Spawn all four servers, each holding a clone of the shutdown token (set
    // up early, above) and the shared in-flight counter. If a signal already
    // fired during init the token is cancelled, so each server breaks out of
    // its accept loop on the first select! without taking a connection.
    let portmap_handle = {
        let token = shutdown_token.clone();
        let in_flight = in_flight.clone();
        tokio::spawn(async move { portmap_server.serve(token, in_flight).await })
    };
    let mount_handle = {
        let token = shutdown_token.clone();
        let in_flight = in_flight.clone();
        tokio::spawn(async move { mount_server.serve(token, in_flight).await })
    };
    let nfs_handle = {
        let token = shutdown_token.clone();
        let in_flight = in_flight.clone();
        tokio::spawn(async move { nfs_server.serve(token, in_flight).await })
    };
    let admin_handle = {
        let admin_future = build_admin_future(
            &config.admin,
            admin_context,
            shutdown_token.clone(),
            in_flight.clone(),
        )?;
        tokio::spawn(admin_future)
    };

    // Block until the shutdown signal fires (the signal task cancels the
    // token). Until then every server is busy in its own task.
    shutdown_token.cancelled().await;
    let drain_start_instant = Instant::now();
    tracing::info!("shutdown signal received, draining in-flight requests");

    // Drain every server, bounded by the configured timeout. The join
    // collects each server's exit status; errors are logged (not
    // propagated) because we're already shutting down and want the other
    // servers to finish regardless. If the timeout fires, `drain` is dropped
    // — detaching (not aborting) the still-running tasks — and the daemon
    // exits anyway with status 0.
    let drain_timeout = Duration::from_secs(config.shutdown.drain_timeout_seconds);
    let drain = async {
        let (portmap_res, mount_res, nfs_res, admin_res) =
            tokio::join!(portmap_handle, mount_handle, nfs_handle, admin_handle);
        for (label, res) in [
            ("portmap", portmap_res),
            ("mount", mount_res),
            ("nfs", nfs_res),
            ("admin", admin_res),
        ] {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(
                        server = label,
                        "server exited with error during shutdown: {err:#}"
                    )
                }
                Err(join_err) => {
                    tracing::warn!(
                        server = label,
                        "server task panicked during shutdown: {join_err}"
                    )
                }
            }
        }
    };
    let _ = shutdown::drain_with_timeout(drain, drain_timeout, &in_flight).await;

    // Flush the audit writer: closes its channel and joins the writer task so
    // every queued event reaches disk. A no-op for the `NoopAuditWriter`.
    audit_for_shutdown.shutdown().await;

    // Fallback admin-socket cleanup. `serve_with_shutdown` already removes
    // the socket on a clean drain; this covers the timeout path where that
    // task may not have returned. Best-effort — the file is usually gone.
    if config.admin.enabled {
        let _ = std::fs::remove_file(&config.admin.socket_path);
    }

    tracing::info!(
        total_ms = drain_start_instant.elapsed().as_millis(),
        "shutdown complete"
    );
    Ok(())
}
