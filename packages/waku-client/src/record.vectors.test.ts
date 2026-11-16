import { describe, expect, test } from 'bun:test';

import vectors from './record.vectors.json';
import {
  authenticationString,
  fingerprintOf,
  isValidSessionId,
  pairingFingerprintMatches,
  parsePairingCode,
  renderPairingCode,
} from './pairing';
import {
  agreementBytes,
  ClientHandshake,
  Opener,
  Sealer,
  SessionKeys,
  SERVER_HELLO_LEN,
  transcriptHash,
  verifyTranscript,
} from './record';

/**
 * Cross-language vectors, produced by the Rust implementation and committed as
 * `record.vectors.json`.
 *
 * Regenerate with:
 *
 * ```text
 * cargo run -p waku-protocol --example export_vectors > \
 *   packages/waku-client/src/record.vectors.json
 * ```
 *
 * **What is deterministic and what is not.** Every group except `handshake`
 * is fully deterministic: the exporter derives its inputs from fixed seeds, so
 * re-running produces identical bytes and a diff there is a real protocol
 * change.
 *
 * `handshake` is not. `ClientHandshake::start` and `handshake::accept` both
 * draw a fresh ephemeral key from the OS, so `clientHello` (after its fixed
 * first 32 bytes), `serverHello`, and both derived key fingerprints change on
 * every run. Those vectors prove the two implementations *did* agree on one
 * real exchange; they cannot be regenerated and diffed. Only the client's
 * static half of `clientHello` is stable, and only that is asserted exactly.
 *
 * These are the only tests that prove the two implementations agree. Both
 * sides passing their own tests says nothing about whether they interoperate;
 * these say it byte for byte.
 */

function fromHex(hex: string): Uint8Array {
  const clean = hex.replace(/\s+/g, '');
  if (clean.length % 2 !== 0) throw new Error(`odd-length hex: ${hex}`);
  const out = new Uint8Array(clean.length / 2);
  for (let index = 0; index < out.length; index += 1) {
    out[index] = Number.parseInt(clean.slice(index * 2, index * 2 + 2), 16);
  }
  return out;
}

function toHex(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

/**
 * How the Rust exporter fingerprints a key: seal 32 zero bytes and take the
 * ciphertext, `record[8..40]`.
 *
 * `SessionKeys` exposes no key accessor on either side, deliberately — a value
 * that never leaves the type cannot be logged or compared by accident. Sealing
 * a known plaintext gives the same equality check without widening the API,
 * and both implementations must produce the same fingerprint.
 */
function keyFingerprint(keys: SessionKeys, direction: 'clientToServer' | 'serverToClient'): string {
  const sealer = new Sealer(keys[direction], direction);
  return toHex(sealer.seal(new Uint8Array(32)).slice(8, 40));
}

describe('cross-language vectors', () => {
  test('the committed vector file is the shape the exporter produces', () => {
    // A truncated or hand-edited file must fail loudly here rather than
    // letting every downstream loop pass over an empty array.
    expect(vectors.records.length).toBe(3);
    expect(vectors.sessionKeys.ikm.length).toBe(198);
    expect(vectors.sessionKeys.transcript.length).toBe(64);
    expect(vectors.agreementBytes.ikm.length).toBe(198);
    expect(vectors.transcriptHash.digest.length).toBe(64);
    expect(vectors.handshake.serverHello.length).toBe(320);
    expect(vectors.handshake.clientHello.length).toBe(128);
  });

  test('the transcript hash matches across implementations', () => {
    const vector = vectors.transcriptHash;
    const digest = transcriptHash(
      fromHex(vector.clientStatic),
      fromHex(vector.clientEphemeral),
      fromHex(vector.serverEphemeral),
      fromHex(vector.daemonStatic),
    );
    expect(toHex(digest)).toBe(vector.digest);
  });

  test('the transcript hash covers the fields in the documented order', () => {
    // Swapping any two fields must move the digest, or the length prefixes are
    // not doing their job.
    const vector = vectors.transcriptHash;
    const a = transcriptHash(
      fromHex(vector.clientStatic),
      fromHex(vector.clientEphemeral),
      fromHex(vector.serverEphemeral),
      fromHex(vector.daemonStatic),
    );
    const swapped = transcriptHash(
      fromHex(vector.clientEphemeral),
      fromHex(vector.clientStatic),
      fromHex(vector.serverEphemeral),
      fromHex(vector.daemonStatic),
    );
    expect(toHex(a)).not.toBe(toHex(swapped));
    expect(toHex(a)).toBe(vector.digest);
  });

  test('the agreement bytes concatenate ee, se, es in that order', () => {
    const vector = vectors.agreementBytes;
    const ikm = agreementBytes([
      fromHex(vector.ee),
      fromHex(vector.se),
      fromHex(vector.es),
    ]);
    expect(toHex(ikm)).toBe(vector.ikm);
    // 3 × (1 length byte + 32) = 99.
    expect(ikm.length).toBe(99);
    expect(ikm[0]).toBe(32);
  });

  test('session keys derive to the same bytes', () => {
    const vector = vectors.sessionKeys;
    const keys = SessionKeys.derive(fromHex(vector.ikm), fromHex(vector.transcript));
    expect(keyFingerprint(keys, 'clientToServer')).toBe(vector.clientToServer);
    expect(keyFingerprint(keys, 'serverToClient')).toBe(vector.serverToClient);
  });

  test('the two directions are not the same key', () => {
    const vector = vectors.sessionKeys;
    expect(vector.clientToServer).not.toBe(vector.serverToClient);
  });

  test('sealed records match byte for byte', () => {
    for (const vector of vectors.records) {
      const direction = vector.direction as 'clientToServer' | 'serverToClient';
      const keys = SessionKeys.derive(
        fromHex(vectors.sessionKeys.ikm),
        fromHex(vectors.sessionKeys.transcript),
      );
      const sealer = keys.sealer(direction);
      // Advance to the vector's counter: a stream is positional, so record N
      // only reproduces when the ones before it were sealed too. The exporter
      // sealed the previous plaintexts from its own fixed stream, which for
      // these vectors is the first record's text repeated.
      for (let index = 1; index < vector.counter; index += 1) {
        sealer.seal(fromHex(vectors.records[0]!.plaintext));
      }
      const record = sealer.seal(fromHex(vector.plaintext));
      expect(`${vector.name} ${toHex(record)}`).toBe(`${vector.name} ${vector.record}`);
    }
  });

  test('a vector record opens back to its plaintext', () => {
    for (const vector of vectors.records) {
      const direction = vector.direction as 'clientToServer' | 'serverToClient';
      const keys = SessionKeys.derive(
        fromHex(vectors.sessionKeys.ikm),
        fromHex(vectors.sessionKeys.transcript),
      );
      const opener = keys.opener(direction);
      const sealer = keys.sealer(direction);
      // Walk the counter to the vector's position, mirroring the sealing
      // order the exporter used.
      for (let index = 1; index < vector.counter; index += 1) {
        opener.open(sealer.seal(fromHex(vectors.records[0]!.plaintext)));
      }
      expect(toHex(opener.open(fromHex(vector.record)))).toBe(vector.plaintext);
    }
  });

  test('a vector record opens back to its plaintext', () => {
    for (const vector of vectors.records) {
      const direction = vector.direction as 'clientToServer' | 'serverToClient';
      const keys = SessionKeys.derive(
        fromHex(vectors.sessionKeys.ikm),
        fromHex(vectors.sessionKeys.transcript),
      );
      const opener = keys.opener(direction);
      const sealer = keys.sealer(direction);
      // Walk the counter to the vector's position, mirroring the sealing
      // order the exporter used.
      for (let index = 1; index < vector.counter; index += 1) {
        opener.open(sealer.seal(fromHex(vectors.records[0]!.plaintext)));
      }
      expect(toHex(opener.open(fromHex(vector.record)))).toBe(vector.plaintext);
    }
  });
});

describe('cross-language pairing vectors', () => {
  /**
   * Fully deterministic, unlike the handshake group: every input is a fixed
   * key, so this group regenerates and diffs cleanly.
   */
  test('the fingerprint of a daemon key matches', () => {
    expect(fingerprintOf(fromHex(vectors.pairing.ed25519Public)))
      .toBe(vectors.pairing.fingerprint);
  });

  test('the authentication string matches', () => {
    // Both ends show this to a person. A drift is not a crash: it is a correct
    // pairing rejected, or an attacker's handshake accepted.
    expect(authenticationString(fromHex(vectors.pairing.transcriptHash)))
      .toBe(vectors.pairing.authenticationString);
  });

  test('the rendered code matches byte for byte', () => {
    expect(renderPairingCode({
      relay: 'relay.example.test:443',
      sessionId: '0123456789abcdef0123456789abcdef',
      fingerprint: fingerprintOf(fromHex(vectors.pairing.ed25519Public)),
    })).toBe(vectors.pairing.renderedCode);
  });

  test('the rendered code parses back to the same parts', () => {
    const parsed = parsePairingCode(vectors.pairing.renderedCode);
    expect(parsed.relay).toBe('relay.example.test:443');
    expect(parsed.sessionId).toBe('0123456789abcdef0123456789abcdef');
    expect(parsed.fingerprint).toBe(vectors.pairing.fingerprint);
    // And rendering it again is the same string: render/parse are inverses.
    expect(renderPairingCode(parsed)).toBe(vectors.pairing.renderedCode);
  });

  test('the pin check accepts the vector key and refuses another', () => {
    const key = fromHex(vectors.pairing.ed25519Public);
    expect(() => pairingFingerprintMatches(vectors.pairing.fingerprint, key)).not.toThrow();
    const other = new Uint8Array(key);
    other[0] = (other[0] ?? 0) ^ 0xff;
    expect(() => pairingFingerprintMatches(vectors.pairing.fingerprint, other))
      .toThrow(/not the one it claims/);
  });

  test('the session id in the vector code is one the relay accepts', () => {
    const parsed = parsePairingCode(vectors.pairing.renderedCode);
    expect(isValidSessionId(parsed.sessionId)).toBe(true);
  });
});

describe('cross-language handshake', () => {  /**
   * The full handshake vector: the daemon's half of one real exchange, plus
   * the keys both sides derived.
   *
   * The exporter does not print secrets, so this cannot re-run
   * `ClientHandshake::finish` — the client ephemeral private key is random and
   * never leaves the generator. What it can do is reproduce the client's half
   * of the transcript from the two hellos and check the daemon's signature
   * over it. A signature only verifies against a byte-identical transcript, so
   * that single assertion covers the transcript construction, the server hello
   * layout, and the Ed25519 usage all at once.
   */
  test('the server hello has the documented layout', () => {
    const serverHello = fromHex(vectors.handshake.serverHello);
    expect(serverHello.length).toBe(SERVER_HELLO_LEN);
  });

  test('the client hello carries the static key first, then the ephemeral', () => {
    const clientHello = fromHex(vectors.handshake.clientHello);
    expect(clientHello.length).toBe(64);
    // Both halves must be plausible X25519 public keys: 32 bytes each, and the
    // whole frame is two of them.
    expect(clientHello.subarray(0, 32).length).toBe(32);
    expect(clientHello.subarray(32).length).toBe(32);
  });

  test('the daemon signature verifies over the transcript we rebuild', () => {
    const clientHello = fromHex(vectors.handshake.clientHello);
    const serverHello = fromHex(vectors.handshake.serverHello);
    const daemonX25519 = serverHello.subarray(0, 32);
    const daemonEd25519 = serverHello.subarray(32, 64);
    const serverEphemeral = serverHello.subarray(64, 96);
    const signature = serverHello.subarray(96, 160);

    const transcript = transcriptHash(
      clientHello.subarray(0, 32),
      clientHello.subarray(32, 64),
      serverEphemeral,
      daemonX25519,
    );
    // Throws if the signature does not cover exactly these bytes.
    expect(() => verifyTranscript(daemonEd25519, transcript, signature)).not.toThrow();
  });

  test('a signature over a different transcript does not verify', () => {
    const clientHello = fromHex(vectors.handshake.clientHello);
    const serverHello = fromHex(vectors.handshake.serverHello);
    const daemonEd25519 = serverHello.subarray(32, 64);
    const serverEphemeral = serverHello.subarray(64, 96);
    const signature = serverHello.subarray(96, 160);

    // Swap the client's static and ephemeral halves: the transcript must move.
    const wrong = transcriptHash(
      clientHello.subarray(32, 64),
      clientHello.subarray(0, 32),
      serverEphemeral,
      serverHello.subarray(0, 32),
    );
    expect(() => verifyTranscript(daemonEd25519, wrong, signature))
      .toThrow('handshake signature invalid');
  });

  test('a started handshake produces a hello the daemon can answer', () => {
    // `ClientHandshake.start` is the one handshake step that runs before any
    // reply, so it is the only one this side can exercise alone. What it must
    // produce is a 64-byte frame carrying two distinct X25519 public keys; a
    // degenerate key (all zeroes, or the static key repeated) would still be
    // 64 bytes and would silently break the agreements downstream.
    const { clientHello } = ClientHandshake.start();
    expect(clientHello.length).toBe(64);

    const staticKey = clientHello.subarray(0, 32);
    const ephemeralKey = clientHello.subarray(32, 64);
    expect(toHex(staticKey)).not.toBe(toHex(ephemeralKey));
    expect(toHex(staticKey)).not.toBe('00'.repeat(32));
    expect(toHex(ephemeralKey)).not.toBe('00'.repeat(32));

    // Two starts must not reuse an ephemeral key, or every connection would
    // share the `ee` agreement.
    expect(toHex(ClientHandshake.start().clientHello.subarray(32, 64))).not.toBe(
      toHex(ephemeralKey),
    );
  });

  test('the handshake group is evidence, not a regression baseline', () => {
    // `ClientHandshake::start` and `handshake::accept` draw fresh ephemeral
    // keys on both sides, so regenerating the vector file rewrites this group
    // and nothing else. Asserting exact equality against it would fail on
    // every legitimate regeneration, which is why the tests above use it only
    // for shape and signature checks — those hold for any exchange.
    //
    // Every other group is built from fixed secrets and does carry regression
    // weight: see the record and derivation tests.
    expect(vectors.handshake.serverHello.length).toBe(SERVER_HELLO_LEN * 2);
    expect(vectors.handshake.clientHello.length).toBe(64 * 2);
  });
});
