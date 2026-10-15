// Entry gates shared by the NFS procedure handlers.
//
// Two policies are enforced here, both resolved through the
// [`ExportRegistry`] view of the backend:
//
// - the per-export `read_only` flag: every handler that mutates the
//   filesystem (write, create, setattr, mkdir, remove, rmdir, rename,
//   symlink, link, mknod) calls [`check_writable`] and short-circuits to
//   `NFS3ERR_ROFS`. COMMIT is intentionally not gated — see the commit
//   handler for the rationale;
// - POSIX permissions for the caller: [`identity_for`] applies the export's
//   squash policy to the RPC credential, and [`require`] / [`require_io`]
//   compare the resulting identity against the object's mode bits (see
//   [`super::perm`]) before the backend call, returning `NFS3ERR_ACCES`.

use crate::fsal::{ExportRegistry, FileAttributes, FileHandle, NfsBackend};
use crate::protocol::v3::nfs::nfsstat3;
use crate::rpc::auth::Credentials;

use super::error::classify_handle_error;
use super::perm::{self, Identity};

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

/// The identity `cred` acts as inside the export that owns `handle`.
///
/// `AUTH_NONE` is always the export's anonymous identity. For `AUTH_SYS`,
/// `squash = "all"` maps everyone, `"root"` maps uid 0 / gid 0 (including
/// a 0 in the supplementary groups) to `anonuid` / `anongid`, and `"none"`
/// trusts the credential as sent.
pub fn identity_for(
    registry: &dyn ExportRegistry,
    handle: &FileHandle,
    cred: &Credentials,
) -> Identity {
    use crate::config::Squash;

    let policy = registry.squash_policy(handle);
    let anonymous = || Identity {
        uid: policy.anonuid,
        gid: policy.anongid,
        gids: Vec::new(),
    };
    match cred {
        Credentials::Anonymous => anonymous(),
        Credentials::Sys { uid, gid, gids } => match policy.squash {
            Squash::All => anonymous(),
            Squash::None => Identity {
                uid: *uid,
                gid: *gid,
                gids: gids.clone(),
            },
            Squash::Root => {
                let map_uid = |id: u32| if id == 0 { policy.anonuid } else { id };
                let map_gid = |id: u32| if id == 0 { policy.anongid } else { id };
                Identity {
                    uid: map_uid(*uid),
                    gid: map_gid(*gid),
                    gids: gids.iter().copied().map(map_gid).collect(),
                }
            }
        },
    }
}

/// Fetch `handle`'s attributes and require `want` permission bits on it.
///
/// Returns the attributes so the handler can reuse them (most need the
/// pre-op attributes for `wcc_data` anyway). A handle the backend cannot
/// resolve maps through [`classify_handle_error`] (usually `NFS3ERR_STALE`);
/// a permission miss is `NFS3ERR_ACCES`.
pub async fn require(
    backend: &dyn NfsBackend,
    handle: &FileHandle,
    cred: &Credentials,
    want: u32,
) -> Result<FileAttributes, nfsstat3> {
    let attrs = backend
        .getattr(handle)
        .await
        .map_err(|e| classify_handle_error(&e))?;
    let who = identity_for(backend, handle, cred);
    if perm::check(&attrs, &who, want) {
        Ok(attrs)
    } else {
        Err(nfsstat3::NFS3ERR_ACCES)
    }
}

/// [`require`] for READ / WRITE / COMMIT, with the owner override of
/// [`perm::check_io`].
pub async fn require_io(
    backend: &dyn NfsBackend,
    handle: &FileHandle,
    cred: &Credentials,
    want: u32,
) -> Result<FileAttributes, nfsstat3> {
    let attrs = backend
        .getattr(handle)
        .await
        .map_err(|e| classify_handle_error(&e))?;
    let who = identity_for(backend, handle, cred);
    if perm::check_io(&attrs, &who, want) {
        Ok(attrs)
    } else {
        Err(nfsstat3::NFS3ERR_ACCES)
    }
}

/// Sticky-bit rule for REMOVE / RMDIR / RENAME: in a directory with
/// `S_ISVTX` set, an entry may only be removed or renamed by its owner,
/// the directory's owner or root (`NFS3ERR_PERM` otherwise). `dir_attrs`
/// are the ones [`require`] already fetched. An entry that cannot be
/// looked up passes, so the backend call reports `NFS3ERR_NOENT` itself.
pub async fn check_sticky(
    backend: &dyn NfsBackend,
    dir_handle: &FileHandle,
    dir_attrs: &FileAttributes,
    name: &str,
    cred: &Credentials,
) -> Result<(), nfsstat3> {
    if dir_attrs.mode & 0o1000 == 0 {
        return Ok(());
    }
    let who = identity_for(backend, dir_handle, cred);
    if who.is_superuser() || dir_attrs.uid == who.uid {
        return Ok(());
    }
    let Ok(child) = backend.lookup(dir_handle, name).await else {
        return Ok(());
    };
    match backend.getattr(&child).await {
        Ok(attrs) if attrs.uid == who.uid => Ok(()),
        Ok(_) => Err(nfsstat3::NFS3ERR_PERM),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig as ConfigBackend, DEFAULT_ANON_ID, ExportConfig, Squash};
    use crate::fsal::MultiExportFilesystem;
    use tempfile::TempDir;

    fn export(name: &str, uid: u32, path: std::path::PathBuf, read_only: bool) -> ExportConfig {
        ExportConfig {
            name: name.to_string(),
            uid,
            read_only,
            squash: Default::default(),
            anonuid: crate::config::DEFAULT_ANON_ID,
            anongid: crate::config::DEFAULT_ANON_ID,
            backend: ConfigBackend::Local { path },
        }
    }

    fn export_with_squash(
        name: &str,
        uid: u32,
        path: std::path::PathBuf,
        squash: Squash,
    ) -> ExportConfig {
        ExportConfig {
            squash,
            anonuid: 4000,
            anongid: 4001,
            ..export(name, uid, path, false)
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

    #[test]
    fn identity_follows_export_squash_policy() {
        let r = TempDir::new().unwrap();
        let a = TempDir::new().unwrap();
        let n = TempDir::new().unwrap();
        let router = MultiExportFilesystem::build_from_config(&[
            export_with_squash("/root", 1, r.path().to_path_buf(), Squash::Root),
            export_with_squash("/all", 2, a.path().to_path_buf(), Squash::All),
            export_with_squash("/none", 3, n.path().to_path_buf(), Squash::None),
        ])
        .unwrap();
        let root_h = router.root_handle_for("/root").unwrap();
        let all_h = router.root_handle_for("/all").unwrap();
        let none_h = router.root_handle_for("/none").unwrap();

        let root_cred = Credentials::Sys {
            uid: 0,
            gid: 0,
            gids: vec![0, 10],
        };
        let user_cred = Credentials::Sys {
            uid: 1000,
            gid: 100,
            gids: vec![24],
        };
        let anon = Identity {
            uid: 4000,
            gid: 4001,
            gids: vec![],
        };

        // root squash: only the zero ids move.
        assert_eq!(
            identity_for(&router, &root_h, &root_cred),
            Identity {
                uid: 4000,
                gid: 4001,
                gids: vec![4001, 10]
            }
        );
        assert_eq!(
            identity_for(&router, &root_h, &user_cred),
            Identity {
                uid: 1000,
                gid: 100,
                gids: vec![24]
            }
        );
        // all squash: everyone is anonymous.
        assert_eq!(identity_for(&router, &all_h, &user_cred), anon);
        // no squash: root stays root.
        assert_eq!(
            identity_for(&router, &none_h, &root_cred),
            Identity {
                uid: 0,
                gid: 0,
                gids: vec![0, 10]
            }
        );
        // AUTH_NONE is anonymous under every policy.
        for h in [&root_h, &all_h, &none_h] {
            assert_eq!(identity_for(&router, h, &Credentials::Anonymous), anon);
        }
    }

    #[test]
    fn identity_for_unknown_handle_is_root_squashed_nobody() {
        let data = TempDir::new().unwrap();
        let router = MultiExportFilesystem::build_from_config(&[export(
            "/data",
            1,
            data.path().to_path_buf(),
            false,
        )])
        .unwrap();
        let bogus: FileHandle = vec![0xff; 32];
        let who = identity_for(&router, &bogus, &Credentials::root());
        assert_eq!(who.uid, DEFAULT_ANON_ID);
        assert_eq!(who.gid, DEFAULT_ANON_ID);
    }

    #[tokio::test]
    async fn require_distinguishes_stale_from_acces() {
        let data = TempDir::new().unwrap();
        std::fs::write(data.path().join("f"), b"x").unwrap();
        std::fs::set_permissions(
            data.path().join("f"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        let router = MultiExportFilesystem::build_from_config(&[export_with_squash(
            "/data",
            1,
            data.path().to_path_buf(),
            Squash::None,
        )])
        .unwrap();
        let root_h = router.root_handle_for("/data").unwrap();
        let f = crate::fsal::Filesystem::lookup(&router, &root_h, "f")
            .await
            .unwrap();
        // `cargo test` runs as root inside the build container, where the
        // file would be root-owned and root bypasses the bits; hand it to
        // uid 1000 there so the owner path is exercised either way.
        let owner = if unsafe { libc::geteuid() } == 0 {
            let c = std::ffi::CString::new(data.path().join("f").to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::chown(c.as_ptr(), 1000, 1000) }, 0);
            1000
        } else {
            std::fs::metadata(data.path().join("f")).unwrap().uid_of()
        };

        // A caller who is neither owner nor root cannot read 0600.
        let stranger = Credentials::sys(owner.wrapping_add(1).max(1), 65000);
        assert_eq!(
            require(&router, &f, &stranger, perm::READ)
                .await
                .unwrap_err(),
            nfsstat3::NFS3ERR_ACCES
        );
        // The owner override lets the owner through even without the bit.
        std::fs::set_permissions(
            data.path().join("f"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        assert!(
            require_io(&router, &f, &Credentials::sys(owner, 65000), perm::READ)
                .await
                .is_ok()
        );
        assert_eq!(
            require(&router, &f, &Credentials::sys(owner, 65000), perm::READ)
                .await
                .unwrap_err(),
            nfsstat3::NFS3ERR_ACCES
        );
        // Unknown handles are stale, not a permission problem.
        let bogus: FileHandle = vec![0xff; 32];
        assert_eq!(
            require(&router, &bogus, &Credentials::root(), perm::READ)
                .await
                .unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
    }

    /// `std::os::unix::fs::MetadataExt::uid` without the trait import noise.
    trait UidOf {
        fn uid_of(&self) -> u32;
    }
    impl UidOf for std::fs::Metadata {
        fn uid_of(&self) -> u32 {
            std::os::unix::fs::MetadataExt::uid(self)
        }
    }
}
