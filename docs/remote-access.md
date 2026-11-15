# Remote access: relay and end-to-end encryption

How Waku should let a daemon on your own machine be reached from anywhere,
through a relay that cannot read or replay what passes through it.

This is a design document, not a description of shipped behaviour. Nothing in
it exists yet. It is written first because the security shape has to settle
before the transport does — the current wire protocol cannot survive an
untrusted middle hop in its present form.

## The problem the current shape has

The daemon authenticates a client with a bearer token carried in the clear
inside the first message:

```rust
ClientMessage::Hello {
    protocol_version: u32,
    token: String,
    client_id: Uuid,
    resume_from: Vec<ReplayCursor>,
}
```

`crates/waku-core/src/server.rs` compares it with `token_matches`, and the
desktop's Settings → Daemon page hands that same token out. It grants full
control of the daemon host: tasks, files, Git, and a shell.

Today that is defensible, because the token only ever crosses a link the user
controls — loopback, a private network, or a TLS tunnel the user terminates.
`tungstenite` already supports `wss://` with native roots
(`crates/waku-client/Cargo.toml`), and the daemon refuses a non-loopback bind
unless `--allow-non-loopback` is passed.

It stops being defensible the moment a third party forwards the connection. An
operator of that hop would see the token, could replay it, and could read every
task, transcript and terminal byte. So:

**End-to-end encryption is a precondition of a relay, not an enhancement to it.**
The relay work is not started until the encryption layer exists and is proven.

## Why a WebSocket relay rather than WebRTC

Both were considered. WebSocket wins on this project's specific constraints.

| Concern | WebSocket relay | WebRTC |
| --- | --- | --- |
| Client change | the client already speaks `wss://`; a relay is one more hop | a new connection stack: ICE, DTLS, SCTP data channels |
| Payload fit | text streams and terminal bytes, where latency tolerance is generous | optimised for low-latency media, which this is not |
| NAT traversal | outbound 443 works through corporate firewalls and reverse proxies | hole punching fails often enough that TURN relay is needed anyway |
| Three clients | one code path reused by desktop, web and mobile | each client adapts its own WebRTC stack |
| Failure mode | a dropped socket reconnects | partial connectivity, ICE restarts, fallback paths |

WebRTC's real advantage is peer-to-peer media with low latency. Remote control
of a coding agent needs neither, and the fallback path for a failed hole punch
is a relay — which means building the relay anyway.

The one thing kept from the WebRTC world is its threat model: assume the
transport is hostile, and make the applications speak end to end regardless.

## Shape

```
desktop / web / mobile          relay                 daemon host
        |                         |                        |
        |-- wss, opaque bytes --->|<--- wss, opaque bytes -|
        |                         |                        |
        +------- encrypted -------+------- encrypted ------+
                  (relay cannot read either half)
```

The daemon dials **out** to the relay. This is the point of the design: a
machine behind a home router, a corporate firewall or a CGNAT never needs to
accept an inbound connection or open a port.

The relay's whole job is to pair two sockets and move bytes. It holds:

- a session identifier, short-lived
- two connected sockets
- byte counters, for abuse limits

It does **not** hold a token, a transcript, a command, a path, or a public key
it could use to impersonate either end.

## Encryption

An application layer inside the WebSocket, so the relay's cooperation is never
required and never trusted.

**Primitives.** X25519 for key agreement, ChaCha20-Poly1305 for records,
HKDF-SHA256 for the key schedule. These are the same choices TLS 1.3 makes and
every Rust implementation is well audited. The Noise Protocol Framework
(`Noise_XX` or `Noise_IK`) is the preferred construction because it is a
reviewed composition rather than a hand-rolled one; a hand-rolled variant is
acceptable only if a reviewer signs off on the transcript binding.

**Pairing.** The user's device and the daemon must agree on a key without the
relay learning it.

1. The daemon shows, in Settings → Daemon, a short-lived pairing code and a QR
   code. Both encode the relay address, a session id, and the daemon's
   ephemeral public key.
2. The client connects to the relay, presents the session id, and completes the
   key agreement with that public key.
3. Both sides derive a session key and display a short authentication string.
   The user compares it once.
4. Only then does the client present its daemon token — encrypted.

Step 4 matters: **the token becomes an application-layer credential rather than
a transport credential.** The relay never sees it, and a successful handshake
does not by itself grant control.

**Forward secrecy.** Ephemeral keys per connection, so a later compromise of a
device does not decrypt recorded traffic.

**Replay.** Every frame carries a monotonically increasing counter inside the
authenticated payload; the receiver rejects a repeated or out-of-order frame.
This also covers the `resume_from` replay-cursor path, which must be
authenticated for the same reason.

**What this does not protect.** A compromised endpoint, a hostile daemon host,
or a user who hands out a pairing code. The design protects the path, and says
so rather than implying more.

## Protocol impact

The current `Hello` carries the token in the clear and assumes a trusted link.
Under a relay it must change. Two options:

**Option A — encrypt the existing envelope.** Wrap the whole WebSocket payload
in an encrypted record. `Hello` keeps its shape and simply becomes ciphertext.
The protocol version does not change. Cheapest, and the relay sees nothing but
sizes and timing.

**Option B — a handshake phase before `Hello`.** Add `PROTOCOL_VERSION` 8 with a
key-agreement exchange before the existing messages, so the token never appears
in a plaintext-capable frame even in principle. Cleaner against a future
inspection requirement, but it touches the contract every client generates
types from.

**Recommendation: Option A.** The security property is identical — after the
record layer, `Hello` is never plaintext — and it avoids regenerating types for
three clients. Option B becomes worth its cost only if the daemon must prove
something to the relay itself.

Either way, `crates/waku-protocol` gains a transport-independent record layer
and the existing seven-version contract is left alone.

## Abuse and operations

A relay forwards full-control sessions, so it needs limits it can enforce
without reading content:

- per-session byte and duration caps, since it knows sizes and times
- a connection rate limit per source
- no persistent storage of payloads, ever
- session ids that expire whether or not they were used

The relay must also be able to say "no" — a self-hosted relay is the supported
default, and a public one is a convenience, never a requirement.

## Staging

Each step is useful on its own, and each is judged before the next begins.

**Step 1 — self-hosted relay, trusted.** The user runs the relay. Pairing works,
the daemon dials out, the client connects. Encryption may be deferred here
because the operator already controls both ends. Proves the transport shape.

**Step 2 — end-to-end encryption.** The record layer and pairing land. From
here the relay operator is untrusted by construction, and step 1's relay
becomes safe to hand to someone else.

**Step 3 — public relay.** Only after step 2. Adds abuse limits and the
operational work a shared relay implies.

Reversing this order, or shipping a public relay without step 2, would put
users' full-control tokens in an operator's hands.

**Done when** a client reaches a daemon behind a NAT through the relay, the
relay's operator cannot read a task title from the traffic, and a replayed
frame is rejected.

## Open questions

These need answers before implementation, not during it:

1. Where does the daemon's long-lived identity key live, and how does the user
   rotate it after a compromise?
2. Should a pairing code be single-use, or reusable until it expires?
3. Does the mobile client hold a hardware-backed key, and what happens on a
   device that does not?
4. Who runs the public relay, and what does its abuse policy say?
