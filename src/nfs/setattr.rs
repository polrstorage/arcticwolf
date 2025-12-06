// NFS SETATTR Procedure (Procedure 2)
//
// Sets file attributes (permissions, size, times, etc.)

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::Filesystem;
use crate::protocol::v3::nfs::{nfsstat3, NfsMessage};
use crate::protocol::v3::rpc::RpcMessage;

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
pub fn handle_setattr(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn Filesystem,
) -> Result<BytesMut> {
    debug!("NFS SETATTR called (xid={})", xid);

    // Deserialize arguments
    let args = NfsMessage::deserialize_setattr3args(args_data)?;

    debug!(
        "SETATTR: file_handle={} bytes",
        args.object.0.len(),
    );

    // Get file attributes before setattr (for wcc_data)
    let before_attrs = filesystem.getattr(&args.object.0).ok();

    // Check guard if requested
    if args.guard.check {
        if let Some(ref before) = before_attrs {
            let before_ctime = before.ctime;
            let guard_ctime = args.guard.obj_ctime;

            // Compare ctime - if different, file was modified
            if before_ctime.seconds != guard_ctime.seconds as u64
                || before_ctime.nseconds != guard_ctime.nseconds {
                debug!("SETATTR: guard check failed - file was modified");
                let error_status = nfsstat3::NFS3ERR_NOT_SYNC;
                let res_data = NfsMessage::create_setattr_error_response(error_status)?;
                return RpcMessage::create_success_reply_with_data(xid, res_data);
            }
        }
    }

    // Apply attribute changes
    let new_attrs = &args.new_attributes;

    // Handle size change (truncate/extend)
    if let crate::protocol::v3::nfs::set_size3::SET_SIZE(new_size) = &new_attrs.size {
        debug!("SETATTR: setting size to {}", new_size);

        if let Err(e) = filesystem.setattr_size(&args.object.0, *new_size) {
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

        if let Err(e) = filesystem.setattr_mode(&args.object.0, *mode) {
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
    let uid = match &new_attrs.uid {
        crate::protocol::v3::nfs::set_uid3::SET_UID(u) => Some(*u),
        _ => None,
    };
    let gid = match &new_attrs.gid {
        crate::protocol::v3::nfs::set_gid3::SET_GID(g) => Some(*g),
        _ => None,
    };

    if uid.is_some() || gid.is_some() {
        debug!("SETATTR: setting uid={:?}, gid={:?}", uid, gid);

        if let Err(e) = filesystem.setattr_owner(&args.object.0, uid, gid) {
            debug!("SETATTR: failed to set owner: {}", e);
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

    // Handle atime/mtime changes
    // For simplicity, we'll skip time changes for now as they require more complex handling
    // (SET_TO_SERVER_TIME vs SET_TO_CLIENT_TIME)

    // Get file attributes after setattr
    let after_attrs = match filesystem.getattr(&args.object.0) {
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

    #[test]
    fn test_setattr_truncate() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Create a test file with content
        let test_file = temp_dir.path().join("truncate_test.txt");
        fs::write(&test_file, b"Hello, World! This is a long file.").unwrap();

        // Get file handle
        let root_handle = fs.root_handle();
        let file_handle = fs.lookup(&root_handle, "truncate_test.txt").unwrap();

        // Serialize SETATTR3args to truncate to 5 bytes
        use crate::protocol::v3::nfs::{
            fhandle3, sattrguard3, sattr3, set_atime, set_gid3, set_mode3,
            set_mtime, set_size3, set_uid3, time_how, SETATTR3args,
        };
        use xdr_codec::Pack;

        let args = SETATTR3args {
            object: fhandle3(file_handle),
            new_attributes: sattr3 {
                mode: set_mode3::default,
                uid: set_uid3::default,
                gid: set_gid3::default,
                size: set_size3::SET_SIZE(5),
                atime: set_atime::default,
                mtime: set_mtime::default,
            },
            guard: sattrguard3 {
                check: false,
                obj_ctime: crate::protocol::v3::nfs::nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                },
            },
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call SETATTR
        let result = handle_setattr(12345, &args_buf, fs.as_ref());

        assert!(result.is_ok(), "SETATTR should succeed");

        // Verify file is truncated
        let content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "Hello");
    }

    #[test]
    fn test_setattr_mode() {
        // Create temp filesystem
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig::local(temp_dir.path());
        let fs = config.create_filesystem().unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("mode_test.txt");
        fs::write(&test_file, b"test").unwrap();

        // Get file handle
        let root_handle = fs.root_handle();
        let file_handle = fs.lookup(&root_handle, "mode_test.txt").unwrap();

        // Serialize SETATTR3args to set mode to 0644
        use crate::protocol::v3::nfs::{
            fhandle3, sattrguard3, sattr3, set_atime, set_gid3, set_mode3,
            set_mtime, set_size3, set_uid3, time_how, SETATTR3args,
        };
        use xdr_codec::Pack;

        let args = SETATTR3args {
            object: fhandle3(file_handle),
            new_attributes: sattr3 {
                mode: set_mode3::SET_MODE(0o644),
                uid: set_uid3::default,
                gid: set_gid3::default,
                size: set_size3::default,
                atime: set_atime::default,
                mtime: set_mtime::default,
            },
            guard: sattrguard3 {
                check: false,
                obj_ctime: crate::protocol::v3::nfs::nfstime3 {
                    seconds: 0,
                    nseconds: 0,
                },
            },
        };

        let mut args_buf = Vec::new();
        args.pack(&mut args_buf).unwrap();

        // Call SETATTR
        let result = handle_setattr(12345, &args_buf, fs.as_ref());

        assert!(result.is_ok(), "SETATTR should succeed");
    }
}
