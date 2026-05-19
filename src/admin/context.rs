//! Shared state for admin request handlers.
//!
//! Phase 1 has no real handlers, so the context is intentionally empty. The
//! struct still exists (and is threaded through `serve()` and the
//! dispatcher) so later phases can add fields — the filesystem handle for
//! exports mutation (Phase 4/5), the tracing reload handle (Phase 3), the
//! audit log writer (Phase 6), etc. — without touching the call sites.
//!
//! `Clone` and `Default` are deliberately *not* derived: Phase 3's
//! `tracing_subscriber::reload::Handle<EnvFilter, Registry>` does not
//! implement `Clone`, and Phase 6's audit-log writer does not implement
//! `Default`. Dropping those derives now means later phases can add the
//! fields without noisy `derive`-list changes. Consumers share the context
//! via `Arc<AdminContext>` (see [`AdminContext::shared`]).

use std::sync::Arc;

#[non_exhaustive]
pub struct AdminContext {}

impl AdminContext {
    // `Default` is intentionally not implemented (see module doc), so silence
    // the lint that asks for one.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {}
    }

    /// Convenience for the call site in `main.rs`, which always wraps the
    /// context in an `Arc` so it can be cloned cheaply into per-connection
    /// tasks.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}
