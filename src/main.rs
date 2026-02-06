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

/// Register all RPC services in the portmapper registry
///
/// This makes services discoverable via PMAPPROC_GETPORT queries.
fn register_services(registry: &portmap::Registry, port: u32) {
    const IPPROTO_TCP: u32 = 6;

    println!("Registering services:");

    // Register Portmapper itself (program 100000)
    let portmap_tcp = mapping {
        prog: 100000, // PORTMAP
        vers: 2,      // Version 2
        prot: IPPROTO_TCP,
        port,
    };
    registry.set(&portmap_tcp);
    println!("  ✓ Portmapper v2 (TCP) on port {}", port);

    // Register MOUNT protocol (program 100005)
    let mount_tcp = mapping {
        prog: 100005, // MOUNT
        vers: 3,      // MOUNTv3
        prot: IPPROTO_TCP,
        port,
    };
    registry.set(&mount_tcp);
    println!("  ✓ MOUNT v3 (TCP) on port {}", port);

    // Register NFS protocol (program 100003)
    let nfs_tcp = mapping {
        prog: 100003, // NFS
        vers: 3,      // NFSv3
        prot: IPPROTO_TCP,
        port,
    };
    registry.set(&nfs_tcp);
    println!("  ✓ NFS v3 (TCP) on port {}", port);

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

    // Validate port - port 0 would bind to a random port which is not useful for a server
    if config.server.port == 0 {
        anyhow::bail!("Invalid port 0. Port must be between 1 and 65535.");
    }

    println!("Arctic Wolf NFS Server");
    println!("======================");
    println!("Configuration:");
    println!("  Bind address: {}", config.bind_addr());
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

    // Register services in portmapper
    register_services(&registry, config.server.port as u32);

    // Create and run RPC server with filesystem
    let server = rpc::server::RpcServer::new(config.bind_addr(), registry, filesystem);
    server.run().await?;

    Ok(())
}
