// MOUNT Protocol Middleware
//
// Wraps xdrgen-generated MOUNT types and provides serialization helpers

use anyhow::Result;
use bytes::BytesMut;
use std::io::Cursor;
use xdr_codec::{Pack, Unpack};

use crate::fsal::ExportInfo;

// Include xdrgen-generated MOUNT types
#[allow(
    dead_code,
    deprecated,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unused_assignments,
    clippy::all
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/mount_generated.rs"));
}

// Re-export generated types
pub use generated::*;

/// `mountstat3::MNT3ERR_NOENT` (RFC 1813) — returned by MNT when the
/// client-supplied dirpath does not match any configured export.
///
/// Bound to the xdrgen-generated enum so the wire value stays in sync if the
/// XDR definition is ever renumbered.
pub const MNT3ERR_NOENT: u32 = mountstat3::MNT3ERR_NOENT as u32;

/// Wrapper for MOUNT messages providing serialization helpers
pub struct MountMessage;

impl MountMessage {
    /// Deserialize MOUNT request arguments (dirpath)
    pub fn deserialize_dirpath(data: &[u8]) -> Result<String> {
        let mut cursor = Cursor::new(data);
        let (path_wrapper, _bytes_read) = dirpath::unpack(&mut cursor)?;
        Ok(path_wrapper.0) // Extract String from newtype wrapper
    }

    /// Serialize MOUNT response
    pub fn serialize_mountres3(res: &mountres3) -> Result<BytesMut> {
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    /// Create a successful mount response
    pub fn create_mount_ok(fhandle_bytes: Vec<u8>) -> mountres3 {
        mountres3::MNT3_OK(mountres3_ok {
            fhandle: fhandle3(fhandle_bytes), // Wrap in newtype
            auth_flavors: vec![0],            // AUTH_NONE
        })
    }

    /// Serialize a MNT error response carrying the given `mountstat3` status.
    ///
    /// RFC 1813 §5.2.1: when `fhs_status != MNT3_OK` the union has a `void`
    /// arm, so the wire form is just the 4-byte big-endian discriminator and
    /// nothing follows.
    pub fn serialize_mount_status(status: u32) -> BytesMut {
        BytesMut::from(&status.to_be_bytes()[..])
    }

    /// Serialize MOUNT EXPORT result — an XDR linked list of `exportnode`
    /// (RFC 1813 §5.2.5, Appendix I `exports`).
    ///
    /// Wire format for each export:
    ///   * `bool = 1` (list continues)
    ///   * `dirpath ex_dir` (XDR string)
    ///   * `groups ex_groups` (XDR linked list of `groupnode`) — Arctic Wolf
    ///     does not implement host access control, so this is always the
    ///     empty list (`bool = 0`)
    ///
    /// After the last export, a final `bool = 0` terminates the list.
    ///
    /// Hand-rolled (rather than via xdrgen) because the protocol uses
    /// self-referential pointer types (`exportnode *next`) that xdrgen's
    /// generated `Pack` impl handles awkwardly; the wire format is fixed and
    /// the by-hand version stays readable.
    pub fn serialize_exports(exports: &[ExportInfo]) -> BytesMut {
        let mut buf = Vec::with_capacity(64 + exports.len() * 32);
        for export in exports {
            // List-continues marker for this exportnode.
            buf.extend_from_slice(&1u32.to_be_bytes());

            // ex_dir: XDR string (length + bytes + zero padding to 4 bytes).
            let name_bytes = export.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_be_bytes());
            buf.extend_from_slice(name_bytes);
            let pad = (4 - (name_bytes.len() % 4)) % 4;
            buf.extend(std::iter::repeat_n(0u8, pad));

            // ex_groups: empty list — no host ACL support in v1.
            buf.extend_from_slice(&0u32.to_be_bytes());
        }
        // End-of-list marker.
        buf.extend_from_slice(&0u32.to_be_bytes());
        BytesMut::from(&buf[..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_exports_empty_emits_terminator_only() {
        let bytes = MountMessage::serialize_exports(&[]);
        assert_eq!(bytes.as_ref(), &[0, 0, 0, 0]);
    }

    #[test]
    fn serialize_exports_one_entry_has_correct_layout() {
        let exports = vec![ExportInfo {
            name: "/data".to_string(),
            uid: 1,
            read_only: false,
            fsal: "local".to_string(),
        }];
        let bytes = MountMessage::serialize_exports(&exports);

        // Expected wire form:
        //   00 00 00 01            list-continues
        //   00 00 00 05            string length = 5
        //   2f 64 61 74 61 00 00 00  "/data" + 3 bytes padding
        //   00 00 00 00            ex_groups = empty list
        //   00 00 00 00            list-terminator
        let expected: &[u8] = &[
            0, 0, 0, 1, 0, 0, 0, 5, b'/', b'd', b'a', b't', b'a', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(bytes.as_ref(), expected);
    }

    #[test]
    fn serialize_exports_two_entries_emits_two_continuations_and_one_terminator() {
        let exports = vec![
            ExportInfo {
                name: "/data".to_string(),
                uid: 1,
                read_only: false,
                fsal: "local".to_string(),
            },
            ExportInfo {
                name: "/backup".to_string(),
                uid: 2,
                read_only: true,
                fsal: "local".to_string(),
            },
        ];
        let bytes = MountMessage::serialize_exports(&exports);

        // Count list-continues markers (4-byte 0x00000001) and terminator.
        // Each export contributes one continues marker and one zero groups
        // marker; the final terminator is a single zero marker. So in 4-byte
        // chunks we should see (per export): 1, len, name+pad..., 0; then 0.
        // Easiest assertion: parse it back.
        let mut cursor = 0usize;
        let mut decoded = Vec::new();
        loop {
            let cont = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
            cursor += 4;
            if cont == 0 {
                break;
            }
            assert_eq!(cont, 1, "list-continues marker must be 0 or 1");
            let name_len =
                u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            let name = std::str::from_utf8(&bytes[cursor..cursor + name_len])
                .unwrap()
                .to_string();
            cursor += name_len;
            // Skip padding to next 4-byte boundary.
            let pad = (4 - (name_len % 4)) % 4;
            cursor += pad;
            // ex_groups (empty list).
            let groups = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
            cursor += 4;
            assert_eq!(groups, 0, "ex_groups must be empty");
            decoded.push(name);
        }
        assert_eq!(decoded, vec!["/data", "/backup"]);
        assert_eq!(cursor, bytes.len(), "must consume the entire buffer");
    }

    #[test]
    fn serialize_mount_status_emits_four_byte_discriminator() {
        let bytes = MountMessage::serialize_mount_status(MNT3ERR_NOENT);
        assert_eq!(bytes.as_ref(), &[0, 0, 0, 2]);
    }
}
