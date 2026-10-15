//! POSIX permission checks for NFS procedures (RFC 1813 §4.4).
//!
//! The server runs every backend operation as its own process identity, so
//! the kernel never sees the NFS caller. These helpers reproduce the
//! `generic_permission()` rules in user space against the attributes the
//! FSAL reports: owner / group / other mode bits, supplementary groups and
//! the uid 0 bypass. Each NFS handler calls them on entry via
//! [`super::access_check`], before touching the filesystem.
//!
//! This is check-then-act: a concurrent chmod between the check and the
//! backend call is not caught. User-space NFS servers (unfs3, Ganesha's
//! VFS FSAL without `setfsuid`) share that window; `setfsuid(2)` is not an
//! option here because it is per-thread and tokio tasks migrate between
//! threads.

use crate::fsal::{FileAttributes, FileType};
use crate::protocol::v3::nfs::nfsstat3;

/// Permission bits in the low octal digit, as `access(2)` uses them.
pub const READ: u32 = 0o4;
pub const WRITE: u32 = 0o2;
pub const EXEC: u32 = 0o1;

/// Caller identity after the export's squash policy has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

impl Identity {
    pub fn is_superuser(&self) -> bool {
        self.uid == 0
    }

    /// True if `gid` is the primary or one of the supplementary groups.
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.gids.contains(&gid)
    }

    /// The permission-bit triplet that applies to `who` for this object.
    fn class_bits(&self, attrs: &FileAttributes) -> u32 {
        let mode = attrs.mode & 0o777;
        if attrs.uid == self.uid {
            (mode >> 6) & 0o7
        } else if self.in_group(attrs.gid) {
            (mode >> 3) & 0o7
        } else {
            mode & 0o7
        }
    }
}

/// Does `who` hold every bit in `want` on the object described by `attrs`?
///
/// uid 0 passes read and write unconditionally; execute still needs at
/// least one x bit on anything that is not a directory, so root cannot
/// "execute" a plain data file.
pub fn check(attrs: &FileAttributes, who: &Identity, want: u32) -> bool {
    if who.is_superuser() {
        if want & EXEC != 0 && attrs.ftype != FileType::Directory && attrs.mode & 0o111 == 0 {
            return false;
        }
        return true;
    }
    who.class_bits(attrs) & want == want
}

/// [`check`] with the owner override RFC 1813 §4.4 recommends for READ,
/// WRITE and COMMIT: a client checks permissions at `open(2)` time, so a
/// later `chmod` by the owner must not break I/O on the already-open file.
pub fn check_io(attrs: &FileAttributes, who: &Identity, want: u32) -> bool {
    attrs.uid == who.uid || check(attrs, who, want)
}

/// Permission rules for SETATTR, mirroring `chmod(2)`, `chown(2)` and
/// `truncate(2)`:
///
/// - mode, uid, gid: the caller must own the object or be root
///   (`NFS3ERR_PERM` otherwise);
/// - uid: only root may change it (`_POSIX_CHOWN_RESTRICTED`);
/// - gid: a non-root owner may only move it to a group they belong to;
/// - size: needs write permission (`NFS3ERR_ACCES`), owner override
///   included as for WRITE.
pub fn check_setattr(
    attrs: &FileAttributes,
    who: &Identity,
    want_mode: bool,
    want_uid: Option<u32>,
    want_gid: Option<u32>,
    want_size: bool,
) -> Result<(), nfsstat3> {
    let owner = attrs.uid == who.uid;
    if who.is_superuser() {
        // root may do anything here; size still goes through the fs.
    } else {
        if (want_mode || want_uid.is_some() || want_gid.is_some()) && !owner {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
        if let Some(uid) = want_uid
            && uid != attrs.uid
        {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
        if let Some(gid) = want_gid
            && gid != attrs.gid
            && !who.in_group(gid)
        {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
    }
    if want_size && !check_io(attrs, who, WRITE) {
        return Err(nfsstat3::NFS3ERR_ACCES);
    }
    Ok(())
}

// ACCESS3 request bits (RFC 1813 §3.3.4).
pub const ACCESS3_READ: u32 = 0x0001;
pub const ACCESS3_LOOKUP: u32 = 0x0002;
pub const ACCESS3_MODIFY: u32 = 0x0004;
pub const ACCESS3_EXTEND: u32 = 0x0008;
pub const ACCESS3_DELETE: u32 = 0x0010;
pub const ACCESS3_EXECUTE: u32 = 0x0020;

/// Answer an ACCESS request: the subset of `requested` that `who` holds.
///
/// LOOKUP applies to directories and EXECUTE to everything else, both
/// backed by the x bit. MODIFY / EXTEND / DELETE map to the w bit (DELETE
/// on a directory also needs x, like `unlink(2)` on an entry) and are
/// withheld entirely on a read-only export.
pub fn access_bits(attrs: &FileAttributes, who: &Identity, requested: u32, read_only: bool) -> u32 {
    let is_dir = attrs.ftype == FileType::Directory;
    let mut granted = 0;
    if requested & ACCESS3_READ != 0 && check(attrs, who, READ) {
        granted |= ACCESS3_READ;
    }
    if requested & ACCESS3_LOOKUP != 0 && is_dir && check(attrs, who, EXEC) {
        granted |= ACCESS3_LOOKUP;
    }
    if requested & ACCESS3_EXECUTE != 0 && !is_dir && check(attrs, who, EXEC) {
        granted |= ACCESS3_EXECUTE;
    }
    if !read_only {
        if requested & ACCESS3_MODIFY != 0 && check(attrs, who, WRITE) {
            granted |= ACCESS3_MODIFY;
        }
        if requested & ACCESS3_EXTEND != 0 && check(attrs, who, WRITE) {
            granted |= ACCESS3_EXTEND;
        }
        let delete_want = if is_dir { WRITE | EXEC } else { WRITE };
        if requested & ACCESS3_DELETE != 0 && check(attrs, who, delete_want) {
            granted |= ACCESS3_DELETE;
        }
    }
    granted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsal::FileTime;

    fn attrs(ftype: FileType, mode: u32, uid: u32, gid: u32) -> FileAttributes {
        let t = FileTime {
            seconds: 0,
            nseconds: 0,
        };
        FileAttributes {
            ftype,
            mode,
            nlink: 1,
            uid,
            gid,
            size: 0,
            used: 0,
            rdev: (0, 0),
            fsid: 0,
            fileid: 1,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }

    fn who(uid: u32, gid: u32, gids: &[u32]) -> Identity {
        Identity {
            uid,
            gid,
            gids: gids.to_vec(),
        }
    }

    #[test]
    fn owner_group_other_classes() {
        let f = attrs(FileType::RegularFile, 0o640, 1000, 100);
        assert!(check(&f, &who(1000, 1, &[]), READ | WRITE));
        assert!(check(&f, &who(2000, 100, &[]), READ));
        assert!(!check(&f, &who(2000, 100, &[]), WRITE));
        assert!(check(&f, &who(2000, 1, &[100]), READ), "supplementary gid");
        assert!(!check(&f, &who(2000, 1, &[]), READ));
    }

    #[test]
    fn owner_class_wins_even_when_more_restrictive() {
        // 0o077: owner has nothing, everyone else has everything.
        let f = attrs(FileType::RegularFile, 0o077, 1000, 100);
        assert!(!check(&f, &who(1000, 100, &[]), READ));
        assert!(check(&f, &who(2000, 100, &[]), READ | WRITE | EXEC));
    }

    #[test]
    fn root_bypasses_rw_but_not_exec_on_files() {
        let f = attrs(FileType::RegularFile, 0o000, 1000, 100);
        assert!(check(&f, &who(0, 0, &[]), READ | WRITE));
        assert!(!check(&f, &who(0, 0, &[]), EXEC));
        let x = attrs(FileType::RegularFile, 0o100, 1000, 100);
        assert!(check(&x, &who(0, 0, &[]), EXEC));
        let d = attrs(FileType::Directory, 0o000, 1000, 100);
        assert!(check(&d, &who(0, 0, &[]), EXEC));
    }

    #[test]
    fn io_owner_override() {
        let f = attrs(FileType::RegularFile, 0o000, 1000, 100);
        assert!(check_io(&f, &who(1000, 100, &[]), WRITE));
        assert!(!check_io(&f, &who(2000, 100, &[]), READ));
    }

    #[test]
    fn setattr_rules() {
        let f = attrs(FileType::RegularFile, 0o644, 1000, 100);
        let owner = who(1000, 100, &[200]);
        let other = who(2000, 100, &[]);
        let root = who(0, 0, &[]);

        assert_eq!(check_setattr(&f, &owner, true, None, None, false), Ok(()));
        assert_eq!(
            check_setattr(&f, &other, true, None, None, false),
            Err(nfsstat3::NFS3ERR_PERM)
        );
        // chown is root-only; chown to the current owner is a no-op.
        assert_eq!(
            check_setattr(&f, &owner, false, Some(2000), None, false),
            Err(nfsstat3::NFS3ERR_PERM)
        );
        assert_eq!(
            check_setattr(&f, &owner, false, Some(1000), None, false),
            Ok(())
        );
        assert_eq!(
            check_setattr(&f, &root, false, Some(2000), None, false),
            Ok(())
        );
        // chgrp: owner may move to a group they belong to, nothing else.
        assert_eq!(
            check_setattr(&f, &owner, false, None, Some(200), false),
            Ok(())
        );
        assert_eq!(
            check_setattr(&f, &owner, false, None, Some(300), false),
            Err(nfsstat3::NFS3ERR_PERM)
        );
        // truncate: write permission, owner override included.
        assert_eq!(
            check_setattr(&f, &other, false, None, None, true),
            Err(nfsstat3::NFS3ERR_ACCES)
        );
        let ro_owner = attrs(FileType::RegularFile, 0o444, 1000, 100);
        assert_eq!(
            check_setattr(&ro_owner, &owner, false, None, None, true),
            Ok(())
        );
    }

    #[test]
    fn access_bits_follow_mode_and_type() {
        let all = ACCESS3_READ
            | ACCESS3_LOOKUP
            | ACCESS3_MODIFY
            | ACCESS3_EXTEND
            | ACCESS3_DELETE
            | ACCESS3_EXECUTE;

        let dir = attrs(FileType::Directory, 0o750, 1000, 100);
        assert_eq!(
            access_bits(&dir, &who(1000, 100, &[]), all, false),
            ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE
        );
        assert_eq!(
            access_bits(&dir, &who(2000, 100, &[]), all, false),
            ACCESS3_READ | ACCESS3_LOOKUP
        );
        assert_eq!(access_bits(&dir, &who(2000, 1, &[]), all, false), 0);

        let file = attrs(FileType::RegularFile, 0o755, 1000, 100);
        assert_eq!(
            access_bits(&file, &who(2000, 1, &[]), all, false),
            ACCESS3_READ | ACCESS3_EXECUTE
        );
        // Read-only export withholds every write-class bit, even for the owner.
        assert_eq!(
            access_bits(&file, &who(1000, 100, &[]), all, true),
            ACCESS3_READ | ACCESS3_EXECUTE
        );
    }
}
