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

Worth checking before proposing a new crypto crate: the tree carries these.

| Crate | Version | Brought in by | Usable directly? |
| --- | --- | --- | --- |
| `ring` | 0.17.14 | rustls, through `tungstenite` | **No** — transitive |
| `chacha20` | 0.10.1 | RustCrypto stack | **No** — transitive |
| `hkdf` | 0.12.4 | RustCrypto stack | **No** — transitive |
| `sha2` | 0.10.9 | RustCrypto stack | **No** — transitive |
| `ed25519-dalek` | 2.x | the release updater | Yes, but Linux and Windows only |

A transitive crate cannot be named from our own code; each one added as a direct
dependency is a real change even when the version already compiles. `ring` does
cover X25519, ChaCha20-Poly1305 and HKDF, and `ed25519-dalek` is already a
first-level dependency for update signatures — which makes it the natural
identity key rather than a second signing crate.

**`snow` is the recommendation for Noise**, but it is not free: it brings
`x25519-dalek`, `chacha20poly1305` and a hash, and it is one more cryptographic
surface in an app that ships on three platforms. That dependency review is
step 0, not something to discover mid-implementation.

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

## The one unknown that can double the estimate

**The TypeScript clients need their own Noise implementation.** `snow` is Rust
only; the browser and React Native cannot use it. Whether a mature JS or WASM
implementation exists, and which one, is unverified — and if none is suitable,
step 2 grows into writing and reviewing a handshake implementation in
TypeScript, which is a different kind of work from the rest of this plan.

That question should be answered in step 0.

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
written rather than during:

1. **`snow` or `ring` directly?** A reviewed protocol composition against
   primitives already in the tree. The second option is more code to get right
   and easier to review line by line.
2. **Where does the daemon's long-lived identity key live**, and how is it
   rotated after a compromise? Without one, every pairing is trust-on-first-use.
3. **Single-use pairing codes, or reusable until they expire?**
4. **What happens on a device that cannot hold a hardware-backed key** — the
   mobile client is the open case.
