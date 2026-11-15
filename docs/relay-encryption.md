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

## Key storage

The daemon's identity lives at `~/.waku/identity.key` as versioned JSON:

```json
{"version": 1, "x25519_secret": "<base64>", "ed25519_secret": "<base64>"}
```

Versioned rather than raw bytes so rotation can add fields later without a
format break, and `deny_unknown_fields` so a newer format is refused rather than
half-understood.

**A missing file generates; an unreadable one is an error.** That distinction is
the whole point of the module's error type: a corrupt identity file must never
be silently replaced, because replacing it invalidates every client that ever
paired with this daemon, and nothing on screen would explain why.

The write path never stages a temporary file: the target is claimed with an
exclusive create (`O_EXCL` on unix, `CREATE_NEW` on Windows) and the key is
written straight into it, owner-only from the moment the inode exists rather
than chmodded afterwards. A crash can therefore leave either the previous
identity intact or a zero-length file that the next start rejects as corrupt —
never a complete key at permissive modes, which is what a create-chmod-rename
sequence invites.

Concurrent first starts are the other case: two daemons generating at once once
meant the later rename overwrote the earlier identity, silently invalidating
every client the first one paired with. Because the create is exclusive, exactly
one writer wins the name and the loser gets `AlreadyExists` and *loads* what
won rather than replacing it — so both processes end up holding the same
identity, and the key on disk is the one a client will pair against next.

A `.tmp` file from the older build is still removed on load: that build could
crash between create and rename, leaving a complete private key at whatever
mode its umask allowed.

## Dependencies, and what they are worth

Three crates are added, plus three promoted from the tree. The dalek family
supplies key agreement and signing; RustCrypto supplies the AEAD and the key
derivation.

| Crate | Purpose | Audit |
| --- | --- | --- |
| `x25519-dalek` 2 | key agreement, with `diffie_hellman` and the RFC 7748 all-zero check | No standalone audit; the underlying `curve25519-dalek` was audited by NCC Group in 2020 |
| `ed25519-dalek` 2.2 | identity signing | Same lineage, same audit |
| `chacha20poly1305` 0.10 | records | No published third-party audit |
| `hkdf` 0.12 | session key derivation | No published audit; small enough to read |
| `zeroize` | clearing key material from memory | No audit; a memory-hygiene crate, not a cryptographic one |
| `getrandom` 0.3 | OS entropy | Not a cryptographic implementation; wraps the OS |

**Two of these have no third-party audit report, and that is written down
rather than glossed over.** It is the current state of the Rust ecosystem
rather than a mark against these crates specifically — `ring`, which the first
draft of this plan preferred, has no published audit either; its reputation
comes from Google's internal use and BoringSSL's history.

What follows from that: **the risk sits in how these are composed, not in which
crate was picked.** A reviewed library used wrongly is worse than an unreviewed
one used correctly, and the composition is small enough to be reviewed by hand
— one key agreement, one derivation, one AEAD, one signature check.

Deliberately not done: reaching for `curve25519-dalek`'s `mul_clamped` directly.
The primitive is already compiled, but it has no `diffie_hellman` and no
contributory check, and RFC 7748 requires rejecting an all-zero shared secret.
Skipping the wrapper to avoid a dependency would skip the check with it.

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
