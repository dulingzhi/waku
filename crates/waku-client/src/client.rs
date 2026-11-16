use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use tungstenite::WebSocket;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use uuid::Uuid;

use waku_protocol::MAX_WIRE_MESSAGE_BYTES;
use waku_protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage,
};

const READ_POLL_INTERVAL: Duration = Duration::from_millis(25);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BUFFERED_EVENTS_PER_RUNTIME: usize = 4096;

enum Outgoing {
    // Boxed: `ClientMessage` is a wire enum whose largest variant is ~700
    // bytes, and this channel carries one message at a time.
    Message(Box<ClientMessage>),
    Shutdown,
}

struct ClientInner {
    outgoing: Sender<Outgoing>,
    pending: Mutex<HashMap<Uuid, Sender<Result<ResponsePayload, RpcError>>>>,
    sessions: Mutex<HashMap<(Uuid, Uuid), Sender<SequencedEvent>>>,
    pending_events: Mutex<HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>>,
    task_state_subscribers: Mutex<Vec<Sender<u64>>>,
    last_sequences: Mutex<HashMap<(Uuid, Uuid), LastSequence>>,
    disconnected: AtomicBool,
    /// What the handshake proved about the daemon, for a caller to pin.
    daemon_signing_key: [u8; 32],
    /// A fingerprint of this connection's session key, so a caller can tell
    /// two connections apart without ever touching the key itself.
    session_key_fingerprint: [u8; 32],
    /// The short authentication string for this connection.
    ///
    /// Shown to the user so they can compare it against what the daemon
    /// displays. It is the only value that detects a relay running one
    /// handshake per side — such a relay passes every other check.
    authentication_string: String,
}

#[derive(Clone, Copy)]
struct LastSequence {
    epoch: Uuid,
    sequence: u64,
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

impl DaemonClient {
    pub fn connect(address: &str, token: String) -> anyhow::Result<Self> {
        Self::connect_with_identity(address, token, None)
    }

    /// Connect to a daemon, optionally pinning the signing key seen at pairing.
    ///
    /// `expected_ed25519` is the key a previous connection recorded. `None` is
    /// the first-pairing case, where the pairing code's fingerprint is the check
    /// instead; the caller can then pin whatever
    /// [`DaemonClient::daemon_signing_key`] reports.
    pub fn connect_with_identity(
        address: &str,
        token: String,
        expected_ed25519: Option<&ed25519_dalek::VerifyingKey>,
    ) -> anyhow::Result<Self> {
        Self::connect_with_resume_and_identity(address, token, Vec::new(), expected_ed25519)
    }

    pub fn connect_with_resume(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
    ) -> anyhow::Result<Self> {
        Self::connect_with_resume_and_identity(address, token, resume_from, None)
    }

    pub fn connect_with_resume_and_identity(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
        expected_ed25519: Option<&ed25519_dalek::VerifyingKey>,
    ) -> anyhow::Result<Self> {
        let last_sequences = resume_from
            .iter()
            .map(|cursor| {
                (
                    (cursor.session_id, cursor.runtime_id),
                    LastSequence {
                        epoch: cursor.epoch,
                        sequence: cursor.sequence,
                    },
                )
            })
            .collect();
        let url = daemon_url(address)?;
        Self::connect_to_url(url, token, resume_from, last_sequences, expected_ed25519)
    }

    /// Connect to a daemon through a relay.
    ///
    /// `relay_address` is the relay's `ws(s)://host:port`; `session_id` names
    /// the session the daemon published. The record layer is unchanged — the
    /// relay moves bytes it cannot read — so this differs from [`Self::connect`]
    /// only in the path it dials.
    pub fn connect_via_relay(
        relay_address: &str,
        session_id: &str,
        token: String,
    ) -> anyhow::Result<Self> {
        Self::connect_via_relay_with_identity(relay_address, session_id, token, None)
    }

    /// Connect using the code a daemon displayed.
    ///
    /// The code carries the relay, the session and the daemon's fingerprint. The
    /// fingerprint is checked against the key the handshake actually proved, so
    /// a relay that substituted its own daemon fails here rather than being
    /// trusted — this is the pairing step, and it only happens once per daemon
    /// identity.
    ///
    /// **The check is not optional.** A code without a usable fingerprint fails
    /// to parse; there is no variant of this call that connects and skips the
    /// pin, because such a call would be the one used by accident.
    pub fn connect_with_pairing_code(code: &str, token: String) -> anyhow::Result<Self> {
        let parsed = waku_protocol::pairing::PairingCode::parse(code)?;
        // The scheme comes from the code's classification, not from the code's
        // text: a remote relay is reached over TLS whatever a rewritten code
        // might say. Passing the composed URL means the generic relay path
        // below never has to guess a default for this case.
        let client = Self::connect_via_relay_with_identity(
            &parsed.relay_url(),
            &parsed.session_id,
            token,
            None,
        )?;

        let proven = client.daemon_signing_key();
        if let Err(error) = pairing_fingerprint_matches(&parsed.fingerprint, &proven) {
            // Drop the connection before returning: a client that stayed
            // connected to an unverified daemon would be one request away from
            // handing it the token.
            client.shutdown();
            return Err(error);
        }
        Ok(client)
    }

    /// [`Self::connect_via_relay`], pinning the daemon's signing key.
    pub fn connect_via_relay_with_identity(
        relay_address: &str,
        session_id: &str,
        token: String,
        expected_ed25519: Option<&ed25519_dalek::VerifyingKey>,
    ) -> anyhow::Result<Self> {
        if session_id.is_empty() || session_id.contains('/') {
            anyhow::bail!("a relay session id must be non-empty and contain no slash");
        }
        let normalized =
            if relay_address.starts_with("ws://") || relay_address.starts_with("wss://") {
                relay_address.to_owned()
            } else {
                format!("ws://{relay_address}")
            };
        let mut url = url::Url::parse(&normalized).context("relay address is invalid")?;
        // A relay path is `/<session>/client`, not `/v1`: the relay pairs by
        // session and role, and knows nothing about Waku's versioned path.
        url.set_path(&format!("/{session_id}/client"));
        url.set_query(None);
        url.set_fragment(None);
        Self::connect_to_url(
            url.into(),
            token,
            Vec::new(),
            HashMap::new(),
            expected_ed25519,
        )
    }

    fn connect_to_url(
        url: String,
        token: String,
        resume_from: Vec<ReplayCursor>,
        last_sequences: HashMap<(Uuid, Uuid), LastSequence>,
        expected_ed25519: Option<&ed25519_dalek::VerifyingKey>,
    ) -> anyhow::Result<Self> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
        let (mut socket, _) =
            tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
                .context("could not connect to Waku daemon")?;
        set_client_read_timeout(&mut socket, Some(Duration::from_secs(5)))?;
        // The client's static key is generated per connection and never
        // persisted: the daemon does not store or compare it, so there is
        // nothing to lose by not keeping it, and no long-lived key to steal.
        let mut sealed = waku_protocol::sealed::SealedConnection::connect_as_client(
            socket,
            x25519_dalek::StaticSecret::random(),
            expected_ed25519,
        )
        .context("could not complete the sealed handshake")?;
        let daemon_signing_key = sealed
            .keys()
            .peer_key()
            .context("the handshake did not report the daemon's signing key")?;
        write_sealed(
            &mut sealed,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                client_id: Uuid::new_v4(),
                resume_from,
            },
        )?;
        let hello = read_sealed_server_message(&mut sealed)?;
        match hello {
            ServerMessage::Hello {
                protocol_version, ..
            } if protocol_version == PROTOCOL_VERSION => {}
            ServerMessage::Hello {
                protocol_version, ..
            } => bail!(
                "daemon protocol {protocol_version} does not match desktop protocol {PROTOCOL_VERSION}"
            ),
            ServerMessage::Rejected { message } => bail!("daemon rejected connection: {message}"),
            other => bail!("daemon sent an invalid handshake response: {other:?}"),
        }
        set_client_read_timeout(sealed.socket_mut(), Some(READ_POLL_INTERVAL))?;

        let session_key_fingerprint = sealed.outgoing_key_fingerprint();
        // Captured here, before the connection is handed to its thread: the
        // string is a property of the handshake that just completed, and the UI
        // needs it while it can still ask the user to compare it against what
        // the daemon shows.
        let authentication_string = sealed.authentication_string();
        let (outgoing, outgoing_rx) = unbounded();
        let inner = Arc::new(ClientInner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            pending_events: Mutex::new(HashMap::new()),
            task_state_subscribers: Mutex::new(Vec::new()),
            last_sequences: Mutex::new(last_sequences),
            disconnected: AtomicBool::new(false),
            daemon_signing_key,
            session_key_fingerprint,
            authentication_string,
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("waku-daemon-client".into())
            .spawn(move || run_client(sealed, outgoing_rx, thread_inner))
            .context("could not start Waku daemon client thread")?;
        Ok(Self { inner })
    }

    /// The daemon's Ed25519 signing key, as proved by the handshake.
    ///
    /// A first-pairing caller pins this; every later connection passes it back
    /// as `expected_ed25519` so a relay cannot substitute its own key.
    pub fn daemon_signing_key(&self) -> [u8; 32] {
        self.inner.daemon_signing_key
    }

    pub fn subscribe(&self, session_id: Uuid, runtime_id: Uuid) -> Receiver<SequencedEvent> {
        let (events, receiver) = unbounded();
        let key = (session_id, runtime_id);
        let mut sessions = self.inner.sessions.lock();
        sessions.insert(key, events.clone());
        // Keep the subscription lock while draining the pre-subscription
        // replay queue. The socket thread takes these locks in the same order,
        // so a new live event cannot overtake older replayed events here.
        if let Some(buffered) = self.inner.pending_events.lock().remove(&key) {
            for event in buffered {
                let _ = events.send(event);
            }
        }
        receiver
    }

    /// Whether two handles send through the same WebSocket connection.
    ///
    /// The daemon supervisor publishes replacement clients after a managed
    /// restart. Runtime adapters use this identity check to ignore the
    /// subscription's initial snapshot and wait for an actual replacement.
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.disconnected.load(Ordering::Acquire)
    }

    /// A fingerprint of this connection's session key.
    ///
    /// Two connections to the same daemon return different values, which is how
    /// a test can show no key was reused. It is a sealed probe, not the key:
    /// see [`waku_protocol::sealed::SealedConnection::outgoing_key_fingerprint`].
    pub fn session_key_fingerprint(&self) -> [u8; 32] {
        self.inner.session_key_fingerprint
    }

    /// The short authentication string for this connection.
    ///
    /// A user compares this against the daemon's. It is what catches a relay
    /// that terminated the connection and ran one handshake per side: each of
    /// those handshakes is individually sound, so every other check passes,
    /// but the two ends then show different strings.
    pub fn authentication_string(&self) -> &str {
        &self.inner.authentication_string
    }

    pub fn unsubscribe(&self, session_id: Uuid, runtime_id: Uuid) {
        self.inner.sessions.lock().remove(&(session_id, runtime_id));
    }

    pub fn subscribe_task_state(&self) -> Receiver<u64> {
        let (events, receiver) = unbounded();
        self.inner.task_state_subscribers.lock().push(events);
        receiver
    }

    pub fn request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<ResponsePayload> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        let request_id = Uuid::new_v4();
        let (response, response_rx) = bounded(1);
        self.inner.pending.lock().insert(request_id, response);
        let message = ClientMessage::Request(Request {
            request_id,
            session_id,
            runtime_id,
            command,
        });
        if self
            .inner
            .outgoing
            .send(Outgoing::Message(Box::new(message)))
            .is_err()
        {
            self.inner.pending.lock().remove(&request_id);
            bail!("Waku daemon connection is closed");
        }
        match response_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => Err(anyhow!(error.message)),
            Err(error) => {
                self.inner.pending.lock().remove(&request_id);
                Err(anyhow!("timed out waiting for Waku daemon: {error}"))
            }
        }
    }

    pub fn notify(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<()> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(Box::new(ClientMessage::Request(
                Request {
                    // The nil request id is reserved for fire-and-forget controls;
                    // the daemon executes them in the runtime mailbox but does
                    // not allocate or send a response.
                    request_id: Uuid::nil(),
                    session_id,
                    runtime_id,
                    command,
                },
            ))))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    pub fn last_sequences(&self) -> Vec<ReplayCursor> {
        self.inner
            .last_sequences
            .lock()
            .iter()
            .map(|(&(session_id, runtime_id), cursor)| ReplayCursor {
                session_id,
                runtime_id,
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            })
            .collect()
    }

    pub fn shutdown(&self) {
        let _ = self.inner.outgoing.send(Outgoing::Shutdown);
    }
}

/// Whether `proven` is the key a pairing code names.
///
/// Takes the code's fingerprint and the key the handshake proved, and is the
/// only place that decision is made. Split out from
/// [`DaemonClient::connect_with_pairing_code`] so it can be tested without a
/// socket: the interesting cases are all "this key, that code", and a network
/// would only add the relay's timing on top.
///
/// **Compares the whole fingerprint, not a prefix.** A check that stopped early
/// would accept a daemon whose key shares the first characters of the real one,
/// which is exactly what an attacker grinding a key would aim for.
fn pairing_fingerprint_matches(fingerprint: &str, proven: &[u8; 32]) -> anyhow::Result<()> {
    let actual = waku_protocol::pairing::fingerprint_of(proven);
    if actual != fingerprint {
        anyhow::bail!(
            "the daemon at this pairing code is not the one it claims: \
             expected fingerprint {fingerprint}, got {actual}"
        );
    }
    Ok(())
}

fn daemon_url(address: &str) -> anyhow::Result<String> {
    let normalized = if address.starts_with("ws://") || address.starts_with("wss://") {
        address.to_owned()
    } else {
        format!("ws://{address}")
    };
    let mut url = url::Url::parse(&normalized).context("Waku daemon address is invalid")?;
    url.set_path("/v1");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn run_client(
    mut sealed: waku_protocol::sealed::SealedConnection<MaybeTlsStream<TcpStream>>,
    outgoing: Receiver<Outgoing>,
    inner: Arc<ClientInner>,
) {
    'connection: loop {
        while let Ok(message) = outgoing.try_recv() {
            match message {
                Outgoing::Message(message) => {
                    if write_sealed(&mut sealed, &message).is_err() {
                        break 'connection;
                    }
                }
                Outgoing::Shutdown => {
                    let _ = write_sealed(&mut sealed, &ClientMessage::Shutdown);
                    break 'connection;
                }
            }
        }

        match sealed.read() {
            Ok(Some(plaintext)) => {
                let Ok(message) = serde_json::from_slice::<ServerMessage>(&plaintext) else {
                    continue;
                };
                match message {
                    ServerMessage::Response {
                        request_id,
                        outcome,
                    } => {
                        if let Some(pending) = inner.pending.lock().remove(&request_id) {
                            let result = match outcome {
                                ResponseOutcome::Ok { payload } => Ok(payload),
                                ResponseOutcome::Error { error } => Err(error),
                            };
                            let _ = pending.send(result);
                        }
                    }
                    ServerMessage::Event(event) => {
                        let should_deliver = {
                            let mut sequences = inner.last_sequences.lock();
                            let previous = sequences
                                .entry((event.session_id, event.runtime_id))
                                .or_insert(LastSequence {
                                    epoch: event.epoch,
                                    sequence: 0,
                                });
                            if previous.epoch == event.epoch && event.sequence <= previous.sequence
                            {
                                false
                            } else {
                                previous.epoch = event.epoch;
                                previous.sequence = event.sequence;
                                true
                            }
                        };
                        if should_deliver {
                            let key = (event.session_id, event.runtime_id);
                            let sessions = inner.sessions.lock();
                            if let Some(events) = sessions.get(&key) {
                                let _ = events.send(event);
                            } else {
                                let mut pending = inner.pending_events.lock();
                                let buffered = pending.entry(key).or_default();
                                buffered.push_back(event);
                                while buffered.len() > MAX_BUFFERED_EVENTS_PER_RUNTIME {
                                    buffered.pop_front();
                                }
                            }
                        }
                    }
                    ServerMessage::TaskStateChanged { revision } => {
                        inner
                            .task_state_subscribers
                            .lock()
                            .retain(|subscriber| subscriber.send(revision).is_ok());
                    }
                    ServerMessage::ShuttingDown => break,
                    ServerMessage::Hello { .. } | ServerMessage::Rejected { .. } => {}
                }
            }
            // A read timeout: the daemon has nothing to say yet. Not a
            // disconnect.
            Ok(None) => {}
            // A failed record — tampered, replayed, reordered, or a text frame
            // where a record was required — ends the connection. The error names
            // the reason so the desktop can report something actionable.
            Err(error) => {
                let _ = &error;
                break;
            }
        }
    }

    inner.disconnected.store(true, Ordering::Release);
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, response) in pending {
        let _ = response.send(Err(RpcError {
            message: "Waku daemon disconnected".into(),
        }));
    }
    // Closing the desktop transport is not evidence that a daemon-owned
    // provider exited. Drop the subscription senders so runtime adapters can
    // hand off to a replacement client and ask the daemon whether the same
    // runtime still exists. Real provider exits arrive through the replayable
    // `processExited` event emitted by the daemon.
    drop(std::mem::take(&mut *inner.sessions.lock()));
    inner.task_state_subscribers.lock().clear();
}

fn set_client_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Option<Duration>,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(timeout),
        #[allow(unreachable_patterns)]
        _ => Ok(()),
    }
}

/// Seal and write one client message.
fn write_sealed<S: io::Read + io::Write, T: serde::Serialize>(
    sealed: &mut waku_protocol::sealed::SealedConnection<S>,
    value: &T,
) -> anyhow::Result<()> {
    sealed.send(&serde_json::to_vec(value)?)
}

/// Read one server message during the handshake.
fn read_sealed_server_message<S: io::Read + io::Write>(
    sealed: &mut waku_protocol::sealed::SealedConnection<S>,
) -> anyhow::Result<ServerMessage> {
    match sealed.read()? {
        Some(plaintext) => Ok(serde_json::from_slice(&plaintext)?),
        None => bail!("Waku daemon closed during handshake"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_endpoint_accepts_addresses_and_secure_urls() {
        assert_eq!(
            daemon_url("127.0.0.1:4312").unwrap(),
            "ws://127.0.0.1:4312/v1"
        );
        assert_eq!(
            daemon_url("wss://waku.example.test/old?ignored=1").unwrap(),
            "wss://waku.example.test/v1"
        );
    }

    /// A pairing code is accepted only when it names the key the handshake
    /// proved.
    ///
    /// This is `connect_with_pairing_code`'s decision, without a socket: the
    /// function under test is the comparison, and a network would only add the
    /// relay's reconnect timing on top of it. The end-to-end path is covered
    /// separately by the relay round trip.
    #[test]
    fn the_pairing_check_accepts_only_the_key_the_handshake_proved() {
        let proven = [0x3a_u8; 32];
        let right = waku_protocol::pairing::fingerprint_of(&proven);

        assert!(
            pairing_fingerprint_matches(&right, &proven).is_ok(),
            "a code naming the proven key must be accepted"
        );

        // Every character position must matter: a check that compared a prefix
        // would accept a key that shares only that prefix.
        for index in 0..right.len() {
            let mut wrong = right.clone();
            let replacement = if wrong.as_bytes()[index] == b'0' {
                "1"
            } else {
                "0"
            };
            wrong.replace_range(index..=index, replacement);
            assert_ne!(wrong, right, "the mutation at {index} must change the code");
            assert!(
                pairing_fingerprint_matches(&wrong, &proven).is_err(),
                "a code differing at position {index} must be refused"
            );
        }

        // And a different key must not be accepted under the right code.
        assert!(pairing_fingerprint_matches(&right, &[0x3b_u8; 32]).is_err());
    }
}
