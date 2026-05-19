//! Admin response schema.
//!
//! The response is a tagged union with two arms: a success carrier with an
//! opaque JSON `data` payload, and an error carrier with a human-readable
//! message. Each command picks the shape of its own `data` (Phase 2+
//! defines those). The error path is shared so transport-level failures
//! (codec errors, unknown commands) and handler-level failures use the
//! same wire form.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AdminResponse {
    Ok { data: serde_json::Value },
    Err { error: String },
}

impl AdminResponse {
    /// Construct an error response with the given message.
    pub fn error(message: impl Into<String>) -> Self {
        Self::Err {
            error: message.into(),
        }
    }
}
