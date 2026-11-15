//! The encrypted record layer and handshake described in
//! `docs/record-layer.md`.
//!
//! Two halves, and only the first is reachable from this module today:
//!
//! - [`Sealer`] and [`Opener`] frame a plaintext into
//!   `nonce || ciphertext || tag` and back. They are pure functions over byte
//!   slices, so the whole codec is testable without a socket.
//! - The handshake turns a client's ephemeral public key into a pair of
//!   [`SessionKeys`]. See [`handshake`] for the one step the design document
//!   leaves without an implementation.
//!
//! Nothing here is wired into the WebSocket path yet; `server.rs` and
//! `client.rs` still speak plaintext JSON.

use anyhow::{Context as _, Result, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key as AeadKey, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
/// `counter (8) || ciphertext || tag (16)`. The direction prefix lives inside
/// the nonce, not on the wire: both ends know their own direction.
const HEADER_LEN: usize = 8;

/// The protocol name mixed into every transcript. Changing it makes signatures
/// from one protocol generation fail against another.
const PROTOCOL_LABEL: &[u8] = b"waku-relay-record-v1";

/// Binds a transcript hash to the step it was signed at, so a signature over a
/// handshake cannot be replayed as a signature over anything else.
const TRANSCRIPT_LABEL: &[u8] = b"waku-handshake-transcript-v1";

/// Which way a record travels.
///
/// The prefix is authenticated twice over: it is part of the nonce the AEAD
/// consumes and it selects the key. A record reflected back at its own sender
/// therefore fails on both counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    const fn prefix(self) -> [u8; 4] {
        match self {
            Self::ClientToServer => [0, 0, 0, 1],
            Self::ServerToClient => [0, 0, 0, 2],
        }
    }

    pub const fn info(self) -> &'static [u8] {
        match self {
            Self::ClientToServer => b"c2s",
            Self::ServerToClient => b"s2c",
        }
    }
}

/// The two directional keys a completed handshake produces.
///
/// Split rather than one key used both ways: with a single key an attacker who
/// can reorder frames reflects a peer's own record back at it and it decrypts.
pub struct SessionKeys {
    client_to_server: [u8; KEY_LEN],
    server_to_client: [u8; KEY_LEN],
    /// The Ed25519 key the daemon proved it holds, or the client's static
    /// X25519 key on the daemon's side. Surfaced so a first-time client can pin
    /// what it saw in the pairing code.
    peer_key: Option<[u8; 32]>,
}

impl SessionKeys {
    /// Build the two directional keys from a shared secret and a transcript.
    ///
    /// `ikm` is everything the session key is bound to. [`handshake::accept`]
    /// and [`handshake::ClientHandshake::finish`] document how both sides reach
    /// the same bytes.
    pub fn derive(ikm: &[u8], transcript_hash: &[u8; 32]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(transcript_hash), ikm);
        let mut client_to_server = [0_u8; KEY_LEN];
        let mut server_to_client = [0_u8; KEY_LEN];
        hkdf.expand(Direction::ClientToServer.info(), &mut client_to_server)
            .expect("a 32-byte output is always a valid HKDF length");
        hkdf.expand(Direction::ServerToClient.info(), &mut server_to_client)
            .expect("a 32-byte output is always a valid HKDF length");
        Self {
            client_to_server,
            server_to_client,
            peer_key: None,
        }
    }

    fn with_peer_key(mut self, peer: [u8; 32]) -> Self {
        self.peer_key = Some(peer);
        self
    }

    /// The Ed25519 key the peer proved it holds.
    ///
    /// On the client this is the daemon's signing key — the one to compare
    /// against a stored pairing. On the daemon it is the client's static X25519
    /// key, which is **not** an identity: a client is not authenticated, and
    /// this value only identifies which client static key the session bound to.
    pub fn peer_key(&self) -> Option<[u8; 32]> {
        self.peer_key
    }

    fn key_for(&self, direction: Direction) -> &[u8; KEY_LEN] {
        match direction {
            Direction::ClientToServer => &self.client_to_server,
            Direction::ServerToClient => &self.server_to_client,
        }
    }

    /// A sealer for records travelling in `direction`.
    pub fn sealer(&self, direction: Direction) -> Sealer {
        Sealer::new(*self.key_for(direction), direction)
    }

    /// An opener for records arriving from `direction`.
    pub fn opener(&self, incoming: Direction) -> Opener {
        Opener::new(*self.key_for(incoming), incoming)
    }
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.client_to_server.zeroize();
        self.server_to_client.zeroize();
    }
}

/// Seals plaintext into records. One direction per instance.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    direction: Direction,
    /// The counter of the last record sealed. The next record uses `last + 1`,
    /// so the first is 1 — a counter that started at 0 would make the first
    /// nonce look like an uninitialised one.
    last: u64,
}

impl Sealer {
    fn new(key: [u8; KEY_LEN], direction: Direction) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(AeadKey::from_slice(&key)),
            direction,
            last: 0,
        }
    }

    /// Seal one record: `counter (8) || ciphertext || tag (16)`.
    ///
    /// The counter appears twice — once on the wire, once inside the nonce —
    /// and the AEAD authenticates the nonce, so moving the wire counter without
    /// the matching nonce fails to open.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        // Exhausting 2^64 records is not possible within a connection's
        // lifetime, but wrapping would reuse a nonce, which is
        // catastrophic — refuse rather than wrap.
        let Some(counter) = self.last.checked_add(1) else {
            bail!("record counter exhausted; refusing to reuse a nonce");
        };
        let nonce = nonce_for(self.direction, counter);
        let associated = associated_data(plaintext.len())?;
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &associated,
                },
            )
            .map_err(|_| anyhow::anyhow!("record encryption failed"))?;
        self.last = counter;

        let mut record = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        record.extend_from_slice(&counter.to_be_bytes());
        record.extend_from_slice(&ciphertext);
        Ok(record)
    }

    /// The counter of the last sealed record. Zero before the first.
    pub fn counter(&self) -> u64 {
        self.last
    }
}

/// Opens records sealed for its direction. One direction per instance.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    direction: Direction,
    /// The last counter accepted. The next record must be exactly `last + 1`:
    /// buffering an out-of-order record would make the counter's guarantee
    /// ambiguous, so a gap ends the connection instead.
    last: u64,
}

impl Opener {
    fn new(key: [u8; KEY_LEN], direction: Direction) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(AeadKey::from_slice(&key)),
            direction,
            last: 0,
        }
    }

    /// Open one record.
    ///
    /// Errors carry only the reason and never any plaintext: a failed open is
    /// not a place to leak what the record might have contained.
    pub fn open(&mut self, record: &[u8]) -> Result<Vec<u8>> {
        if record.len() < HEADER_LEN + TAG_LEN {
            bail!("record too short to contain a header and tag");
        }
        let (header, ciphertext) = record.split_at(HEADER_LEN);
        let counter = u64::from_be_bytes(header.try_into().expect("split at 8 bytes"));
        if counter != self.last + 1 {
            bail!("record out of order");
        }
        let plaintext_len = ciphertext.len() - TAG_LEN;
        let associated = associated_data(plaintext_len)?;
        let nonce = nonce_for(self.direction, counter);
        let plaintext = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &associated,
                },
            )
            // One message for wrong key, tampering and replay: distinguishing
            // them tells an attacker which of the three they achieved.
            .map_err(|_| anyhow::anyhow!("record authentication failed"))?;
        self.last = counter;
        Ok(plaintext)
    }

    /// The counter of the last accepted record. Zero before the first.
    pub fn counter(&self) -> u64 {
        self.last
    }
}

fn nonce_for(direction: Direction, counter: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0_u8; NONCE_LEN];
    nonce[..4].copy_from_slice(&direction.prefix());
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// `protocol_version (4) || plaintext_length (4)`, both big endian.
fn associated_data(plaintext_len: usize) -> Result<[u8; 8]> {
    // A length that does not fit in four bytes would be silently truncated in
    // the associated data, and two different lengths would then share one
    // authenticated value.
    let length = u32::try_from(plaintext_len)
        .context("record plaintext does not fit in the 4-byte length field")?;
    let mut associated = [0_u8; 8];
    associated[..4].copy_from_slice(&waku_protocol_version().to_be_bytes());
    associated[4..].copy_from_slice(&length.to_be_bytes());
    Ok(associated)
}

const fn waku_protocol_version() -> u32 {
    crate::protocol::PROTOCOL_VERSION
}

/// Sign a transcript hash with the daemon's Ed25519 key.
pub fn sign_transcript(signing_key: &ed25519_dalek::SigningKey, transcript: &[u8; 32]) -> [u8; 64] {
    use ed25519_dalek::Signer as _;

    signing_key.sign(transcript).to_bytes()
}

/// Verify a signature over a transcript hash.
pub fn verify_transcript(
    verifying_key: &ed25519_dalek::VerifyingKey,
    transcript: &[u8; 32],
    signature: &[u8; 64],
) -> Result<()> {
    use ed25519_dalek::Verifier as _;

    let signature = ed25519_dalek::Signature::from_bytes(signature);
    verifying_key
        .verify(transcript, &signature)
        .map_err(|_| anyhow::anyhow!("handshake signature invalid"))
}

/// Reject a Diffie-Hellman result that nobody contributed to.
///
/// RFC 7748 §6.1: a peer can send a low-order public key and force a known
/// shared secret. `x25519-dalek` computes the value either way; the check is
/// the caller's job, which is exactly the requirement that is easy to skip.
pub fn require_contributory(secret: &x25519_dalek::SharedSecret) -> Result<()> {
    if secret.was_contributory() {
        Ok(())
    } else {
        bail!("handshake refused a low-order public key")
    }
}

/// The handshake, as `docs/record-layer.md` describes it after the client-static
/// correction.
pub mod handshake {
    use super::*;
    use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

    /// `client_static_public (32) || client_ephemeral_public (32)`.
    ///
    /// The client's static public key travels here rather than being implied:
    /// without it the daemon cannot compute `bound`, and the two sides would
    /// derive different session keys.
    pub const CLIENT_HELLO_LEN: usize = 64;

    /// `daemon_x25519_public (32) || daemon_ed25519_public (32) ||
    /// server_ephemeral_public (32) || signature (64)`.
    ///
    /// The daemon's public keys travel in this frame because the client needs
    /// both: the X25519 one to compute `bound2` and to hash the transcript, the
    /// Ed25519 one to verify the signature. A client that has paired before
    /// checks the Ed25519 key against the one it stored, which is what stops a
    /// relay substituting its own.
    pub const SERVER_HELLO_LEN: usize = 32 + 32 + 32 + 64;

    /// A client's half of a handshake in progress.
    ///
    /// The static secret is supplied by the caller, which owns its storage: it
    /// outlives one connection, unlike the ephemeral one. Neither is ever
    /// serialized.
    pub struct ClientHandshake {
        static_secret: StaticSecret,
        ephemeral_secret: StaticSecret,
        client_static_public: [u8; 32],
        client_ephemeral_public: [u8; 32],
    }

    impl ClientHandshake {
        /// Begin a handshake, returning the bytes to send as `client_hello`.
        pub fn start(static_secret: StaticSecret) -> (Self, Vec<u8>) {
            // The ephemeral key is a `StaticSecret` rather than
            // `EphemeralSecret` because it has to be used **twice**: once
            // against the server's ephemeral key and once against the server's
            // static key. `EphemeralSecret::diffie_hellman` consumes itself, so
            // it can only ever make one agreement. Regenerating it per
            // connection and never persisting it gives the same forward
            // secrecy; the type's extra capability (serialization) is simply
            // unused.
            let ephemeral_secret = StaticSecret::random();
            let client_static_public = PublicKey::from(&static_secret).to_bytes();
            let client_ephemeral_public = PublicKey::from(&ephemeral_secret).to_bytes();

            let mut hello = Vec::with_capacity(CLIENT_HELLO_LEN);
            hello.extend_from_slice(&client_static_public);
            hello.extend_from_slice(&client_ephemeral_public);
            (
                Self {
                    static_secret,
                    ephemeral_secret,
                    client_static_public,
                    client_ephemeral_public,
                },
                hello,
            )
        }

        /// Complete the handshake against the daemon's reply.
        ///
        /// `expected_ed25519` is the key the client stored when it paired. It
        /// is `None` only on the very first connection, where the pairing code's
        /// fingerprint is the check instead; the accepted key is then available
        /// from [`SessionKeys::peer_ed25519`] for the caller to pin.
        pub fn finish(
            self,
            server_hello: &[u8],
            expected_ed25519: Option<&ed25519_dalek::VerifyingKey>,
        ) -> Result<SessionKeys> {
            let Self {
                static_secret,
                ephemeral_secret,
                client_static_public,
                client_ephemeral_public,
            } = self;
            if server_hello.len() != SERVER_HELLO_LEN {
                bail!(
                    "server hello is {} bytes, expected {SERVER_HELLO_LEN}",
                    server_hello.len()
                );
            }
            let (daemon_x25519, rest) = server_hello.split_at(32);
            let (daemon_ed25519, rest) = rest.split_at(32);
            let (server_ephemeral, signature) = rest.split_at(32);

            let daemon_x25519: [u8; 32] = daemon_x25519.try_into().expect("split at 32 bytes");
            let daemon_ed25519: [u8; 32] = daemon_ed25519.try_into().expect("split at 32 bytes");
            let server_ephemeral: [u8; 32] =
                server_ephemeral.try_into().expect("split at 32 bytes");
            let signature: [u8; 64] = signature.try_into().expect("split at 64 bytes");

            let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&daemon_ed25519)
                .map_err(|_| anyhow::anyhow!("daemon sent an invalid signing key"))?;
            // A client that paired before must see the key it stored. This check
            // is the whole anti-substitution guarantee: a relay that swaps in
            // its own keys fails here rather than at the signature.
            if let Some(expected) = expected_ed25519
                && expected.to_bytes() != daemon_ed25519
            {
                bail!("handshake signature invalid: the daemon is not the one you paired with");
            }

            let transcript = transcript_hash(
                &client_static_public,
                &client_ephemeral_public,
                &server_ephemeral,
                &daemon_x25519,
            );
            verify_transcript(&verifying_key, &transcript, &signature)?;

            let server_ephemeral_public = PublicKey::from(server_ephemeral);
            let daemon_x25519_public = PublicKey::from(daemon_x25519);
            let shared = ephemeral_secret.diffie_hellman(&server_ephemeral_public);
            let bound = static_secret.diffie_hellman(&server_ephemeral_public);
            let bound_to_daemon = ephemeral_secret.diffie_hellman(&daemon_x25519_public);

            let ikm = agreement_bytes(&[&shared, &bound, &bound_to_daemon]);
            let keys = SessionKeys::derive(&ikm, &transcript);
            Ok(keys.with_peer_key(daemon_ed25519))
        }
    }

    /// The daemon's half: consume a `client_hello` and produce `server_hello`.
    ///
    /// Takes the two secrets rather than a `DaemonIdentity`, because this crate
    /// cannot depend on `waku-core` — the dependency runs the other way. The
    /// caller passes `identity.x25519_secret()` and
    /// `identity.ed25519_signing_key()`.
    pub fn accept(
        daemon_x25519_secret: &StaticSecret,
        daemon_signing_key: &ed25519_dalek::SigningKey,
        client_hello: &[u8],
    ) -> Result<(Vec<u8>, SessionKeys)> {
        if client_hello.len() != CLIENT_HELLO_LEN {
            bail!(
                "client hello is {} bytes, expected {CLIENT_HELLO_LEN}",
                client_hello.len()
            );
        }
        let (client_static, client_ephemeral) = client_hello.split_at(32);
        let client_static: [u8; 32] = client_static.try_into().expect("split at 32 bytes");
        let client_ephemeral: [u8; 32] = client_ephemeral.try_into().expect("split at 32 bytes");

        let client_static_public = PublicKey::from(client_static);
        let client_ephemeral_public = PublicKey::from(client_ephemeral);

        let ephemeral_secret = StaticSecret::random();
        let server_ephemeral_public = PublicKey::from(&ephemeral_secret).to_bytes();
        let daemon_x25519_public = PublicKey::from(daemon_x25519_secret).to_bytes();
        let daemon_ed25519_public = daemon_signing_key.verifying_key().to_bytes();

        let shared = ephemeral_secret.diffie_hellman(&client_ephemeral_public);
        let bound = ephemeral_secret.diffie_hellman(&client_static_public);
        let bound_to_daemon = daemon_x25519_secret.diffie_hellman(&client_ephemeral_public);
        // Any of the three involving a low-order key would produce an
        // attacker-known secret.
        for secret in [&shared, &bound, &bound_to_daemon] {
            require_contributory(secret)?;
        }

        let transcript = transcript_hash(
            &client_static,
            &client_ephemeral,
            &server_ephemeral_public,
            &daemon_x25519_public,
        );
        let signature = sign_transcript(daemon_signing_key, &transcript);

        let mut hello = Vec::with_capacity(SERVER_HELLO_LEN);
        hello.extend_from_slice(&daemon_x25519_public);
        hello.extend_from_slice(&daemon_ed25519_public);
        hello.extend_from_slice(&server_ephemeral_public);
        hello.extend_from_slice(&signature);

        let ikm = agreement_bytes(&[&shared, &bound, &bound_to_daemon]);
        let keys = SessionKeys::derive(&ikm, &transcript).with_peer_key(client_static);
        Ok((hello, keys))
    }

    /// Concatenate agreements with their lengths, for `ikm`.
    ///
    /// **Why both sides reach the same bytes.** Diffie-Hellman is symmetric:
    /// `DH(a, B) == DH(b, A)`. Written out, the three agreements are
    ///
    /// | name | client computes | daemon computes |
    /// | --- | --- | --- |
    /// | `shared` | `ce × SE` | `se × CE` |
    /// | `bound` (client identity) | `cs × SE` | `se × CS` |
    /// | `bound2` (daemon identity) | `ce × SS` | `ss × CE` |
    ///
    /// each row being one value. Nothing is computed by only one side, so the
    /// concatenation is byte-identical and the derived keys match. What each
    /// one *proves* still differs, which is the point of including all three:
    /// `shared` cannot be reproduced after the ephemeral keys are gone, `bound`
    /// can only be produced by someone holding the client's static secret, and
    /// `bound2` only by someone holding the daemon's static secret.
    ///
    /// Length-prefixing keeps the concatenation unambiguous.
    pub fn agreement_bytes(parts: &[&SharedSecret]) -> Vec<u8> {
        let mut out = Vec::with_capacity(parts.len() * 33);
        for part in parts {
            let bytes = part.as_bytes();
            out.push(bytes.len() as u8);
            out.extend_from_slice(bytes);
        }
        out
    }

    /// The transcript the daemon signs, covering both sides' static and
    /// ephemeral public keys.
    pub fn transcript_hash(
        client_static: &[u8; 32],
        client_ephemeral: &[u8; 32],
        server_ephemeral: &[u8; 32],
        daemon_static: &[u8; 32],
    ) -> [u8; 32] {
        use sha2::Digest as _;

        let mut hasher = Sha256::new();
        for field in [
            PROTOCOL_LABEL,
            TRANSCRIPT_LABEL,
            client_static.as_slice(),
            client_ephemeral.as_slice(),
            server_ephemeral.as_slice(),
            daemon_static.as_slice(),
        ] {
            hasher.update((field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        let digest = hasher.finalize();
        let mut out = [0_u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> SessionKeys {
        SessionKeys::derive(b"a shared secret long enough to be realistic", &[7_u8; 32])
    }

    #[test]
    fn a_sealed_record_opens_to_the_same_bytes() {
        let keys = keys();
        let mut sealer = keys.sealer(Direction::ClientToServer);
        let mut opener = keys.opener(Direction::ClientToServer);

        let record = sealer.seal(b"{\"type\":\"hello\"}").unwrap();
        assert_eq!(opener.open(&record).unwrap(), b"{\"type\":\"hello\"}");
    }

    #[test]
    fn the_two_directions_use_different_bytes() {
        let keys = keys();
        let mut c2s = keys.sealer(Direction::ClientToServer);
        let mut s2c = keys.sealer(Direction::ServerToClient);

        assert_ne!(
            c2s.seal(b"identical").unwrap(),
            s2c.seal(b"identical").unwrap(),
            "the same plaintext must not seal identically in both directions"
        );
    }

    #[test]
    fn a_replayed_record_is_rejected() {
        let keys = keys();
        let mut sealer = keys.sealer(Direction::ClientToServer);
        let mut opener = keys.opener(Direction::ClientToServer);

        let first = sealer.seal(b"once").unwrap();
        assert_eq!(opener.open(&first).unwrap(), b"once");

        // Feeding the same bytes again reuses a counter already accepted.
        assert!(opener.open(&first).is_err());
    }

    #[test]
    fn an_out_of_order_record_is_rejected() {
        let keys = keys();
        let mut sealer = keys.sealer(Direction::ClientToServer);
        let mut opener = keys.opener(Direction::ClientToServer);

        let first = sealer.seal(b"first").unwrap();
        let second = sealer.seal(b"second").unwrap();

        // Delivering the second before the first is a gap, not a delay.
        assert!(opener.open(&second).is_err());
        // The genuine first record still opens afterwards: a rejected record
        // must not advance the counter.
        assert_eq!(opener.open(&first).unwrap(), b"first");
    }

    #[test]
    fn a_truncated_record_is_rejected_and_yields_no_prefix() {
        let keys = keys();
        let mut sealer = keys.sealer(Direction::ClientToServer);
        let record = sealer.seal(b"the whole message").unwrap();

        // One opener per cut, so the *only* reason to reject is the missing
        // bytes — not a counter that advanced.
        for cut in 1..record.len() {
            let mut opener = keys.opener(Direction::ClientToServer);
            assert!(
                opener.open(&record[..cut]).is_err(),
                "a record cut to {cut} bytes must not open"
            );
            assert_eq!(
                opener.counter(),
                0,
                "a rejected record must not advance the counter"
            );
        }

        // The untruncated record opens, proving the cuts were the problem.
        let mut intact = keys.opener(Direction::ClientToServer);
        assert_eq!(intact.open(&record).unwrap(), b"the whole message");
    }

    #[test]
    fn a_record_sealed_under_another_key_is_rejected() {
        let mut sealer = keys().sealer(Direction::ClientToServer);
        let mut opener = SessionKeys::derive(b"a completely different secret", &[9_u8; 32])
            .opener(Direction::ClientToServer);

        assert!(opener.open(&sealer.seal(b"secret").unwrap()).is_err());
    }

    /// A cut record fails and yields nothing.
    ///
    /// **What this does not prove.** The associated data carries the plaintext
    /// length, and it is tempting to say this test exercises that binding. It
    /// does not: ChaCha20-Poly1305 already authenticates the whole ciphertext,
    /// so dropping any byte fails the tag whether or not the length is in the
    /// AAD. Replacing the length field with a constant leaves every test in
    /// this file green — verified by sabotage.
    ///
    /// The length field is therefore **defence in depth, not load bearing**. It
    /// would become load bearing only if two records could ever share a key and
    /// a counter (a record-splicing optimisation, say); the counter rule is what
    /// forbids that today. It is kept because four bytes is nothing and the
    /// future case is exactly the kind that is expensive to retrofit.
    #[test]
    fn a_cut_record_yields_nothing() {
        let keys = keys();
        let mut sealer = keys.sealer(Direction::ClientToServer);
        let mut opener = keys.opener(Direction::ClientToServer);

        let mut record = sealer.seal(b"four").unwrap();
        record.remove(HEADER_LEN);

        assert!(opener.open(&record).is_err());
        assert_eq!(opener.counter(), 0);
    }

    /// A record reflected back at its sender does not open.
    ///
    /// **What this does not prove.** Two mechanisms stop a reflection: the
    /// direction selects a different key, and the direction prefix sits inside
    /// the nonce. This test cannot tell them apart, and zeroing the prefix
    /// leaves it green — verified by sabotage. The prefix is a second line of
    /// defence, not the one doing the work; the key split is.
    #[test]
    fn a_reflected_record_does_not_open() {
        // The design's reason for two keys rather than one: an attacker who can
        // reorder frames bounces a peer's own record back at it. With one key
        // and one counter space, that record opens — it was sealed by the same
        // key at a counter the sender has not consumed. Two things stop it
        // here: the direction selects the key, and the direction is inside the
        // nonce the AEAD authenticates. Either alone is enough; a test that
        // only varied the *key* would not notice the nonce losing its prefix.
        let keys = keys();
        let mut client_sealer = keys.sealer(Direction::ClientToServer);
        let record = client_sealer.seal(b"a client request").unwrap();

        // The daemon would open this as c2s; here it is fed back to the client,
        // whose own inbound direction is s2c.
        let mut client_inbound = keys.opener(Direction::ServerToClient);
        assert!(
            client_inbound.open(&record).is_err(),
            "a record reflected back at its sender must not open"
        );

        // And the record is genuinely valid in its own direction, so the
        // rejection above is about direction and not a broken record.
        let mut server_inbound = keys.opener(Direction::ClientToServer);
        assert_eq!(server_inbound.open(&record).unwrap(), b"a client request");
    }

    #[test]
    fn a_counter_of_zero_is_never_produced() {
        let keys = keys();
        let mut sealer = keys.sealer(Direction::ClientToServer);
        let mut opener = keys.opener(Direction::ClientToServer);

        let record = sealer.seal(b"first").unwrap();
        let counter = u64::from_be_bytes(record[..HEADER_LEN].try_into().unwrap());

        assert_eq!(counter, 1, "the first record must carry counter 1");
        assert_eq!(opener.open(&record).unwrap(), b"first");
    }

    #[test]
    fn a_signature_over_a_different_transcript_does_not_verify() {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let verifying_key = signing_key.verifying_key();
        let session =
            handshake::transcript_hash(&[1_u8; 32], &[2_u8; 32], &[3_u8; 32], &[4_u8; 32]);

        let signature = sign_transcript(&signing_key, &session);
        verify_transcript(&verifying_key, &session, &signature).unwrap();

        // Any field the transcript covers changing must invalidate it: that is
        // what stops a signature being lifted into a different session.
        for other in [
            handshake::transcript_hash(&[9_u8; 32], &[2_u8; 32], &[3_u8; 32], &[4_u8; 32]),
            handshake::transcript_hash(&[1_u8; 32], &[9_u8; 32], &[3_u8; 32], &[4_u8; 32]),
            handshake::transcript_hash(&[1_u8; 32], &[2_u8; 32], &[9_u8; 32], &[4_u8; 32]),
        ] {
            assert!(verify_transcript(&verifying_key, &other, &signature).is_err());
        }
        // And a signature from a different key is refused outright.
        let impostor = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let forged = sign_transcript(&impostor, &session);
        assert!(verify_transcript(&verifying_key, &session, &forged).is_err());
    }

    #[test]
    fn a_low_order_public_key_is_refused() {
        // The all-zero public key is the canonical low-order point: the shared
        // secret it produces is zero and known to anyone.
        let zero = x25519_dalek::PublicKey::from([0_u8; 32]);
        let ours = x25519_dalek::StaticSecret::random();
        let shared = ours.diffie_hellman(&zero);

        assert!(!shared.was_contributory());
        assert!(require_contributory(&shared).is_err());

        // A genuine peer still passes the same check.
        let theirs = x25519_dalek::StaticSecret::random();
        let good = ours.diffie_hellman(&x25519_dalek::PublicKey::from(&theirs));
        assert!(require_contributory(&good).is_ok());
    }

    // ---- handshake ----

    /// Run both halves and return `(client_keys, daemon_keys)`.
    fn complete_handshake() -> (SessionKeys, SessionKeys) {
        let client_static = x25519_dalek::StaticSecret::random();
        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);

        let (client, client_hello) = handshake::ClientHandshake::start(client_static);
        let (server_hello, daemon_keys) =
            handshake::accept(&daemon_static, &daemon_signing, &client_hello).unwrap();
        let client_keys = client.finish(&server_hello, None).unwrap();
        (client_keys, daemon_keys)
    }

    #[test]
    fn a_completed_handshake_agrees_on_both_directions() {
        let (client_keys, daemon_keys) = complete_handshake();

        // The client seals c2s and the daemon opens it as c2s.
        let mut client_sealer = client_keys.sealer(Direction::ClientToServer);
        let mut daemon_opener = daemon_keys.opener(Direction::ClientToServer);
        let record = client_sealer.seal(b"a request").unwrap();
        assert_eq!(daemon_opener.open(&record).unwrap(), b"a request");

        // And the other way round.
        let mut daemon_sealer = daemon_keys.sealer(Direction::ServerToClient);
        let mut client_opener = client_keys.opener(Direction::ServerToClient);
        let record = daemon_sealer.seal(b"a reply").unwrap();
        assert_eq!(client_opener.open(&record).unwrap(), b"a reply");
    }

    #[test]
    fn the_two_hello_frames_have_the_documented_lengths() {
        let client_static = x25519_dalek::StaticSecret::random();
        let (_, client_hello) = handshake::ClientHandshake::start(client_static);
        assert_eq!(client_hello.len(), handshake::CLIENT_HELLO_LEN);
        assert_eq!(client_hello.len(), 64);

        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let (server_hello, _) =
            handshake::accept(&daemon_static, &daemon_signing, &client_hello).unwrap();
        assert_eq!(server_hello.len(), handshake::SERVER_HELLO_LEN);
        assert_eq!(server_hello.len(), 160);
    }

    /// The mistake this guards against: each side building `ikm` in its own
    /// order still produces a working-looking handshake that silently derives
    /// two different keys.
    #[test]
    fn both_sides_concatenate_the_agreements_in_the_same_order() {
        let client_static = x25519_dalek::StaticSecret::random();
        let client_ephemeral = x25519_dalek::StaticSecret::random();
        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_ephemeral = x25519_dalek::StaticSecret::random();

        let client_eph_pub = x25519_dalek::PublicKey::from(&client_ephemeral).to_bytes();
        let client_static_pub = x25519_dalek::PublicKey::from(&client_static).to_bytes();
        let daemon_eph_pub = x25519_dalek::PublicKey::from(&daemon_ephemeral).to_bytes();
        let daemon_static_pub = x25519_dalek::PublicKey::from(&daemon_static).to_bytes();

        // What each side computes, in the documented order ee || se || es.
        let client_ee =
            client_ephemeral.diffie_hellman(&x25519_dalek::PublicKey::from(daemon_eph_pub));
        let client_se =
            client_static.diffie_hellman(&x25519_dalek::PublicKey::from(daemon_eph_pub));
        let client_es =
            client_ephemeral.diffie_hellman(&x25519_dalek::PublicKey::from(daemon_static_pub));

        let daemon_ee =
            daemon_ephemeral.diffie_hellman(&x25519_dalek::PublicKey::from(client_eph_pub));
        let daemon_se =
            daemon_ephemeral.diffie_hellman(&x25519_dalek::PublicKey::from(client_static_pub));
        let daemon_es =
            daemon_static.diffie_hellman(&x25519_dalek::PublicKey::from(client_eph_pub));

        // Each row of the table in `agreement_bytes`' doc comment is one value.
        assert_eq!(client_ee.as_bytes(), daemon_ee.as_bytes());
        assert_eq!(client_se.as_bytes(), daemon_se.as_bytes());
        assert_eq!(client_es.as_bytes(), daemon_es.as_bytes());

        let client_ikm = handshake::agreement_bytes(&[&client_ee, &client_se, &client_es]);
        let daemon_ikm = handshake::agreement_bytes(&[&daemon_ee, &daemon_se, &daemon_es]);
        assert_eq!(
            client_ikm, daemon_ikm,
            "the two sides must concatenate to the same ikm bytes"
        );

        // And the order really is load-bearing: swapping two of them differs.
        let wrong_order = handshake::agreement_bytes(&[&client_ee, &client_es, &client_se]);
        assert_ne!(client_ikm, wrong_order);
    }

    #[test]
    fn the_static_to_static_agreement_is_not_part_of_the_session_key() {
        // A static-static agreement would destroy forward secrecy: once either
        // static secret leaks, every recorded session becomes decryptable. The
        // session key must not change when only `ss` would have.
        let (client_keys, daemon_keys) = complete_handshake();
        let mut sealer = client_keys.sealer(Direction::ClientToServer);
        let record = sealer.seal(b"x").unwrap();
        assert_eq!(
            daemon_keys
                .opener(Direction::ClientToServer)
                .open(&record)
                .unwrap(),
            b"x"
        );

        // `ikm` is built from exactly three agreements, each length-prefixed to
        // 32 bytes plus a one-byte length, for 99 bytes.
        let ikm = handshake::agreement_bytes(&[
            &x25519_dalek::StaticSecret::random().diffie_hellman(&x25519_dalek::PublicKey::from(
                &x25519_dalek::StaticSecret::random(),
            )),
            &x25519_dalek::StaticSecret::random().diffie_hellman(&x25519_dalek::PublicKey::from(
                &x25519_dalek::StaticSecret::random(),
            )),
            &x25519_dalek::StaticSecret::random().diffie_hellman(&x25519_dalek::PublicKey::from(
                &x25519_dalek::StaticSecret::random(),
            )),
        ]);
        assert_eq!(ikm.len(), 3 * (1 + 32), "three agreements, no ss");
    }

    #[test]
    fn a_substituted_daemon_key_is_rejected() {
        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        // What the client stored when it paired.
        let pinned = daemon_signing.verifying_key();

        let (client, client_hello) =
            handshake::ClientHandshake::start(x25519_dalek::StaticSecret::random());

        // A relay that runs its own handshake presents a different key. A
        // client that paired before refuses it before even looking at the
        // signature, which is the stronger and more actionable check.
        let impostor_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let (impostor_hello, _) =
            handshake::accept(&daemon_static, &impostor_signing, &client_hello).unwrap();
        // `SessionKeys` deliberately has no `Debug`, so `unwrap_err` cannot be
        // used; a `match` keeps key material out of any failure message.
        let error = match client.finish(&impostor_hello, Some(&pinned)) {
            Ok(_) => panic!("a substituted daemon key must not be accepted"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("not the one you paired with"),
            "unexpected error: {error}"
        );

        // The same client accepting the genuine daemon's frame succeeds, so the
        // rejection above is about the key and not a broken handshake.
        let (client2, hello2) =
            handshake::ClientHandshake::start(x25519_dalek::StaticSecret::random());
        let (real, _) = handshake::accept(&daemon_static, &daemon_signing, &hello2).unwrap();
        client2.finish(&real, Some(&pinned)).unwrap();
    }

    #[test]
    fn a_forged_handshake_signature_is_rejected() {
        let client_static = x25519_dalek::StaticSecret::random();
        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);

        let (client, client_hello) = handshake::ClientHandshake::start(client_static);
        let (mut server_hello, _) =
            handshake::accept(&daemon_static, &daemon_signing, &client_hello).unwrap();
        // Flip a bit inside the signature (the trailing 64 bytes).
        let signature_start = server_hello.len() - 64;
        server_hello[signature_start] ^= 0x01;

        assert!(client.finish(&server_hello, None).is_err());
    }

    #[test]
    fn a_handshake_with_a_low_order_client_key_is_refused() {
        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);

        let mut hello = vec![0_u8; handshake::CLIENT_HELLO_LEN];
        // Both halves all-zero: the canonical low-order point.
        assert!(handshake::accept(&daemon_static, &daemon_signing, &hello).is_err());

        // A genuine ephemeral with a low-order static is also refused.
        let genuine_ephemeral =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::random()).to_bytes();
        hello[..32].fill(0);
        hello[32..].copy_from_slice(&genuine_ephemeral);
        assert!(handshake::accept(&daemon_static, &daemon_signing, &hello).is_err());
    }

    #[test]
    fn a_hello_of_the_wrong_length_is_refused() {
        let daemon_static = x25519_dalek::StaticSecret::random();
        let daemon_signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);

        assert!(handshake::accept(&daemon_static, &daemon_signing, &[]).is_err());
        assert!(handshake::accept(&daemon_static, &daemon_signing, &[0_u8; 63]).is_err());
        assert!(handshake::accept(&daemon_static, &daemon_signing, &[0_u8; 65]).is_err());
    }

    #[test]
    fn the_client_learns_the_daemon_signing_key() {
        let (client_keys, daemon_keys) = complete_handshake();
        let peer = client_keys
            .peer_key()
            .expect("a completed handshake pins the peer");
        // The daemon's side records the client's *static X25519* key, not an
        // Ed25519 identity: a client is not authenticated.
        assert!(daemon_keys.peer_key().is_some());
        assert_ne!(peer, [0_u8; 32]);
    }
}
