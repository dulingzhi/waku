/**
 * Pairing a client to a relayed daemon with a code a person can carry.
 *
 * Transcribed from `crates/waku-protocol/src/pairing.rs`. A code carries no
 * secret — it is shown on screen, photographed, and pasted into chats — so the
 * design assumes an attacker can read it and makes their job equivalent to
 * guessing a 128-bit key.
 *
 * ```text
 * waku://<relay-host>:<port>/<session-id>#<fingerprint>
 * ```
 *
 * The fingerprint is the part that authenticates anything, and it lives in the
 * URI fragment: a URL-aware tool strips a fragment, so a code pasted into a web
 * form loses exactly the part that would have verified the daemon, and the user
 * sees a pairing failure rather than a silent downgrade.
 */

import { sha256 } from '@noble/hashes/sha2.js';

/** `pairing.rs:52`. `waku://` so a code cannot be pasted into a browser by
 * accident, where the fragment would be dropped. */
const SCHEME = 'waku://';

/**
 * `pairing.rs:57`. 16 hex characters = 64 bits: short enough for a person to
 * type, long enough that an attacker cannot grind a colliding key before the
 * session expires.
 */
export const FINGERPRINT_LEN = 16;

/** `pairing.rs:145`. Must match the relay's own `parse_path` rule. */
export const MAX_SESSION_ID_LEN = 128;

export interface PairingCode {
  /**
   * The relay's `host:port`, without a scheme. The client chooses `ws` or
   * `wss` itself rather than trusting the code, because a code that could
   * downgrade to plaintext is a code an attacker rewrites.
   */
  relay: string;
  sessionId: string;
  /** Lowercase hex, {@link FINGERPRINT_LEN} characters. */
  fingerprint: string;
}

/** A pairing code that could not be read. The message is shown to the user. */
export class PairingCodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'PairingCodeError';
  }
}

/** `PairingCode::render` (`pairing.rs:82`). */
export function renderPairingCode(code: PairingCode): string {
  return `${SCHEME}${code.relay}/${code.sessionId}#${code.fingerprint}`;
}

/**
 * `PairingCode::parse` (`pairing.rs:94`).
 *
 * Rejects anything it does not fully understand rather than filling in
 * defaults: a code with a missing fingerprint must fail here, because a
 * connection that proceeds without a pin is exactly the attack this prevents.
 */
export function parsePairingCode(text: string): PairingCode {
  const trimmed = text.trim();
  if (!trimmed.startsWith(SCHEME)) {
    throw new PairingCodeError(`a pairing code starts with ${SCHEME}`);
  }
  const rest = trimmed.slice(SCHEME.length);

  // The fragment splits off first: everything before it is what a server would
  // see, and it has to be well-formed on its own.
  const hashIndex = rest.indexOf('#');
  if (hashIndex < 0) {
    throw new PairingCodeError("a pairing code ends with #<fingerprint>; this one has no '#'");
  }
  const withoutFragment = rest.slice(0, hashIndex);
  const fragment = rest.slice(hashIndex + 1);

  const fingerprint = fragment.trim().toLowerCase();
  if (fingerprint.length !== FINGERPRINT_LEN) {
    throw new PairingCodeError(
      `a pairing code's fingerprint is ${FINGERPRINT_LEN} hex characters; ` +
        `this one has ${fingerprint.length}`,
    );
  }
  if (!/^[0-9a-f]+$/.test(fingerprint)) {
    throw new PairingCodeError("a pairing code's fingerprint must be hex characters");
  }

  // `<relay>/<session-id>`, where the relay may contain no path of its own.
  // `rsplit_once('/')` is what the Rust side uses, so the session id is the
  // last segment and a relay carrying slashes is rejected below.
  const slashIndex = withoutFragment.lastIndexOf('/');
  if (slashIndex < 0) {
    throw new PairingCodeError('a pairing code is <relay>/<session-id>#<fingerprint>');
  }
  const relay = withoutFragment.slice(0, slashIndex);
  const sessionId = withoutFragment.slice(slashIndex + 1);
  if (!relay) {
    throw new PairingCodeError('a pairing code needs a relay address before the session id');
  }
  if (relay.includes('/')) {
    throw new PairingCodeError("a pairing code's relay address must be host:port, with no path");
  }
  if (!sessionId) {
    throw new PairingCodeError('a pairing code needs a session id');
  }

  return { relay, sessionId, fingerprint };
}

/** `is_valid_session_id` (`pairing.rs:152`). */
export function isValidSessionId(sessionId: string): boolean {
  if (!sessionId) return false;
  if (sessionId.length > MAX_SESSION_ID_LEN) return false;
  for (const char of sessionId) {
    const isAlnum =
      (char >= '0' && char <= '9') ||
      (char >= 'A' && char <= 'Z') ||
      (char >= 'a' && char <= 'z');
    if (!isAlnum && char !== '-' && char !== '_') return false;
  }
  return true;
}

/**
 * Whether `host[:port]` names the local machine (`pairing.rs:169`).
 *
 * Only the textual forms that unambiguously mean loopback. Anything else —
 * including a hostname that resolves to loopback — is treated as remote, which
 * errs toward `wss`.
 */
function isLoopbackHost(relay: string): boolean {
  let host = relay;
  if (relay.startsWith('[')) {
    // `[::1]:8080`
    const close = relay.indexOf(']');
    if (close < 0) return false;
    host = relay.slice(1, close);
  } else {
    const colon = relay.lastIndexOf(':');
    if (colon >= 0) {
      const port = relay.slice(colon + 1);
      if (port.length > 0 && /^[0-9]+$/.test(port)) host = relay.slice(0, colon);
    }
  }
  return host === '127.0.0.1' || host === 'localhost' || host === '::1';
}

/**
 * Which WebSocket scheme a client should use to reach this relay
 * (`PairingCode::relay_scheme`, `pairing.rs:152`).
 *
 * **Never taken from the code.** A code travels through chat windows and
 * photographs, so a code that could name `ws://` would be one an attacker
 * rewrites to strip the transport encryption — and the fingerprint check would
 * still pass, because it authenticates the daemon rather than the path.
 *
 * A loopback relay gets `ws://`: it never leaves the machine, and demanding a
 * certificate there would push self-hosters toward copying one instead of
 * toward the safe default. Everything else gets `wss://`.
 */
export function relayScheme(code: PairingCode): 'ws' | 'wss' {
  return isLoopbackHost(code.relay) ? 'ws' : 'wss';
}

/** The full URL a client dials (`PairingCode::relay_url`, `pairing.rs:161`). */
export function relayUrl(code: PairingCode): string {
  return `${relayScheme(code)}://${code.relay}`;
}

/**
 * `fingerprint_of` (`pairing.rs:171`).
 *
 * Sixteen lowercase hex characters: the first 8 bytes of SHA-256 over the
 * Ed25519 public key. Both ends compute this from the same bytes — the daemon
 * from its own key when it shows a code, the client from the key the handshake
 * proved — so a client comparing anything else (base64, a different truncation)
 * would reject a correct pairing in a way that looks like an attack.
 */
export function fingerprintOf(ed25519Public: Uint8Array): string {
  const digest = sha256(ed25519Public);
  return [...digest.subarray(0, FINGERPRINT_LEN / 2)]
    .map((byte) => byte.toString(16).padStart(2, '0'))
    .join('');
}

/**
 * `authentication_string` (`pairing.rs:195`).
 *
 * Twelve decimal digits in three groups of four, derived from successive
 * bytes of the handshake transcript. This is the value a user compares once,
 * and it is the only check that catches a relay running one handshake per
 * side: two handshakes produce two different strings.
 *
 * **It comes from the transcript, never from a session key.** A hash of the
 * key would be reproducible from a key an attacker had already recovered and
 * would not be bound to what was negotiated; the transcript covers every
 * public key both sides exchanged.
 */
export function authenticationString(transcriptHash: Uint8Array): string {
  const digits: string[] = [];
  for (let index = 0; index < 12; index += 1) {
    if (index > 0 && index % 4 === 0) digits.push(' ');
    digits.push(String(transcriptHash[index]! % 10));
  }
  return digits.join('');
}

/**
 * Whether `proven` is the key the pairing code names.
 *
 * The single place this decision is made. **Compares the whole fingerprint,
 * never a prefix**: a check that stopped early would accept a daemon whose key
 * shares the first characters of the real one, which is what an attacker
 * grinding a key aims for.
 */
export function pairingFingerprintMatches(
  fingerprint: string,
  provenEd25519: Uint8Array,
): void {
  const actual = fingerprintOf(provenEd25519);
  if (actual !== fingerprint) {
    throw new PairingCodeError(
      'the daemon at this pairing code is not the one it claims: ' +
        `expected fingerprint ${fingerprint}, got ${actual}`,
    );
  }
}
