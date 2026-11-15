# Remote access: implementation plan

How to build the relay and the end-to-end encryption `remote-access.md`
describes, in an order where each step is useful and each is judged before the
next begins.

The design settles the security shape. This settles the order of work.

## What is already in the dependency tree

Worth checking before proposing a new crypto crate: the tree already carries
the pieces.

| Crate | Version | Brought in by |
| --- | --- | --- |
| `ring` | 0.17.14 | rustls, through `tungstenite` |
| `chacha20` | 0.10.1 | the RustCrypto stack |
| `hkdf` | 0.12.4 | the same |
| `sha2` | 0.10.9 | the same |
| `ed25519` | 2.x | the release-signing code |

`ring` alone covers X25519 key agreement, ChaCha20-Poly1305 AEAD, HKDF-SHA256
and a CSPRNG. So the encryption layer can be built from crates already being
compiled, rather than adding a new cryptography dependency to a desktop app
that has to ship on three platforms.

That matters beyond build time. A new crypto crate is a new supply-chain
surface and a new set of platform builds to trust, on a feature whose failure
mode is "the user's machine is controlled by someone else".

## Step 1 — A relay, trusted operator

The smallest thing that proves the transport: a daemon behind NAT reaches a
client through a hop neither endpoint controls.

Scope:

- **A relay binary.** Its own crate, `crates/waku-relay`, speaking the same
  WebSocket the daemon already serves. It pairs two sockets by session id and
  copies bytes. It reads no frames.
- **Daemon dials out.** A `--relay <url>` flag starts an outbound connection.
  This is the point of the whole exercise: a machine behind a router never
  accepts an inbound connection.
- **A session id.** The daemon mints one and prints it, alongside the existing
  token, in Settings → Daemon. A client supplies it to reach that daemon.
- **Client connects through the relay.** `DaemonClient::connect` already
  accepts a URL; the relay address plus a session id is still a URL.

Deliberately not in scope: encryption, pairing codes, a public relay, and any
change to the wire protocol. The daemon-to-client bytes are the same bytes they
are today, carried over one more hop.

**This step is only safe when both endpoints are the operator's own.** The
token crosses the relay in the clear, exactly as it does over an
operator-controlled TLS tunnel today. It is a step toward the goal, not a
shippable public feature, and it must not be described as private.

Judged by: a daemon on one machine, a client on another, a relay on a third,
and `ls` in the terminal returns that daemon host's directory listing.

## Step 2 — End-to-end encryption

Now the relay becomes untrusted by construction.

The record layer goes **above** the WebSocket, so the relay never needs to know
it exists:

- X25519 ephemeral key agreement via `ring`
- ChaCha20-Poly1305 records via `ring`, keyed by HKDF-SHA256
- A monotonically increasing counter inside every authenticated record, so a
  replayed or reordered frame is rejected

Hand-rolling this is the risk. A reviewed construction is better, and `noise`
implementations in Rust (`snow`) carry one — but `snow` would be a new crate
that pulls its own primitives. The decision between "use `snow`" and "compose
`ring` primitives in a documented, reviewable way" is the one thing in this
plan that needs a second opinion before code is written.

Whichever is chosen, the shape is the same: the existing `Hello` message keeps
its fields and is simply carried inside the encrypted record. That is Option A
from the design document, and it avoids regenerating the wire types three
clients depend on.

Judged by: the relay operator cannot read a task title out of the traffic, a
record captured and replayed is rejected, and the existing test suite passes
with every transport test running through the record layer.

## Step 3 — A public relay

Only after step 2. Adds what a shared relay needs and a self-hosted one does
not: per-session byte and duration caps, a connection rate limit per source,
and an abuse policy someone is accountable for.

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
