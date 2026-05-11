//! Shared NFS error mapping helpers.
//!
//! Phase 5 of #26 introduced multiple per-handle stat/info lookups that all
//! need to map FSAL-level anyhow errors into the appropriate nfsstat3 code.
//! This module exists so the few new call sites share a single mapping
//! function. The wider refactor — making FSAL return a typed error so
//! handlers stop string-matching — is tracked separately.

use crate::protocol::v3::nfs::nfsstat3;

/// Classify a FSAL error as `NFS3ERR_STALE` or `NFS3ERR_IO`.
///
/// Walks the anyhow error chain looking for a `std::io::Error` so callers
/// like `LocalFilesystem::getattr` — which wrap the underlying io error in
/// a context string that does not contain "not found" — still get mapped
/// to `NFS3ERR_STALE` when the kind is `NotFound`. Falls back to substring
/// matching for FSAL-internal anyhow errors (e.g. the router's
/// "Invalid handle: stale export uid X") that don't carry an io::Error.
pub(crate) fn classify_handle_error(e: &anyhow::Error) -> nfsstat3 {
    for cause in e.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return match io_err.kind() {
                std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_STALE,
                _ => nfsstat3::NFS3ERR_IO,
            };
        }
    }
    let s = e.to_string();
    if s.contains("not found") || s.contains("Invalid handle") {
        nfsstat3::NFS3ERR_STALE
    } else {
        nfsstat3::NFS3ERR_IO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::v3::nfs::nfsstat3;

    #[test]
    fn classifies_stale_substrings() {
        assert_eq!(
            classify_handle_error(&anyhow::anyhow!("file not found")),
            nfsstat3::NFS3ERR_STALE
        );
        assert_eq!(
            classify_handle_error(&anyhow::anyhow!("Invalid handle: too short")),
            nfsstat3::NFS3ERR_STALE
        );
    }

    #[test]
    fn classifies_other_as_io() {
        assert_eq!(
            classify_handle_error(&anyhow::anyhow!("permission denied")),
            nfsstat3::NFS3ERR_IO
        );
    }

    #[test]
    fn classifies_io_not_found_through_context() {
        use std::io;
        let io_err = io::Error::new(io::ErrorKind::NotFound, "no such file or directory");
        let anyhow_err: anyhow::Error =
            anyhow::Error::from(io_err).context("Failed to stat: \"/tmp/missing\"");
        assert_eq!(classify_handle_error(&anyhow_err), nfsstat3::NFS3ERR_STALE);
    }

    #[test]
    fn classifies_io_permission_denied_as_io() {
        use std::io;
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let anyhow_err: anyhow::Error = anyhow::Error::from(io_err).context("Failed to read");
        assert_eq!(classify_handle_error(&anyhow_err), nfsstat3::NFS3ERR_IO);
    }
}
