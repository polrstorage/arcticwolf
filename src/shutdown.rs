//! Graceful-shutdown primitives shared by the daemon entry point (`main.rs`)
//! and the integration tests (issue #25 Phase 8).
//!
//! The daemon coordinates shutdown with a single
//! [`tokio_util::sync::CancellationToken`] threaded into every accept loop
//! (the three RPC servers and the admin server). The token is cancelled
//! once, on the first `SIGTERM`/`SIGINT`; each loop stops accepting new
//! connections and drains its in-flight tasks. This module holds the two
//! pieces of that flow that are worth unit-testing in isolation:
//!
//! - [`drain_with_timeout`] wraps the whole drain in a ceiling so a stuck
//!   handler can't keep the process alive forever.
//! - [`Signal::escalation_exit_code`] maps a *second* signal (the operator
//!   pressing Ctrl-C again, or a second `SIGTERM` from an impatient init)
//!   to the conventional `128 + signum` exit code used when the daemon
//!   aborts the drain and exits immediately.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Process-wide count of in-flight per-connection handler tasks across every
/// accept loop (the three RPC servers and the admin server).
///
/// A single `Arc<InFlight>` is shared by all four servers (constructed in
/// `main.rs`): each bumps the counter just before spawning a connection
/// handler and decrements it when that handler finishes (via [`InFlightGuard`],
/// which decrements on drop so a panicking handler still settles the count).
/// At drain-timeout time [`drain_with_timeout`] reads the aggregate so the
/// `warn!` can report how many handlers were still running when the ceiling
/// fired — a bare "drain timed out" is far less actionable than "drain timed
/// out with 4 requests still in flight". (Fix 9)
#[derive(Debug, Default)]
pub struct InFlight {
    count: AtomicUsize,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the count for a newly spawned handler. Prefer [`InFlightGuard`]
    /// over calling this directly so the matching [`InFlight::dec`] can't be
    /// missed on an early return or panic.
    pub fn inc(&self) {
        self.count.fetch_add(1, Relaxed);
    }

    /// Drop the count for a finished handler.
    pub fn dec(&self) {
        self.count.fetch_sub(1, Relaxed);
    }

    /// Current number of in-flight handlers.
    pub fn load(&self) -> usize {
        self.count.load(Relaxed)
    }
}

/// RAII guard: increments the [`InFlight`] count on construction and
/// decrements it on drop. Move one into each per-connection handler task so
/// the count settles whether the handler returns normally, returns early via
/// `?`, or panics (drop runs on unwind).
pub struct InFlightGuard(Arc<InFlight>);

impl InFlightGuard {
    pub fn new(counter: Arc<InFlight>) -> Self {
        counter.inc();
        Self(counter)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// Which Unix signal triggered (or escalated) the shutdown.
///
/// Kept deliberately tiny — it exists so the exit-code mapping and the
/// second-signal escalation policy can be unit-tested without installing a
/// real signal handler (which is process-global and racy under `cargo
/// test`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGINT` — typically Ctrl-C at an interactive terminal.
    Interrupt,
    /// `SIGTERM` — typically `kill` / an init system asking us to stop.
    Terminate,
}

impl Signal {
    /// Conventional process exit code for a *second-signal escalation*:
    /// `128 + signum`. A daemon that receives a second `SIGINT`/`SIGTERM`
    /// while already draining abandons the drain and exits immediately with
    /// this code, mirroring what the shell reports for a process killed by
    /// the signal:
    ///
    /// - `SIGINT`  (signum 2)  → `130`
    /// - `SIGTERM` (signum 15) → `143`
    pub fn escalation_exit_code(self) -> i32 {
        match self {
            Signal::Interrupt => 130,
            Signal::Terminate => 143,
        }
    }
}

/// Outcome of [`drain_with_timeout`]: either every server task finished
/// within the ceiling, or the ceiling fired first. Both carry the wall-clock
/// time actually spent so the caller can log a single "shutdown complete in
/// Xms" line.
#[derive(Debug)]
pub enum DrainOutcome {
    /// All in-flight work drained before the timeout. Carries elapsed time.
    Completed(Duration),
    /// The timeout fired with work still in flight. Carries elapsed time
    /// (≈ the timeout). The daemon exits anyway, with status 0 — a slow
    /// drain is not a failure.
    TimedOut(Duration),
}

/// Await `drain` (the combined join of every server's accept-loop task),
/// bounded by `timeout`.
///
/// `timeout` is a *ceiling*, not a target: if `drain` completes in 5ms this
/// returns in 5ms. Only a handler that refuses to finish makes the daemon
/// wait the full window. On completion this logs at `info`; on timeout it
/// logs at `warn` (and the caller still exits 0). The boolean-ish
/// [`DrainOutcome`] is returned rather than relying on the log so callers and
/// tests have a structured result to branch on.
pub async fn drain_with_timeout(
    drain: impl Future<Output = ()>,
    timeout: Duration,
    in_flight: &InFlight,
) -> DrainOutcome {
    let started = Instant::now();
    match tokio::time::timeout(timeout, drain).await {
        Ok(()) => {
            let elapsed = started.elapsed();
            tracing::info!(ms = elapsed.as_millis(), "drain complete");
            DrainOutcome::Completed(elapsed)
        }
        Err(_) => {
            let elapsed = started.elapsed();
            // Report the aggregate in-flight handler count across all four
            // servers so the operator sees *how much* work was abandoned, not
            // just that the ceiling fired. (Fix 9)
            tracing::warn!(
                ms = elapsed.as_millis(),
                timeout_secs = timeout.as_secs(),
                in_flight = in_flight.load(),
                "drain timeout exceeded, exiting with in-flight requests still pending",
            );
            DrainOutcome::TimedOut(elapsed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalation_exit_codes_follow_128_plus_signum() {
        // The full second-signal path calls `std::process::exit`, which we
        // can't exercise in-process without forking. Instead we unit-test
        // the pure decision the handler makes: which exit code a second
        // signal maps to. SIGINT=2 → 130, SIGTERM=15 → 143.
        assert_eq!(Signal::Interrupt.escalation_exit_code(), 130);
        assert_eq!(Signal::Terminate.escalation_exit_code(), 143);
    }

    #[test]
    fn in_flight_guard_increments_then_decrements_on_drop() {
        // The guard is what keeps the aggregate count honest across early
        // returns and panics: construction bumps, drop settles. (Fix 9)
        let counter = Arc::new(InFlight::new());
        assert_eq!(counter.load(), 0);
        {
            let _g1 = InFlightGuard::new(counter.clone());
            let _g2 = InFlightGuard::new(counter.clone());
            assert_eq!(counter.load(), 2, "two live guards → count 2");
        }
        assert_eq!(counter.load(), 0, "dropping both guards settles the count");
    }

    #[tokio::test]
    async fn drain_with_timeout_completes_fast_when_work_finishes() {
        // A drain that finishes immediately must return `Completed` well
        // under the (generous) ceiling, proving the timeout is a ceiling and
        // not a fixed wait.
        let in_flight = InFlight::new();
        let outcome =
            drain_with_timeout(std::future::ready(()), Duration::from_secs(30), &in_flight).await;
        match outcome {
            DrainOutcome::Completed(elapsed) => {
                assert!(
                    elapsed < Duration::from_secs(1),
                    "ready future must drain near-instantly; took {elapsed:?}",
                );
            }
            DrainOutcome::TimedOut(_) => panic!("a ready drain must not time out"),
        }
    }

    #[tokio::test]
    async fn drain_with_timeout_fires_when_handler_is_stuck() {
        // Model a handler that sleeps far longer than the ceiling. The drain
        // must give up at ~the timeout, not wait for the handler.
        let in_flight = InFlight::new();
        let start = Instant::now();
        let outcome = drain_with_timeout(
            tokio::time::sleep(Duration::from_secs(60)),
            Duration::from_secs(1),
            &in_flight,
        )
        .await;
        let elapsed = start.elapsed();
        match outcome {
            DrainOutcome::TimedOut(_) => {}
            DrainOutcome::Completed(_) => panic!("a 60s handler must trip the 1s ceiling"),
        }
        assert!(
            elapsed < Duration::from_millis(1500),
            "drain must abort at ~the 1s ceiling, not wait for the 60s handler; took {elapsed:?}",
        );
    }
}
