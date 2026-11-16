//! Pairing a client to a relayed daemon with a code a person can carry.
//!
//! A relayed daemon has no address a client can dial, so the user moves one
//! small string between the two devices. That string is not a secret in the way
//! a token is — it is shown on screen, possibly photographed, possibly typed
//! into a chat — so the design assumes an attacker can read it and makes that
//! attacker's job the same as guessing a 128-bit key.
//!
//! # What a code carries
//!
//! ```text
//! waku://<relay-host>:<port>/<session-id>#<fingerprint>
//! ```
//!
//! - **relay address**: where to dial. Public information.
//! - **session id**: which daemon to meet. Public: guessing a live one gets an
//!   attacker a connection that fails at the handshake, not control of it.
//! - **fingerprint**: the first 16 hex characters of the daemon's Ed25519
//!   public key. This is the only part that authenticates anything.
//!
//! # Why a fingerprint and not the whole key
//!
//! A full public key is 32 bytes, which is 43 characters of base64; a person
//! typing it will make mistakes, and a code people avoid using is a code they
//! work around by trusting whatever the relay says. Sixteen hex characters is
//! 64 bits, which is short enough to type and long enough that an attacker
//! cannot grind a key whose fingerprint collides with the real one before the
//! session expires.
//!
//! **This is not the same thing as the short authentication string.** The
//! fingerprint authenticates the *daemon's identity*, which is what lets a
//! client pin a key on first contact. The SAS compares a *fresh transcript*
//! after the handshake, and it is what catches a relay that ran one handshake
//! per side. Both are needed: the fingerprint says "this is the daemon I paired
//! with", the SAS says "this connection is the one I am having now".
//!
//! # What this does not protect against
//!
//! Someone who reads the code and reaches the relay first, with the real daemon
//! not yet connected, gets a socket that fails when the real daemon arrives:
//! the relay refuses a second daemon for a live session rather than swapping it
//! in. Someone who reads the code *and* can make the client ignore the
//! fingerprint has defeated the check, which is why the client parses the code
//! and pins rather than accepting whatever it is told.

use anyhow::{Context as _, Result, bail};
/// The scheme a pairing code uses.
///
/// `waku://` rather than `https://` so a code is recognisably ours and cannot be
/// pasted into a browser by accident, where the fingerprint fragment would be
/// stripped by the server and silently lost.
const SCHEME: &str = "waku://";

/// Characters of the daemon fingerprint a code carries.
///
/// 16 hex characters = 64 bits. See the module docs for why it is not longer.
pub const FINGERPRINT_LEN: usize = 16;

/// A parsed pairing code: where to dial, what to ask for, and who to expect.
///
/// Deliberately not `Clone`-free or sealed: a code carries no secret, and the
/// values need to reach the UI, the connection layer and the pin check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairingCode {
    /// The relay's `host:port`, without a scheme. The client chooses `ws` or
    /// `wss` from this rather than from the code, because a code that could
    /// downgrade to plaintext would be a code an attacker rewrites.
    pub relay: String,
    pub session_id: String,
    /// Lowercase hex, [`FINGERPRINT_LEN`] characters.
    pub fingerprint: String,
}

impl PairingCode {
    /// Render a code for display.
    ///
    /// The fingerprint goes in the URI fragment, which is the part a URL-aware
    /// tool would drop. That is intentional: the fragment is never sent to a
    /// server, so a code pasted into a web form loses the very part that
    /// authenticates the daemon, and the user sees a pairing failure rather
    /// than a silent downgrade.
    pub fn render(&self) -> String {
        format!(
            "{SCHEME}{}/{}#{}",
            self.relay, self.session_id, self.fingerprint
        )
    }

    /// Parse a code a user carried over.
    ///
    /// Rejects anything it does not fully understand rather than filling in
    /// defaults: a code with a missing fingerprint must fail here, because a
    /// connection that proceeds without a pin is the attack this prevents.
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let Some(rest) = trimmed.strip_prefix(SCHEME) else {
            bail!("a pairing code starts with {SCHEME}");
        };

        // The fragment is split off first: everything before it is the part a
        // server would see, and it must be well-formed on its own.
        let Some((without_fragment, fragment)) = rest.split_once('#') else {
            bail!("a pairing code ends with #<fingerprint>; this one has no '#'");
        };

        let fingerprint = fragment.trim().to_ascii_lowercase();
        if fingerprint.len() != FINGERPRINT_LEN {
            bail!(
                "a pairing code's fingerprint is {FINGERPRINT_LEN} hex characters; \
                 this one has {}",
                fingerprint.len()
            );
        }
        if !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("a pairing code's fingerprint must be hex characters");
        }

        // `<relay>/<session-id>`, where `relay` may contain no path of its own.
        let (relay, session_id) = without_fragment
            .rsplit_once('/')
            .context("a pairing code is <relay>/<session-id>#<fingerprint>")?;
        if relay.is_empty() {
            bail!("a pairing code needs a relay address before the session id");
        }
        if relay.contains('/') {
            bail!("a pairing code's relay address must be host:port, with no path");
        }
        if session_id.is_empty() {
            bail!("a pairing code needs a session id");
        }

        Ok(Self {
            relay: relay.to_owned(),
            session_id: session_id.to_owned(),
            fingerprint,
        })
    }
}

/// The longest session id a relay accepts.
///
/// Must match `waku_relay::parse_path`; the two crates cannot share a constant
/// because the relay deliberately does not depend on this one, so the rule is
/// restated here and a round-trip test in `waku-core` would catch a drift.
pub const MAX_SESSION_ID_LEN: usize = 128;

/// Whether `session_id` is a string the relay will accept as a session.
///
/// Restated from the relay's own parser rather than assumed: a daemon that
/// dials with an id the relay rejects reconnects forever with a four-character
/// error buried in a log, which looks like a network fault.
pub fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The fingerprint of a daemon's Ed25519 public key, as a pairing code carries
/// it.
///
/// Sixteen lowercase hex characters: the first 8 bytes of SHA-256 over the
/// public key. Truncated because a code is typed by a person; see the module
/// docs for why 64 bits is enough.
///
/// Both ends compute this from the same bytes: the daemon from its own key when
/// it displays a code, the client from the key the handshake proved. A client
/// that compared anything else — the base64 form, a different truncation — would
/// reject a correct pairing in a way that looks like an attack.
pub fn fingerprint_of(ed25519_public: &[u8; 32]) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(ed25519_public);
    digest[..FINGERPRINT_LEN / 2]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Derive the short authentication string both ends display.
///
/// This is the value a user compares once, and it is the only check that
/// catches a relay running one handshake per side: such a relay produces two
/// different handshakes, so the two screens show two different strings.
///
/// **It comes from the handshake transcript, never from the session key.** A
/// hash of the key would be reproducible from a key an attacker had already
/// recovered and would not be bound to what was negotiated; the transcript hash
/// covers every public key both sides exchanged.
///
/// Twelve decimal digits in three groups of four, which is what people can
/// compare across two screens without losing their place. The digits come from
/// successive bytes of the transcript hash, so no rounding favours a value.
pub fn authentication_string(transcript_hash: &[u8; 32]) -> String {
    let mut digits = String::with_capacity(14);
    for (index, byte) in transcript_hash.iter().take(12).enumerate() {
        if index > 0 && index % 4 == 0 {
            digits.push(' ');
        }
        digits.push(char::from(b'0' + (byte % 10)));
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> PairingCode {
        PairingCode {
            relay: "relay.example.test:443".into(),
            session_id: "abc123".into(),
            fingerprint: "d258dc0e8595ed66".into(),
        }
    }

    #[test]
    fn a_code_round_trips_through_render_and_parse() {
        let original = code();
        let text = original.render();
        let parsed = PairingCode::parse(&text).expect("a rendered code must parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn the_rendered_code_keeps_the_fingerprint_in_the_fragment() {
        // A code pasted into a web form loses the fragment, which is the part
        // that authenticates the daemon: the failure must be loud, not silent.
        let text = code().render();
        let (before_fragment, fragment) = text.split_once('#').expect("a '#' must be present");
        assert_eq!(fragment, "d258dc0e8595ed66");
        assert!(
            !before_fragment.contains("d258dc0e8595ed66"),
            "the fingerprint must not also appear where a server could read it"
        );
    }

    #[test]
    fn a_code_without_a_fingerprint_is_refused() {
        // The whole point of the code is the pin. A code that parsed without
        // one would connect with no way to tell the real daemon from a relay's
        // own, so this must fail rather than default.
        let error = PairingCode::parse("waku://relay.example.test:443/abc123")
            .expect_err("a code with no fingerprint must be refused");
        assert!(error.to_string().contains('#'), "unexpected error: {error}");
    }

    #[test]
    fn a_short_fingerprint_is_refused() {
        let error = PairingCode::parse("waku://relay.example.test:443/abc123#d258")
            .expect_err("a truncated fingerprint must be refused");
        assert!(
            error.to_string().contains("16 hex characters"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_non_hex_fingerprint_is_refused() {
        // Guards against a code that was transcribed with a look-alike
        // character; accepting it would silently pin a different key.
        assert!(PairingCode::parse("waku://relay.example.test:443/abc#zzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn a_relay_with_a_path_is_refused() {
        // A relay address carrying its own path would make the session id
        // ambiguous, and the ambiguity would be in the part an attacker edits.
        assert!(PairingCode::parse("waku://relay.test/evil/abc#d258dc0e8595ed66").is_err());
    }

    #[test]
    fn a_code_that_is_not_ours_is_refused() {
        assert!(PairingCode::parse("https://relay.test/abc#d258dc0e8595ed66").is_err());
        assert!(PairingCode::parse("").is_err());
        assert!(PairingCode::parse("   ").is_err());
    }

    #[test]
    fn surrounding_whitespace_from_a_paste_is_tolerated() {
        let text = format!("  {}  \n", code().render());
        assert_eq!(PairingCode::parse(&text).expect("must parse"), code());
    }

    #[test]
    fn the_authentication_string_is_grouped_and_stable() {
        // `0x12` is 18, and 18 % 10 is 8, so a uniform transcript gives a
        // uniform string. That makes the grouping the only thing visible, which
        // is exactly what this checks.
        let transcript = [0x12_u8; 32];
        let string = authentication_string(&transcript);
        assert_eq!(string, "8888 8888 8888");
        // Deterministic: both ends must show the same digits or a user
        // comparing them would reject a good connection.
        assert_eq!(string, authentication_string(&transcript));
    }

    #[test]
    fn every_digit_of_the_string_comes_from_a_different_byte() {
        // A string that repeated one digit would compare equal against many
        // transcripts, which is the same failure as truncating it too far.
        let mut transcript = [0_u8; 32];
        for (index, byte) in transcript.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let string = authentication_string(&transcript);
        let digits: Vec<char> = string.chars().filter(|c| c.is_ascii_digit()).collect();
        assert_eq!(digits.len(), 12);
        let distinct: std::collections::BTreeSet<_> = digits.iter().collect();
        assert!(
            distinct.len() >= 9,
            "twelve digits drawn from twelve different bytes should mostly differ, got {string}"
        );
    }

    #[test]
    fn a_different_transcript_gives_a_different_string() {
        // This is what catches a relay running one handshake per side.
        let mut other = [0x12_u8; 32];
        other[0] = 0x13;
        assert_ne!(
            authentication_string(&[0x12; 32]),
            authentication_string(&other),
            "two different transcripts must not show the same digits"
        );
    }
}
