// NFS RENAME Procedure (14)
//
// Rename a file or directory

use anyhow::Result;
use bytes::BytesMut;
use tracing::{debug, warn};

use crate::fsal::NfsBackend;
use crate::protocol::v3::nfs::{NfsMessage, nfsstat3};
use crate::protocol::v3::rpc::RpcMessage;
use crate::rpc::auth::Credentials;

/// Handle NFS RENAME request
///
/// Renames or moves a file/directory from one location to another.
///
/// # Arguments
/// * `xid` - Transaction ID from RPC call
/// * `args_data` - Serialized RENAME3args
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply with RENAME3res
pub async fn handle_rename(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn NfsBackend,
    cred: &Credentials,
) -> Result<BytesMut> {
    debug!("NFS RENAME: xid={}", xid);

    // Parse arguments
    let args = NfsMessage::deserialize_rename3args(args_data)?;

    debug!(
        "  from: dir {} bytes, name {}",
        args.from_dir.0.len(),
        args.from_name.0
    );
    debug!(
        "  to: dir {} bytes, name {}",
        args.to_dir.0.len(),
        args.to_name.0
    );

    // Reject renames where either side belongs to a read-only export.
    // POSIX rename(2) writes both source and target directories, so a
    // read-only source export must reject the operation even when the
    // target export would accept the write — matches the kernel NFS
    // server's behavior and avoids partial-state surprises.
    if let Err(status) = super::access_check::check_writable(filesystem, &args.from_dir.0) {
        debug!("RENAME denied: source dir belongs to a read-only export");
        return create_rename_response(xid, status, None, None);
    }
    if let Err(status) = super::access_check::check_writable(filesystem, &args.to_dir.0) {
        debug!("RENAME denied: target dir belongs to a read-only export");
        return create_rename_response(xid, status, None, None);
    }

    // Both directories are written: write + search permission on each,
    // plus the sticky-bit ownership rule on the source entry and on the
    // target entry it would replace — the backend's rename(2) runs as the
    // daemon and would otherwise overwrite it regardless of its owner.
    let from_attrs = match super::access_check::require(
        filesystem,
        &args.from_dir.0,
        cred,
        super::perm::WRITE | super::perm::EXEC,
    )
    .await
    {
        Ok(attrs) => attrs,
        Err(status) => {
            debug!("RENAME denied on source dir: {:?}", status);
            return create_rename_response(xid, status, None, None);
        }
    };
    let to_attrs = if args.from_dir.0 == args.to_dir.0 {
        from_attrs.clone()
    } else {
        match super::access_check::require(
            filesystem,
            &args.to_dir.0,
            cred,
            super::perm::WRITE | super::perm::EXEC,
        )
        .await
        {
            Ok(attrs) => attrs,
            Err(status) => {
                debug!("RENAME denied on target dir: {:?}", status);
                return create_rename_response(xid, status, None, None);
            }
        }
    };
    if let Err(status) = super::access_check::check_sticky(
        filesystem,
        &args.from_dir.0,
        &from_attrs,
        &args.from_name.0,
        cred,
    )
    .await
    {
        debug!("RENAME denied by sticky bit on the source entry");
        return create_rename_response(xid, status, None, None);
    }
    if let Err(status) = super::access_check::check_sticky(
        filesystem,
        &args.to_dir.0,
        &to_attrs,
        &args.to_name.0,
        cred,
    )
    .await
    {
        debug!("RENAME denied by sticky bit on the target entry");
        return create_rename_response(xid, status, None, None);
    }

    // Get source directory attributes before operation (for wcc_data)
    let _fromdir_before = filesystem.getattr(&args.from_dir.0).await.ok();

    // Get target directory attributes before operation (for wcc_data)
    // Only if different from source directory
    let _todir_before = if args.from_dir.0 == args.to_dir.0 {
        None // Same directory, use fromdir_before
    } else {
        filesystem.getattr(&args.to_dir.0).await.ok()
    };

    // Perform rename operation
    match filesystem
        .rename(
            &args.from_dir.0,
            &args.from_name.0,
            &args.to_dir.0,
            &args.to_name.0,
        )
        .await
    {
        Ok(()) => {
            debug!("RENAME OK: '{}' -> '{}'", args.from_name.0, args.to_name.0);

            // Get source directory attributes after operation
            let fromdir_after = match filesystem.getattr(&args.from_dir.0).await {
                Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                Err(e) => {
                    warn!("Failed to get source dir attributes after rename: {}", e);
                    None
                }
            };

            // Get target directory attributes after operation
            let todir_after = if args.from_dir.0 == args.to_dir.0 {
                fromdir_after // Same directory (Copy type)
            } else {
                match filesystem.getattr(&args.to_dir.0).await {
                    Ok(attr) => Some(NfsMessage::fsal_to_fattr3(&attr)),
                    Err(e) => {
                        warn!("Failed to get target dir attributes after rename: {}", e);
                        None
                    }
                }
            };

            create_rename_response(xid, nfsstat3::NFS3_OK, fromdir_after, todir_after)
        }
        Err(e) => {
            warn!("RENAME failed for '{}': {}", args.from_name.0, e);

            // Determine appropriate error code
            let error_string = e.to_string();
            let status = if error_string.contains("not found") || error_string.contains("No such") {
                nfsstat3::NFS3ERR_NOENT
            } else if error_string.contains("already exists")
                || error_string.contains("File exists")
            {
                nfsstat3::NFS3ERR_EXIST
            } else if error_string.contains("permission") || error_string.contains("Permission") {
                nfsstat3::NFS3ERR_ACCES
            } else if error_string.contains("not a directory")
                || error_string.contains("Not a directory")
            {
                nfsstat3::NFS3ERR_NOTDIR
            } else if error_string.contains("is a directory")
                || error_string.contains("Is a directory")
            {
                nfsstat3::NFS3ERR_ISDIR
            } else if error_string.contains("not empty")
                || error_string.contains("Directory not empty")
            {
                nfsstat3::NFS3ERR_NOTEMPTY
            } else if error_string.contains("cross-device")
                || error_string.contains("Invalid cross-device")
            {
                nfsstat3::NFS3ERR_XDEV
            } else {
                // Try to get std::io::Error from anyhow::Error
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    match io_err.kind() {
                        std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
                        std::io::ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
                        std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
                        _ => nfsstat3::NFS3ERR_IO,
                    }
                } else {
                    nfsstat3::NFS3ERR_IO
                }
            };

            // Try to get current directory attributes for wcc_data
            let fromdir_after = filesystem
                .getattr(&args.from_dir.0)
                .await
                .ok()
                .map(|attr| NfsMessage::fsal_to_fattr3(&attr));
            let todir_after = if args.from_dir.0 == args.to_dir.0 {
                fromdir_after // Copy type
            } else {
                filesystem
                    .getattr(&args.to_dir.0)
                    .await
                    .ok()
                    .map(|attr| NfsMessage::fsal_to_fattr3(&attr))
            };

            create_rename_response(xid, status, fromdir_after, todir_after)
        }
    }
}

/// Create RENAME response
fn create_rename_response(
    xid: u32,
    status: nfsstat3,
    fromdir_attr: Option<crate::protocol::v3::nfs::fattr3>,
    todir_attr: Option<crate::protocol::v3::nfs::fattr3>,
) -> Result<BytesMut> {
    use xdr_codec::Pack;

    let mut buf = Vec::new();

    // 1. nfsstat3 status
    (status as i32).pack(&mut buf)?;

    // 2. wcc_data for source directory (fromdir_wcc)
    // wcc_data = pre_op_attr + post_op_attr

    // 2.1 pre_op_attr (before the operation) - we don't track this, so send FALSE
    false.pack(&mut buf)?;

    // 2.2 post_op_attr (after the operation)
    match &fromdir_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    // 3. wcc_data for target directory (todir_wcc)
    // wcc_data = pre_op_attr + post_op_attr

    // 3.1 pre_op_attr (before the operation) - we don't track this, so send FALSE
    false.pack(&mut buf)?;

    // 3.2 post_op_attr (after the operation)
    match &todir_attr {
        Some(attr) => {
            true.pack(&mut buf)?;
            attr.pack(&mut buf)?;
        }
        None => {
            false.pack(&mut buf)?;
        }
    }

    let res_data = BytesMut::from(&buf[..]);

    debug!(
        "RENAME response: status={:?}, response size: {} bytes",
        status,
        res_data.len()
    );

    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::local::LocalFilesystem;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_rename_file() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rename");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create a test file
        let mut file = fs::File::create(test_dir.join("oldname.txt")).unwrap();
        file.write_all(b"test content").unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_rename".to_string(), 1).unwrap();

        // Get root handle
        let root_handle = fs.root_file_handle();

        // Create RENAME3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        // from_dir (fhandle3)
        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        // from_name (filename3)
        let from_name = crate::protocol::v3::nfs::filename3("oldname.txt".to_string());
        from_name.pack(&mut args_buf).unwrap();

        // to_dir (fhandle3) - same directory
        fhandle.pack(&mut args_buf).unwrap();

        // to_name (filename3)
        let to_name = crate::protocol::v3::nfs::filename3("newname.txt".to_string());
        to_name.pack(&mut args_buf).unwrap();

        // Call RENAME
        let result = handle_rename(12345, &args_buf, &fs, &Credentials::root()).await;
        assert!(result.is_ok(), "RENAME should succeed");

        // Verify file was renamed
        assert!(
            !test_dir.join("oldname.txt").exists(),
            "Old file should not exist"
        );
        assert!(
            test_dir.join("newname.txt").exists(),
            "New file should exist"
        );

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_rename_directory() {
        // Create test directory
        let test_dir = PathBuf::from("/tmp/nfs_test_rename_dir");
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        // Create a test subdirectory
        fs::create_dir(test_dir.join("olddir")).unwrap();

        // Create filesystem
        let fs = LocalFilesystem::new("/tmp/nfs_test_rename_dir".to_string(), 1).unwrap();

        // Get root handle
        let root_handle = fs.root_file_handle();

        // Create RENAME3args manually
        use xdr_codec::Pack;
        let mut args_buf = Vec::new();

        let fhandle = crate::protocol::v3::nfs::fhandle3(root_handle.clone());
        fhandle.pack(&mut args_buf).unwrap();

        let from_name = crate::protocol::v3::nfs::filename3("olddir".to_string());
        from_name.pack(&mut args_buf).unwrap();

        fhandle.pack(&mut args_buf).unwrap();

        let to_name = crate::protocol::v3::nfs::filename3("newdir".to_string());
        to_name.pack(&mut args_buf).unwrap();

        // Call RENAME
        let result = handle_rename(12346, &args_buf, &fs, &Credentials::root()).await;
        assert!(result.is_ok(), "RENAME should succeed");

        // Verify directory was renamed
        assert!(
            !test_dir.join("olddir").exists(),
            "Old directory should not exist"
        );
        assert!(
            test_dir.join("newdir").exists(),
            "New directory should exist"
        );

        // Cleanup
        fs::remove_dir_all(&test_dir).unwrap();
    }

    /// RENAME3args for a rename within `dir`.
    fn rename_args(dir: &crate::fsal::FileHandle, from: &str, to: &str) -> Vec<u8> {
        use crate::protocol::v3::nfs::{fhandle3, filename3};
        use xdr_codec::Pack;
        let mut buf = Vec::new();
        fhandle3(dir.clone()).pack(&mut buf).unwrap();
        filename3(from.to_string()).pack(&mut buf).unwrap();
        fhandle3(dir.clone()).pack(&mut buf).unwrap();
        filename3(to.to_string()).pack(&mut buf).unwrap();
        buf
    }

    /// NFS status of a reply: it follows the 24-byte accepted RPC header.
    fn status_of(reply: &[u8]) -> i32 {
        i32::from_be_bytes(reply[24..28].try_into().unwrap())
    }

    #[tokio::test]
    async fn test_rename_sticky_dir_protects_the_replaced_entry() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipping: needs root to chown to another uid");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let dir = temp_dir.path();
        // A root-owned 1777 directory holding entries of two plain users.
        fs::set_permissions(dir, PermissionsExt::from_mode(0o1777)).unwrap();
        for (name, uid) in [("mine", 1000), ("mine2", 1000), ("theirs", 2000)] {
            let path = dir.join(name);
            fs::write(&path, name).unwrap();
            let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::chown(c.as_ptr(), uid, uid) }, 0);
        }
        let fs = LocalFilesystem::new(dir, 1).unwrap();
        let root_handle = fs.root_file_handle();
        let cred = Credentials::sys(1000, 1000);

        // Renaming over another user's entry is refused and changes nothing.
        let reply = handle_rename(1, &rename_args(&root_handle, "mine", "theirs"), &fs, &cred)
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3ERR_PERM as i32);
        assert_eq!(fs::read(dir.join("theirs")).unwrap(), b"theirs");
        assert!(dir.join("mine").exists());

        // Over the caller's own entry it goes through.
        let reply = handle_rename(2, &rename_args(&root_handle, "mine", "mine2"), &fs, &cred)
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3_OK as i32);
        assert_eq!(fs::read(dir.join("mine2")).unwrap(), b"mine");
        assert!(!dir.join("mine").exists());
    }
}
