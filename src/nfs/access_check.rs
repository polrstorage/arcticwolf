// Read-only export gate for NFS write-class procedures.
//
// Every NFS handler that mutates the filesystem (write, create, setattr,
// mkdir, remove, rmdir, rename, symlink, link, mknod) calls
// [`check_writable`] at entry. The check resolves the handle's export uid
// prefix through the [`ExportRegistry`] view and short-circuits to
// `NFS3ERR_ROFS` when the owning export was configured with `read_only =
// true` in `config.toml`. COMMIT is intentionally not gated — see the
// commit handler for the rationale.

use crate::fsal::{ExportRegistry, FileHandle};
use crate::protocol::v3::nfs::nfsstat3;

/// Return `Err(NFS3ERR_ROFS)` if `handle` belongs to a read-only export.
///
/// Handles with an unknown / missing export-uid prefix are treated as
/// writable here; the per-operation handlers will see the same handle
/// fail the subsequent backend call with `NFS3ERR_STALE`, so we don't
/// need to duplicate that classification.
pub fn check_writable(registry: &dyn ExportRegistry, handle: &FileHandle) -> Result<(), nfsstat3> {
    if registry.is_read_only(handle) {
        Err(nfsstat3::NFS3ERR_ROFS)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig as ConfigBackend, ExportConfig};
    use crate::fsal::MultiExportFilesystem;
    use tempfile::TempDir;

    fn export(name: &str, uid: u32, path: std::path::PathBuf, read_only: bool) -> ExportConfig {
        ExportConfig {
            name: name.to_string(),
            uid,
            read_only,
            backend: ConfigBackend::Local { path },
        }
    }

    #[test]
    fn check_writable_allows_rw_handle() {
        let data = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();
        let router = MultiExportFilesystem::build_from_config(&[
            export("/data", 1, data.path().to_path_buf(), false),
            export("/backup", 2, backup.path().to_path_buf(), true),
        ])
        .unwrap();

        let rw = router.root_handle_for("/data").unwrap();
        assert!(check_writable(&router, &rw).is_ok());
    }

    #[test]
    fn check_writable_rejects_ro_handle_with_rofs() {
        let data = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();
        let router = MultiExportFilesystem::build_from_config(&[
            export("/data", 1, data.path().to_path_buf(), false),
            export("/backup", 2, backup.path().to_path_buf(), true),
        ])
        .unwrap();

        let ro = router.root_handle_for("/backup").unwrap();
        let err = check_writable(&router, &ro).expect_err("must reject");
        assert_eq!(err, nfsstat3::NFS3ERR_ROFS);
    }

    #[test]
    fn check_writable_allows_unknown_uid() {
        // The handler will fail this handle with NFS3ERR_STALE downstream
        // when the backend can't resolve it; the read-only gate should not
        // also masquerade an unknown handle as NFS3ERR_ROFS.
        let data = TempDir::new().unwrap();
        let router = MultiExportFilesystem::build_from_config(&[export(
            "/data",
            1,
            data.path().to_path_buf(),
            false,
        )])
        .unwrap();

        let bogus: FileHandle = vec![0xff; 32];
        assert!(check_writable(&router, &bogus).is_ok());
    }
}
