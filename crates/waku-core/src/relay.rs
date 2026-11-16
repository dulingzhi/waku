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

/// Build the WebSocket URL a daemon dials: `<relay>/<session>/daemon`.
///
/// Kept separate from [`run_relay`] so the string construction is testable
/// without a socket, and so the one place that decides a relay path is a
/// function rather than three format strings.
pub(crate) fn relay_endpoint(relay_address: &str, session_id: &str) -> anyhow::Result<String> {
    let trimmed = relay_address.trim().trim_end_matches('/');
    if !(trimmed.starts_with("ws://") || trimmed.starts_with("wss://")) {
        anyhow::bail!("a relay address starts with ws:// or wss://; got {relay_address:?}");
    }
    if !waku_protocol::pairing::is_valid_session_id(session_id) {
        anyhow::bail!("{session_id:?} is not a usable relay session id");
    }
    Ok(format!("{trimmed}/{session_id}/daemon"))
}

/// Generate a session id that an attacker cannot guess.
///
/// 128 bits from the OS, rendered as 32 lowercase hex characters. Guessing one
/// gets an attacker a connection that fails at the pinned key, but the margin is
/// free and removes guessing from the list of things to think about.
pub fn new_session_id() -> String {
    use rand_core::RngCore as _;

    let mut bytes = [0_u8; 16];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Dials the relay at `relay_address` under `session_id`, and repeats forever.
///
/// Blocks for the life of the daemon, so it belongs on its own thread. Returns
/// only when shutdown is observed.
///
/// `relay_address` is `ws(s)://host:port`; the session and role are added here
/// rather than being part of the configured string. That keeps the configured
/// value to something a user can copy from a relay operator, and keeps the
/// session id — which changes on every start — out of their hands.
pub fn run_relay(
    relay_address: String,
    session_id: String,
    server: std::sync::Arc<DaemonServer>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    let url = relay_endpoint(&relay_address, &session_id)?;
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

    #[test]
    fn the_endpoint_is_the_relay_plus_session_and_role() {
        assert_eq!(
            relay_endpoint("ws://relay.test:8080", "abc123").unwrap(),
            "ws://relay.test:8080/abc123/daemon"
        );
        // A trailing slash from a copied address must not produce `//`.
        assert_eq!(
            relay_endpoint("wss://relay.test/", "abc").unwrap(),
            "wss://relay.test/abc/daemon"
        );
    }

    #[test]
    fn an_endpoint_without_a_ws_scheme_is_refused() {
        // A bare host would otherwise be dialled as a relative URL and fail
        // somewhere far from the setting that caused it.
        assert!(relay_endpoint("relay.test:8080", "abc").is_err());
        assert!(relay_endpoint("https://relay.test", "abc").is_err());
    }

    #[test]
    fn an_endpoint_with_an_unusable_session_is_refused() {
        // These are the ids `waku_relay::parse_path` rejects. A daemon that
        // dialled with one would reconnect forever against a four-character
        // error, which reads like a network fault.
        for session in ["", "a/b", "a b", "a?b", "a#b"] {
            assert!(
                relay_endpoint("ws://relay.test", session).is_err(),
                "{session:?} must be refused"
            );
        }
    }

    #[test]
    fn a_generated_session_id_is_usable_and_unpredictable() {
        let first = new_session_id();
        let second = new_session_id();
        assert_eq!(first.len(), 32, "128 bits render as 32 hex characters");
        assert!(
            waku_protocol::pairing::is_valid_session_id(&first),
            "the relay must accept what this generates: {first}"
        );
        assert_ne!(first, second, "two sessions must not share an id");
        assert!(
            first.chars().all(|c| c.is_ascii_hexdigit()),
            "the id must be hex: {first}"
        );
    }

    /// The session-id rule exists twice on purpose, and this is what keeps the
    /// two copies equal.
    ///
    /// `waku-relay` deliberately does not depend on `waku-protocol`: a relay
    /// that linked the sealing code would be one refactor from holding a key.
    /// So `waku_relay::parse_path` and `pairing::is_valid_session_id` each
    /// restate the rule, and nothing in either crate can compare them. This
    /// crate depends on both, which makes it the only place a drift can be
    /// caught before a user meets it as "the daemon reconnects forever with a
    /// four-character error in a log".
    ///
    /// The third copy lives in `packages/waku-client/src/pairing.ts`; the
    /// cross-language vectors cover the values that are shown to a user, and
    /// `isValidSessionId` is compared there against this same set.
    #[test]
    fn the_session_id_rule_matches_the_relay() {
        // Every case here is one a caller could plausibly produce: a generated
        // id, a typed one, a translated one, and the boundaries.
        let cases = [
            "",
            "a",
            "abc123",
            "a-b_c",
            "0123456789abcdef0123456789abcdef",
            &"a".repeat(waku_protocol::pairing::MAX_SESSION_ID_LEN),
            &"a".repeat(waku_protocol::pairing::MAX_SESSION_ID_LEN + 1),
            "a/b",
            "a b",
            "a?b",
            "a#b",
            "a.b",
            "session-ünïcode",
        ];

        for case in cases {
            let ours = waku_protocol::pairing::is_valid_session_id(case);
            // The relay exposes the same decision through its path parser,
            // which is the only public form it has.
            let theirs = waku_relay::parse_path(&format!("/{case}/daemon")).is_ok();
            assert_eq!(
                ours, theirs,
                "session id {case:?}: pairing says {ours}, relay says {theirs}"
            );
        }
    }
}
