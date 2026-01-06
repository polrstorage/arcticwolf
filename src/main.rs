use anyhow::Result;
use std::sync::Arc;
use tracing_subscriber;

mod fsal;
mod mount;
mod nfs;
mod portmap;
mod protocol;
mod rpc;

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
        prog: 100000,  // PORTMAP
        vers: 2,       // Version 2
        prot: IPPROTO_TCP,
        port,
    };
    registry.set(&portmap_tcp);
    println!("  ✓ Portmapper v2 (TCP) on port {}", port);

    // Register MOUNT protocol (program 100005)
    let mount_tcp = mapping {
        prog: 100005,  // MOUNT
        vers: 3,       // MOUNTv3
        prot: IPPROTO_TCP,
        port,
    };
    registry.set(&mount_tcp);
    println!("  ✓ MOUNT v3 (TCP) on port {}", port);

    // Register NFS protocol (program 100003)
    let nfs_tcp = mapping {
        prog: 100003,  // NFS
        vers: 3,       // NFSv3
        prot: IPPROTO_TCP,
        port,
    };
    registry.set(&nfs_tcp);
    println!("  ✓ NFS v3 (TCP) on port {}", port);

    println!();
}


#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("Arctic Wolf NFS Server");
    println!("======================");
    println!("Architecture:");
    println!("- XDR: xdrgen + xdr-codec (supports string, union, arrays)");
    println!("- Protocol: v3 (RPC, MOUNT, NFS)");
    println!("- Middleware: Type-safe serialization/deserialization");
    println!("- FSAL: File System Abstraction Layer");
    println!();
    println!("Starting RPC server on 0.0.0.0:4000");
    println!();

    // Initialize FSAL (File System Abstraction Layer)
    // Export /tmp/nfs_exports as the NFS export root
    let export_path = std::path::PathBuf::from("/tmp/nfs_exports");
    println!("Initializing FSAL:");
    println!("  Export path: {}", export_path.display());

    let fsal_config = BackendConfig::local(&export_path);
    let filesystem: Arc<dyn fsal::Filesystem> = Arc::from(fsal_config.create_filesystem()?);

    let root_handle = filesystem.root_handle().await;
    println!("  Root handle: {} bytes", root_handle.len());
    println!();

    // Create portmapper registry
    let registry = portmap::Registry::new();

    // Register services in portmapper
    // Note: Currently all services share port 4000
    // In production, these would be on different ports (111, 2049, 20048)
    register_services(&registry, 4000);

    // Create and run RPC server with filesystem
    let server = rpc::server::RpcServer::new("0.0.0.0:4000".to_string(), registry, filesystem);
    server.run().await?;

    Ok(())
}
