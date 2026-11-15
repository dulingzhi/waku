# The encrypted record layer

How a daemon and a client turn a WebSocket carrying JSON into one carrying
ciphertext, without either side being able to fall back to plaintext and without
the relay learning anything.

Companion to `relay-encryption.md`, which settles why the scheme is asymmetric
rather than a full handshake. This settles what the bytes look like.

## Where it sits

Today both ends speak `Message::Text` holding JSON — `crates/waku-core/src/server.rs`
reads at one point and writes at another, and `crates/waku-client/src/client.rs`
does the same. The record layer replaces that framing:

```
plaintext JSON  →  record  →  Message::Binary
```

Nothing above the record layer changes. `ClientMessage`, `ServerMessage`, the
replay cursor, and the resume path all keep their shapes; they are simply sealed
before they reach the socket.

**The socket carries binary from the first frame.** There is no negotiation
phase in which JSON is acceptable, because a phase like that is a downgrade
window. The choice is made in the WebSocket subprotocol header, before any frame
is sent.

## The handshake

One round trip, no state machine.

1. **The client** generates an ephemeral X25519 key pair, and sends:

   ```
   client_hello = ephemeral_public (32 bytes)
   ```

2. **The daemon** generates its own ephemeral pair, computes the shared secrets,
   and replies:

   ```
   server_hello = server_ephemeral_public (32) || signature (64)
   ```

   The signature covers a transcript hash: the protocol name as bytes, both
   ephemeral public keys, the daemon's static X25519 public key, and a label
   string. The label is what stops a signature from one context being replayed
   into another.

3. **Both sides** derive the session key:

   ```
   shared   = X25519(ephemeral, other_ephemeral)   // RFC 7748, all-zero rejected
   bound    = X25519(static, other_ephemeral)      // proves the daemon's identity
   key_seed = HKDF-SHA256(salt = transcript_hash, ikm = shared || bound)
   ```

   Two directions, two keys:

   ```
   client_to_server = HKDF(key_seed, info = "c2s")
   server_to_client = HKDF(key_seed, info = "s2c")
   ```

   Separate directions are not decoration. A single key shared both ways lets an
   attacker who can reorder frames reflect a peer's own record back at it, and
   it would decrypt.

4. **A client that has paired before** verifies the signature against the
   Ed25519 key it stored. One that has not has nothing to compare — see the
   pairing code, which carries a fingerprint of that key.

**Why both `shared` and `bound` go into the seed.** `shared` alone gives forward
secrecy but no authentication: any relay could run its own ephemeral exchange
with each side. `bound` alone would tie the session to the static key forever.
Together, a client that cannot forge `bound` cannot produce the same key, and the
static key never encrypts a record directly.

## A record

```
nonce (12) || ciphertext (n) || tag (16)
```

- **nonce**: 96-bit, formed as `direction_prefix (4) || counter (8, big endian)`
- **counter**: starts at 1 for the first record of a connection, increments per
  record, never reused within a connection
- **associated data**: `protocol_version (4, big endian) || plaintext_length (4,
  big endian)`

The associated data is authenticated but not encrypted. Version binding means a
record cannot be replayed into a connection speaking a different version, and
the length binding means a truncated record fails rather than decrypting to a
prefix.

**Replay and reordering.** The counter is part of the nonce, so a replayed
record reuses a nonce the receiver has already seen and the AEAD rejects it. The
receiver also checks that the counter is exactly one greater than the last
accepted one; a record that arrives out of order ends the connection rather
than being buffered, because buffering would make the counter's guarantee
ambiguous.

**Why a per-record nonce rather than a single counter field.** Putting the
counter in the nonce means an attacker cannot move it: it is inside the bytes
the AEAD authenticates through its nonce parameter.

## What ends a connection

A record that fails authentication is not retried and not logged with content.
The connection closes. The reasons worth distinguishing in a log line, without
any plaintext:

- `record authentication failed` — wrong key, tampering, or replay
- `record out of order` — a gap or a repeat
- `handshake signature invalid` — a relay substituted its key, or the daemon was
  reinstalled with a new identity

The client shows the last as a specific message, because "this daemon is not the
one you paired with" is actionable in a way that a generic failure is not.

## What is deliberately absent

- **No plaintext fallback.** Covered above; a flag that turns this off is a flag
  an attacker turns off.
- **No compression.** Compressing before encrypting leaks length patterns.
- **No record reordering or buffering.** See above.
- **No key ratcheting.** The session key lives for one connection. A connection
  that lasts long enough for a ratchet to matter is a separate design problem,
  and this one is already bounded by the daemon's reconnect behaviour.

## Test plan

The layer must be testable without a socket, so the codec is a pair of functions
over byte slices:

| Test | What it proves |
| --- | --- |
| Round trip | A sealed record opens to the same bytes |
| Two directions differ | Sealing the same plaintext in each direction yields different bytes |
| Counter reuse is rejected | Replaying a captured record fails to open |
| Reordering is rejected | Opening records out of order fails |
| Truncation is rejected | Cutting a record short fails, and does not yield a prefix |
| Wrong key is rejected | A record sealed under one key does not open under another |
| Length is bound | A record whose authenticated length disagrees with its content fails |
| Signature covers the transcript | A signature over a different transcript does not verify |
| All-zero shared secret | A low-order public key is refused rather than producing a key |

The last one is the RFC 7748 requirement that is easy to skip: without it a
peer can force a known shared secret.
