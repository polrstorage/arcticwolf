// NFS Protocol Middleware
//
// Wraps xdrgen-generated NFS types and provides serialization helpers

use anyhow::Result;
use bytes::BytesMut;
use std::io::Cursor;
use xdr_codec::{Pack, Unpack};

use crate::fsal;

// Include xdrgen-generated NFS types
#[allow(dead_code, non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/nfs_generated.rs"));
}

// Re-export generated types
pub use generated::*;

/// Wrapper for NFS messages providing serialization helpers
pub struct NfsMessage;

impl NfsMessage {
    /// Deserialize GETATTR request
    pub fn deserialize_getattr3args(data: &[u8]) -> Result<GETATTR3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = GETATTR3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize GETATTR response
    pub fn serialize_getattr3res(res: &GETATTR3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Deserialize LOOKUP request
    pub fn deserialize_lookup3args(data: &[u8]) -> Result<LOOKUP3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = LOOKUP3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize LOOKUP response
    pub fn serialize_lookup3res(res: &LOOKUP3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful LOOKUP response
    pub fn create_lookup_ok(
        object: fhandle3,
        obj_attributes: fattr3,
        dir_attributes: fattr3,
    ) -> LOOKUP3res {
        LOOKUP3res::NFS3_OK(LOOKUP3resok {
            object,
            obj_attributes,
            dir_attributes,
        })
    }

    /// Create a LOOKUP error response
    ///
    /// LOOKUP error includes directory attributes in the failure case
    pub fn create_lookup_error_response(status: nfsstat3) -> Result<BytesMut> {
        // For LOOKUP error, we need status + post_op_attr (dir_attributes)
        // Since we don't have dir_attributes in error path, we use FALSE for post_op_attr
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        false.pack(&mut buf)?;  // dir_attributes: post_op_attr = FALSE (no attributes)
        Ok(BytesMut::from(&buf[..]))
    }

    /// Deserialize READ request
    pub fn deserialize_read3args(data: &[u8]) -> Result<READ3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = READ3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize READ response
    pub fn serialize_read3res(res: &READ3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful READ response
    pub fn create_read_ok(
        file_attributes: fattr3,
        count: u32,
        eof: bool,
        data: Vec<u8>,
    ) -> READ3res {
        READ3res::NFS3_OK(READ3resok {
            file_attributes,
            count,
            eof,
            data,
        })
    }

    /// Create a READ error response
    pub fn create_read_error_response(status: nfsstat3) -> Result<BytesMut> {
        // For READ error, we need status + post_op_attr (file_attributes)
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        false.pack(&mut buf)?;  // file_attributes: post_op_attr = FALSE (no attributes)
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful GETATTR response
    pub fn create_getattr_ok(attrs: fattr3) -> GETATTR3res {
        GETATTR3res::NFS3_OK(GETATTR3resok {
            obj_attributes: attrs,
        })
    }

    /// Create a GETATTR error response
    ///
    /// Since the default variant can't be serialized, we manually create
    /// the error response by serializing just the status code
    pub fn create_getattr_error_response(status: nfsstat3) -> Result<BytesMut> {
        // For GETATTR error responses, we only need to serialize the nfsstat3 error code
        // The XDR union has 'default: void', so no additional data
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    // ===== WRITE Helpers =====

    /// Deserialize WRITE request
    pub fn deserialize_write3args(data: &[u8]) -> Result<WRITE3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = WRITE3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Create a WRITE error response
    pub fn create_write_error_response(status: nfsstat3) -> Result<BytesMut> {
        // For WRITE error, we need status + wcc_data (file_wcc)
        // wcc_data = pre_op_attr + post_op_attr
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        false.pack(&mut buf)?;  // pre_op_attr = FALSE (no before attributes)
        false.pack(&mut buf)?;  // post_op_attr = FALSE (no after attributes)
        Ok(BytesMut::from(&buf[..]))
    }

    // ===== SETATTR Helpers =====

    /// Deserialize SETATTR request
    pub fn deserialize_setattr3args(data: &[u8]) -> Result<SETATTR3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = SETATTR3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Create a SETATTR error response
    pub fn create_setattr_error_response(status: nfsstat3) -> Result<BytesMut> {
        // For SETATTR error, we need status + wcc_data (obj_wcc)
        // wcc_data = pre_op_attr + post_op_attr
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        false.pack(&mut buf)?;  // pre_op_attr = FALSE
        false.pack(&mut buf)?;  // post_op_attr = FALSE
        Ok(BytesMut::from(&buf[..]))
    }

    // ===== CREATE Helpers =====

    /// Deserialize CREATE request
    pub fn deserialize_create3args(data: &[u8]) -> Result<CREATE3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = CREATE3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Create a CREATE error response
    pub fn create_create_error_response(status: nfsstat3) -> Result<BytesMut> {
        // For CREATE error, we need status + dir_wcc
        // dir_wcc = pre_op_attr + post_op_attr
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        false.pack(&mut buf)?;  // pre_op_attr = FALSE
        false.pack(&mut buf)?;  // post_op_attr = FALSE
        Ok(BytesMut::from(&buf[..]))
    }

    // ===== ACCESS Helpers =====

    /// Deserialize ACCESS request
    pub fn deserialize_access3args(data: &[u8]) -> Result<ACCESS3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = ACCESS3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize ACCESS response
    pub fn serialize_access3res(res: &ACCESS3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful ACCESS response
    pub fn create_access_ok(obj_attributes: fattr3, access: u32) -> ACCESS3res {
        ACCESS3res::NFS3_OK(ACCESS3resok {
            obj_attributes,
            access,
        })
    }

    /// Create an ACCESS error response
    pub fn create_access_error_response(status: nfsstat3) -> Result<BytesMut> {
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    // ===== FSSTAT Helpers =====

    /// Deserialize FSSTAT request
    pub fn deserialize_fsstat3args(data: &[u8]) -> Result<FSSTAT3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = FSSTAT3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize FSSTAT response
    pub fn serialize_fsstat3res(res: &FSSTAT3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful FSSTAT response
    #[allow(clippy::too_many_arguments)]
    pub fn create_fsstat_ok(
        obj_attributes: fattr3,
        tbytes: u64,
        fbytes: u64,
        abytes: u64,
        tfiles: u64,
        ffiles: u64,
        afiles: u64,
        invarsec: u32,
    ) -> FSSTAT3res {
        FSSTAT3res::NFS3_OK(FSSTAT3resok {
            obj_attributes,
            tbytes,
            fbytes,
            abytes,
            tfiles,
            ffiles,
            afiles,
            invarsec,
        })
    }

    /// Create an FSSTAT error response
    pub fn create_fsstat_error_response(status: nfsstat3) -> Result<BytesMut> {
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        false.pack(&mut buf)?;  // obj_attributes: post_op_attr = FALSE (no attributes)
        Ok(BytesMut::from(&buf[..]))
    }

    // ===== FSINFO Helpers =====

    /// Deserialize FSINFO request
    pub fn deserialize_fsinfo3args(data: &[u8]) -> Result<FSINFO3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = FSINFO3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize FSINFO response
    pub fn serialize_fsinfo3res(res: &FSINFO3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful FSINFO response with manual post_op_attr serialization
    ///
    /// RFC 1813 requires post_op_attr (bool discriminator + optional fattr3) but xdrgen
    /// doesn't support bool discriminators. We manually serialize the response here.
    #[allow(clippy::too_many_arguments)]
    pub fn create_fsinfo_ok(
        obj_attributes: fattr3,
        rtmax: u32,
        rtpref: u32,
        rtmult: u32,
        wtmax: u32,
        wtpref: u32,
        wtmult: u32,
        dtpref: u32,
        maxfilesize: u64,
        time_delta_seconds: u32,
        time_delta_nseconds: u32,
        properties: u32,
    ) -> Result<BytesMut> {
        // Manually serialize FSINFO response with proper post_op_attr format
        let mut buf = Vec::new();

        // 1. nfsstat3 status = NFS3_OK (0)
        (nfsstat3::NFS3_OK as i32).pack(&mut buf)?;

        // 2. post_op_attr: discriminator (bool TRUE = 1) + fattr3
        true.pack(&mut buf)?;
        obj_attributes.pack(&mut buf)?;

        // 3. FSINFO fields
        rtmax.pack(&mut buf)?;
        rtpref.pack(&mut buf)?;
        rtmult.pack(&mut buf)?;
        wtmax.pack(&mut buf)?;
        wtpref.pack(&mut buf)?;
        wtmult.pack(&mut buf)?;
        dtpref.pack(&mut buf)?;
        maxfilesize.pack(&mut buf)?;

        // 4. time_delta
        let time_delta = nfstime3 {
            seconds: time_delta_seconds,
            nseconds: time_delta_nseconds,
        };
        time_delta.pack(&mut buf)?;

        // 5. properties
        properties.pack(&mut buf)?;

        Ok(BytesMut::from(&buf[..]))
    }

    /// Create an FSINFO error response with manual post_op_attr serialization
    ///
    /// Error responses have post_op_attr with discriminator FALSE (no attributes)
    pub fn create_fsinfo_error_response(status: nfsstat3) -> Result<BytesMut> {
        let mut buf = Vec::new();

        // 1. nfsstat3 status (error code)
        (status as i32).pack(&mut buf)?;

        // 2. post_op_attr: discriminator (bool FALSE = 0, no fattr3 follows)
        false.pack(&mut buf)?;

        Ok(BytesMut::from(&buf[..]))
    }

    /// Convert FSAL FileAttributes to NFS fattr3
    ///
    /// Maps our internal file attributes representation to the NFSv3 wire format
    pub fn fsal_to_fattr3(attrs: &fsal::FileAttributes) -> fattr3 {
        // Convert FileType to ftype3
        let ftype = match attrs.ftype {
            fsal::FileType::RegularFile => ftype3::NF3REG,
            fsal::FileType::Directory => ftype3::NF3DIR,
            fsal::FileType::BlockDevice => ftype3::NF3BLK,
            fsal::FileType::CharDevice => ftype3::NF3CHR,
            fsal::FileType::SymbolicLink => ftype3::NF3LNK,
            fsal::FileType::Socket => ftype3::NF3SOCK,
            fsal::FileType::NamedPipe => ftype3::NF3FIFO,
        };

        // Convert rdev tuple (u32, u32) to u64
        let rdev = ((attrs.rdev.0 as u64) << 32) | (attrs.rdev.1 as u64);

        fattr3 {
            type_: ftype,
            mode: attrs.mode,
            nlink: attrs.nlink,
            uid: attrs.uid,
            gid: attrs.gid,
            size: attrs.size,
            used: attrs.used,
            rdev,
            fsid: attrs.fsid,
            fileid: attrs.fileid,
            atime: nfstime3 {
                seconds: attrs.atime.seconds as u32,
                nseconds: attrs.atime.nseconds,
            },
            mtime: nfstime3 {
                seconds: attrs.mtime.seconds as u32,
                nseconds: attrs.mtime.nseconds,
            },
            ctime: nfstime3 {
                seconds: attrs.ctime.seconds as u32,
                nseconds: attrs.ctime.nseconds,
            },
        }
    }

    /// Deserialize READDIR request
    pub fn deserialize_readdir3args(data: &[u8]) -> Result<READDIR3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = READDIR3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Serialize READDIR response
    pub fn serialize_readdir3res(res: &READDIR3res) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful READDIR response
    pub fn create_readdir_ok(
        dir_attributes: fattr3,
        cookieverf: cookieverf3,
        entries: Option<Box<entry3>>,
        eof: bool,
    ) -> READDIR3res {
        READDIR3res::NFS3_OK(READDIR3resok {
            dir_attributes,
            cookieverf,
            reply: dirlist3 { entries, eof },
        })
    }

    /// Create a READDIR error response
    pub fn create_readdir_error_response(status: nfsstat3) -> Result<BytesMut> {
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Deserialize READDIRPLUS3args from XDR bytes
    pub fn deserialize_readdirplus3args(data: &[u8]) -> Result<READDIRPLUS3args> {
        let mut cursor = Cursor::new(data);
        let (args, _bytes_read) = READDIRPLUS3args::unpack(&mut cursor)?;
        Ok(args)
    }

    /// Create a READDIRPLUS error response
    pub fn create_readdirplus_error_response(status: nfsstat3) -> Result<BytesMut> {
        let mut buf = Vec::new();
        (status as i32).pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }
}
