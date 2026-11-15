# Relay encryption: public-key transport

How the relay carries a daemon's traffic without being able to read it, using
asymmetric encryption rather than a full handshake protocol.

This replaces the Noise-based plan in `remote-access-plan.md`. That plan treated
all three clients as peers that each needed a complete handshake state machine.
They are not peers. The desktop is the only place a daemon runs; web and mobile
are control surfaces that talk to it. That asymmetry is what makes a much
smaller scheme correct.

## The shape

```
desktop (daemon host)          relay                 web / mobile
        |                        |                        |
        |-- encrypted records -->|<-- encrypted records --|
        |                        |                        |
   holds private key        forwards bytes          holds only public key
```

The daemon generates a long-lived key pair once. Its public key is what a client
needs to talk to it — not a copy of the daemon's token.

## What each side does

**The daemon** holds an X25519 private key and an Ed25519 signing key, both
generated on first use and stored under `~/.waku/` with owner-only permissions.
The X25519 key does key agreement; the signing key lets a client confirm it is
talking to the same daemon it paired with.

**A client** holds the daemon's public keys after pairing. It does not need a
private key of its own to read what the daemon sends — the session key is
derived from a fresh ephemeral exchange, so forward secrecy still holds.

**The relay** holds nothing. It forwards bytes between two sockets matched by
session id.

## Per-connection key agreement

A long-lived key alone would mean every connection shares one key. Instead:

1. The client generates an ephemeral X25519 key pair for this connection.
2. It sends the public half, encrypted to the daemon's static public key.
3. The daemon combines its static private key with the client's ephemeral
   public key, and generates an ephemeral key pair of its own.
4. Both sides derive a session key through HKDF over the shared secret plus both
   ephemeral public keys — binding the transcript so the same inputs cannot
   produce a different key.
5. The daemon signs a transcript hash with its Ed25519 key. A client that has
   paired before checks the signature against the key it stored; this is what
   stops a relay from substituting its own key on a later connection.

That is one round trip, no state machine, and it gives forward secrecy: the
static key alone does not decrypt a recorded session.

## Records

ChaCha20-Poly1305 over each WebSocket binary message, with:

- a 64-bit counter in the associated data, so dropped, reordered or replayed
  records fail authentication
- a per-direction key, so the two streams cannot be cross-fed
- the plaintext length in the associated data, so a record cannot be truncated

**No fallback.** The record layer is required. A client that does not get it
closes the connection rather than continuing in the clear, because a layer an
attacker can strip is a layer they strip first. The negotiation lives in the
WebSocket subprotocol, below the protocol version, so no wire type changes.

## What this actually protects

| Attacker | Result |
| --- | --- |
| Passive relay operator | Sees ciphertext only |
| Active relay that tries to substitute keys | The signature check fails on any client that has paired before |
| Replay of a recorded session | Rejected by the counter and by the fresh ephemeral exchange |
| Relay that drops or reorders records | Authentication fails; the connection ends rather than silently losing data |

**The honest limit**: a client pairing for the first time has nothing to compare
against, so that first connection trusts whatever key it is given. This is
trust-on-first-use, and it is what the pairing code exists to narrow: the code
carries a fingerprint of the daemon's public key, so the first connection is
verified out of band rather than trusted blindly.

## Why not Noise

Noise is the right tool when both ends are peers that may each need to
authenticate to the other and neither has an established identity. Here the
daemon has an identity the client learns once, and the client only needs to read
and write. A full handshake protocol would add a state machine, a
specification-conformance surface, and — in JavaScript — a hand-written
implementation to review, all to provide properties this shape already has.

The pieces are smaller and easier to check: X25519, HKDF, ChaCha20-Poly1305, and
one signature verification.

## Cost

| Piece | Where | Days |
| --- | --- | --- |
| Key generation, storage, permissions | `crates/waku-core` | 2-3 |
| Record layer (Rust) | `waku-protocol` or a sibling crate | 4-5 |
| Key agreement and signing (Rust) | daemon side | 3-4 |
| Client side (Rust, for the desktop client) | `waku-client` | 2-3 |
| `@noble`-based client (TypeScript, shared by web and mobile) | `packages/waku-client` | 5-6 |
| Pairing code with a key fingerprint | desktop Settings → Daemon | 3-4 |
| Integration test: relay cannot read a task title | — | 2 |

Roughly 21-27 days, with the TypeScript side as one shared implementation rather
than two.

## Open questions

1. **Where does the daemon's private key live** — `~/.waku/identity.key` at 0600,
   or the system keychain? The first is portable across the three platforms; the
   second is where a macOS user expects a key to be. A hybrid is possible and is
   more code.
2. **How does the pairing code carry the key fingerprint** without becoming too
   long to retype? A shortened fingerprint is a weaker check, and how much
   weaker needs deciding rather than assuming.
3. **What rotates the daemon key**, and what happens to paired clients when it
   does? Without an answer, a compromised key is compromised forever.
