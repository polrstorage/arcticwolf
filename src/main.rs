#[cfg(not(target_os = "linux"))]
compile_error!("Arctic Wolf NFS server only supports Linux");

use anyhow::Result;
use std::sync::Arc;

mod config;
mod fsal;
mod mount;
mod nfs;
mod portmap;
mod protocol;
mod rpc;

use config::Config;
use fsal::BackendConfig;
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

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration first (before tracing init)
    let config = Config::load()?;

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
    println!("  FSAL backend: {}", config.fsal.backend);
    println!("  Export path: {}", config.fsal.export_path.display());
    println!("  Log level: {}", log_level_str);
    println!();

    // Initialize FSAL (File System Abstraction Layer)
    println!("Initializing FSAL:");

    // Validate backend - currently only "local" is supported
    if config.fsal.backend != "local" {
        anyhow::bail!(
            "Unsupported FSAL backend '{}'. Currently only 'local' is supported.",
            config.fsal.backend
        );
    }

    let fsal_config = BackendConfig::local(&config.fsal.export_path);
    let filesystem: Arc<dyn fsal::Filesystem> = Arc::from(fsal_config.create_filesystem()?);

    let root_handle = filesystem.root_handle().await;
    println!("  Root handle: {} bytes", root_handle.len());
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

    // Run all three servers concurrently
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
    }

    Ok(())
}
