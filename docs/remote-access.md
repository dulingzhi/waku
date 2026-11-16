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

1. The daemon shows, in Settings → Daemon, a short-lived pairing code. It
   carries the relay address and a session id the **daemon** generated. It does
   **not** carry an ephemeral key: a key written down before a connection exists
   has stopped being ephemeral, and forward secrecy depends on it being created
   per connection.
2. The client connects to the relay, presents the session id, and completes the
   key agreement, generating its ephemeral key at that moment.
3. Both sides derive a session key and display a short authentication string.
   The user compares it once.
4. Only then does the client present its daemon token — encrypted.

Step 4 matters: **the token becomes an application-layer credential rather than
a transport credential.** The relay never sees it, and a successful handshake
does not by itself grant control.

**Why the session id does not need to be a secret the relay hands out.** An
earlier draft had the relay distribute it to both sides. It does not have to,
and having it generate the id would make the relay the authority on which
endpoint belongs to which session — a role it does not need. What actually stops
a relay from substituting its own daemon is the pinned Ed25519 key
(`record.rs:482`): a client that paired before refuses a `server_hello` carrying
any other key. A guessed session id therefore gets an attacker a connection that
fails at the handshake, not control of a session.

The relay still needs to know the id to pair the two sockets. That tells it two
sockets belong together and nothing else — no identity, no token, no key.


**The authentication string.** This is a security boundary, not a display
detail, and it must not be invented here. Two properties are required, and a
short hash over the session key has neither:

- It has to cover the **whole transcript** — every public key both sides
  exchanged — or an intermediary can run one handshake per side and sit between
  them with both comparisons passing.
- It has to be long enough that grinding a collision is not cheaper than
  attacking the handshake itself.

**What the implementation actually does, and where that differs from the plan.**
An earlier version of this section said the string is the first half of the
Noise handshake hash. Noise is not used: `record.rs` hand-rolls a one-round-trip
handshake, so there is no Noise handshake hash to read. The equivalent value is
`handshake::transcript_hash` — a SHA-256 over the protocol label, the transcript
label, and all four public keys (client static and ephemeral, server ephemeral,
daemon static), each length-prefixed. That has the first property for the same
reason a Noise handshake hash does: it is a digest of everything that was
negotiated, so a spliced connection produces two different values.

It does not yet have a shipped display form. When the pairing UI lands, the
string must be derived from that transcript hash and shown with enough digits to
make the second property true; truncating it to four characters would undo the
work the digest does.

**The plan's own warning still applies**: whatever is displayed must come from
the transcript, never from a hash of the session key, because a key hash is not
bound to what was negotiated.


**No plaintext fallback.** The record layer is required, not offered. A layer
that can be negotiated away is a layer an attacker strips before doing anything
else. The choice is made by frame type rather than by a subprotocol header: the
first frame on the socket is a binary `client_hello`, every frame after it is a
record, and a text frame anywhere ends the connection. A client that predates
the record layer gets one plaintext `Rejected` explaining that an encrypted
connection is required, and is then closed — an explanation, not a fallback.


**Forward secrecy.** Ephemeral keys per connection, so a later compromise of a
device does not decrypt recorded traffic.

**Replay.** Every frame carries a monotonically increasing counter inside the
authenticated payload; the receiver rejects a repeated or out-of-order frame.
The `resume_from` replay cursor needs the same treatment, and it is not
sufficient on its own: it authenticates messages within a connection, not a
whole connection replayed later. The record layer therefore also binds an epoch
that a fresh handshake changes, so a recorded connection cannot be replayed as
a new one.

**Threat model.** Three attackers, with different capabilities:

| Attacker | Can do | Defeated by |
| --- | --- | --- |
| Passive relay | Read every byte | The record layer; it sees ciphertext |
| Malicious relay | Read, drop, reorder, replay, or splice connections | Encryption for reading, counters for order, the handshake hash for splicing |
| Relay plus a compromised endpoint | Everything the endpoint can do | Nothing here — see below |

The third is out of scope on purpose. A design that claims to defend against a
compromised endpoint is either lying or is asking the user for a second device.

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

The order below is not the obvious one. Encryption comes **first**, before any
relay exists, and the reason is that a plaintext relay mode cannot be shipped
safely even as an interim step.

**Step 0 — decisions before code.** A threat model, the choice of Noise
implementation, and the authentication string's derivation. Every one of these
changes what gets written, and the last is a security boundary rather than a
detail.

**Step 1 — end-to-end encryption, no relay.** The record layer lands on the
existing connections: loopback, a private network, or a TLS tunnel the user
terminates. It is independently useful — `wss://` with an encrypted record
layer is already better than a bearer token crossing a trusted link — and it can
be exercised against real users of the feature that exists today.

This step must not accept a plaintext fallback. A layer that can be negotiated
away is a layer an intermediary can strip, so the negotiation belongs in the
WebSocket subprotocol and a client that does not get it refuses the connection.

**Step 2 — self-hosted relay.** The user runs the relay; the daemon dials out;
the client supplies a session id. This is the first time a connection crosses a
machine neither endpoint owns, which is why encryption precedes it rather than
following it: a relay operator is assumed untrusted from the moment the relay
exists, and no amount of "it is your own server" makes a UI that looks identical
in both modes safe. Operators end up running relays they do not fully control —
a VPS, a shared host, a friend's machine — and the client cannot tell.

Step 1 is done, so this step starts from a connection that is already opaque to
anything carrying it. The work is therefore only transport-shaped: pair two
sockets, move bytes, enforce limits, and hold nothing else.

**What the relay is allowed to know**, restated because it is the property the
step is judged on: a session id, two sockets, and byte counts. Not a token, not
a transcript, not a command, not a path, not a key it could use to impersonate
either end. An implementation that adds a debug hook printing payloads has
undone the step regardless of what the tests say.

**Step 3 — public relay.** Adds abuse limits and the operational work a shared
relay implies.

**What is deliberately not a step: a plaintext relay.** It would prove the
transport shape just as well, and it would put full-control tokens in an
operator's hands while looking no different on screen. The transport shape is
worth proving later, once the layer above it exists.

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
