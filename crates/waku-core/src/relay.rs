//! Outbound relay connection.
//!
//! A daemon behind a NAT cannot be reached by dialing it, so it dials the relay
//! instead and serves that connection like any other. Everything about the
//! connection itself — the sealed handshake, the token check, the message loop —
//! is [`crate::server::DaemonServer::serve_dialed`], which the listener also
//! uses. This module only decides *when* to dial and how long to wait after a
//! failure; it must never grow a second copy of the connection handling, or the
//! two directions would drift apart on encryption and limits.
//!
//! Nothing here is a tunnel: the relay forwards a WebSocket stream, and the
//! daemon speaks its normal protocol over it.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tungstenite::client::IntoClientRequest as _;

use crate::server::DaemonServer;

/// The first wait after a failed dial. Short enough that a relay restart
/// reconnects promptly, long enough that a tight failure loop cannot hammer it.
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// The longest wait between attempts. A relay that stays down overnight costs
/// one dial per interval rather than a stream of them.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How much each consecutive failure multiplies the wait by.
const BACKOFF_MULTIPLIER: u32 = 2;

/// One dial attempt's outcome, separated from the retry loop so the backoff
/// arithmetic is testable without a network.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DialOutcome {
    /// The connection was established and has since ended. The next attempt
    /// keeps the backoff. A relay that accepts and immediately drops a
    /// connection is still failing, so resetting here would spin.
    Served,
    /// The dial itself failed: DNS, TCP, TLS, or the WebSocket upgrade.
    Failed,
}

/// The wait before the next attempt.
///
/// `consecutive_failures` counts attempts that did not produce a served
/// connection. Zero means "no failures yet", which is the first attempt's wait.
pub(crate) fn next_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let mut wait = INITIAL_BACKOFF;
    for _ in 1..consecutive_failures {
        wait = wait.saturating_mul(BACKOFF_MULTIPLIER);
        if wait >= MAX_BACKOFF {
            return MAX_BACKOFF;
        }
    }
    wait.min(MAX_BACKOFF)
}

/// Dials `url`, hands the connection to the shared server, and repeats forever.
///
/// Blocks for the life of the daemon, so it belongs on its own thread. Returns
/// only when shutdown is observed.
pub fn run_relay(
    url: String,
    server: std::sync::Arc<DaemonServer>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    // Validate once, up front: a typo in the relay URL should fail at startup
    // with a clear message, not as an endless reconnect loop that looks like a
    // network problem.
    let request = url
        .clone()
        .into_client_request()
        .with_context(|| format!("the relay URL {url:?} is not a valid WebSocket URL"))?;
    waku_protocol::diagnostics::log(format!("waku-daemon relay: dialing {url}"));

    let mut consecutive_failures: u32 = 0;
    while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
        let wait = next_backoff(consecutive_failures);
        if !wait.is_zero() {
            waku_protocol::diagnostics::log(format!(
                "waku-daemon relay: waiting {} ms before retry ({} consecutive failures)",
                wait.as_millis(),
                consecutive_failures
            ));
            // Sleep in slices so a shutdown during a 30 s wait is not delayed
            // by 30 s.
            if sleep_until_shutdown(wait, &shutdown) {
                break;
            }
        }

        let outcome = dial_once(&url, request.clone(), &server, &shutdown);
        match outcome {
            DialOutcome::Served => {
                waku_protocol::diagnostics::log("waku-daemon relay: connection closed by the peer");
                // The connection came up, so the next failure starts its wait
                // from the bottom again rather than from the ceiling.
                consecutive_failures = 1;
            }
            DialOutcome::Failed => {
                consecutive_failures = consecutive_failures.saturating_add(1);
            }
        }
    }
    Ok(())
}

/// Sleeps for `duration`, returning `true` if shutdown interrupted it.
fn sleep_until_shutdown(
    duration: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    const SLICE: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + duration;
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(SLICE.min(deadline - now));
    }
}

/// One dial attempt. Blocks while the connection is served.
fn dial_once(
    url: &str,
    request: tungstenite::handshake::client::Request,
    server: &std::sync::Arc<DaemonServer>,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> DialOutcome {
    let Some(permit) = server.claim_dialed_connection() else {
        waku_protocol::diagnostics::log("waku-daemon relay: at the connection limit, not dialing");
        return DialOutcome::Failed;
    };

    let connector = match tungstenite::connect(request) {
        Ok((socket, _response)) => {
            waku_protocol::diagnostics::log(format!("waku-daemon relay: connected to {url}"));
            socket
        }
        Err(error) => {
            // The error names the stage (DNS, TLS, HTTP status) and never
            // carries the token, which is sent later inside the sealed record.
            waku_protocol::diagnostics::log(format!(
                "waku-daemon relay: dial to {url} failed: {error}"
            ));
            return DialOutcome::Failed;
        }
    };

    match server.serve_dialed(connector, shutdown.clone(), permit) {
        Ok(()) => DialOutcome::Served,
        Err(error) => {
            waku_protocol::diagnostics::log(format!(
                "waku-daemon relay: could not serve the connection: {error:#}"
            ));
            DialOutcome::Served
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_attempt_does_not_wait() {
        assert_eq!(next_backoff(0), Duration::ZERO);
    }

    #[test]
    fn consecutive_failures_back_off_exponentially() {
        assert_eq!(next_backoff(1), Duration::from_millis(500));
        assert_eq!(next_backoff(2), Duration::from_secs(1));
        assert_eq!(next_backoff(3), Duration::from_secs(2));
        assert_eq!(next_backoff(4), Duration::from_secs(4));
        assert_eq!(next_backoff(5), Duration::from_secs(8));
        assert_eq!(next_backoff(6), Duration::from_secs(16));
        // Capped from here on rather than continuing to double.
        assert_eq!(next_backoff(7), MAX_BACKOFF);
        assert_eq!(next_backoff(8), MAX_BACKOFF);
        assert_eq!(next_backoff(1_000), MAX_BACKOFF);
    }

    /// The backoff is what keeps a dead relay from being hammered. A fixed
    /// retry would make every value here equal, which is the regression this
    /// pins: the sequence must strictly grow until it saturates.
    #[test]
    fn the_wait_strictly_grows_until_it_saturates() {
        let mut previous = next_backoff(1);
        let mut failures = 2;
        // Walk until the sequence reaches the cap. Bounding the loop by the
        // number of attempts instead of by saturation would stop at 16s and
        // then assert against a 30s cap, which is a contradiction rather than a
        // test.
        while previous < MAX_BACKOFF {
            let wait = next_backoff(failures);
            assert!(
                wait > previous,
                "attempt {failures} must wait longer than the previous one"
            );
            previous = wait;
            failures += 1;
        }
        assert_eq!(previous, MAX_BACKOFF);
        // And it stays there rather than wrapping around to a short wait.
        assert_eq!(next_backoff(failures), MAX_BACKOFF);
    }

    #[test]
    fn a_retry_storm_is_impossible() {
        // Twelve consecutive failures at the worst case add up to well over a
        // minute, so a daemon cannot produce more than a couple of dials a
        // second even if the relay accepts and drops instantly.
        let total: Duration = (7..19).map(next_backoff).sum();
        assert!(
            total >= Duration::from_secs(360),
            "twelve capped retries take {total:?}, which is too fast to be safe"
        );
    }
}
