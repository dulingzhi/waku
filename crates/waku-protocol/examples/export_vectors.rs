//! Emit cross-language test vectors as JSON on stdout.
//!
//! The TypeScript client re-derives every one of these and must land on the
//! same bytes. That is the only check that proves the two implementations
//! agree; without it "both sides pass their own tests" says nothing about
//! whether they interoperate.
//!
//! Run with:
//!
//! ```text
//! cargo run -p waku-protocol --example export_vectors > vectors.json
//! ```

use ed25519_dalek::SigningKey;
use waku_protocol::record::handshake;
use waku_protocol::record::{Direction, SessionKeys};

/// The first `n` bytes of a record sealed over a fixed plaintext, used as a
/// key fingerprint.
///
/// This exists because `SessionKeys` deliberately exposes no accessor for the
/// raw keys — a value that never leaves the type cannot be logged or compared
/// by accident. Sealing a known plaintext gives the same equality check
/// without widening the API.
fn key_fingerprint(keys: &SessionKeys, direction: Direction) -> String {
    let mut sealer = keys.sealer(direction);
    // A 32-byte plaintext, so the record is exactly `8 (counter) + 32
    // (ciphertext) + 16 (tag)` and the ciphertext can be sliced out whole.
    let record = sealer.seal(&[0_u8; 32]).expect("seal");
    hex(&record[8..40])
}
use x25519_dalek::{PublicKey, StaticSecret};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Deterministic keys, so a regenerated file is identical and a diff means a
/// real change rather than fresh randomness.
fn secret(seed: u8) -> StaticSecret {
    StaticSecret::from([seed; 32])
}

fn main() {
    let mut out = String::from("{\n");

    // --- transcript hash -------------------------------------------------
    // Four fixed public keys. Catches a wrong label, a wrong field order, or a
    // missing 8-byte length prefix.
    let client_static = PublicKey::from(&secret(0x11)).to_bytes();
    let client_ephemeral = PublicKey::from(&secret(0x22)).to_bytes();
    let server_ephemeral = PublicKey::from(&secret(0x33)).to_bytes();
    let daemon_static = PublicKey::from(&secret(0x44)).to_bytes();

    let transcript = handshake::transcript_hash(
        &client_static,
        &client_ephemeral,
        &server_ephemeral,
        &daemon_static,
    );

    // --- agreement bytes -------------------------------------------------
    // `ikm = (32 || ee) || (32 || se) || (32 || es)`, 99 bytes. Catches a
    // reordered concatenation.
    let ee = secret(0x55).diffie_hellman(&PublicKey::from([0x66; 32]));
    let se = secret(0x77).diffie_hellman(&PublicKey::from([0x88; 32]));
    let es = secret(0x99).diffie_hellman(&PublicKey::from([0xaa; 32]));
    let ikm = handshake::agreement_bytes(&[&ee, &se, &es]);

    // --- session keys ----------------------------------------------------
    let keys = SessionKeys::derive(&ikm, &transcript);
    let mut c2s = keys.sealer(Direction::ClientToServer);
    let mut s2c = keys.sealer(Direction::ServerToClient);

    // --- records ---------------------------------------------------------
    // Counter starts at 1. The plaintext is deliberately not a round number of
    // bytes: a length that is a multiple of the block size would hide an
    // off-by-one in the length field.
    let record_plaintext = b"waku cross-language vector";
    let first_record = c2s.seal(record_plaintext).expect("seal");
    let second_record = c2s.seal(b"second").expect("seal");
    let s2c_record = s2c.seal(record_plaintext).expect("seal");

    // --- a full handshake ------------------------------------------------
    // Both sides derive; the exporter prints the daemon's half so the
    // TypeScript side can drive `ClientHandshake::finish` against it.
    let daemon_x25519 = secret(0xbb);
    let daemon_signing = SigningKey::from_bytes(&[0xcc; 32]);
    let client_static_secret = secret(0xdd);

    let (client_handshake, client_hello) = handshake::ClientHandshake::start(client_static_secret);
    let (server_hello, daemon_keys) =
        handshake::accept(&daemon_x25519, &daemon_signing, &client_hello).expect("accept");
    let client_keys = client_handshake
        .finish(&server_hello, None)
        .expect("finish");

    // The two sides must have derived the same session key. If they have not,
    // the vectors below are useless and this file says so.
    let mut client_sealer = client_keys.sealer(Direction::ClientToServer);
    let mut daemon_opener = daemon_keys.opener(Direction::ClientToServer);
    let probe = client_sealer.seal(b"agreement probe").expect("seal");
    daemon_opener
        .open(&probe)
        .expect("the two sides must agree before vectors are exported");

    out.push_str(&format!(
        "  \"transcriptHash\": {{\n    \
         \"clientStatic\": \"{}\",\n    \
         \"clientEphemeral\": \"{}\",\n    \
         \"serverEphemeral\": \"{}\",\n    \
         \"daemonStatic\": \"{}\",\n    \
         \"digest\": \"{}\"\n  }},\n",
        hex(&client_static),
        hex(&client_ephemeral),
        hex(&server_ephemeral),
        hex(&daemon_static),
        hex(&transcript),
    ));

    out.push_str(&format!(
        "  \"agreementBytes\": {{\n    \
         \"ee\": \"{}\",\n    \
         \"se\": \"{}\",\n    \
         \"es\": \"{}\",\n    \
         \"ikm\": \"{}\"\n  }},\n",
        hex(ee.as_bytes()),
        hex(se.as_bytes()),
        hex(es.as_bytes()),
        hex(&ikm),
    ));

    out.push_str(&format!(
        "  \"sessionKeys\": {{\n    \
         \"ikm\": \"{}\",\n    \
         \"transcript\": \"{}\",\n    \
         \"clientToServer\": \"{}\",\n    \
         \"serverToClient\": \"{}\"\n  }},\n",
        hex(&ikm),
        hex(&transcript),
        key_fingerprint(&keys, Direction::ClientToServer),
        key_fingerprint(&keys, Direction::ServerToClient),
    ));

    out.push_str(&format!(
        "  \"records\": [\n    \
         {{\"name\": \"counter-1-c2s\", \"direction\": \"clientToServer\", \
         \"counter\": 1, \"plaintext\": \"{}\", \"record\": \"{}\"}},\n    \
         {{\"name\": \"counter-2-c2s\", \"direction\": \"clientToServer\", \
         \"counter\": 2, \"plaintext\": \"{}\", \"record\": \"{}\"}},\n    \
         {{\"name\": \"counter-1-s2c\", \"direction\": \"serverToClient\", \
         \"counter\": 1, \"plaintext\": \"{}\", \"record\": \"{}\"}}\n  ],\n",
        hex(record_plaintext),
        hex(&first_record),
        hex(b"second"),
        hex(&second_record),
        hex(record_plaintext),
        hex(&s2c_record),
    ));

    out.push_str(&format!(
        "  \"handshake\": {{\n    \
         \"clientHello\": \"{}\",\n    \
         \"serverHello\": \"{}\",\n    \
         \"clientToServer\": \"{}\",\n    \
         \"serverToClient\": \"{}\"\n  }}\n",
        hex(&client_hello),
        hex(&server_hello),
        key_fingerprint(&client_keys, Direction::ClientToServer),
        key_fingerprint(&client_keys, Direction::ServerToClient),
    ));

    out.push('}');
    println!("{out}");
}
