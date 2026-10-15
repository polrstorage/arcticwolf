// NFS SETATTR Procedure (Procedure 2)
//
// Sets file attributes (permissions, size, times, etc.)

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::NfsBackend;
use crate::protocol::v3::nfs::{NfsMessage, nfsstat3};
use crate::protocol::v3::rpc::RpcMessage;
use crate::rpc::auth::Credentials;

/// Handle NFS SETATTR procedure (procedure 2)
///
/// Sets file attributes such as mode, uid, gid, size, atime, mtime.
/// Most commonly used to truncate files before writing.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized SETATTR3args (file handle + new_attributes + guard)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with status and attributes
pub async fn handle_setattr(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn NfsBackend,
    cred: &Credentials,
) -> Result<BytesMut> {
    debug!("NFS SETATTR called (xid={})", xid);

    // Deserialize arguments
    let args = NfsMessage::deserialize_setattr3args(args_data)?;

    debug!("SETATTR: file_handle={} bytes", args.object.0.len(),);

    // Reject mutations against handles whose export is configured read_only.
    if let Err(status) = super::access_check::check_writable(filesystem, &args.object.0) {
        debug!("SETATTR denied: handle belongs to a read-only export");
        let res_data = NfsMessage::create_setattr_error_response(status)?;
        return RpcMessage::create_success_reply_with_data(xid, res_data);
    }

    // Attributes before the change: the guard and the permission checks
    // below need them, so an object we cannot stat is not touched at all.
    let before = match filesystem.getattr(&args.object.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("SETATTR: failed to get attributes: {}", e);
            let status = super::error::classify_handle_error(&e);
            let res_data = NfsMessage::create_setattr_error_response(status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Check guard if requested (guard is a union: CHECK with ctime or DONT_CHECK)
    if let crate::protocol::v3::nfs::sattrguard3::CHECK(guard_ctime) = &args.guard {
        // Compare ctime - if different, file was modified
        if before.ctime.seconds != guard_ctime.seconds as u64
            || before.ctime.nseconds != guard_ctime.nseconds
        {
            debug!("SETATTR: guard check failed - file was modified");
            let error_status = nfsstat3::NFS3ERR_NOT_SYNC;
            let res_data = NfsMessage::create_setattr_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    }

    // Apply attribute changes
    let new_attrs = &args.new_attributes;

    let want_mode = matches!(
        new_attrs.mode,
        crate::protocol::v3::nfs::set_mode3::SET_MODE(_)
    );
    let want_size = matches!(
        new_attrs.size,
        crate::protocol::v3::nfs::set_size3::SET_SIZE(_)
    );
    let uid = match &new_attrs.uid {
        crate::protocol::v3::nfs::set_uid3::SET_UID(u) => Some(*u),
        _ => None,
    };
    let gid = match &new_attrs.gid {
        crate::protocol::v3::nfs::set_gid3::SET_GID(g) => Some(*g),
        _ => None,
    };

    // chmod/chown need ownership (or root), truncate needs write
    // permission; see `perm::check_setattr` for the exact rules.
    let who = super::access_check::identity_for(filesystem, &args.object.0, cred);
    if let Err(status) = super::perm::check_setattr(&before, &who, want_mode, uid, gid, want_size) {
        debug!("SETATTR denied: {:?}", status);
        let res_data = NfsMessage::create_setattr_error_response(status)?;
        return RpcMessage::create_success_reply_with_data(xid, res_data);
    }

    // Handle size change (truncate/extend)
    if let crate::protocol::v3::nfs::set_size3::SET_SIZE(new_size) = &new_attrs.size {
        debug!("SETATTR: setting size to {}", new_size);

        if let Err(e) = filesystem.setattr_size(&args.object.0, *new_size).await {
            debug!("SETATTR: failed to set size: {}", e);
            let error_status = if e.to_string().contains("not found") {
                nfsstat3::NFS3ERR_STALE
            } else if e.to_string().contains("Permission denied") {
                nfsstat3::NFS3ERR_ACCES
            } else if e.to_string().contains("Read-only") {
                nfsstat3::NFS3ERR_ROFS
            } else {
                nfsstat3::NFS3ERR_IO
            };
            let res_data = NfsMessage::create_setattr_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    }

    // Handle mode change (permissions)
    if let crate::protocol::v3::nfs::set_mode3::SET_MODE(mode) = &new_attrs.mode {
        debug!("SETATTR: setting mode to {:o}", mode);

        if let Err(e) = filesystem.setattr_mode(&args.object.0, *mode).await {
            debug!("SETATTR: failed to set mode: {}", e);
            let error_status = if e.to_string().contains("not found") {
                nfsstat3::NFS3ERR_STALE
            } else if e.to_string().contains("Permission denied") {
                nfsstat3::NFS3ERR_ACCES
            } else {
                nfsstat3::NFS3ERR_IO
            };
            let res_data = NfsMessage::create_setattr_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    }

    // Handle uid/gid change
    if uid.is_some() || gid.is_some() {
        debug!("SETATTR: setting uid={:?}, gid={:?}", uid, gid);

        if let Err(e) = filesystem.setattr_owner(&args.object.0, uid, gid).await {
            debug!("SETATTR: failed to set owner: {}", e);
            // chown(2) reports EPERM ("Operation not permitted") both for
            // a non-owner and for a daemon without CAP_CHOWN.
            let error_status = if e.to_string().contains("not found") {
                nfsstat3::NFS3ERR_STALE
            } else if e.to_string().contains("Permission denied")
                || e.to_string().contains("not permitted")
            {
                nfsstat3::NFS3ERR_PERM
            } else {
                nfsstat3::NFS3ERR_IO
            };
            let res_data = NfsMessage::create_setattr_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    }

    // Handle atime/mtime changes
    // For simplicity, we'll skip time changes for now as they require more complex handling
    // (SET_TO_SERVER_TIME vs SET_TO_CLIENT_TIME)

    // Get file attributes after setattr
    let after_attrs = match filesystem.getattr(&args.object.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("SETATTR: failed to get attributes after setattr: {}", e);
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_setattr_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!("SETATTR success");

    // Convert FSAL attributes to NFS fattr3
    let nfs_after_attrs = NfsMessage::fsal_to_fattr3(&after_attrs);

    // Create SETATTR response with wcc_data
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. obj_wcc: wcc_data
    // pre_op_attr (optional)
    false.pack(&mut buf)?; // pre_op_attr = FALSE

    // post_op_attr (after attributes)
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_after_attrs.pack(&mut buf)?;

    let res_data = BytesMut::from(&buf[..]);

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::{BackendConfig, Filesystem};
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_setattr_truncate() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::Local {
            path: temp_dir.path().to_path_buf(),
        };
        let fs = config.create_filesystem(1).unwrap();

        // Create a test file with content
        let test_file = temp_dir.path().join("truncate_test.txt");
        fs::write(&test_file, b"Hello, World! This is a long file.").unwrap();

        // Get file handle and current ctime for guard
        let root_handle = fs.root_file_handle();
        let file_handle = fs.lookup(&root_handle, "truncate_test.txt").await.unwrap();
        let before_attrs = fs.getattr(&file_handle).await.unwrap();

        // Serialize SETATTR3args to truncate to 5 bytes
        use crate::protocol::v3::nfs::{
            SETATTR3args, fhandle3, nfstime3, sattr3, sattrguard3, set_atime, set_gid3, set_mode3,
            set_mtime, set_size3, set_uid3,
        };
        use xdr_codec::Pack;

        let args = SETATTR3args {
            object: fhandle3(file_handle),
            new_attributes: sattr3 {
                mode: set_mode3::SET_MODE(0o644),
                uid: set_uid3::SET_UID(0),
                gid: set_gid3::SET_GID(0),
                size: set_size3::SET_SIZE(5),
                atime: set_atime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
                mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
            },
            guard: sattrguard3::CHECK(nfstime3 {
                seconds: before_attrs.ctime.seconds as u32,
                nseconds: before_attrs.ctime.nseconds as u32,
            }),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call SETATTR
        let result = handle_setattr(12345, &args_buf, fs.as_ref(), &Credentials::root()).await;

        assert!(result.is_ok(), "SETATTR should succeed");

        // Verify file is truncated
        let content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "Hello");
    }

    #[tokio::test]
    async fn test_setattr_mode() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::Local {
            path: temp_dir.path().to_path_buf(),
        };
        let fs = config.create_filesystem(1).unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("mode_test.txt");
        fs::write(&test_file, b"test").unwrap();

        // Get file handle and current ctime for guard
        let root_handle = fs.root_file_handle();
        let file_handle = fs.lookup(&root_handle, "mode_test.txt").await.unwrap();
        let before_attrs = fs.getattr(&file_handle).await.unwrap();

        // Serialize SETATTR3args to set mode to 0644
        use crate::protocol::v3::nfs::{
            SETATTR3args, fhandle3, nfstime3, sattr3, sattrguard3, set_atime, set_gid3, set_mode3,
            set_mtime, set_size3, set_uid3,
        };
        use xdr_codec::Pack;

        let args = SETATTR3args {
            object: fhandle3(file_handle),
            new_attributes: sattr3 {
                mode: set_mode3::SET_MODE(0o644),
                uid: set_uid3::SET_UID(0),
                gid: set_gid3::SET_GID(0),
                size: set_size3::SET_SIZE(4),
                atime: set_atime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
                mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                }),
            },
            guard: sattrguard3::CHECK(nfstime3 {
                seconds: before_attrs.ctime.seconds as u32,
                nseconds: before_attrs.ctime.nseconds as u32,
            }),
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call SETATTR
        let result = handle_setattr(12345, &args_buf, fs.as_ref(), &Credentials::root()).await;

        assert!(result.is_ok(), "SETATTR should succeed");
    }

    /// SETATTR3args that only change the uid, with no guard. xdrgen cannot
    /// pack the `default` arms, so the DONT_SET / DONT_CHANGE / DONT_CHECK
    /// discriminators are written by hand.
    fn chown_args(handle: &crate::fsal::FileHandle, uid: u32) -> Vec<u8> {
        use crate::protocol::v3::nfs::{fhandle3, set_uid3};
        use xdr_codec::Pack;
        let mut buf = Vec::new();
        fhandle3(handle.clone()).pack(&mut buf).unwrap();
        0i32.pack(&mut buf).unwrap(); // mode: DONT_SET_MODE
        set_uid3::SET_UID(uid).pack(&mut buf).unwrap();
        0i32.pack(&mut buf).unwrap(); // gid: DONT_SET_GID
        0i32.pack(&mut buf).unwrap(); // size: DONT_SET_SIZE
        0i32.pack(&mut buf).unwrap(); // atime: DONT_CHANGE
        0i32.pack(&mut buf).unwrap(); // mtime: DONT_CHANGE
        0i32.pack(&mut buf).unwrap(); // guard: DONT_CHECK
        buf
    }

    /// NFS status of a reply: it follows the 24-byte accepted RPC header.
    fn status_of(reply: &[u8]) -> i32 {
        i32::from_be_bytes(reply[24..28].try_into().unwrap())
    }

    #[tokio::test]
    async fn test_setattr_dangling_symlink_still_checks_ownership() {
        use std::os::unix::fs::MetadataExt;
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::Local {
            path: temp_dir.path().to_path_buf(),
        };
        let fs = config.create_filesystem(1).unwrap();
        let link = temp_dir.path().join("dangle");
        std::os::unix::fs::symlink("/nowhere", &link).unwrap();
        let root_handle = fs.root_file_handle();
        let handle = fs.lookup(&root_handle, "dangle").await.unwrap();
        let owner = fs::symlink_metadata(&link).unwrap().uid();
        let stranger = Credentials::sys(owner.wrapping_add(1).max(1), 65000);

        // The link has an owner of its own, and a stranger may not change it.
        let reply = handle_setattr(1, &chown_args(&handle, 0), fs.as_ref(), &stranger)
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3ERR_PERM as i32);
        assert_eq!(fs::symlink_metadata(&link).unwrap().uid(), owner);

        // An object that cannot be stat'ed is not touched either.
        fs::remove_file(&link).unwrap();
        let reply = handle_setattr(
            2,
            &chown_args(&handle, 0),
            fs.as_ref(),
            &Credentials::root(),
        )
        .await
        .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3ERR_STALE as i32);
        assert!(!link.exists() && fs::symlink_metadata(&link).is_err());
    }
}
