//! Length-prefixed JSON wire codec for the admin server.
//!
//! Every admin message is encoded as a 4-byte big-endian length followed by
//! the UTF-8 JSON body, using `tokio_util::codec::LengthDelimitedCodec` for
//! the framing. JSON is chosen over a tighter binary encoding for two
//! reasons: payloads contain free-form strings (log lines, error messages,
//! export paths) where escaping a binary frame buys nothing, and the
//! `arcticwolfctl raw '<json>'` debug command requires the wire format to
//! be human-readable.
//!
//! This module exposes:
//! - [`framed`] wraps a Tokio `AsyncRead + AsyncWrite` stream in a
//!   `LengthDelimitedCodec`, so each read or write is one length-prefixed
//!   frame of raw bytes — the JSON body is encoded and decoded separately
//!   by the helpers below.
//! - [`decode_request`] / [`encode_response`] are the raw JSON conversion
//!   functions, kept public so unit tests (and Phase 2 batch tooling) can
//!   exercise them without setting up a socket pair.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::request::AdminRequest;
use super::response::AdminResponse;

/// Maximum size of a single admin frame (1 MiB).
///
/// Admin payloads are small (status snapshots, exports lists, metrics
/// dumps); the cap exists so a buggy or malicious client cannot make the
/// daemon allocate gigabytes by sending a crafted length prefix. 1 MiB is
/// well above any plausible legitimate payload — the largest expected
/// response is `metrics` in Prometheus format, which for a handful of
/// counters/histograms is still firmly in the kilobyte range.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Build the `LengthDelimitedCodec` used by both sides of the connection.
fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

/// Wrap a stream so each frame is a single JSON-encoded `AdminRequest` or
/// `AdminResponse`. The framing is symmetric (same codec on both ends).
pub fn framed<S>(io: S) -> Framed<S, LengthDelimitedCodec>
where
    S: AsyncRead + AsyncWrite,
{
    Framed::new(io, codec())
}

/// Decode a single frame's bytes into an `AdminRequest`.
///
/// Surfaces serde errors as plain `String`s so the dispatcher can wrap
/// them in an `AdminResponse::Err` without leaking the serde error type
/// into the public API. The message starts with "Unknown command" when
/// the tagged-enum discriminator is unrecognized, which the client uses
/// to distinguish "unknown command" from other parse failures.
pub fn decode_request(frame: &[u8]) -> Result<AdminRequest, String> {
    match serde_json::from_slice::<AdminRequest>(frame) {
        Ok(req) => Ok(req),
        Err(err) => Err(translate_decode_error(err)),
    }
}

fn translate_decode_error(err: serde_json::Error) -> String {
    // Classify with `serde_json::error::Category` rather than matching the
    // English message text. Pattern-matching `err.to_string()` against
    // "unknown variant " coupled the CLI's user-facing error to serde_json's
    // internal wording — any localization or message tweak there would
    // silently break the "Unknown command" contract.
    //
    // The four categories from `serde_json` are:
    //   - `Data`   : structurally valid JSON, but the schema doesn't match
    //                (unknown variant, wrong field type, missing field).
    //                For the admin protocol this is overwhelmingly an
    //                unknown command, so we surface it as such.
    //   - `Syntax` : the bytes are not valid JSON at all.
    //   - `Eof`    : valid-so-far JSON cut short mid-token.
    //   - `Io`     : a reader-level failure; not applicable when decoding
    //                from a `&[u8]`, but matched for completeness so a
    //                future move to `from_reader` doesn't fall through.
    match err.classify() {
        serde_json::error::Category::Data => format!("Unknown command: {err}"),
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
            format!("Malformed admin request: {err}")
        }
        serde_json::error::Category::Io => {
            format!("I/O error reading admin request: {err}")
        }
    }
}

/// Encode a response as a single frame's bytes.
///
/// `serde_json::to_vec` cannot fail for the shapes used by `AdminResponse`
/// (the `data` field is already a `serde_json::Value`, which is always
/// serializable; the error arm is a plain `String`). We still return a
/// `Result` so callers don't have to assume that.
pub fn encode_response(response: &AdminResponse) -> Result<Bytes, serde_json::Error> {
    serde_json::to_vec(response).map(Bytes::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use tokio_util::codec::{Decoder, Encoder};

    /// Round-trip the codec: encode an `AdminRequest`, then run the bytes
    /// back through the framing layer and parse them with
    /// `decode_request`. Pins both the framing math (4-byte BE length
    /// prefix, then payload) and the JSON tagging.
    #[test]
    fn codec_round_trip_status() {
        let original = AdminRequest::Status;
        let payload = serde_json::to_vec(&original).expect("serialize");

        let mut codec = codec();
        let mut buf = BytesMut::new();
        codec
            .encode(payload.clone().into(), &mut buf)
            .expect("encode");

        // Length prefix occupies 4 bytes; the rest is the JSON body.
        assert_eq!(buf.len(), 4 + payload.len());
        let len_prefix = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(len_prefix as usize, payload.len());

        let frame = codec
            .decode(&mut buf)
            .expect("decode")
            .expect("frame ready");
        let decoded = decode_request(&frame).expect("decode_request");
        assert_eq!(decoded, original);
    }

    /// The dispatcher distinguishes "unknown command" from other parse
    /// errors by the error message prefix. This test pins that contract so
    /// the CLI's user-facing error stays correct as serde_json's default
    /// wording evolves.
    #[test]
    fn decode_request_unknown_command_has_prefixed_error() {
        let raw = br#"{"command":"unknown-thing"}"#;
        let err = decode_request(raw).expect_err("unknown variant must fail");
        assert!(
            err.contains("Unknown command"),
            "unknown-command errors must start with 'Unknown command'; got: {err}",
        );
        assert!(
            err.contains("unknown-thing"),
            "error should mention the offending variant; got: {err}",
        );
    }

    /// Malformed JSON is a different failure than an unknown command and
    /// must not get rewritten with the "Unknown command" prefix — otherwise
    /// the CLI would mis-classify it. Classification goes through
    /// `serde_json::error::Category::Syntax`, which we surface as
    /// "Malformed admin request: ...".
    #[test]
    fn decode_request_malformed_json_uses_malformed_prefix() {
        let raw = b"not json at all";
        let err = decode_request(raw).expect_err("non-json must fail");
        assert!(
            !err.starts_with("Unknown command"),
            "syntax errors must not get the unknown-command prefix; got: {err}",
        );
        assert!(
            err.starts_with("Malformed admin request"),
            "syntax errors must be tagged 'Malformed admin request'; got: {err}",
        );
    }

    /// A truncated JSON object (valid-so-far but cut short) classifies as
    /// `Category::Eof`, which shares the "Malformed admin request" prefix
    /// with `Category::Syntax`.
    #[test]
    fn decode_request_truncated_json_uses_malformed_prefix() {
        let raw = br#"{"command":"#;
        let err = decode_request(raw).expect_err("truncated json must fail");
        assert!(
            err.starts_with("Malformed admin request"),
            "EOF mid-frame must be tagged 'Malformed admin request'; got: {err}",
        );
    }

    /// Data-category errors include not just unknown discriminators (already
    /// covered by `decode_request_unknown_command_has_prefixed_error`) but
    /// also requests that are structurally valid JSON yet not a request —
    /// the most common being a missing `command` tag. All of these surface
    /// to the CLI as "Unknown command", which is the operator-facing
    /// phrasing for "I cannot service this request".
    #[test]
    fn decode_request_missing_tag_uses_unknown_command_prefix() {
        // No discriminator at all is a `Category::Data` error in serde_json
        // ("missing field `command`"). Previously this would have leaked the
        // raw serde message; the new mapping surfaces it as a command issue
        // so the CLI's operator-facing wording stays consistent.
        let raw = b"{}";
        let err = decode_request(raw).expect_err("missing tag must fail");
        assert!(
            err.starts_with("Unknown command"),
            "missing discriminator must use the 'Unknown command' prefix; got: {err}",
        );
    }

    /// `encode_response` produces bytes that `serde_json::from_slice`
    /// decodes back into an equal value. Guards the discriminator key
    /// (`status`) and the snake_case naming against accidental changes.
    #[test]
    fn encode_response_round_trip() {
        let resp = AdminResponse::Err {
            error: "Command not implemented in phase 1".to_string(),
        };
        let encoded = encode_response(&resp).expect("encode");
        let decoded: AdminResponse = serde_json::from_slice(&encoded).expect("decode");
        assert_eq!(decoded, resp);

        // The wire shape uses `status` as the tag — pin it explicitly so a
        // refactor that renames the tag doesn't silently break clients.
        let as_value: serde_json::Value =
            serde_json::from_slice(&encoded).expect("decode as value");
        assert_eq!(as_value["status"], "err");
    }

    /// Bound the maximum frame size so the codec rejects oversize payloads
    /// rather than allocating unboundedly. The exact byte limit is a
    /// policy choice, but it has to exist.
    #[test]
    fn codec_rejects_oversize_frames() {
        let mut codec = codec();
        let mut buf = BytesMut::new();
        let too_big = vec![b'a'; MAX_FRAME_BYTES + 1];
        let err = codec
            .encode(too_big.into(), &mut buf)
            .expect_err("oversize encode must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("max") || msg.contains("frame"),
            "oversize error should mention the limit; got: {msg}",
        );
    }
}
