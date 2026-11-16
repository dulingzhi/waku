import { describe, expect, test } from 'bun:test';

import vectors from './record.vectors.json';

import {
  authenticationString,
  fingerprintOf,
  FINGERPRINT_LEN,
  isValidSessionId,
  MAX_SESSION_ID_LEN,
  pairingFingerprintMatches,
  PairingCodeError,
  parsePairingCode,
  relayScheme,
  relayUrl,
  renderPairingCode,
  type PairingCode,
} from './pairing';

/** The fixture `pairing.rs` tests use (`pairing.rs:210-216`). */
function code(): PairingCode {
  return {
    relay: 'relay.example.test:443',
    sessionId: 'abc123',
    fingerprint: 'd258dc0e8595ed66',
  };
}

function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let index = 0; index < out.length; index += 1) {
    out[index] = Number.parseInt(hex.slice(index * 2, index * 2 + 2), 16);
  }
  return out;
}

describe('pairing codes', () => {
  test('a code round-trips through render and parse', () => {
    const original = code();
    expect(parsePairingCode(renderPairingCode(original))).toEqual(original);
  });

  test('the rendered code keeps the fingerprint in the fragment', () => {
    // A code pasted into a web form loses the fragment, which is the part that
    // authenticates the daemon: the failure must be loud, not silent.
    const text = renderPairingCode(code());
    const [beforeFragment, fragment] = text.split('#');
    expect(fragment).toBe('d258dc0e8595ed66');
    expect(beforeFragment).not.toContain('d258dc0e8595ed66');
  });

  test('a code without a fingerprint is refused', () => {
    // The pin is the whole point. A code that parsed without one would connect
    // with no way to tell the real daemon from a relay's own.
    expect(() => parsePairingCode('waku://relay.example.test:443/abc123'))
      .toThrow(/#/);
  });

  test('a short fingerprint is refused', () => {
    let error: unknown;
    try {
      parsePairingCode('waku://relay.example.test:443/abc123#d258');
    } catch (cause) {
      error = cause;
    }
    expect(error).toBeInstanceOf(PairingCodeError);
    expect((error as Error).message).toContain('16 hex characters');
  });

  test('a non-hex fingerprint is refused', () => {
    // Guards a code transcribed with a look-alike character; accepting it would
    // silently pin a different key.
    expect(() => parsePairingCode('waku://relay.example.test:443/abc#zzzzzzzzzzzzzzzz'))
      .toThrow();
  });

  test('a relay with a path is refused', () => {
    // A relay address carrying its own path would make the session id
    // ambiguous, and the ambiguity would sit in the part an attacker edits.
    expect(() => parsePairingCode('waku://relay.test/evil/abc#d258dc0e8595ed66'))
      .toThrow(/no path/);
  });

  test('a code that is not ours is refused', () => {
    expect(() => parsePairingCode('https://relay.test/abc#d258dc0e8595ed66')).toThrow();
    expect(() => parsePairingCode('')).toThrow();
    expect(() => parsePairingCode('   ')).toThrow();
  });

  test('surrounding whitespace from a paste is tolerated', () => {
    const text = `  ${renderPairingCode(code())}  \n`;
    expect(parsePairingCode(text)).toEqual(code());
  });

  test('an uppercase fingerprint is normalised to lowercase', () => {
    expect(parsePairingCode('waku://relay.test:443/abc#D258DC0E8595ED66').fingerprint)
      .toBe('d258dc0e8595ed66');
  });

  test('an empty relay or session id is refused', () => {
    expect(() => parsePairingCode('waku:///abc#d258dc0e8595ed66')).toThrow();
    expect(() => parsePairingCode('waku://relay.test:443/#d258dc0e8595ed66')).toThrow();
  });

  test('a code with no path separator is refused', () => {
    expect(() => parsePairingCode('waku://relay.test#d258dc0e8595ed66')).toThrow();
  });
});

describe('session ids', () => {
  test('accepts alphanumerics, dashes and underscores', () => {
    expect(isValidSessionId('abc123')).toBe(true);
    expect(isValidSessionId('a-b_c')).toBe(true);
    expect(isValidSessionId('A1')).toBe(true);
  });

  test('refuses the characters the relay would reject', () => {
    expect(isValidSessionId('')).toBe(false);
    expect(isValidSessionId('a/b')).toBe(false);
    expect(isValidSessionId('a b')).toBe(false);
    expect(isValidSessionId('a#b')).toBe(false);
    expect(isValidSessionId('a.b')).toBe(false);
  });

  test('refuses an over-long id', () => {
    expect(isValidSessionId('a'.repeat(MAX_SESSION_ID_LEN))).toBe(true);
    expect(isValidSessionId('a'.repeat(MAX_SESSION_ID_LEN + 1))).toBe(false);
  });
});

describe('relay scheme', () => {
  /**
   * The cases `pairing.rs` tests pin. The scheme is chosen, never read from the
   * code, so these are the rules — not examples.
   */
  const code = (relay: string): PairingCode => ({
    relay,
    sessionId: 'abc',
    fingerprint: 'd258dc0e8595ed66',
  });

  test('a remote relay is reached over TLS whatever the code says', () => {
    for (const relay of [
      'relay.example.test:443',
      'relay.example.test',
      '203.0.113.7:8080',
      // A hostname that merely looks local is remote until proven otherwise.
      '127.0.0.1.nip.io:443',
      'localhost.evil.test:443',
      '10.0.0.5:9000',
    ]) {
      expect(`${relay}:${relayScheme(code(relay))}`).toBe(`${relay}:wss`);
      expect(relayUrl(code(relay))).toBe(`wss://${relay}`);
    }
  });

  test('a loopback relay does not need a certificate', () => {
    for (const relay of ['127.0.0.1:8080', 'localhost:8080', '[::1]:8080']) {
      expect(`${relay}:${relayScheme(code(relay))}`).toBe(`${relay}:ws`);
      expect(relayUrl(code(relay))).toBe(`ws://${relay}`);
    }
  });

  test('a relay with no port is still classified', () => {
    // A last-colon split that treated the host as a port would silently mark a
    // remote host as loopback.
    expect(relayScheme(code('relay.example.test'))).toBe('wss');
    expect(relayScheme(code('localhost'))).toBe('ws');
  });

  test('an unbracketed IPv6-ish relay is treated as remote', () => {
    // `2001:db8::1` ends in `:1`, whose "port" is numeric, so the host is
    // `2001:db8:` — not loopback, and therefore `wss`. Erring toward TLS is
    // the safe direction for anything that is not exactly loopback.
    expect(relayScheme(code('2001:db8::1'))).toBe('wss');
    // The literal `::1` with no port splits as host `:` plus port `1`, which
    // is not the loopback spelling either; only a bracketed form is.
    expect(relayScheme(code('::1'))).toBe('wss');
  });

  test('an empty bracket does not crash or claim loopback', () => {
    expect(relayScheme(code('[broken'))).toBe('wss');
    expect(relayScheme(code('[]:80'))).toBe('wss');
  });
});

describe('fingerprints', () => {
  test('is the first 8 bytes of SHA-256 as lowercase hex', () => {
    // A zero key: SHA-256 of 32 zero bytes starts 66687aadf862bd77, which is
    // the well-known digest prefix for that input.
    const fingerprint = fingerprintOf(new Uint8Array(32));
    expect(fingerprint.length).toBe(FINGERPRINT_LEN);
    expect(fingerprint).toBe('66687aadf862bd77');
  });

  test('is lowercase hex only', () => {
    const fingerprint = fingerprintOf(hexToBytes('ff'.repeat(32)));
    expect(/^[0-9a-f]{16}$/.test(fingerprint)).toBe(true);
  });

  test('changes when the key changes', () => {
    const base = new Uint8Array(32).fill(7);
    const other = new Uint8Array(32).fill(7);
    other[31] = 8;
    expect(fingerprintOf(base)).not.toBe(fingerprintOf(other));
  });
});

describe('pairing fingerprint matching', () => {
  const key = hexToBytes('11'.repeat(32));
  const fingerprint = fingerprintOf(key);

  test('accepts the code that names this key', () => {
    expect(() => pairingFingerprintMatches(fingerprint, key)).not.toThrow();
  });

  test('refuses a key the code does not name', () => {
    const other = hexToBytes('22'.repeat(32));
    expect(() => pairingFingerprintMatches(fingerprint, other)).toThrow(
      /not the one it claims/,
    );
  });

  test('compares the whole fingerprint, never a prefix', () => {
    // The sabotage check: a match that stopped after the first four characters
    // would accept this key. Grinding a key that shares a prefix is exactly
    // what an attacker aims for, so the full string has to be compared.
    const prefix = fingerprint.slice(0, 4);
    const impostor = findKeyWithFingerprintPrefix(prefix, key);
    expect(fingerprintOf(impostor).slice(0, 4)).toBe(prefix);
    expect(fingerprintOf(impostor)).not.toBe(fingerprint);
    expect(() => pairingFingerprintMatches(fingerprint, impostor)).toThrow(
      /not the one it claims/,
    );
  });
});

/** Search a small key space for one whose fingerprint starts with `prefix`.
 * Deterministic, so the test does not flake on a random draw. */
function findKeyWithFingerprintPrefix(prefix: string, avoid: Uint8Array): Uint8Array {
  for (let counter = 0; counter < 200_000; counter += 1) {
    const candidate = new Uint8Array(32);
    const view = new DataView(candidate.buffer);
    view.setUint32(0, counter, false);
    if (fingerprintOf(candidate).startsWith(prefix)) {
      if (fingerprintOf(candidate) === fingerprintOf(avoid)) continue;
      return candidate;
    }
  }
  throw new Error(`no key with fingerprint prefix ${prefix} found`);
}

describe('authentication strings', () => {
  test('is grouped into fours and stable', () => {
    // `0x12` is 18, and 18 % 10 is 8, so a uniform transcript gives a uniform
    // string; the grouping is then the only thing visible, which is what this
    // checks. Matches `pairing.rs:288-294`.
    const transcript = new Uint8Array(32).fill(0x12);
    const value = authenticationString(transcript);
    expect(value).toBe('8888 8888 8888');
    expect(authenticationString(transcript)).toBe(value);
  });

  test('every digit comes from a different byte', () => {
    // A string that repeated one digit would compare equal against many
    // transcripts, which is the same failure as truncating it too far.
    const transcript = new Uint8Array(32);
    for (let index = 0; index < transcript.length; index += 1) transcript[index] = index;
    const digits = authenticationString(transcript).replace(/ /g, '');
    expect(digits.length).toBe(12);
    expect(new Set(digits).size).toBeGreaterThanOrEqual(9);
  });

  test('uses only the first twelve bytes of the transcript', () => {
    const base = new Uint8Array(32).fill(1);
    const later = new Uint8Array(base);
    // Byte 12 is past the compared window, so the string must not move.
    later[12] = 9;
    expect(authenticationString(later)).toBe(authenticationString(base));
    // Byte 11 is the last one inside it.
    const last = new Uint8Array(base);
    last[11] = 9;
    expect(authenticationString(last)).not.toBe(authenticationString(base));
  });

  test('a different transcript gives a different string', () => {
    // This is what catches a relay running one handshake per side.
    const one = new Uint8Array(32).fill(0x12);
    const other = new Uint8Array(32).fill(0x12);
    other[0] = 0x13;
    expect(authenticationString(one)).not.toBe(authenticationString(other));
  });

  test('only digits and the three separators appear', () => {
    const transcript = new Uint8Array(32);
    for (let index = 0; index < 32; index += 1) transcript[index] = index * 7;
    expect(/^\d{4} \d{4} \d{4}$/.test(authenticationString(transcript))).toBe(true);
  });
});

describe('cross-language pairing vectors', () => {
  // The Rust exporter writes these from fixed inputs, so they are reproducible
  // and a diff means the protocol changed. Without this group the two
  // implementations could drift and the failure would surface as a correct
  // pairing rejected -- or, worse, an attacker's handshake accepted, since the
  // fingerprint is the only thing standing between a client and a substituted
  // daemon.
  test('the fingerprint matches the Rust implementation', () => {
    expect(fingerprintOf(hexToBytes(vectors.pairing.ed25519Public))).toBe(
      vectors.pairing.fingerprint,
    );
  });

  test('the authentication string matches the Rust implementation', () => {
    expect(authenticationString(hexToBytes(vectors.pairing.transcriptHash))).toBe(
      vectors.pairing.authenticationString,
    );
  });

  test('a rendered code round-trips and carries the expected fingerprint', () => {
    const parsed = parsePairingCode(vectors.pairing.renderedCode);
    expect(parsed.fingerprint).toBe(vectors.pairing.fingerprint);
    expect(parsed.relay).toBe('relay.example.test:443');
    expect(renderPairingCode(parsed)).toBe(vectors.pairing.renderedCode);
  });
});
