#[cfg(not(target_os = "linux"))]
compile_error!("Arctic Wolf NFS server only supports Linux");

use anyhow::Result;
use std::sync::Arc;

// `config` is intentionally NOT redeclared with `mod config;` here. It lives
// in the library crate (see `src/lib.rs`) and is consumed from the binary
// via `arcticwolf::config`. This avoids the dual-compilation issue that
// happens when both `lib.rs` and `main.rs` declare the same module (the
// module gets compiled twice into separate types).
//
// The other modules (fsal/mount/nfs/portmap/protocol/rpc) are still
// declared below for backward compatibility — they are the binary's own
// view of the same sources. Migrating those is a separate cleanup; the
// `config` consolidation is what mattered for #26 because the new
// `Config` / `ExportConfig` types crossed the lib boundary.
mod fsal;
mod mount;
mod nfs;
mod portmap;
mod protocol;
mod rpc;

// `admin` (issue #25) is consumed from the library crate the same way
// `config` is — there's no reason to have a binary-local copy and doing
// so would re-trigger the dual-compilation problem documented above.
use arcticwolf::admin;
use arcticwolf::config::{self, Config};
use protocol::v3::portmap::mapping;

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
/// When `enabled = false`, returns a `pending` future — the caller drops
/// it into `tokio::select!` alongside the RPC servers, and that branch
/// never fires. When enabled, binds the socket eagerly so any setup
/// failure (missing parent dir, EACCES, stale non-socket at the path)
/// returns from `main()` with a clear error instead of being deferred.
fn build_admin_future(
    admin: &config::AdminConfig,
    context: Arc<admin::AdminContext>,
) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>>>>> {
    if !admin.enabled {
        return Ok(Box::pin(std::future::pending::<Result<()>>()));
    }

    println!("Admin server:");
    println!("  Socket: {}", admin.socket_path.display());
    println!("  Mode: {:o}", admin.socket_mode);
    println!();

    let listener = admin::server::bind_admin_socket(&admin.socket_path, admin.socket_mode)?;
    let socket_path = admin.socket_path.clone();
    Ok(Box::pin(admin::serve(listener, socket_path, context)))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Capture the process start instant up front so the admin `status`
    // command can report an accurate uptime.
    let start_time = std::time::Instant::now();

    // Load configuration first (before tracing init). Wrapped in an `Arc`
    // so it can be shared with the admin context without a deep copy.
    let config = Arc::new(Config::load()?);

    // Initialize tracing with configured log level
    // Priority: config file -> RUST_LOG env -> "info"
    let log_level_str = config.logging.effective_level();
    let log_level = match log_level_str.parse() {
        Ok(level) => level,
        Err(_) => {
            eprintln!(
                "Warning: Invalid log level '{}', falling back to 'info'",
                log_level_str
            );
            tracing::Level::INFO
        }
    };
    tracing_subscriber::fmt().with_max_level(log_level).init();

    // The log level that actually took effect. When `log_level_str` fails
    // to parse we fall back to INFO above, so this must report `info`
    // rather than the rejected configuration string — the startup banner
    // and the admin `status` command both surface it to operators.
    let startup_log_level = log_level.to_string().to_lowercase();

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
    println!("  Log level: {}", startup_log_level);
    println!();

    // Initialize FSAL (File System Abstraction Layer).
    //
    // MultiExportFilesystem owns one backend per configured export and
    // routes operations by the uid prefix in each file handle. MOUNT MNT
    // resolves the client-supplied dirpath against `ExportRegistry`; NFS
    // still consumes the wrapper as `&dyn Filesystem`.
    println!("Initializing FSAL:");

    let filesystem: Arc<dyn fsal::NfsBackend> = Arc::new(
        fsal::MultiExportFilesystem::build_from_config(&config.exports)?,
    );

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
    )
    .await?;

    let mount_server = rpc::server::RpcServer::bind(
        &config.bind_addr_for(config.server.mount_port),
        registry.clone(),
        filesystem.clone(),
        vec![100005], // MOUNT only
    )
    .await?;

    // Get the actual mount port (may be dynamically assigned)
    let actual_mount_port = mount_server.local_port()? as u32;

    let nfs_server = rpc::server::RpcServer::bind(
        &config.bind_addr_for(config.server.nfs_port),
        registry.clone(),
        filesystem.clone(),
        vec![100003], // NFS only
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

    // `AdminContext` lives in the library crate, so it needs the library's
    // `NfsBackend` view — a distinct type from the binary-local `fsal`
    // module compiled into this binary (see the module comment at the top
    // of this file). Until that duplication is cleaned up, build a second
    // router from the same export config purely so the admin `status`
    // command can report `export_count`; both routers observe identical
    // configuration.
    let admin_filesystem: Arc<dyn arcticwolf::fsal::NfsBackend> = Arc::new(
        arcticwolf::fsal::MultiExportFilesystem::build_from_config(&config.exports)?,
    );
    let admin_context = admin::AdminContext::shared(
        start_time,
        server_metadata,
        startup_log_level,
        admin_filesystem,
        config.clone(),
    );

    let admin_future = build_admin_future(&config.admin, admin_context)?;

    // Run all servers concurrently. The admin branch is a `pending` future
    // when admin is disabled, so its presence in `select!` is free.
    tokio::select! {
        result = portmap_server.serve() => {
            result?;
        }
        result = mount_server.serve() => {
            result?;
        }
        result = nfs_server.serve() => {
            result?;
        }
        result = admin_future => {
            result?;
        }
    }

    Ok(())
}
