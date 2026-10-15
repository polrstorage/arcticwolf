// NFS CREATE Procedure (Procedure 8)
//
// Creates a new file

use anyhow::Result;
use bytes::BytesMut;
use tracing::debug;

use crate::fsal::{FileHandle, FileType, NfsBackend};
use crate::protocol::v3::nfs::{NfsMessage, createhow3, nfsstat3, sattr3, set_mode3, set_size3};
use crate::protocol::v3::rpc::RpcMessage;
use crate::rpc::auth::Credentials;

/// Handle NFS CREATE procedure (procedure 8)
///
/// Creates a new regular file.
///
/// # Arguments
/// * `xid` - Transaction ID from the request
/// * `args_data` - Serialized CREATE3args (dir handle + filename + how)
/// * `filesystem` - Filesystem instance
///
/// # Returns
/// Serialized RPC reply message with new file handle
pub async fn handle_create(
    xid: u32,
    args_data: &[u8],
    filesystem: &dyn NfsBackend,
    cred: &Credentials,
) -> Result<BytesMut> {
    debug!("NFS CREATE called (xid={})", xid);
    debug!(
        "CREATE: args_data = {} bytes, hex: {:02x?}",
        args_data.len(),
        args_data
    );

    // Deserialize arguments
    let args = NfsMessage::deserialize_create3args(args_data)?;

    let filename = &args.name.0;
    debug!(
        "CREATE: dir_handle={} bytes, filename={}",
        args.where_dir.0.len(),
        filename
    );

    // Reject creates against handles whose export is configured read_only.
    if let Err(status) = super::access_check::check_writable(filesystem, &args.where_dir.0) {
        debug!("CREATE denied: parent dir belongs to a read-only export");
        let res_data = NfsMessage::create_create_error_response(status)?;
        return RpcMessage::create_success_reply_with_data(xid, res_data);
    }

    // Creating an entry needs write + search permission on the directory.
    // The attributes double as the pre-op view for ownership inheritance.
    let dir_before = match super::access_check::require(
        filesystem,
        &args.where_dir.0,
        cred,
        super::perm::WRITE | super::perm::EXEC,
    )
    .await
    {
        Ok(attrs) => attrs,
        Err(status) => {
            debug!("CREATE denied: {:?}", status);
            let res_data = NfsMessage::create_create_error_response(status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // The backend hands the new object to the caller itself, through the
    // descriptor it created it with, so nothing a concurrent rename swaps
    // into the directory meanwhile can be handed over by mistake.
    let owner =
        super::access_check::owner_for_new(filesystem, &args.where_dir.0, &dir_before, cred);
    let mode = match &args.how {
        createhow3::UNCHECKED(attrs) | createhow3::GUARDED(attrs) => match &attrs.mode {
            set_mode3::SET_MODE(m) => *m,
            _ => 0o644, // Default mode
        },
        // EXCLUSIVE carries a verifier instead of attributes. It is still
        // handled like GUARDED: the verifier is not stored in atime/mtime,
        // so a retransmitted create is reported as NFS3ERR_EXIST.
        createhow3::EXCLUSIVE(_verf) => 0o644,
    };
    let created = filesystem
        .create(&args.where_dir.0, filename, mode, Some(owner))
        .await;

    // The backend opens O_CREAT|O_EXCL, so a success is a brand-new file.
    // A name that is already taken is NFS3ERR_EXIST for GUARDED and
    // EXCLUSIVE; UNCHECKED keeps that file as it is (see `reuse_existing`).
    let file_handle = match created {
        Ok(handle) => handle,
        Err(e) if is_exists(&e) => match &args.how {
            createhow3::UNCHECKED(attrs) => {
                match reuse_existing(filesystem, &args.where_dir.0, filename, attrs, cred).await {
                    Ok(handle) => handle,
                    Err(status) => {
                        debug!("CREATE (UNCHECKED) on existing file denied: {:?}", status);
                        let res_data = NfsMessage::create_create_error_response(status)?;
                        return RpcMessage::create_success_reply_with_data(xid, res_data);
                    }
                }
            }
            createhow3::GUARDED(_) | createhow3::EXCLUSIVE(_) => {
                debug!("CREATE failed: {} exists", filename);
                let res_data = NfsMessage::create_create_error_response(nfsstat3::NFS3ERR_EXIST)?;
                return RpcMessage::create_success_reply_with_data(xid, res_data);
            }
        },
        Err(e) => {
            debug!("CREATE failed: {}", e);
            let error_status = if e.to_string().contains("not found") {
                nfsstat3::NFS3ERR_NOENT
            } else if e.to_string().contains("Not a directory") {
                nfsstat3::NFS3ERR_NOTDIR
            } else if e.to_string().contains("Permission denied") {
                nfsstat3::NFS3ERR_ACCES
            } else if e.to_string().contains("No space") {
                nfsstat3::NFS3ERR_NOSPC
            } else if e.to_string().contains("Read-only") {
                nfsstat3::NFS3ERR_ROFS
            } else {
                nfsstat3::NFS3ERR_IO
            };
            let res_data = NfsMessage::create_create_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get file attributes
    let file_attrs = match filesystem.getattr(&file_handle).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("CREATE: failed to get file attributes: {}", e);
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_create_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    // Get directory attributes after create
    let dir_attrs = match filesystem.getattr(&args.where_dir.0).await {
        Ok(attrs) => attrs,
        Err(e) => {
            debug!("CREATE: failed to get dir attributes: {}", e);
            let error_status = nfsstat3::NFS3ERR_IO;
            let res_data = NfsMessage::create_create_error_response(error_status)?;
            return RpcMessage::create_success_reply_with_data(xid, res_data);
        }
    };

    debug!(
        "CREATE success: new file handle {} bytes",
        file_handle.len()
    );

    // Convert FSAL attributes to NFS fattr3
    let nfs_file_attrs = NfsMessage::fsal_to_fattr3(&file_attrs);
    let nfs_dir_attrs = NfsMessage::fsal_to_fattr3(&dir_attrs);

    // Create CREATE response
    use xdr_codec::Pack;
    let mut buf = Vec::new();

    // 1. nfsstat3 status = NFS3_OK (0)
    (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

    // 2. CREATE3resok
    // obj: post_op_fh3 (optional file handle)
    true.pack(&mut buf)?; // handle_follows = TRUE

    // file handle (variable-length opaque)
    let handle_len = file_handle.len() as u32;
    handle_len.pack(&mut buf)?;
    buf.extend_from_slice(&file_handle);
    // Add padding
    let padding = (4 - (file_handle.len() % 4)) % 4;
    buf.resize(buf.len() + padding, 0);

    // obj_attributes: post_op_attr (optional attributes)
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_file_attrs.pack(&mut buf)?;

    // dir_wcc: wcc_data (directory weak cache consistency)
    // pre_op_attr
    false.pack(&mut buf)?; // pre_op_attr = FALSE

    // post_op_attr
    true.pack(&mut buf)?; // attributes_follow = TRUE
    nfs_dir_attrs.pack(&mut buf)?;

    let res_data = BytesMut::from(&buf[..]);

    // Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}

/// Did the backend refuse `create` because the name is already taken?
fn is_exists(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
}

/// UNCHECKED on a name that already exists (RFC 1813 §3.3.8).
///
/// The file stays as it is, owner and mode included: the sattr3 describes
/// the file the client meant to create, not a chmod of this one. Only a
/// requested size is applied, as knfsd does, and only to a regular file
/// the caller may write (owner override included, as for WRITE).
async fn reuse_existing(
    filesystem: &dyn NfsBackend,
    dir_handle: &FileHandle,
    name: &str,
    attrs: &sattr3,
    cred: &Credentials,
) -> Result<FileHandle, nfsstat3> {
    let handle = filesystem.lookup(dir_handle, name).await.map_err(|e| {
        debug!("CREATE: {} vanished after reporting EEXIST: {}", name, e);
        nfsstat3::NFS3ERR_IO
    })?;
    let set_size3::SET_SIZE(size) = &attrs.size else {
        return Ok(handle);
    };
    let existing = filesystem
        .getattr(&handle)
        .await
        .map_err(|e| super::error::classify_handle_error(&e))?;
    if existing.ftype != FileType::RegularFile {
        return Ok(handle);
    }
    let who = super::access_check::identity_for(filesystem, &handle, cred);
    if !super::perm::check_io(&existing, &who, super::perm::WRITE) {
        return Err(nfsstat3::NFS3ERR_ACCES);
    }
    filesystem.setattr_size(&handle, *size).await.map_err(|e| {
        debug!("CREATE: failed to truncate existing {}: {}", name, e);
        nfsstat3::NFS3ERR_IO
    })?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::BackendConfig;
    use crate::fsal::local::LocalFilesystem;
    use crate::protocol::v3::nfs::{
        CREATE3args, createverf3, fhandle3, filename3, nfstime3, set_atime, set_gid3, set_mtime,
        set_uid3,
    };
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::TempDir;
    use xdr_codec::Pack;

    /// A world-writable export root, so non-root callers may create in it.
    fn open_export() -> (Box<LocalFilesystem>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        fs::set_permissions(temp_dir.path(), PermissionsExt::from_mode(0o777)).unwrap();
        let config = BackendConfig::Local {
            path: temp_dir.path().to_path_buf(),
        };
        (config.create_filesystem(1).unwrap(), temp_dir)
    }

    fn sattr(mode: u32, size: u64) -> sattr3 {
        sattr3 {
            mode: set_mode3::SET_MODE(mode),
            uid: set_uid3::SET_UID(0),
            gid: set_gid3::SET_GID(0),
            size: set_size3::SET_SIZE(size),
            atime: set_atime::SET_TO_CLIENT_TIME(nfstime3 {
                seconds: 0,
                nseconds: 0,
            }),
            mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                seconds: 0,
                nseconds: 0,
            }),
        }
    }

    /// CREATE3args for UNCHECKED without a size. xdrgen cannot pack the
    /// `default` arm of `set_size3`, so the DONT_SET_SIZE discriminator
    /// is written by hand.
    fn unchecked_without_size_args(dir: &FileHandle, name: &str, mode: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        fhandle3(dir.clone()).pack(&mut buf).unwrap();
        filename3(name.to_string()).pack(&mut buf).unwrap();
        0i32.pack(&mut buf).unwrap(); // createhow3: UNCHECKED
        set_mode3::SET_MODE(mode).pack(&mut buf).unwrap();
        set_uid3::SET_UID(0).pack(&mut buf).unwrap();
        set_gid3::SET_GID(0).pack(&mut buf).unwrap();
        0i32.pack(&mut buf).unwrap(); // set_size3: DONT_SET_SIZE
        let epoch = nfstime3 {
            seconds: 0,
            nseconds: 0,
        };
        set_atime::SET_TO_CLIENT_TIME(epoch.clone())
            .pack(&mut buf)
            .unwrap();
        set_mtime::SET_TO_CLIENT_TIME(epoch).pack(&mut buf).unwrap();
        buf
    }

    fn create_args(dir: &FileHandle, name: &str, how: createhow3) -> Vec<u8> {
        let args = CREATE3args {
            where_dir: fhandle3(dir.clone()),
            name: filename3(name.to_string()),
            how,
        };
        let mut buf = Vec::new();
        args.pack(&mut buf).unwrap();
        buf
    }

    /// NFS status of a reply: it follows the 24-byte accepted RPC header.
    fn status_of(reply: &[u8]) -> i32 {
        i32::from_be_bytes(reply[24..28].try_into().unwrap())
    }

    fn running_as_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn chown(path: &std::path::Path, uid: u32, gid: u32) {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::chown(c.as_ptr(), uid, gid) }, 0);
    }

    #[tokio::test]
    async fn test_create_hands_file_to_caller() {
        if !running_as_root() {
            eprintln!("skipping: needs root to chown to another uid");
            return;
        }
        let (fs, temp_dir) = open_export();
        let root_handle = fs.root_file_handle();
        let args = create_args(
            &root_handle,
            "mine.txt",
            createhow3::UNCHECKED(sattr(0o644, 0)),
        );

        // LocalFilesystem's registry view never squashes, so uid 1000 stays.
        let reply = handle_create(1, &args, fs.as_ref(), &Credentials::sys(1000, 1001))
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3_OK as i32);
        let meta = fs::metadata(temp_dir.path().join("mine.txt")).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (1000, 1001));
    }

    #[tokio::test]
    async fn test_create_file() {
        let (fs, temp_dir) = open_export();
        let root_handle = fs.root_file_handle();
        let args = create_args(
            &root_handle,
            "new_file.txt",
            createhow3::UNCHECKED(sattr(0o644, 0)),
        );

        let reply = handle_create(12345, &args, fs.as_ref(), &Credentials::root())
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3_OK as i32);
        assert!(temp_dir.path().join("new_file.txt").exists());
    }

    #[tokio::test]
    async fn test_create_existing_file_unchecked() {
        let (fs, temp_dir) = open_export();
        let existing = temp_dir.path().join("existing.txt");
        fs::write(&existing, b"old content").unwrap();
        let root_handle = fs.root_file_handle();

        // UNCHECKED on an existing name succeeds and applies the size.
        let args = create_args(
            &root_handle,
            "existing.txt",
            createhow3::UNCHECKED(sattr(0o644, 0)),
        );
        let reply = handle_create(12345, &args, fs.as_ref(), &Credentials::root())
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3_OK as i32);
        assert_eq!(fs::metadata(&existing).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_create_unchecked_keeps_owner_and_mode_of_existing_file() {
        if !running_as_root() {
            eprintln!("skipping: needs root to chown to another uid");
            return;
        }
        let (fs, temp_dir) = open_export();
        let theirs = temp_dir.path().join("theirs");
        fs::write(&theirs, b"keep").unwrap();
        fs::set_permissions(&theirs, PermissionsExt::from_mode(0o666)).unwrap();
        chown(&theirs, 2000, 2000);
        let root_handle = fs.root_file_handle();

        // A writable file is truncated for uid 1000, but neither the mode
        // from the sattr3 nor the caller's identity are applied to it.
        let args = create_args(
            &root_handle,
            "theirs",
            createhow3::UNCHECKED(sattr(0o600, 0)),
        );
        let reply = handle_create(1, &args, fs.as_ref(), &Credentials::sys(1000, 1000))
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3_OK as i32);
        let meta = fs::metadata(&theirs).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (2000, 2000));
        assert_eq!(meta.mode() & 0o777, 0o666);
        assert_eq!(meta.len(), 0);
    }

    #[tokio::test]
    async fn test_create_unchecked_needs_write_to_truncate_existing_file() {
        let (fs, temp_dir) = open_export();
        let theirs = temp_dir.path().join("theirs");
        fs::write(&theirs, b"keep").unwrap();
        fs::set_permissions(&theirs, PermissionsExt::from_mode(0o644)).unwrap();
        let root_handle = fs.root_file_handle();
        // Anyone but the file's owner (the test process) and root.
        let owner = fs::metadata(&theirs).unwrap().uid();
        let stranger = Credentials::sys(owner.wrapping_add(1).max(1), 65000);

        // Asking for a size needs write permission on the file.
        let args = create_args(
            &root_handle,
            "theirs",
            createhow3::UNCHECKED(sattr(0o644, 0)),
        );
        let reply = handle_create(1, &args, fs.as_ref(), &stranger)
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3ERR_ACCES as i32);
        assert_eq!(fs::read(&theirs).unwrap(), b"keep");
        assert_eq!(fs::metadata(&theirs).unwrap().mode() & 0o777, 0o644);

        // Without a size there is nothing to apply, so it just succeeds.
        let args = unchecked_without_size_args(&root_handle, "theirs", 0o600);
        let reply = handle_create(2, &args, fs.as_ref(), &stranger)
            .await
            .unwrap();
        assert_eq!(status_of(&reply), nfsstat3::NFS3_OK as i32);
        assert_eq!(fs::read(&theirs).unwrap(), b"keep");
        assert_eq!(fs::metadata(&theirs).unwrap().mode() & 0o777, 0o644);
    }

    #[tokio::test]
    async fn test_create_guarded_and_exclusive_refuse_existing_file() {
        let (fs, temp_dir) = open_export();
        let existing = temp_dir.path().join("existing.txt");
        fs::write(&existing, b"old content").unwrap();
        let root_handle = fs.root_file_handle();

        for how in [
            createhow3::GUARDED(sattr(0o644, 0)),
            createhow3::EXCLUSIVE(createverf3([0; 8])),
        ] {
            let args = create_args(&root_handle, "existing.txt", how);
            let reply = handle_create(1, &args, fs.as_ref(), &Credentials::root())
                .await
                .unwrap();
            assert_eq!(status_of(&reply), nfsstat3::NFS3ERR_EXIST as i32);
            assert_eq!(fs::read(&existing).unwrap(), b"old content");
        }
    }
}
