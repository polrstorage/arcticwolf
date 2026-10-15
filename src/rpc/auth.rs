//! RPC credential handling (RFC 5531 §8.2 / Appendix A).
//!
//! Every RPC call carries a credential (`cred`) and a verifier (`verf`).
//! This module turns the credential into a [`Credentials`] value that the
//! NFS handlers use for permission checks, or into an `auth_stat` the
//! server sends back in a `MSG_DENIED` / `AUTH_ERROR` reply.
//!
//! Only two flavors are accepted:
//!
//! - `AUTH_NONE`: the caller asserts no identity. It is mapped to the
//!   export's anonymous uid/gid downstream, the same way the Linux kernel
//!   server treats `AUTH_NULL`.
//! - `AUTH_SYS` (`AUTH_UNIX`): uid, gid and up to 16 supplementary gids,
//!   trusted as-is. Per-export squashing is applied later by the NFS
//!   layer, which is the only place that knows which export a handle
//!   belongs to.
//!
//! Everything else (`AUTH_SHORT`, `AUTH_DH`, `RPCSEC_GSS`) is rejected with
//! `AUTH_BADCRED`, which is also what `nfsd` returns for a flavor it has no
//! handler for.

use std::io::Cursor;

use xdr_codec::Unpack;

use crate::protocol::v3::rpc::{auth_flavor, auth_stat, auth_sys_params, opaque_auth};

/// Identity asserted by the RPC credential of one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// `AUTH_NONE`: no identity. Resolved to the export's anonymous
    /// uid/gid before any permission check.
    Anonymous,
    /// `AUTH_SYS`: Unix identity as sent by the client.
    Sys {
        uid: u32,
        gid: u32,
        /// Supplementary groups, at most 16 (RFC 5531 Appendix A).
        gids: Vec<u32>,
    },
}

impl Credentials {
    /// `AUTH_SYS` uid 0 / gid 0 with no supplementary groups. Used by unit
    /// tests that want the pre-squash superuser.
    pub fn root() -> Self {
        Self::Sys {
            uid: 0,
            gid: 0,
            gids: Vec::new(),
        }
    }

    /// `AUTH_SYS` identity with the given primary uid/gid and no
    /// supplementary groups.
    pub fn sys(uid: u32, gid: u32) -> Self {
        Self::Sys {
            uid,
            gid,
            gids: Vec::new(),
        }
    }
}

/// Decode the credential and verifier of an RPC call.
///
/// Returns the `auth_stat` to send in an `AUTH_ERROR` reply when the call
/// must be denied: an unsupported flavor or an undecodable `AUTH_SYS` body
/// is `AUTH_BADCRED`; a verifier that is not `AUTH_NONE` is `AUTH_BADVERF`
/// (both accepted flavors require a null verifier).
pub fn parse_credentials(cred: &opaque_auth, verf: &opaque_auth) -> Result<Credentials, auth_stat> {
    if verf.flavor != auth_flavor::AUTH_NONE {
        return Err(auth_stat::AUTH_BADVERF);
    }

    match cred.flavor {
        auth_flavor::AUTH_NONE => Ok(Credentials::Anonymous),
        auth_flavor::AUTH_SYS => {
            let mut cursor = Cursor::new(&cred.body[..]);
            let (params, _) =
                auth_sys_params::unpack(&mut cursor).map_err(|_| auth_stat::AUTH_BADCRED)?;
            Ok(Credentials::Sys {
                uid: params.uid,
                gid: params.gid,
                gids: params.gids,
            })
        }
        _ => Err(auth_stat::AUTH_BADCRED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdr_codec::Pack;

    fn none() -> opaque_auth {
        opaque_auth {
            flavor: auth_flavor::AUTH_NONE,
            body: Vec::new(),
        }
    }

    fn sys_body(uid: u32, gid: u32, gids: &[u32]) -> Vec<u8> {
        let params = auth_sys_params {
            stamp: 42,
            machinename: "client".to_string(),
            uid,
            gid,
            gids: gids.to_vec(),
        };
        let mut buf = Vec::new();
        params.pack(&mut buf).unwrap();
        buf
    }

    fn sys(body: Vec<u8>) -> opaque_auth {
        opaque_auth {
            flavor: auth_flavor::AUTH_SYS,
            body,
        }
    }

    #[test]
    fn auth_none_is_anonymous() {
        assert_eq!(
            parse_credentials(&none(), &none()),
            Ok(Credentials::Anonymous)
        );
    }

    #[test]
    fn auth_sys_decodes_uid_gid_and_groups() {
        let cred = sys(sys_body(1000, 100, &[4, 24, 1000]));
        assert_eq!(
            parse_credentials(&cred, &none()),
            Ok(Credentials::Sys {
                uid: 1000,
                gid: 100,
                gids: vec![4, 24, 1000],
            })
        );
    }

    #[test]
    fn auth_sys_with_truncated_body_is_badcred() {
        let mut body = sys_body(1000, 100, &[]);
        body.truncate(body.len() - 6);
        assert_eq!(
            parse_credentials(&sys(body), &none()),
            Err(auth_stat::AUTH_BADCRED)
        );
    }

    #[test]
    fn auth_sys_with_too_many_groups_is_badcred() {
        // RFC 5531 caps gids at 16; the XDR bound rejects a 17th. `pack`
        // enforces the same bound, so build the body by hand.
        let mut body = sys_body(1000, 100, &[]);
        body.truncate(body.len() - 4); // drop the empty gids length
        body.extend_from_slice(&17u32.to_be_bytes());
        for gid in 1..=17u32 {
            body.extend_from_slice(&gid.to_be_bytes());
        }
        let cred = sys(body);
        assert_eq!(
            parse_credentials(&cred, &none()),
            Err(auth_stat::AUTH_BADCRED)
        );
    }

    #[test]
    fn unsupported_flavor_is_badcred() {
        for flavor in [
            auth_flavor::AUTH_SHORT,
            auth_flavor::AUTH_DH,
            auth_flavor::RPCSEC_GSS,
        ] {
            let cred = opaque_auth {
                flavor,
                body: vec![0; 8],
            };
            assert_eq!(
                parse_credentials(&cred, &none()),
                Err(auth_stat::AUTH_BADCRED),
                "{flavor:?}"
            );
        }
    }

    #[test]
    fn non_null_verifier_is_badverf() {
        let verf = opaque_auth {
            flavor: auth_flavor::AUTH_SYS,
            body: Vec::new(),
        };
        assert_eq!(
            parse_credentials(&sys(sys_body(0, 0, &[])), &verf),
            Err(auth_stat::AUTH_BADVERF)
        );
    }
}
