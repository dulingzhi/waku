# Remote access: implementation plan

How to build the relay and the end-to-end encryption `remote-access.md`
describes, in an order where each step is useful and each is judged before the
next begins.

The design settles the security shape. This settles the order of work.

## The order is encryption first, and that is deliberate

The obvious plan — build a relay, then secure it — is the wrong one, and
`remote-access.md` records why: a plaintext relay mode looks identical on screen
to an encrypted one, so users will run it against relays they do not control.
Encryption lands before any relay exists.

## What is already in the dependency tree — and what that is worth

Worth checking before proposing a new crypto crate — and worth checking
properly, because a first pass got this wrong.

| Crate | In `Cargo.lock` | Actually compiled for this target | Usable directly? |
| --- | --- | --- | --- |
| `ring` | 0.17.14 | **No** — `cargo tree -i ring` prints nothing | No |
| `hkdf` | 0.12.4 | **No** — only parent is `oo7`, a Linux keyring crate we do not use | No |
| `sha2` | 0.10.9 | Yes, through `ed25519-dalek` | No — transitive |
| `chacha20` | 0.10.1 | Yes, but only as `rand`'s PRNG backend | **No — it is not an AEAD** |
| `chacha20poly1305` | **absent** | — | — |
| `curve25519-dalek` | 4.1.3 | Yes, through `ed25519-dalek` | No — transitive |
| `ed25519-dalek` | 2.2.0 | Linux and Windows updater only | Yes, after promotion |
| `subtle` | 2.6 | Yes, a `waku-core` dependency | Yes |

Being in the lockfile is not being available. `ring` and `hkdf` sit in the lock
with no live dependency on this target, and `chacha20` is a stream cipher used
as `rand`'s backend — it carries no Poly1305 and no AEAD construction, so it
cannot stand in for `chacha20poly1305`, which is not in the tree at all.

What that leaves is one genuinely reusable base: `ed25519-dalek` already drags
in `curve25519-dalek`, `sha2`, `subtle` and `zeroize`. `x25519-dalek` is a thin
layer over the same `curve25519-dalek`, so choosing the dalek family for key
agreement adds no second curve implementation to audit.

**The missing X25519 layer is not optional even though the primitive is there.**
`curve25519-dalek` exposes `mul_clamped` but no `diffie_hellman` and no
all-zero check, and RFC 7748 requires a contributory-behaviour check against
low-order points. Reaching into the primitive to skip that is precisely the
mistake `x25519-dalek` exists to prevent.

**`snow` is the recommendation for Noise**, but it is not free: it brings
`x25519-dalek`, `chacha20poly1305` and a hash, and it is one more cryptographic
surface in an app that ships on three platforms. That dependency review is
step 0, not something to discover mid-implementation.

Two facts about `snow` worth having in writing before anyone relies on it:

- **It has not had a formal audit.** Its README says so in bold, and reads as a
  design statement rather than a hedge: what it offers is a careful
  implementation of a published specification, not a substitute for reviewing
  the protocol built on top of it. That review is this plan's step 0.
- **It fits the toolchain.** `snow` 0.10 targets edition 2024 and Rust 1.85;
  this workspace is edition 2024 on Rust 1.99, so no version negotiation is
  needed.

## The relay client is a new code path on the daemon side

The daemon cannot currently make an outbound TLS connection at all:
`crates/waku-core/Cargo.toml` enables `tungstenite` with `handshake` only, and
`crates/waku-core/src/server.rs` only ever accepts. A daemon that dials out
needs TLS client support and a reconnect loop — code that does not exist
anywhere in the crate today. That is the largest single-line underestimate in
the earlier plan.

## Steps

| Step | Content | Days | Depends on |
| --- | --- | --- | --- |
| 0 | Threat model, Noise implementation choice, authentication-string derivation, dependency review | 2-3 | — |
| 1 | Record layer in `waku-protocol`, Rust client and daemon | 8-10 | 0 |
| 2 | Record layer in the TypeScript clients | 6-8 | 1 |
| 3 | Pairing UI (text code, no QR) and the daemon identity key | 5-6 | 1 |
| 4 | `crates/waku-relay`, local loopback integration test | 5-6 | 1, 2 |
| 5 | Daemon outbound connection and reconnect | 4-5 | 4 |
| 6 | Address generation wired into three clients | 2-3 | 5 |

Steps 0 through 3 are 21-27 days and are the sensible first delivery: encrypted
transport, usable today, with no relay in the picture.

## The TypeScript side: researched, and the answer is mixed

This was the unknown that could double the estimate. It no longer is — but the
answer is not "use this library".

**What exists, and why none of them is a drop-in:**

- [`noise-handshake`](https://socket.dev/npm/package/noise-handshake) — the
  serialisation target for these experiments, and it fits: handshake state,
  `rx`/`tx` transport keys, and a `hash` field that is the handshake hash the
  authentication string needs. But it depends on `sodium-universal`, which is a
  **native module**. React Native needs a native build, and the web client would
  need a WASM bundle. Its health is also middling.
- [`relayly`](https://socket.dev/npm/package/relayly) — a whole relay product,
  not a primitive, and it says outright that it hand-writes Noise XX over
  `@noble` primitives because *"no maintained Noise library fits this exact
  cipher suite"*.
- [`@kangentic/protocol`](https://npmjs.com/package/@kangentic/protocol) — same
  pattern: pure TypeScript Noise over `@noble/curves`, `@noble/hashes` and
  `@noble/ciphers`, no native module, runs on Node and React Native. It also
  uses a short authentication string and documents the pairing ceremony.
- [`@stonygrey/torann`](https://socket.dev/npm/package/@stonygrey/torann) — an
  Expo module wrapping Swift and Kotlin Noise implementations. Native, so it is
  React Native only, excludes the web client, and requires iOS 18 / API 33.

**The pattern is the finding.** Every project that needed Noise in JavaScript
wrote it over `@noble` rather than adopting a Noise library, and the reasons are
consistent: the existing implementations pull native modules, and the ones that
do not are either unmaintained or not Noise as a library.

`@noble` itself is the solid half — audited, pure TypeScript, tree-shakable,
works on browser, Node and React Native with a `getRandomValues` polyfill on
Hermes. One project measured a full Noise IK handshake at 16 ms in Node and a
1 KiB frame at 0.09 ms, which is comfortably enough for a transcript.

**So step 2 has three routes**, none of them free:

| Route | Cost | Risk |
| --- | --- | --- |
| Adopt `noise-handshake` | Lowest | A native module in both clients, plus a web build path |
| Write Noise over `@noble` | 6-8 days, twice (web and mobile) | Hand-written handshake logic in the client that holds the token |
| Depend on a product like `relayly` or `@kangentic` | Lowest code | Ties this feature to a third party's roadmap and threat model |

A route has to be chosen in step 0, before the record layer is written, because
the Rust and TypeScript sides must agree on a wire format byte for byte.

## What this plan does not promise

- **A compromised endpoint.** The design protects the path. If the daemon host
  is hostile, encryption changes nothing about that.
- **Hiding metadata.** The relay still sees connection times, addresses and
  message sizes. That is inherent to relaying and is stated rather than papered
  over.
- **A token that never leaks.** It stops crossing the wire in the clear. Any
  client that stores it can still have it stolen.

## Open questions before step 2

Each of these changes the code, so they should be answered before it is
written rather than during.

1. **Which TypeScript route?** Researched above; still a choice between a native
   module, a hand-written Noise over `@noble`, and depending on a product. It
   decides how much hand-written handshake code sits in the client that holds
   the token.
2. **Where does the daemon's long-lived identity key live**, and how is it
   rotated after a compromise? Without one, every pairing is trust-on-first-use.
   The obvious shapes are a file at `~/.waku/identity.key` with 0600, or the
   system keychain. The first is portable and the second is where a macOS user
   expects a key; choosing one is a decision, not a detail.
3. **Single-use pairing codes, or reusable until they expire?** A single-use
   code cannot be replayed, but it also cannot be shown on a screen for someone
   to come back to later.
4. **Does the relay or the daemon mint the session id?** The daemon is simpler.
   The relay can reject an id already in use, which the daemon cannot know.

**Answered, and no longer open:** `snow` over a hand-rolled composition, and
`ring` cannot be used directly because it is transitive. The authentication
string comes from the Noise handshake hash, which both `snow`
(`get_handshake_hash`) and `noise-handshake` (`hash` in the completed state)
expose.
