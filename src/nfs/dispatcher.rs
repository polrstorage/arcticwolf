// NFS Procedure Dispatcher
//
// Routes incoming NFS RPC calls to the appropriate procedure handler.
//
// The dispatcher takes `&dyn NfsBackend` so each handler can borrow the
// trait view it actually needs: most handlers only touch `Filesystem`,
// while the write-class handlers additionally consult `ExportRegistry`
// via `check_writable` to enforce per-export `read_only` configuration.

use anyhow::{Result, anyhow};
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::NfsBackend;
use crate::metrics::Metrics;
use crate::protocol::v3::rpc::rpc_call_msg;

use super::{
    access, commit, create, fsinfo, fsstat, getattr, link, lookup, mkdir, mknod, null, pathconf,
    read, readdir, readdirplus, readlink, remove, rename, rmdir, setattr, symlink, write,
};

/// Dispatch NFS procedure call to appropriate handler
///
/// # Arguments
/// * `call` - Parsed RPC call message
/// * `args_data` - Procedure arguments data
/// * `backend` - Combined filesystem + export registry view
/// * `metrics` - Operational counters; the matching per-op counter is
///   bumped on entry (Phase 7). The server-wide `rpc_errors_total` is bumped
///   centrally in `crate::rpc::server` when this function returns `Err`, so
///   it is not touched here.
///
/// # Returns
/// Serialized RPC reply message
pub async fn dispatch(
    call: &rpc_call_msg,
    args_data: &[u8],
    backend: &dyn NfsBackend,
    metrics: &Metrics,
) -> Result<BytesMut> {
    let procedure = call.proc_;
    let xid = call.xid;

    debug!(
        "NFS dispatcher: procedure={}, xid={}, version={}",
        procedure, xid, call.vers
    );

    // Verify NFS version
    if call.vers != 3 {
        warn!("Unsupported NFS version: {}", call.vers);
        return Err(anyhow!("NFS version {} not supported", call.vers));
    }

    // Count the op on entry, before the handler runs — an op that the
    // handler later rejects (bad args, stale handle) still counts as a
    // dispatched invocation of that procedure.
    metrics.nfs_ops.record(procedure);

    // Dispatch based on procedure number
    match procedure {
        0 => {
            // NULL - test procedure
            null::handle_null(xid).await
        }
        1 => {
            // GETATTR - get file attributes
            getattr::handle_getattr(xid, args_data, backend).await
        }
        2 => {
            // SETATTR - set file attributes
            setattr::handle_setattr(xid, args_data, backend).await
        }
        3 => {
            // LOOKUP - lookup filename
            lookup::handle_lookup(xid, args_data, backend).await
        }
        4 => {
            // ACCESS - check file access permissions
            access::handle_access(xid, args_data, backend).await
        }
        5 => {
            // READLINK - read symbolic link
            readlink::handle_readlink(xid, args_data, backend).await
        }
        6 => {
            // READ - read from file
            read::handle_read(xid, args_data, backend).await
        }
        16 => {
            // READDIR - read directory entries
            readdir::handle_readdir(xid, args_data, backend).await
        }
        18 => {
            // FSSTAT - get filesystem statistics
            fsstat::handle_fsstat(xid, args_data, backend).await
        }
        19 => {
            // FSINFO - get filesystem information
            fsinfo::handle_fsinfo(xid, args_data, backend).await
        }
        20 => {
            // PATHCONF - get filesystem path configuration
            pathconf::handle_pathconf(xid, args_data, backend).await
        }
        17 => {
            // READDIRPLUS - read directory entries with attributes
            readdirplus::handle_readdirplus(xid, args_data, backend).await
        }
        7 => {
            // WRITE - write to file
            write::handle_write(xid, args_data, backend).await
        }
        8 => {
            // CREATE - create file
            create::handle_create(xid, args_data, backend).await
        }
        9 => {
            // MKDIR - create directory
            mkdir::handle_mkdir(xid, args_data, backend).await
        }
        10 => {
            // SYMLINK - create symbolic link
            symlink::handle_symlink(xid, args_data, backend).await
        }
        11 => {
            // MKNOD - create special file
            mknod::handle_mknod(xid, args_data, backend).await
        }
        12 => {
            // REMOVE - remove file
            remove::handle_remove(xid, args_data, backend).await
        }
        13 => {
            // RMDIR - remove directory
            rmdir::handle_rmdir(xid, args_data, backend).await
        }
        14 => {
            // RENAME - rename file or directory
            rename::handle_rename(xid, args_data, backend).await
        }
        15 => {
            // LINK - create hard link
            link::handle_link(xid, args_data, backend).await
        }
        21 => {
            // COMMIT - commit cached writes to stable storage.
            // Intentionally not gated by check_writable: Linux nfs-utils
            // serves COMMIT on read-only exports too, so writes that race
            // with an export flip don't surface as ROFS at sync time.
            commit::handle_commit(xid, args_data, backend).await
        }
        _ => {
            warn!("Unknown NFS procedure: {}", procedure);
            create_notsupp_response(xid)
        }
    }
}

/// Create a NFS3ERR_NOTSUPP error response
fn create_notsupp_response(xid: u32) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();
    (crate::protocol::v3::nfs::nfsstat3::NFS3ERR_NOTSUPP as i32).pack(&mut buf)?;
    let res_data = BytesMut::from(&buf[..]);
    crate::protocol::v3::rpc::RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig, ExportConfig};
    use crate::fsal::MultiExportFilesystem;
    use crate::protocol::v3::rpc::RpcMessage;
    use std::sync::atomic::Ordering::Relaxed;
    use tempfile::TempDir;

    /// Hand-roll an NFSv3 RPC call header for `proc_` and round-trip it
    /// through `deserialize_call` so the test stays independent of the
    /// xdrgen field naming (mirrors `mount::mnt`'s `make_mnt_call`).
    fn make_nfs_call(proc_: u32) -> rpc_call_msg {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&0u32.to_be_bytes()); // xid
        buf.extend_from_slice(&0u32.to_be_bytes()); // mtype = CALL
        buf.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        buf.extend_from_slice(&100003u32.to_be_bytes()); // NFS program
        buf.extend_from_slice(&3u32.to_be_bytes()); // NFS v3
        buf.extend_from_slice(&proc_.to_be_bytes());
        // cred + verf: both AUTH_NONE with empty body.
        for _ in 0..4 {
            buf.extend_from_slice(&0u32.to_be_bytes());
        }
        RpcMessage::deserialize_call(&buf).expect("synthetic call must deserialize")
    }

    /// A real single-export `MultiExportFilesystem` rooted at a tempdir. The
    /// per-op counter is bumped before the handler runs, so the handlers can
    /// fail on the empty arg buffer without affecting what we assert.
    fn test_backend() -> (MultiExportFilesystem, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let exports = vec![ExportConfig {
            name: "/data".to_string(),
            uid: 1,
            read_only: false,
            backend: BackendConfig::Local {
                path: dir.path().to_path_buf(),
            },
        }];
        let fs = MultiExportFilesystem::build_from_config(&exports).expect("build backend");
        (fs, dir)
    }

    #[tokio::test]
    async fn dispatch_bumps_per_op_counters() {
        let (backend, _dir) = test_backend();
        let metrics = Metrics::new();

        // 3 GETATTR (proc 1), 2 READ (proc 6). Empty args make the handlers
        // themselves error, but the op counter is bumped on entry — which is
        // exactly the contract under test.
        for _ in 0..3 {
            let call = make_nfs_call(1);
            let _ = dispatch(&call, &[], &backend, &metrics).await;
        }
        for _ in 0..2 {
            let call = make_nfs_call(6);
            let _ = dispatch(&call, &[], &backend, &metrics).await;
        }

        assert_eq!(metrics.nfs_ops.getattr.load(Relaxed), 3);
        assert_eq!(metrics.nfs_ops.read.load(Relaxed), 2);
        assert_eq!(metrics.nfs_ops.write.load(Relaxed), 0);

        // NULL (proc 0) is the one path that succeeds with empty args.
        let call = make_nfs_call(0);
        dispatch(&call, &[], &backend, &metrics)
            .await
            .expect("NULL must succeed");
        assert_eq!(metrics.nfs_ops.null.load(Relaxed), 1);
    }

    #[tokio::test]
    async fn dispatch_rejects_non_v3_without_counting() {
        let (backend, _dir) = test_backend();
        let metrics = Metrics::new();
        // proc 1 (GETATTR) but version is forced to 2 by editing the call —
        // simpler to just build a v2 header.
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&0u32.to_be_bytes()); // xid
        buf.extend_from_slice(&0u32.to_be_bytes()); // mtype
        buf.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        buf.extend_from_slice(&100003u32.to_be_bytes());
        buf.extend_from_slice(&2u32.to_be_bytes()); // NFS vers = 2 (unsupported)
        buf.extend_from_slice(&1u32.to_be_bytes()); // GETATTR
        for _ in 0..4 {
            buf.extend_from_slice(&0u32.to_be_bytes());
        }
        let call = RpcMessage::deserialize_call(&buf).expect("deserialize");
        let result = dispatch(&call, &[], &backend, &metrics).await;
        assert!(result.is_err(), "NFS v2 must be rejected");
        // Version check precedes the counter bump, so nothing is recorded.
        assert_eq!(metrics.nfs_ops.getattr.load(Relaxed), 0);
    }
}
