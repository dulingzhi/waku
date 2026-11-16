/**
 * The encrypted record layer and handshake, byte-compatible with
 * `crates/waku-protocol/src/record.rs`.
 *
 * This is a transcription, not a design: every constant and ordering below is
 * copied from the Rust implementation because the two ends derive one key and
 * frame one record, and a single differing byte means the handshake never
 * completes. When `record.rs` changes, this changes with it.
 *
 * Pure JS (via `@noble`), so it runs in a browser and in React Native/Hermes.
 * Nothing here reaches for `crypto.subtle`, which Hermes does not have.
 */

import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { ed25519, x25519 } from '@noble/curves/ed25519.js';
import { hkdf } from '@noble/hashes/hkdf.js';
import { sha256 } from '@noble/hashes/sha2.js';

import { authenticationString } from './pairing';

/** `record.rs:23-28`. */
const NONCE_LEN = 12;
const TAG_LEN = 16;
const KEY_LEN = 32;
/** `counter (8) || ciphertext || tag (16)`; the direction prefix lives in the
 * nonce, not on the wire. */
const HEADER_LEN = 8;

/** `record.rs:32`. Mixed into every transcript; changing it makes signatures
 * from one protocol generation fail against another. */
const PROTOCOL_LABEL = asciiBytes('waku-relay-record-v1');

/** `record.rs:36`. Binds a transcript hash to the step it was signed at. */
const TRANSCRIPT_LABEL = asciiBytes('waku-handshake-transcript-v1');

/** `handshake::CLIENT_HELLO_LEN` and `SERVER_HELLO_LEN` (`record.rs:338,348`). */
export const CLIENT_HELLO_LEN = 64;
export const SERVER_HELLO_LEN = 160;

/** `Direction::prefix` / `Direction::info` (`record.rs:50-62`). */
const C2S_PREFIX = Uint8Array.from([0, 0, 0, 1]);
const S2C_PREFIX = Uint8Array.from([0, 0, 0, 2]);
const C2S_INFO = asciiBytes('c2s');
const S2C_INFO = asciiBytes('s2c');

export type Direction = 'clientToServer' | 'serverToClient';

/** The protocol version bound into every record's associated data. Comes from
 * the generated protocol constants so the two cannot drift. */
import { PROTOCOL_VERSION } from './generated/constants';

function asciiBytes(text: string): Uint8Array {
  const out = new Uint8Array(text.length);
  for (let index = 0; index < text.length; index += 1) out[index] = text.charCodeAt(index) & 0xff;
  return out;
}

function concat(parts: readonly Uint8Array[]): Uint8Array {
  let length = 0;
  for (const part of parts) length += part.length;
  const out = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

/** `u64` big endian, the counter's wire form and its nonce form. */
function u64be(value: bigint): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, value, false);
  return out;
}

function u32be(value: number): Uint8Array {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, value, false);
  return out;
}

function readU64be(bytes: Uint8Array): bigint {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getBigUint64(0, false);
}

/**
 * `nonce_for` (`record.rs:269`): `direction_prefix (4) || counter (8, BE)`.
 *
 * The prefix is authenticated twice over — inside the nonce the AEAD consumes,
 * and as the key selector — so a record reflected back at its own sender fails
 * both ways.
 */
function nonceFor(direction: Direction, counter: bigint): Uint8Array {
  const nonce = new Uint8Array(NONCE_LEN);
  nonce.set(direction === 'clientToServer' ? C2S_PREFIX : S2C_PREFIX, 0);
  nonce.set(u64be(counter), 4);
  return nonce;
}

/**
 * `associated_data` (`record.rs:277`): `protocol_version (4, BE) ||
 * plaintext_length (4, BE)`.
 *
 * A length that does not fit in four bytes would be silently truncated in the
 * authenticated value, so two different lengths would share one AAD.
 */
function associatedData(plaintextLength: number): Uint8Array {
  if (plaintextLength > 0xffff_ffff) {
    throw new Error('record plaintext does not fit in the 4-byte length field');
  }
  const out = new Uint8Array(8);
  out.set(u32be(PROTOCOL_VERSION), 0);
  out.set(u32be(plaintextLength), 4);
  return out;
}

function keyInfo(direction: Direction): Uint8Array {
  return direction === 'clientToServer' ? C2S_INFO : S2C_INFO;
}

/** Zeroize a key once it is no longer needed. Best effort in a GC language. */
function wipe(bytes: Uint8Array): void {
  bytes.fill(0);
}

/**
 * The two directional keys a completed handshake produces (`record.rs:69`).
 *
 * Split rather than one key used both ways: with a single key an attacker who
 * can reorder frames reflects a peer's own record back at it and it decrypts.
 */
export class SessionKeys {
  readonly clientToServer: Uint8Array;
  readonly serverToClient: Uint8Array;
  /**
   * The Ed25519 key the daemon proved it holds. Surfaced so a first-time
   * client can pin what it saw.
   */
  readonly peerEd25519: Uint8Array | null;
  /**
   * The transcript both sides hashed, kept so the short authentication string
   * can be derived from it (`record.rs:77-83`).
   *
   * Not a secret: it is a digest of public keys that crossed the wire. Retained
   * rather than recomputed because recomputing needs both sides' ephemeral
   * keys, and the point of the string is to be checkable after the handshake
   * has already consumed them.
   */
  readonly transcriptHash: Uint8Array;

  private constructor(
    clientToServer: Uint8Array,
    serverToClient: Uint8Array,
    peerEd25519: Uint8Array | null,
    transcriptHash: Uint8Array,
  ) {
    this.clientToServer = clientToServer;
    this.serverToClient = serverToClient;
    this.peerEd25519 = peerEd25519;
    this.transcriptHash = transcriptHash;
  }

  /**
   * `SessionKeys::derive` (`record.rs:84`).
   *
   * `HKDF-SHA256(salt = transcript_hash, ikm = ikm)`, then one `expand` per
   * direction. The salt is the transcript hash, not empty: that is what binds
   * the keys to the exchanged public keys.
   */
  static derive(ikm: Uint8Array, transcriptHash: Uint8Array): SessionKeys {
    if (transcriptHash.length !== 32) {
      throw new Error(`transcript hash is ${transcriptHash.length} bytes, expected 32`);
    }
    return new SessionKeys(
      hkdf(sha256, ikm, transcriptHash, C2S_INFO, KEY_LEN),
      hkdf(sha256, ikm, transcriptHash, S2C_INFO, KEY_LEN),
      null,
      transcriptHash,
    );
  }

  private withPeerEd25519(peer: Uint8Array): SessionKeys {
    return new SessionKeys(
      this.clientToServer,
      this.serverToClient,
      peer,
      this.transcriptHash,
    );
  }

  /**
   * The short authentication string a user compares once
   * (`SessionKeys::authentication_string`, `record.rs:118`).
   *
   * **Both sides must be shown this value.** It is the only check that covers
   * the connection actually in progress, rather than the daemon's identity,
   * which the pairing fingerprint already covers.
   */
  authenticationString(): string {
    return authenticationString(this.transcriptHash);
  }

  /** A sealer for records travelling in `direction`. */
  sealer(direction: Direction): Sealer {
    return new Sealer(this.keyFor(direction), direction);
  }

  /** An opener for records arriving from `direction`. */
  opener(incoming: Direction): Opener {
    return new Opener(this.keyFor(incoming), incoming);
  }

  /**
   * Seal outgoing and open incoming for **the client** (`record.rs:132`):
   * clients encrypt with `c2s` and decrypt with `s2c`.
   */
  clientSide(): { sealer: Sealer; opener: Opener } {
    return {
      sealer: this.sealer('clientToServer'),
      opener: this.opener('serverToClient'),
    };
  }

  private keyFor(direction: Direction): Uint8Array {
    return direction === 'clientToServer' ? this.clientToServer : this.serverToClient;
  }

  /** Best-effort key erasure; the object is unusable afterwards. */
  destroy(): void {
    wipe(this.clientToServer);
    wipe(this.serverToClient);
  }

  /** Internal: build the client half with the daemon's key attached. */
  static deriveForClient(
    ikm: Uint8Array,
    transcriptHash: Uint8Array,
    peerEd25519: Uint8Array,
  ): SessionKeys {
    const keys = SessionKeys.derive(ikm, transcriptHash);
    return keys.withPeerEd25519(peerEd25519);
  }
}

/**
 * Seals plaintext into records (`record.rs:156`). One direction per instance.
 */
export class Sealer {
  private readonly key: Uint8Array;
  private readonly direction: Direction;
  /**
   * The counter of the last record sealed; the next record uses `last + 1`, so
   * the first is 1. A counter starting at 0 would make the first nonce look
   * like an uninitialised one.
   */
  private last = 0n;

  constructor(key: Uint8Array, direction: Direction) {
    if (key.length !== KEY_LEN) {
      throw new Error(`record key is ${key.length} bytes, expected ${KEY_LEN}`);
    }
    this.key = key;
    this.direction = direction;
  }

  /**
   * Seal one record: `counter (8) || ciphertext || tag (16)`.
   *
   * The counter appears twice — once on the wire, once inside the nonce — and
   * the AEAD authenticates the nonce, so moving the wire counter without the
   * matching nonce fails to open.
   */
  seal(plaintext: Uint8Array): Uint8Array {
    // Wrapping would reuse a nonce, which is catastrophic: refuse rather than
    // wrap. 2^64 records cannot happen within a connection's lifetime.
    if (this.last === 0xffff_ffff_ffff_ffffn) {
      throw new Error('record counter exhausted; refusing to reuse a nonce');
    }
    const counter = this.last + 1n;
    const nonce = nonceFor(this.direction, counter);
    const ciphertext = chacha20poly1305(this.key, nonce, associatedData(plaintext.length))
      .encrypt(plaintext);
    this.last = counter;
    return concat([u64be(counter), ciphertext]);
  }

  /** The counter of the last sealed record; 0 before the first. */
  get counter(): bigint {
    return this.last;
  }
}

/**
 * Opens records sealed for its direction (`record.rs:213`). One direction per
 * instance.
 */
export class Opener {
  private readonly key: Uint8Array;
  private readonly direction: Direction;
  /**
   * The last counter accepted; the next must be exactly `last + 1`. Buffering
   * an out-of-order record would make the counter's guarantee ambiguous, so a
   * gap ends the connection instead.
   */
  private last = 0n;

  constructor(key: Uint8Array, direction: Direction) {
    if (key.length !== KEY_LEN) {
      throw new Error(`record key is ${key.length} bytes, expected ${KEY_LEN}`);
    }
    this.key = key;
    this.direction = direction;
  }

  /**
   * Open one record.
   *
   * Errors carry only the reason and never any plaintext: a failed open is not
   * a place to leak what the record might have contained.
   */
  open(record: Uint8Array): Uint8Array {
    if (record.length < HEADER_LEN + TAG_LEN) {
      throw new Error('record too short to contain a header and tag');
    }
    const counter = readU64be(record.subarray(0, HEADER_LEN));
    if (counter !== this.last + 1n) {
      throw new Error('record out of order');
    }
    const ciphertext = record.subarray(HEADER_LEN);
    const plaintextLength = ciphertext.length - TAG_LEN;
    const nonce = nonceFor(this.direction, counter);
    let plaintext: Uint8Array;
    try {
      plaintext = chacha20poly1305(this.key, nonce, associatedData(plaintextLength))
        .decrypt(ciphertext);
    } catch {
      // One message for wrong key, tampering and replay: distinguishing them
      // tells an attacker which of the three they achieved.
      throw new Error('record authentication failed');
    }
    this.last = counter;
    return plaintext;
  }

  /** The counter of the last accepted record; 0 before the first. */
  get counter(): bigint {
    return this.last;
  }
}

/**
 * The transcript the daemon signs (`handshake::transcript_hash`,
 * `record.rs:596`).
 *
 * Six fields, each preceded by its own 8-byte big-endian length, in this
 * order: protocol label, transcript label, client static, client ephemeral,
 * server ephemeral, daemon static.
 */
export function transcriptHash(
  clientStatic: Uint8Array,
  clientEphemeral: Uint8Array,
  serverEphemeral: Uint8Array,
  daemonStatic: Uint8Array,
): Uint8Array {
  const hasher = sha256.create();
  for (const field of [
    PROTOCOL_LABEL,
    TRANSCRIPT_LABEL,
    clientStatic,
    clientEphemeral,
    serverEphemeral,
    daemonStatic,
  ]) {
    hasher.update(u64be(BigInt(field.length)));
    hasher.update(field);
  }
  return hasher.digest();
}

/**
 * Concatenate agreements with their lengths, for `ikm` (`record.rs:584`).
 *
 * Each part is preceded by one byte holding its length (32), so the total is
 * 99 bytes. The order is `shared, bound, bound_to_daemon` and must not change:
 * both sides compute the same three values because Diffie-Hellman is
 * symmetric.
 */
export function agreementBytes(parts: readonly Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.length * 33);
  let offset = 0;
  for (const part of parts) {
    if (part.length > 0xff) throw new Error('agreement is too long to length-prefix');
    out[offset] = part.length;
    offset += 1;
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

/**
 * Verify the daemon's signature over the transcript (`record.rs:301`).
 *
 * `ed25519.verify` returns false rather than throwing for a malformed
 * signature, which is what the caller wants: one error, no detail.
 */
export function verifyTranscript(
  verifyingKey: Uint8Array,
  transcript: Uint8Array,
  signature: Uint8Array,
): void {
  if (signature.length !== 64) {
    throw new Error('handshake signature invalid');
  }
  let valid = false;
  try {
    valid = ed25519.verify(signature, transcript, verifyingKey);
  } catch {
    valid = false;
  }
  if (!valid) throw new Error('handshake signature invalid');
}

/**
 * The client's half of a handshake in progress (`handshake::ClientHandshake`,
 * `record.rs:416`).
 *
 * The client's static key is generated fresh for each connection and never
 * persisted; the daemon does not use it as an identity.
 */
export class ClientHandshake {
  private readonly staticSecret: Uint8Array;
  private readonly ephemeralSecret: Uint8Array;
  private readonly clientStaticPublic: Uint8Array;
  private readonly clientEphemeralPublic: Uint8Array;

  private constructor(
    staticSecret: Uint8Array,
    ephemeralSecret: Uint8Array,
    clientStaticPublic: Uint8Array,
    clientEphemeralPublic: Uint8Array,
  ) {
    this.staticSecret = staticSecret;
    this.ephemeralSecret = ephemeralSecret;
    this.clientStaticPublic = clientStaticPublic;
    this.clientEphemeralPublic = clientEphemeralPublic;
  }

  /**
   * Begin a handshake, returning the instance and the bytes to send as
   * `client_hello`: `client_static_public (32) || client_ephemeral_public (32)`.
   */
  static start(): { handshake: ClientHandshake; clientHello: Uint8Array } {
    const staticSecret = x25519.utils.randomSecretKey();
    const ephemeralSecret = x25519.utils.randomSecretKey();
    const clientStaticPublic = x25519.getPublicKey(staticSecret);
    const clientEphemeralPublic = x25519.getPublicKey(ephemeralSecret);
    return {
      handshake: new ClientHandshake(
        staticSecret,
        ephemeralSecret,
        clientStaticPublic,
        clientEphemeralPublic,
      ),
      clientHello: concat([clientStaticPublic, clientEphemeralPublic]),
    };
  }

  /**
   * Complete the handshake against the daemon's reply
   * (`ClientHandshake::finish`, `record.rs:450`).
   *
   * `expectedEd25519` is the key the daemon must prove it holds. The check
   * happens *before* the signature so a substituted key fails with the
   * actionable message rather than a generic invalid signature.
   */
  finish(serverHello: Uint8Array, expectedEd25519?: Uint8Array | null): SessionKeys {
    if (serverHello.length !== SERVER_HELLO_LEN) {
      throw new Error(
        `server hello is ${serverHello.length} bytes, expected ${SERVER_HELLO_LEN}`,
      );
    }
    const daemonX25519 = serverHello.subarray(0, 32);
    const daemonEd25519 = serverHello.subarray(32, 64);
    const serverEphemeral = serverHello.subarray(64, 96);
    const signature = serverHello.subarray(96, 160);

    if (expectedEd25519 && !bytesEqual(expectedEd25519, daemonEd25519)) {
      throw new Error(
        'handshake signature invalid: the daemon is not the one you paired with',
      );
    }

    const transcript = transcriptHash(
      this.clientStaticPublic,
      this.clientEphemeralPublic,
      serverEphemeral,
      daemonX25519,
    );
    verifyTranscript(daemonEd25519, transcript, signature);

    // Order matters: shared, bound, bound_to_daemon.
    const shared = x25519.getSharedSecret(this.ephemeralSecret, serverEphemeral);
    const bound = x25519.getSharedSecret(this.staticSecret, serverEphemeral);
    const boundToDaemon = x25519.getSharedSecret(this.ephemeralSecret, daemonX25519);
    requireContributory(shared);
    requireContributory(bound);
    requireContributory(boundToDaemon);

    const ikm = agreementBytes([shared, bound, boundToDaemon]);
    const keys = SessionKeys.deriveForClient(ikm, transcript, daemonEd25519);
    wipe(ikm);
    wipe(shared);
    wipe(bound);
    wipe(boundToDaemon);
    wipe(this.staticSecret);
    wipe(this.ephemeralSecret);
    return keys;
  }
}

/**
 * Reject a Diffie-Hellman result nobody contributed to (`record.rs:319`,
 * RFC 7748 §6.1).
 *
 * A peer can send a low-order public key and force a known shared secret. The
 * all-zero result is the tell, and `x25519` throws for the most degenerate
 * inputs, but the check is ours to make on every agreement.
 */
function requireContributory(secret: Uint8Array): void {
  for (const byte of secret) {
    if (byte !== 0) return;
  }
  throw new Error('handshake refused a low-order public key');
}

export function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.length !== right.length) return false;
  let diff = 0;
  // Constant-time for equal lengths: the daemon's key is a comparison, not a
  // secret, but a length-agnostic loop costs nothing here.
  for (let index = 0; index < left.length; index += 1) diff |= left[index]! ^ right[index]!;
  return diff === 0;
}

export { asciiBytes };
