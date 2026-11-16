import { describe, expect, test } from 'bun:test';

import {
  agreementBytes,
  asciiBytes,
  bytesEqual,
  ClientHandshake,
  CLIENT_HELLO_LEN,
  Opener,
  Sealer,
  SERVER_HELLO_LEN,
  SessionKeys,
  transcriptHash,
  verifyTranscript,
} from './record';

import { ed25519, x25519 } from '@noble/curves/ed25519.js';

/** A fixed 32-byte transcript stand-in, so the tests do not depend on a random
 * handshake where they only exercise the codec. */
const TRANSCRIPT = new Uint8Array(32).fill(7);
const IKM = asciiBytes('a shared secret long enough to be realistic');

function sessionKeys(): SessionKeys {
  return SessionKeys.derive(IKM, TRANSCRIPT);
}

function utf8(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

function hex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString('hex');
}

describe('record codec', () => {
  test('a sealed record opens to the same bytes', () => {
    const keys = sessionKeys();
    const sealer = keys.sealer('clientToServer');
    const opener = keys.opener('clientToServer');
    const record = sealer.seal(utf8('{"type":"hello"}'));
    expect(new TextDecoder().decode(opener.open(record))).toBe('{"type":"hello"}');
  });

  test('the first record is counter 1 and the wire header carries it big-endian', () => {
    const record = sessionKeys().sealer('clientToServer').seal(utf8('x'));
    expect(record.length).toBe(8 + 1 + 16);
    expect(hex(record.subarray(0, 8))).toBe('0000000000000001');
  });

  test('counters increment by one per record', () => {
    const sealer = sessionKeys().sealer('clientToServer');
    sealer.seal(utf8('a'));
    const second = sealer.seal(utf8('b'));
    expect(hex(second.subarray(0, 8))).toBe('0000000000000002');
    expect(sealer.counter).toBe(2n);
  });

  test('two directions produce different bytes for the same plaintext', () => {
    const keys = sessionKeys();
    const plaintext = utf8('same text');
    const c2s = keys.sealer('clientToServer').seal(plaintext);
    const s2c = keys.sealer('serverToClient').seal(plaintext);
    expect(hex(c2s.subarray(8))).not.toBe(hex(s2c.subarray(8)));
  });

  test('replaying a captured record is rejected', () => {
    const keys = sessionKeys();
    const record = keys.sealer('clientToServer').seal(utf8('once'));
    const opener = keys.opener('clientToServer');
    expect(new TextDecoder().decode(opener.open(record))).toBe('once');
    // Same bytes, same counter: the opener's counter has moved on.
    expect(() => opener.open(record)).toThrow('record out of order');
  });

  test('reordering is rejected', () => {
    const keys = sessionKeys();
    const sealer = keys.sealer('clientToServer');
    const first = sealer.seal(utf8('one'));
    const second = sealer.seal(utf8('two'));
    const opener = keys.opener('clientToServer');
    expect(() => opener.open(second)).toThrow('record out of order');
    expect(new TextDecoder().decode(opener.open(first))).toBe('one');
  });

  test('a record shorter than a header and tag is rejected', () => {
    const opener = sessionKeys().opener('clientToServer');
    expect(() => opener.open(new Uint8Array(23))).toThrow(
      'record too short to contain a header and tag',
    );
  });

  test('truncating a record fails authentication rather than yielding a prefix', () => {
    const record = sessionKeys().sealer('clientToServer').seal(utf8('truncate me'));
    const opener = sessionKeys().opener('clientToServer');
    expect(() => opener.open(record.subarray(0, record.length - 1))).toThrow(
      'record authentication failed',
    );
  });

  test('a record sealed under one key does not open under another', () => {
    const sealed = sessionKeys().sealer('clientToServer').seal(utf8('secret'));
    const other = SessionKeys.derive(asciiBytes('a different shared secret'), TRANSCRIPT);
    expect(() => other.opener('clientToServer').open(sealed)).toThrow(
      'record authentication failed',
    );
  });

  test('flipping one ciphertext bit fails authentication', () => {
    const record = sessionKeys().sealer('clientToServer').seal(utf8('tamper me'));
    record[9] = (record[9] ?? 0) ^ 1;
    expect(() => sessionKeys().opener('clientToServer').open(record)).toThrow(
      'record authentication failed',
    );
  });

  test('moving the wire counter without the matching nonce fails', () => {
    const keys = sessionKeys();
    const sealer = keys.sealer('clientToServer');
    const first = sealer.seal(utf8('one'));
    const second = sealer.seal(utf8('two'));
    // Splice record 2's header onto record 1's ciphertext. The counter passes
    // the ordering check, but the nonce the opener derives no longer matches
    // the one used to seal, so authentication fails.
    const spliced = new Uint8Array(first);
    spliced.set(second.subarray(0, 8), 0);
    const opener = keys.opener('clientToServer');
    expect(new TextDecoder().decode(opener.open(first))).toBe('one');
    expect(() => opener.open(spliced)).toThrow('record authentication failed');
  });

  test('the authenticated length binds the record', () => {
    const keys = sessionKeys();
    // Sealing a 3-byte plaintext authenticates AAD length 3; asking the opener
    // for a different length by handing it a longer ciphertext cannot match.
    const record = keys.sealer('clientToServer').seal(utf8('abc'));
    const padded = new Uint8Array(record.length + 1);
    padded.set(record);
    const opener = keys.opener('clientToServer');
    expect(() => opener.open(padded)).toThrow();
  });

  test('the counter refuses to wrap', () => {
    const sealer = sessionKeys().sealer('clientToServer');
    // Reach the last usable counter directly: sealing 2^64 records is not an
    // option, and the guard is what matters.
    const reachable = sealer as unknown as { last: bigint };
    reachable.last = 0xffff_ffff_ffff_fffen;
    sealer.seal(utf8('last'));
    expect(() => sealer.seal(utf8('overflow'))).toThrow(
      'record counter exhausted; refusing to reuse a nonce',
    );
  });
});

describe('key derivation', () => {
  test('separate directions derive different keys', () => {
    const keys = sessionKeys();
    expect(hex(keys.clientToServer)).not.toBe(hex(keys.serverToClient));
    expect(keys.clientToServer.length).toBe(32);
    expect(keys.serverToClient.length).toBe(32);
  });

  test('the salt is the transcript, not empty', () => {
    const withTranscript = SessionKeys.derive(IKM, TRANSCRIPT);
    const withOtherTranscript = SessionKeys.derive(IKM, new Uint8Array(32).fill(8));
    expect(hex(withTranscript.clientToServer))
      .not.toBe(hex(withOtherTranscript.clientToServer));
  });

  test('the client seals c2s and opens s2c', () => {
    const keys = sessionKeys();
    const client = keys.clientSide();
    const daemonSealer = keys.sealer('serverToClient');
    const record = daemonSealer.seal(utf8('from daemon'));
    expect(new TextDecoder().decode(client.opener.open(record))).toBe('from daemon');
    // The client's sealer must be the c2s key.
    expect(hex(client.sealer.seal(utf8('x'))))
      .toBe(hex(keys.sealer('clientToServer').seal(utf8('x'))));
  });
});

describe('transcript and agreements', () => {
  const field = (byte: number) => new Uint8Array(32).fill(byte);

  test('the transcript is a 32-byte hash over six length-prefixed fields', () => {
    const digest = transcriptHash(field(1), field(2), field(3), field(4));
    expect(digest.length).toBe(32);
  });

  test('every transcript field changes the hash', () => {
    const base = transcriptHash(field(1), field(2), field(3), field(4));
    expect(hex(transcriptHash(field(9), field(2), field(3), field(4)))).not.toBe(hex(base));
    expect(hex(transcriptHash(field(1), field(9), field(3), field(4)))).not.toBe(hex(base));
    expect(hex(transcriptHash(field(1), field(2), field(9), field(4)))).not.toBe(hex(base));
    expect(hex(transcriptHash(field(1), field(2), field(3), field(9)))).not.toBe(hex(base));
  });

  test('the field order is fixed', () => {
    // Swapping two fields must not produce the same hash; length prefixes are
    // what keep the concatenation unambiguous.
    const forward = transcriptHash(field(1), field(2), field(3), field(4));
    const swapped = transcriptHash(field(2), field(1), field(3), field(4));
    expect(hex(forward)).not.toBe(hex(swapped));
  });

  test('agreements are each prefixed with a one-byte length', () => {
    const parts = [field(1), field(2), field(3)];
    const bytes = agreementBytes(parts);
    expect(bytes.length).toBe(99);
    expect(bytes[0]).toBe(32);
    expect(bytes[33]).toBe(32);
    expect(bytes[66]).toBe(32);
  });

  test('agreement order is load-bearing', () => {
    const a = agreementBytes([field(1), field(2), field(3)]);
    const b = agreementBytes([field(2), field(1), field(3)]);
    expect(hex(a)).not.toBe(hex(b));
  });
});

describe('handshake', () => {
  /** A daemon-side handshake, mirroring `record::handshake::accept`, so the
   * client half can be exercised without a socket. */
  function daemonRespond(clientHello: Uint8Array) {
    const daemonX25519Secret = x25519.utils.randomSecretKey();
    const daemonX25519Public = x25519.getPublicKey(daemonX25519Secret);
    const signingKey = ed25519.utils.randomSecretKey();
    const daemonEd25519Public = ed25519.getPublicKey(signingKey);
    const serverEphemeralSecret = x25519.utils.randomSecretKey();
    const serverEphemeralPublic = x25519.getPublicKey(serverEphemeralSecret);

    const clientStatic = clientHello.subarray(0, 32);
    const clientEphemeral = clientHello.subarray(32, 64);

    const transcript = transcriptHash(
      clientStatic,
      clientEphemeral,
      serverEphemeralPublic,
      daemonX25519Public,
    );
    const signature = ed25519.sign(transcript, signingKey);

    const shared = x25519.getSharedSecret(serverEphemeralSecret, clientEphemeral);
    const bound = x25519.getSharedSecret(serverEphemeralSecret, clientStatic);
    const boundToDaemon = x25519.getSharedSecret(daemonX25519Secret, clientEphemeral);
    const daemonKeys = SessionKeys.derive(
      agreementBytes([shared, bound, boundToDaemon]),
      transcript,
    );

    const serverHello = new Uint8Array(SERVER_HELLO_LEN);
    serverHello.set(daemonX25519Public, 0);
    serverHello.set(daemonEd25519Public, 32);
    serverHello.set(serverEphemeralPublic, 64);
    serverHello.set(signature, 96);
    return { serverHello, daemonKeys, daemonEd25519Public, daemonX25519Public, serverEphemeralPublic };
  }

  test('both sides reach the same session keys', () => {
    const { handshake, clientHello } = ClientHandshake.start();
    expect(clientHello.length).toBe(CLIENT_HELLO_LEN);
    const daemon = daemonRespond(clientHello);
    const clientKeys = handshake.finish(daemon.serverHello);
    expect(hex(clientKeys.clientToServer)).toBe(hex(daemon.daemonKeys.clientToServer));
    expect(hex(clientKeys.serverToClient)).toBe(hex(daemon.daemonKeys.serverToClient));
  });

  test('the client pins the daemon signing key it saw', () => {
    const { handshake, clientHello } = ClientHandshake.start();
    const daemon = daemonRespond(clientHello);
    const keys = handshake.finish(daemon.serverHello);
    expect(keys.peerEd25519).not.toBeNull();
    expect(bytesEqual(keys.peerEd25519!, daemon.daemonEd25519Public)).toBe(true);
  });

  test('a substituted daemon key is refused before the signature', () => {
    const { handshake, clientHello } = ClientHandshake.start();
    const daemon = daemonRespond(clientHello);
    const otherKey = ed25519.getPublicKey(ed25519.utils.randomSecretKey());
    expect(() => handshake.finish(daemon.serverHello, otherKey)).toThrow(
      'handshake signature invalid: the daemon is not the one you paired with',
    );
  });

  test('a server hello of the wrong length is refused', () => {
    const { handshake } = ClientHandshake.start();
    expect(() => handshake.finish(new Uint8Array(159))).toThrow(
      'server hello is 159 bytes, expected 160',
    );
  });

  test('a corrupted signature is refused', () => {
    const { handshake, clientHello } = ClientHandshake.start();
    const daemon = daemonRespond(clientHello);
    const broken = new Uint8Array(daemon.serverHello);
    broken[100] = (broken[100] ?? 0) ^ 1;
    expect(() => handshake.finish(broken)).toThrow('handshake signature invalid');
  });

  test('a signature over a different transcript does not verify', () => {
    const signingKey = ed25519.utils.randomSecretKey();
    const verifyingKey = ed25519.getPublicKey(signingKey);
    const transcript = new Uint8Array(32).fill(1);
    const signature = ed25519.sign(transcript, signingKey);
    expect(() => verifyTranscript(verifyingKey, transcript, signature)).not.toThrow();
    expect(() => verifyTranscript(verifyingKey, new Uint8Array(32).fill(2), signature))
      .toThrow('handshake signature invalid');
  });

  test('a low-order public key is refused rather than producing a known secret', () => {
    // An all-zero client ephemeral makes every agreement all-zero. x25519
    // rejects the input outright, which is the same refusal one layer down.
    const clientStatic = x25519.getPublicKey(x25519.utils.randomSecretKey());
    const clientHello = new Uint8Array(CLIENT_HELLO_LEN);
    clientHello.set(clientStatic, 0);
    expect(() => daemonRespond(clientHello)).toThrow();
  });

  test('every handshake draws fresh keys', () => {
    const one = ClientHandshake.start();
    const two = ClientHandshake.start();
    expect(hex(one.clientHello)).not.toBe(hex(two.clientHello));
  });
});

describe('bytesEqual', () => {
  test('compares content and length', () => {
    expect(bytesEqual(new Uint8Array([1, 2]), new Uint8Array([1, 2]))).toBe(true);
    expect(bytesEqual(new Uint8Array([1, 2]), new Uint8Array([1, 3]))).toBe(false);
    expect(bytesEqual(new Uint8Array([1, 2]), new Uint8Array([1, 2, 3]))).toBe(false);
  });
});
