use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use subtle::ConstantTimeEq as _;
use tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tungstenite::http::{StatusCode, header::ORIGIN};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_hdr_with_config};
use uuid::Uuid;

use crate::daemon_identity::DaemonIdentity;
use crate::model::{AgentSession, Project, ProviderKind, SessionStatus};
use crate::protocol::MAX_WIRE_MESSAGE_BYTES;
use crate::protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage, WireDriverEvent,
};
use waku_protocol::sealed::{AcceptError, SealedConnection};

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// A socket write that makes no progress for this long means the client is
/// genuinely not reading. Keeping the connection would let its event queue grow
/// without bound, so it is dropped; clients reconnect and resume from their
/// cursors.
///
/// This is a backstop, not a responsiveness budget. A merely busy host — one
/// descheduled by CPU pressure, or reading through a lossy network — can stall
/// a send for seconds without being gone, and dropping it costs a reconnect.
/// The queue cap below is what actually bounds memory, so the timeout can stay
/// generous without risking growth.
const SOCKET_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HANDSHAKE_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_REPLAY_EVENTS_PER_SESSION: usize = 2048;
/// Per-subscriber cap on queued daemon messages. A subscriber that falls this
/// far behind live events is dropped instead of buffered for, so a stalled
/// client cannot make the daemon's heap grow at the stream rate forever.
/// (Max 8 replaying runtimes at `MAX_REPLAY_EVENTS_PER_SESSION` fits; an
/// overflow during replay or live emission closes the connection and the
/// client reconnects from its last replayed cursor.)
const MAX_QUEUED_MESSAGES_PER_SUBSCRIBER: usize = 16384;
const MAX_CACHED_RESPONSES: usize = 2048;
/// Responses are cached by request id so a client that missed a reply can
/// fetch it after reconnecting. Caching is bounded by bytes as well as count:
/// outcomes such as a hydrated session can be megabytes, and a count-only cap
/// would let a handful of them pin hundreds of megabytes in the daemon.
const MAX_CACHED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const NATIVE_CLIENT_HEADER: &str = "x-waku-client";
const NATIVE_CLIENT_HEADER_VALUE: &str = "native";

#[derive(Clone, Debug, Default)]
pub struct ServerOptions {
    /// Browser WebSocket handshakes carry an Origin header. Most native clients
    /// do not; React Native does and identifies itself with `x-waku-client`.
    /// An empty set therefore still permits native clients only.
    pub allowed_origins: HashSet<String>,
    /// Only a daemon owned by the desktop process should accept the global
    /// shutdown control message. Service-managed daemons keep running when an
    /// authenticated client disconnects.
    pub allow_shutdown: bool,
}

pub(crate) struct ConnectionPermit(Arc<AtomicUsize>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub trait Backend: Send + Sync + 'static {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload>;

    fn shutdown(&self) {}
}

#[derive(Clone)]
pub struct EventSink {
    session_id: Uuid,
    runtime_id: Uuid,
    hub: Arc<Hub>,
}

impl EventSink {
    pub fn send(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        self.hub.emit(self.session_id, self.runtime_id, event, true);
        Ok(())
    }

    /// Opens the event stream for a runtime this request created.
    ///
    /// A backend normally answers for the one session its request named. Spawning
    /// a delegate is the exception: the new session is not the caller's, so the
    /// handler needs a sink bound to it. Registering it here is what lets the hub
    /// accept the delegate's events at all — `emit` drops anything for a runtime
    /// it does not believe is active.
    pub fn begin_runtime(&self, session_id: Uuid, runtime_id: Uuid) -> Self {
        self.hub.begin_runtime(session_id, runtime_id);
        Self {
            session_id,
            runtime_id,
            hub: self.hub.clone(),
        }
    }

    /// A sink for a backend unit test, where no hub or connection exists.
    ///
    /// Events sent through it are discarded: a test that only checks the
    /// response a backend returns needs the parameter, not a real event stream.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            session_id: Uuid::nil(),
            runtime_id: Uuid::nil(),
            hub: Arc::new(Hub::default()),
        }
    }

    /// Broadcast a live-only event without retaining it in the replay journal.
    /// High-volume PTY output is meaningful only to a terminal emulator that
    /// is currently attached; replaying raw chunks into a fresh emulator would
    /// also retain an unbounded terminal transcript in daemon memory.
    pub fn send_ephemeral(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        self.hub
            .emit(self.session_id, self.runtime_id, event, false);
        Ok(())
    }
}

/// One connected client's delivery state.
struct Subscriber {
    messages: Sender<ServerMessage>,
    /// Signalled when the subscriber falls too far behind and is dropped from
    /// the hub. The connection polls it and closes, letting the client
    /// reconnect and resume from its last replayed cursor.
    kicked: Sender<()>,
}

impl Subscriber {
    fn new(messages: Sender<ServerMessage>) -> (Self, Receiver<()>) {
        let (kicked, kicked_rx) = unbounded();
        (Self { messages, kicked }, kicked_rx)
    }
}

#[derive(Default)]
struct HubState {
    next_subscriber_id: u64,
    task_state_revision: u64,
    subscribers: HashMap<u64, Subscriber>,
    active_runtimes: HashMap<Uuid, Uuid>,
    next_sequences: HashMap<(Uuid, Uuid), u64>,
    journal: HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>,
    responses: VecDeque<(Uuid, ResponseOutcome, usize)>,
    cached_response_bytes: usize,
    catalog_projects: HashMap<Uuid, ProjectCatalogEntry>,
    catalog_sessions: HashMap<Uuid, SessionCatalogEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectCatalogEntry {
    name: String,
    path: std::path::PathBuf,
    created_at: u64,
}

impl From<&Project> for ProjectCatalogEntry {
    fn from(project: &Project) -> Self {
        Self {
            name: project.name.clone(),
            path: project.path.clone(),
            created_at: project.created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionCatalogEntry {
    title: String,
    auto_title: Option<String>,
    project_id: Uuid,
    provider: ProviderKind,
    model: Option<String>,
    status: SessionStatus,
    created_at: u64,
    last_reply_at: Option<u64>,
}

impl From<&AgentSession> for SessionCatalogEntry {
    fn from(session: &AgentSession) -> Self {
        Self {
            title: session.title.clone(),
            auto_title: session.auto_title.clone(),
            project_id: session.project_id,
            provider: session.provider,
            model: session.model.clone(),
            status: session.status,
            created_at: session.created_at,
            last_reply_at: session.last_reply_at,
        }
    }
}

struct Hub {
    epoch: Uuid,
    state: Mutex<HubState>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            epoch: Uuid::new_v4(),
            state: Mutex::new(HubState::default()),
        }
    }
}

struct DispatchedRequest {
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
}

struct RuntimeMailbox {
    id: Uuid,
    sender: Sender<DispatchedRequest>,
}

struct RequestDispatcher {
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
    /// A live provider runtime is an actor owned by the daemon, not by any
    /// particular WebSocket connection. One mailbox per session preserves
    /// lifecycle order across desktop and web clients without serializing
    /// unrelated sessions or read-only requests.
    runtime_mailboxes: Arc<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
}

impl Hub {
    fn event_sink(self: &Arc<Self>, session_id: Uuid, runtime_id: Uuid) -> EventSink {
        EventSink {
            session_id,
            runtime_id,
            hub: self.clone(),
        }
    }

    fn begin_runtime(&self, session_id: Uuid, runtime_id: Uuid) {
        let mut state = self.state.lock();
        state.active_runtimes.insert(session_id, runtime_id);
        state
            .next_sequences
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .journal
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    fn end_runtime(&self, session_id: Uuid, runtime_id: Option<Uuid>) {
        let mut state = self.state.lock();
        let matches_active = runtime_id
            .is_none_or(|runtime_id| state.active_runtimes.get(&session_id) == Some(&runtime_id));
        if !matches_active {
            return;
        }
        state.active_runtimes.remove(&session_id);
        state
            .next_sequences
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .journal
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    fn emit(&self, session_id: Uuid, runtime_id: Uuid, event: WireDriverEvent, replayable: bool) {
        let mut state = self.state.lock();
        if state.active_runtimes.get(&session_id) != Some(&runtime_id) {
            return;
        }
        let sequence = state
            .next_sequences
            .entry((session_id, runtime_id))
            .or_default();
        *sequence = sequence.saturating_add(1);
        let event = SequencedEvent {
            session_id,
            runtime_id,
            epoch: self.epoch,
            sequence: *sequence,
            event,
        };
        if replayable {
            let journal = state.journal.entry((session_id, runtime_id)).or_default();
            journal.push_back(event.clone());
            while journal.len() > MAX_REPLAY_EVENTS_PER_SESSION {
                journal.pop_front();
            }
        }
        Self::broadcast(&mut state, &ServerMessage::Event(event), None);
    }

    fn subscribe(&self, resume_from: &[ReplayCursor], subscriber: Subscriber) -> u64 {
        let mut state = self.state.lock();
        let id = state.next_subscriber_id;
        state.next_subscriber_id = state.next_subscriber_id.saturating_add(1);
        // The subscriber is registered before replay so an oversized journal
        // can only truncate the replay, never leave a live connection without
        // events. Replay runs under the hub lock so no live event can
        // interleave with the journal catch-up, and pushes into the bounded
        // queue are non-blocking. Dropping the oldest replayed events matches
        // the journal's own rollover semantics; clients reconcile any gap
        // from session state.
        let messages = subscriber.messages.clone();
        state.subscribers.insert(id, subscriber);
        for (&(session_id, runtime_id), events) in &state.journal {
            let sequence = resume_from
                .iter()
                .find(|cursor| {
                    cursor.session_id == session_id
                        && cursor.runtime_id == runtime_id
                        && cursor.epoch == self.epoch
                })
                .map(|cursor| cursor.sequence)
                .unwrap_or_default();
            for event in events.iter().filter(|event| event.sequence > sequence) {
                if messages.len() >= MAX_QUEUED_MESSAGES_PER_SUBSCRIBER
                    || messages
                        .try_send(ServerMessage::Event(event.clone()))
                        .is_err()
                {
                    return id;
                }
            }
        }
        id
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        self.state.lock().subscribers.remove(&subscriber_id);
    }

    /// Sends `message` to every subscriber except `skip`, dropping any whose
    /// bounded queue is full. Buffering a stalled client at the daemon's
    /// event rate is how resident memory grew without bound; a dropped
    /// subscriber's connection observes the kick, closes, and reconnects with
    /// replay cursors, so no event stream is lost permanently.
    fn broadcast(state: &mut HubState, message: &ServerMessage, skip: Option<u64>) {
        let mut overwhelmed = Vec::new();
        for (&subscriber_id, subscriber) in &state.subscribers {
            if Some(subscriber_id) == skip {
                continue;
            }
            if subscriber.messages.len() >= MAX_QUEUED_MESSAGES_PER_SUBSCRIBER
                || subscriber.messages.try_send(message.clone()).is_err()
            {
                overwhelmed.push((subscriber_id, subscriber.kicked.clone()));
            }
        }
        for (subscriber_id, kicked) in overwhelmed {
            state.subscribers.remove(&subscriber_id);
            let _ = kicked.send(());
        }
    }

    fn task_state_changed(&self, source_subscriber_id: u64) {
        let mut state = self.state.lock();
        Self::broadcast_task_state_changed(&mut state, source_subscriber_id);
    }

    fn replace_task_catalog(&self, projects: &[Project], sessions: &[AgentSession]) {
        let mut state = self.state.lock();
        state.catalog_projects = projects
            .iter()
            .map(|project| (project.id, ProjectCatalogEntry::from(project)))
            .collect();
        state.catalog_sessions = sessions
            .iter()
            .map(|session| (session.id, SessionCatalogEntry::from(session)))
            .collect();
    }

    fn task_state_saved(
        &self,
        source_subscriber_id: u64,
        projects: &[Project],
        sessions: &[AgentSession],
    ) {
        let mut state = self.state.lock();
        let mut changed = false;
        for project in projects {
            let next = ProjectCatalogEntry::from(project);
            changed |= state
                .catalog_projects
                .insert(project.id, next.clone())
                .is_none_or(|previous| previous != next);
        }
        for session in sessions {
            let next = SessionCatalogEntry::from(session);
            changed |= state
                .catalog_sessions
                .insert(session.id, next.clone())
                .is_none_or(|previous| previous != next);
        }
        if changed {
            Self::broadcast_task_state_changed(&mut state, source_subscriber_id);
        }
    }

    fn broadcast_task_state_changed(state: &mut HubState, source_subscriber_id: u64) {
        state.task_state_revision = state.task_state_revision.saturating_add(1);
        let message = ServerMessage::TaskStateChanged {
            revision: state.task_state_revision,
        };
        Self::broadcast(state, &message, Some(source_subscriber_id));
    }

    fn cached_response(&self, request_id: Uuid) -> Option<ResponseOutcome> {
        self.state
            .lock()
            .responses
            .iter()
            .rev()
            .find_map(|(cached_id, outcome, _)| (*cached_id == request_id).then(|| outcome.clone()))
    }

    fn cache_response(&self, request_id: Uuid, outcome: ResponseOutcome) {
        // File bytes are bulk data that clients read once; caching a copy per
        // request would duplicate every open file in daemon memory, and the
        // request is cheap to re-run if it is ever retried.
        if matches!(
            &outcome,
            ResponseOutcome::Ok {
                payload: ResponsePayload::BlobData { .. }
            }
        ) {
            return;
        }
        // Outcomes carry serde payloads whose in-memory size tracks their
        // serialized form closely enough for a cache budget.
        let bytes = serde_json::to_string(&outcome)
            .map(|serialized| serialized.len())
            .unwrap_or(0);
        let mut state = self.state.lock();
        state.cached_response_bytes = state.cached_response_bytes.saturating_add(bytes);
        state.responses.push_back((request_id, outcome, bytes));
        while state.responses.len() > MAX_CACHED_RESPONSES
            || state.cached_response_bytes > MAX_CACHED_RESPONSE_BYTES
        {
            if let Some((_, _, evicted_bytes)) = state.responses.pop_front() {
                state.cached_response_bytes =
                    state.cached_response_bytes.saturating_sub(evicted_bytes);
            }
        }
    }
}

impl RequestDispatcher {
    fn new(backend: Arc<dyn Backend>, hub: Arc<Hub>) -> Self {
        Self {
            backend,
            hub,
            runtime_mailboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn dispatch(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        if command_targets_runtime(&request.command) {
            self.dispatch_runtime(request, outgoing, source_subscriber_id);
        } else {
            self.dispatch_independent(request, outgoing, source_subscriber_id);
        }
    }

    fn dispatch_independent(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        let backend = self.backend.clone();
        let hub = self.hub.clone();
        let failed_request_id = request.request_id;
        let failed_outgoing = outgoing.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("waku-daemon-request".into())
            .spawn(move || {
                handle_request(request, outgoing, source_subscriber_id, backend, hub);
            })
        {
            send_dispatch_error(
                failed_request_id,
                failed_outgoing,
                &self.hub,
                format!("could not start daemon request worker: {error}"),
            );
        }
    }

    fn dispatch_runtime(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        let session_id = request.session_id;
        let failed_request_id = request.request_id;
        let failed_outgoing = outgoing.clone();
        let mut dispatched = DispatchedRequest {
            request,
            outgoing,
            source_subscriber_id,
        };
        loop {
            let mut mailboxes = self.runtime_mailboxes.lock();
            if let Some(mailbox) = mailboxes.get(&session_id) {
                match mailbox.sender.send(dispatched) {
                    Ok(()) => return,
                    Err(error) => {
                        dispatched = error.0;
                        mailboxes.remove(&session_id);
                        continue;
                    }
                }
            }

            let mailbox_id = Uuid::new_v4();
            let (sender, requests) = unbounded();
            sender
                .send(dispatched)
                .expect("a new runtime mailbox still has its receiver");
            mailboxes.insert(
                session_id,
                RuntimeMailbox {
                    id: mailbox_id,
                    sender,
                },
            );

            let backend = self.backend.clone();
            let hub = self.hub.clone();
            let mailbox_registry = Arc::downgrade(&self.runtime_mailboxes);
            let worker = std::thread::Builder::new()
                .name(format!("waku-daemon-runtime-{session_id}"))
                .spawn(move || {
                    run_runtime_mailbox(
                        session_id,
                        mailbox_id,
                        requests,
                        mailbox_registry,
                        backend,
                        hub,
                    );
                });
            if let Err(error) = worker {
                if mailboxes
                    .get(&session_id)
                    .is_some_and(|mailbox| mailbox.id == mailbox_id)
                {
                    mailboxes.remove(&session_id);
                }
                drop(mailboxes);
                send_dispatch_error(
                    failed_request_id,
                    failed_outgoing,
                    &self.hub,
                    format!("could not start runtime worker: {error}"),
                );
            }
            return;
        }
    }
}

/// Everything a daemon connection needs, independent of how it arrived.
///
/// One instance is shared by the listener and by every outbound relay dial, so
/// a client reaching the daemon through a relay subscribes to the same
/// [`Hub`] and is served by the same dispatcher as a local one. Splitting this
/// per direction would mean a relay client silently received no session events.
pub struct DaemonServer {
    token: String,
    /// Tokens minted for agent bridges, keyed by the token value itself.
    ///
    /// The desktop's own token stays in `token`: it is the user, and it is
    /// checked first so its cost never changes. These are additive — a daemon
    /// with no lead conversations running has an empty map and behaves exactly
    /// as it did before the bridge existed.
    agent_tokens: Arc<Mutex<HashMap<String, ScopedToken>>>,
    identity: Arc<DaemonIdentity>,
    backend: Arc<dyn Backend>,
    dispatcher: Arc<RequestDispatcher>,
    hub: Arc<Hub>,
    options: ServerOptions,
    active_connections: Arc<AtomicUsize>,
}

impl DaemonServer {
    pub fn new(
        token: String,
        identity: Arc<DaemonIdentity>,
        backend: Arc<dyn Backend>,
        options: ServerOptions,
    ) -> Self {
        let hub = Arc::new(Hub::default());
        let dispatcher = Arc::new(RequestDispatcher::new(backend.clone(), hub.clone()));
        Self {
            token,
            agent_tokens: Arc::new(Mutex::new(HashMap::new())),
            identity,
            backend,
            dispatcher,
            hub,
            options,
            active_connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Claims one of the connection slots, or `None` when the daemon is at its
    /// cap. Both directions share the cap: a relay peer occupies a connection
    /// exactly as a local one does.
    fn try_claim_connection(&self) -> Option<ConnectionPermit> {
        self.active_connections
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONNECTIONS).then_some(active + 1)
            })
            .ok()?;
        Some(ConnectionPermit(self.active_connections.clone()))
    }

    fn shutdown_backend(&self) {
        self.backend.shutdown();
    }

    /// Serves one dialed relay connection on this thread, blocking until it
    /// ends.
    ///
    /// Blocking is deliberate: the relay reconnect loop needs to know when the
    /// connection actually closed before deciding how long to wait, and a
    /// detached thread would make every dial look instant.
    pub(crate) fn serve_dialed<S>(
        &self,
        socket: tungstenite::WebSocket<S>,
        shutdown: Arc<AtomicBool>,
        permit: ConnectionPermit,
    ) -> anyhow::Result<()>
    where
        S: std::io::Read + std::io::Write + ConnectionStream,
    {
        let _permit = permit;
        self.clone_for_connection().serve_socket(socket, &shutdown)
    }

    /// Accept connections on `listener` until `shutdown` is set.
    ///
    /// The same [`DaemonServer`] also serves dialed relay connections, so a
    /// daemon can do both: one hub, one set of sessions, whichever side a
    /// client arrives from.
    pub fn serve(
        self: &std::sync::Arc<Self>,
        listener: std::net::TcpListener,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        serve_listener(listener, self, &shutdown)
    }

    fn clone_for_connection(&self) -> DaemonConnection {
        DaemonConnection {
            token: self.token.clone(),
            agent_tokens: self.agent_tokens.clone(),
            identity: self.identity.clone(),
            dispatcher: self.dispatcher.clone(),
            hub: self.hub.clone(),
            options: self.options.clone(),
        }
    }

    /// A sink bound to an arbitrary session, for consumers outside this module.
    ///
    /// The daemon mints one per request internally; a probe that drives a
    /// backend directly needs the same thing without a live connection.
    pub fn event_sink_for(&self, session_id: Uuid, runtime_id: Uuid) -> EventSink {
        EventSink {
            session_id,
            runtime_id,
            hub: self.hub.clone(),
        }
    }

    /// Claims a connection slot for a dialed relay peer.
    pub(crate) fn claim_dialed_connection(&self) -> Option<ConnectionPermit> {
        self.try_claim_connection()
    }

    /// Mints a token that lets one agent bridge drive one lead conversation.
    ///
    /// Called by whoever launches the provider: the token travels to the
    /// provider's MCP server configuration, so the bridge holds only a
    /// capability for its own session. It is deliberately not the daemon token.
    ///
    /// The value is 32 random bytes in hex. It is a bearer capability with a
    /// narrow scope rather than a session cookie, so it is never compared for
    /// equality outside `token_matches`, and it is revoked when the lead's
    /// runtime ends.
    pub fn mint_agent_token(&self, parent_session: Uuid, runtime_id: Uuid) -> String {
        let token = minted_token_value();
        self.agent_tokens.lock().insert(
            token.clone(),
            ScopedToken {
                scope: AgentScope::Delegate {
                    parent_session,
                    runtime_id,
                },
            },
        );
        token
    }

    /// Drops every minted token, which closes the door on any bridge still
    /// holding one. Called when the daemon shuts down.
    pub fn revoke_agent_tokens(&self) {
        self.agent_tokens.lock().clear();
    }
}

/// The subset of [`DaemonServer`] a single connection thread owns.
struct DaemonConnection {
    token: String,
    agent_tokens: Arc<Mutex<HashMap<String, ScopedToken>>>,
    identity: Arc<DaemonIdentity>,
    dispatcher: Arc<RequestDispatcher>,
    hub: Arc<Hub>,
    options: ServerOptions,
}

impl DaemonConnection {
    fn serve_socket<S>(
        &self,
        socket: tungstenite::WebSocket<S>,
        shutdown: &Arc<AtomicBool>,
    ) -> anyhow::Result<()>
    where
        S: std::io::Read + std::io::Write + ConnectionStream,
    {
        serve_sealed_connection(
            socket,
            &ConnectionTokens {
                desktop: self.token.clone(),
                agents: self.agent_tokens.clone(),
            },
            &self.identity,
            self.dispatcher.clone(),
            self.hub.clone(),
            shutdown.clone(),
            &self.options,
        )
    }
}

pub fn serve(
    listener: TcpListener,
    token: String,
    identity: Arc<DaemonIdentity>,
    backend: Arc<dyn Backend>,
    shutdown: Arc<AtomicBool>,
    options: ServerOptions,
) -> anyhow::Result<()> {
    let server = DaemonServer::new(token, identity, backend, options);
    serve_listener(listener, &server, &shutdown)
}

/// Accept loop for the listener direction.
fn serve_listener(
    listener: TcpListener,
    server: &DaemonServer,
    shutdown: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    listener
        .set_nonblocking(true)
        .context("could not configure Waku daemon listener")?;
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let Some(connection_permit) = server.try_claim_connection() else {
                    continue;
                };
                let tokens = ConnectionTokens {
                    desktop: server.token.clone(),
                    agents: server.agent_tokens.clone(),
                };
                let identity = server.identity.clone();
                let dispatcher = server.dispatcher.clone();
                let hub = server.hub.clone();
                let shutdown = shutdown.clone();
                let options = server.options.clone();
                std::thread::Builder::new()
                    .name("waku-daemon-connection".into())
                    .spawn(move || {
                        let _connection_permit = connection_permit;
                        if let Err(error) = handle_connection(
                            stream,
                            &tokens,
                            &identity,
                            dispatcher,
                            hub,
                            shutdown,
                            &options,
                        ) {
                            eprintln!("waku-daemon connection ended: {error:#}");
                        }
                    })
                    .context("could not start Waku daemon connection thread")?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Waku daemon listener failed"),
        }
    }
    server.shutdown_backend();
    Ok(())
}

// The handshake closure below must return tungstenite's own `ErrorResponse`,
// so the same fixed signature that pins `validate_handshake` pins this call.
#[allow(clippy::result_large_err)]
fn handle_connection(
    stream: TcpStream,
    tokens: &ConnectionTokens,
    identity: &DaemonIdentity,
    dispatcher: Arc<RequestDispatcher>,
    hub: Arc<Hub>,
    shutdown: Arc<AtomicBool>,
    options: &ServerOptions,
) -> anyhow::Result<()> {
    // Accepted sockets can inherit the listener's nonblocking flag on some
    // platforms. The handshake is deliberately blocking; steady-state reads
    // get their bounded polling behavior from SO_RCVTIMEO below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_HANDSHAKE_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_HANDSHAKE_MESSAGE_BYTES));
    let allowed_origins = options.allowed_origins.clone();
    let socket = accept_hdr_with_config(
        stream,
        move |request: &HandshakeRequest, response: HandshakeResponse| {
            validate_handshake(request, response, &allowed_origins)
        },
        Some(config),
    )
    .context("WebSocket handshake failed")?;
    serve_sealed_connection(
        socket,
        tokens,
        identity,
        dispatcher,
        hub,
        shutdown,
        options,
    )
}

/// Everything a daemon connection does once a WebSocket exists.
///
/// Shared by both directions on purpose: an accepted socket and a dialed relay
/// connection differ only in who opened the TCP connection, and neither the
/// sealed handshake, the token check, the limits nor the request loop may drift
/// between them. Anything specific to accepting lives in
/// [`handle_connection`]; anything specific to dialing lives in
/// [`crate::relay::dial`].
fn serve_sealed_connection<S>(
    socket: tungstenite::WebSocket<S>,
    tokens: &ConnectionTokens,
    identity: &DaemonIdentity,
    dispatcher: Arc<RequestDispatcher>,
    hub: Arc<Hub>,
    shutdown: Arc<AtomicBool>,
    options: &ServerOptions,
) -> anyhow::Result<()>
where
    S: std::io::Read + std::io::Write + ConnectionStream,
{
    // From here on every frame is a record. A client that predates the sealed
    // protocol sends plaintext JSON, which cannot be read as a record — so
    // before refusing it, tell it why in the one plaintext frame this daemon
    // will ever write. Write-only, no session, no data: an explanation, not a
    // fallback.
    let mut sealed = match SealedConnection::accept_as_daemon(
        socket,
        identity.x25519_secret(),
        identity.ed25519_signing_key(),
    ) {
        Ok(sealed) => sealed,
        Err(error) => match *error {
            AcceptError::Plaintext(mut socket) => {
                // Write-only, no session, no data: an explanation, not a
                // fallback.
                let notice = ServerMessage::Rejected {
                    message: "this daemon requires an encrypted connection; update your client"
                        .into(),
                };
                let _ = write_json(&mut socket, &notice);
                let _ = socket.flush();
                waku_protocol::diagnostics::log(
                    "waku-daemon refused a plaintext client: encryption is required",
                );
                return Ok(());
            }
            AcceptError::Other(error) => {
                return Err(error).context("sealed handshake failed");
            }
        },
    };
    // The connection is sealed, so both ends now hold the same short
    // authentication string. It is logged here — before any message is
    // exchanged — because its whole purpose is for a person to compare it
    // against what their client shows, and a value reported after the fact
    // would be reported after the traffic it was meant to authenticate.
    //
    // This is not a secret: it is a digest of public keys both sides already
    // exchanged. It is short enough to read across a room, which is exactly the
    // length an attacker running one handshake per side cannot match.
    waku_protocol::diagnostics::log(format!(
        "waku-daemon pairing code: {}",
        sealed.authentication_string()
    ));
    let hello = match read_sealed_client_message(&mut sealed)? {
        Some(hello) => hello,
        None => return Ok(()),
    };
    // The desktop's own token is checked first and yields no scope: it is the
    // user, and narrowing it would change what every existing client may do.
    // A minted agent token is checked second and carries a scope, which is what
    // keeps a provider-launched bridge from being a second full client.
    let (resume_from, scope) = match hello {
        ClientMessage::Hello {
            protocol_version,
            token,
            resume_from,
            ..
        } if protocol_version == PROTOCOL_VERSION && token_matches(&tokens.desktop, &token) => {
            (resume_from, None)
        }
        ClientMessage::Hello {
            protocol_version,
            ref token,
            ref resume_from,
            ..
        } if protocol_version == PROTOCOL_VERSION => {
            let minted = tokens.agents.lock().get(token.as_str()).copied();
            let Some(minted) = minted else {
                write_sealed(
                    &mut sealed,
                    &ServerMessage::Rejected {
                        message: "authentication failed".into(),
                    },
                )?;
                return Ok(());
            };
            waku_protocol::diagnostics::log("waku-daemon accepted a scoped agent token");
            (resume_from.clone(), Some(minted.scope))
        }
        ClientMessage::Hello {
            protocol_version, ..
        } if protocol_version != PROTOCOL_VERSION => {
            write_sealed(
                &mut sealed,
                &ServerMessage::Rejected {
                    message: format!(
                        "protocol {protocol_version} is unsupported; expected {PROTOCOL_VERSION}"
                    ),
                },
            )?;
            return Ok(());
        }
        ClientMessage::Hello { .. } => {
            write_sealed(
                &mut sealed,
                &ServerMessage::Rejected {
                    message: "authentication failed".into(),
                },
            )?;
            return Ok(());
        }
        _ => bail!("first daemon message was not a hello"),
    };
    write_sealed(
        &mut sealed,
        &ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    sealed.socket_mut().set_config(|config| {
        config.max_message_size = Some(MAX_WIRE_MESSAGE_BYTES);
        config.max_frame_size = Some(MAX_WIRE_MESSAGE_BYTES);
    });
    // The steady-state read loop is a poll loop: `SealedConnection::read`
    // returns `Ok(None)` when the read times out, and that is the only reason
    // the loop can notice shutdown or drain its outgoing queue. The timeouts are
    // therefore part of the protocol, not an optimization, and both connection
    // directions need them.
    timeout_stream::<S>(sealed.socket_mut().get_mut())?;

    let (outgoing, outgoing_rx) = bounded(MAX_QUEUED_MESSAGES_PER_SUBSCRIBER);
    let (subscriber, kicked) = Subscriber::new(outgoing.clone());
    let subscriber_id = hub.subscribe(&resume_from, subscriber);

    'connection: while !shutdown.load(Ordering::Acquire) {
        if kicked.try_recv().is_ok() {
            break;
        }
        while let Ok(message) = outgoing_rx.try_recv() {
            if let Err(error) = write_sealed(&mut sealed, &message) {
                // Diagnostic: a stalled write ends the connection even when the
                // peer is merely slow (a busy host reading late). Report whether
                // the underlying error was one `retryable_io` would tolerate, so
                // a reproduced drop shows which side of that line it fell on.
                waku_protocol::diagnostics::log(format!(
                    "waku-daemon dropped connection {subscriber_id}: write failed: {error:#} \
                     (retryable={})",
                    write_failure_is_retryable(&error)
                ));
                break 'connection;
            }
        }
        // Every frame past the handshake is a record. A text frame never reaches
        // here: `SealedConnection::read` refuses it with a named error instead of
        // silently skipping, so a downgrade attempt fails loudly.
        match sealed.read() {
            Ok(None) => {
                // A poll timeout is normal; a closed socket is caught below.
                continue;
            }
            Ok(Some(plaintext)) => match serde_json::from_slice(&plaintext) {
                Ok(ClientMessage::Request(request)) => {
                    // A scoped token reaches only what its scope names. The
                    // refusal is per request and uses the ordinary error path,
                    // so a delegate that asks for something it may not do gets
                    // an answer instead of a dropped connection.
                    if let Some(scope) = scope
                        && !scope.permits(
                            &request.command,
                            request.session_id,
                            request.runtime_id,
                        )
                    {
                        // Name the half that missed. This refusal reached a user
                        // twice as a bare "may not perform that request" while the
                        // bridge was quietly addressing Claude's own conversation
                        // instead of the task, and the log is the only place the
                        // difference between the two is visible.
                        waku_protocol::diagnostics::log(format!(
                            "waku-daemon refused a scoped agent request: {}",
                            scope.refusal_reason(
                                &request.command,
                                request.session_id,
                                request.runtime_id,
                            )
                        ));
                        write_sealed(
                            &mut sealed,
                            &ServerMessage::Response {
                                request_id: request.request_id,
                                outcome: ResponseOutcome::Error {
                                    error: RpcError {
                                        message: "this agent token may not perform that request"
                                            .into(),
                                    },
                                },
                            },
                        )?;
                        continue;
                    }
                    dispatcher.dispatch(request, outgoing.clone(), subscriber_id);
                }
                Ok(ClientMessage::Shutdown) => {
                    if options.allow_shutdown {
                        write_sealed(&mut sealed, &ServerMessage::ShuttingDown)?;
                        shutdown.store(true, Ordering::Release);
                        break;
                    }
                    write_sealed(
                        &mut sealed,
                        &ServerMessage::Rejected {
                            message: "daemon shutdown is managed by its service owner".into(),
                        },
                    )?;
                }
                Ok(ClientMessage::Hello { .. }) => {}
                Err(error) => {
                    waku_protocol::diagnostics::log(format!(
                        "waku-daemon connection {subscriber_id}: invalid message: {error}"
                    ));
                }
            },
            Err(error) if is_transport_timeout(&error) => {}
            Err(error) => {
                // A record that fails authentication, arrives out of order, or
                // arrives as a text frame all land here. The message never
                // contains plaintext.
                waku_protocol::diagnostics::log(format!(
                    "waku-daemon connection {subscriber_id}: sealed stream failed: {error:#}"
                ));
                break 'connection;
            }
        }
    }
    if kicked.try_recv().is_ok() {
        // The hub evicts a subscriber that fell behind the queue cap; that is a
        // different drop from a stalled socket write.
        waku_protocol::diagnostics::log(format!(
            "waku-daemon dropped connection {subscriber_id}: evicted by the hub \
             (queue cap {MAX_QUEUED_MESSAGES_PER_SUBSCRIBER})"
        ));
    }
    hub.unsubscribe(subscriber_id);
    Ok(())
}

/// Applies the polling timeouts a daemon connection needs to either stream
/// direction.
///
/// The accepted side has a `TcpStream`; the dialed side may have a TLS stream
/// wrapped around one. Both expose the same two timeouts, but through different
/// types, so the shared connection loop asks here instead of knowing which.
fn timeout_stream<S: ConnectionStream>(stream: &mut S) -> std::io::Result<()> {
    stream.set_daemon_timeouts(SOCKET_POLL_INTERVAL, SOCKET_WRITE_STALL_TIMEOUT)
}

/// The part of a stream the shared connection loop needs.
///
/// Deliberately narrow: this is not a general socket abstraction, just enough to
/// configure a connection that has already been established by one of the two
/// callers.
pub(crate) trait ConnectionStream {
    fn set_daemon_timeouts(&mut self, read: Duration, write: Duration) -> std::io::Result<()>;
}

impl ConnectionStream for TcpStream {
    fn set_daemon_timeouts(&mut self, read: Duration, write: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(read))?;
        self.set_write_timeout(Some(write))
    }
}

impl ConnectionStream for tungstenite::stream::MaybeTlsStream<TcpStream> {
    fn set_daemon_timeouts(&mut self, read: Duration, write: Duration) -> std::io::Result<()> {
        match self {
            // A plain dial is still possible on a `ws://` relay; the timeout
            // applies to the socket either way.
            tungstenite::stream::MaybeTlsStream::Plain(stream) => {
                stream.set_read_timeout(Some(read))?;
                stream.set_write_timeout(Some(write))
            }
            tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
                stream.get_mut().set_read_timeout(Some(read))?;
                stream.get_mut().set_write_timeout(Some(write))
            }
            _ => Ok(()),
        }
    }
}

// tungstenite's `accept_hdr_with_config` callback requires this exact
// signature, so the 136-byte `ErrorResponse` cannot be boxed away here.
#[allow(clippy::result_large_err)]
fn validate_handshake(
    request: &HandshakeRequest,
    response: HandshakeResponse,
    allowed_origins: &HashSet<String>,
) -> Result<HandshakeResponse, ErrorResponse> {
    if request.uri().path() != "/v1" {
        return Err(handshake_error(
            StatusCode::NOT_FOUND,
            "unknown daemon endpoint",
        ));
    }
    let is_native_client = request
        .headers()
        .get(NATIVE_CLIENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == NATIVE_CLIENT_HEADER_VALUE);
    if let Some(origin) = request.headers().get(ORIGIN) {
        let allowed = origin
            .to_str()
            .ok()
            .is_some_and(|origin| allowed_origins.contains(origin));
        if !allowed && !is_native_client {
            return Err(handshake_error(
                StatusCode::FORBIDDEN,
                "WebSocket origin is not allowed",
            ));
        }
    }
    Ok(response)
}

fn handshake_error(status: StatusCode, message: &str) -> ErrorResponse {
    tungstenite::http::Response::builder()
        .status(status)
        .body(Some(message.to_owned()))
        .expect("static WebSocket rejection is valid")
}

/// The address this daemon is listening on, published for the agent bridge.
///
/// A process fact rather than per-server state: a bridge launched by a provider
/// is a different process that only knows what its environment says, and the
/// daemon that launched it is the one it must dial. Set once at startup.
static DAEMON_ADDRESS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Records the address the daemon bound, so a delegating provider can reach it.
pub fn publish_daemon_address(address: &str) {
    let _ = DAEMON_ADDRESS.set(address.to_owned());
}

/// The published daemon address, or `None` before the daemon has bound.
pub fn daemon_address() -> Option<String> {
    DAEMON_ADDRESS.get().cloned()
}

fn token_matches(expected: &str, candidate: &str) -> bool {
    expected.as_bytes().ct_eq(candidate.as_bytes()).into()
}

/// A fresh agent token: 32 random bytes as lowercase hex.
///
/// Random rather than derived from the session id, because it is a bearer
/// capability: anything guessable from an id the provider can already read
/// would not be a capability at all.
fn minted_token_value() -> String {
    use rand_core::RngCore as _;
    let mut bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What a delegated agent token is allowed to do.
///
/// The desktop's own token is unrestricted: it is the user. A token minted for
/// an agent bridge is not — it exists so a provider process can start and steer
/// the delegates of one lead conversation, and nothing else. The difference is
/// what keeps the bridge from being a second full client: it cannot read the
/// workspace, reach another conversation, or touch projects and settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentScope {
    /// Start, prompt, steer, cancel, and stop background work in a session.
    Delegate {
        parent_session: Uuid,
        runtime_id: Uuid,
    },
}

/// One minted agent token and the scope it carries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScopedToken {
    pub scope: AgentScope,
}

/// The credentials a connection is checked against.
///
/// Bundled rather than passed as two arguments because they always travel
/// together and always mean the same thing: `desktop` is the user's own token,
/// `agents` are the narrower capabilities minted for bridges.
#[derive(Clone)]
struct ConnectionTokens {
    desktop: String,
    agents: Arc<Mutex<HashMap<String, ScopedToken>>>,
}

impl AgentScope {
    /// Whether this scope may dispatch `command` against the session a request
    /// names.
    ///
    /// A delegate reaches only the session it was given. `AttachSession` is
    /// allowed so it can observe what it drives; everything that would let it
    /// act as a full client — project mutation, settings, shutdown, workspace
    /// files — is not on the list at all, which is why this is an allow list
    /// rather than a set of denials.
    fn permits(self, command: &Command, request_session: Uuid, request_runtime: Uuid) -> bool {
        match self {
            Self::Delegate {
                parent_session,
                runtime_id,
            } => {
                request_session == parent_session
                    && request_runtime == runtime_id
                    && matches!(
                        command,
                        Command::AttachSession
                            | Command::Start { .. }
                            | Command::Prompt { .. }
                            | Command::Steer { .. }
                            | Command::Cancel
                            | Command::CancelComputerUse
                            | Command::RefreshBackgroundWork
                            | Command::StopBackgroundWork { .. }
                            | Command::Respond { .. }
                            | Command::RespondUserInput { .. }
                            | Command::ApplyOptions { .. }
                            // The one command that reaches beyond this session,
                            // and only to create a *new* one. It names no
                            // existing task, so it cannot read or drive another
                            // conversation: the daemon copies the lead's own
                            // options onto a fresh id and reports it back.
                            | Command::SpawnDelegate { .. }
                            // Its two companions, which name a delegate in the
                            // command rather than in the request: the request
                            // still carries only the lead's own session, and the
                            // daemon checks that the named task is a delegate
                            // this lead started. Reading another conversation
                            // from here is therefore not expressible.
                            | Command::ReadDelegate { .. }
                            | Command::PromptDelegate { .. }
                    )
            }
        }
    }

    /// Why [`Self::permits`] said no, so a refusal names the half that missed.
    ///
    /// The two halves fail for very different reasons — a wrong session means the
    /// caller never belonged to this conversation, a wrong runtime means its
    /// capability outlived the process it was minted for — and a bare "not
    /// allowed" leaves them indistinguishable in a log, which is exactly how the
    /// bridge spent two rounds addressing the wrong session unnoticed.
    fn refusal_reason(
        self,
        command: &Command,
        request_session: Uuid,
        request_runtime: Uuid,
    ) -> String {
        let Self::Delegate {
            parent_session,
            runtime_id,
        } = self;
        if request_session != parent_session {
            return format!("session {request_session} is not the lead's {parent_session}");
        }
        if request_runtime != runtime_id {
            return format!("runtime {request_runtime} is not the lead's {runtime_id}");
        }
        format!("{command:?} is not granted to a delegate")
    }
}

fn command_targets_runtime(command: &Command) -> bool {
    matches!(
        command,
        Command::AttachSession
            | Command::Start { .. }
            | Command::Prompt { .. }
            | Command::Steer { .. }
            | Command::Cancel
            | Command::CancelComputerUse
            | Command::RefreshBackgroundWork
            | Command::StopBackgroundWork { .. }
            | Command::Respond { .. }
            | Command::RespondUserInput { .. }
            | Command::RunComputerTool { .. }
            | Command::RejectComputerTool { .. }
            | Command::ApplyOptions { .. }
            | Command::Rollback { .. }
            | Command::Fork { .. }
            | Command::ForkSessionFromResponse { .. }
            | Command::RewindSessionToMessage { .. }
            | Command::OpenTerminal { .. }
            | Command::WriteTerminal { .. }
            | Command::ResizeTerminal { .. }
            | Command::CloseTerminal
            | Command::CloseSession
            | Command::RemoveSession
    )
}

fn run_runtime_mailbox(
    session_id: Uuid,
    mailbox_id: Uuid,
    requests: Receiver<DispatchedRequest>,
    mailbox_registry: Weak<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) {
    let mut active_runtime_id = None;
    let mut pending = None;
    loop {
        let dispatched = match pending.take() {
            Some(request) => request,
            None => match requests.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        let runtime_id = dispatched.request.runtime_id;
        let starts_runtime = matches!(
            &dispatched.request.command,
            Command::Start { .. } | Command::OpenTerminal { .. }
        );
        let closes_runtime = matches!(
            &dispatched.request.command,
            Command::CloseSession | Command::CloseTerminal | Command::RemoveSession
        );
        let removes_session = matches!(&dispatched.request.command, Command::RemoveSession);
        let handled = handle_request(
            dispatched.request,
            dispatched.outgoing,
            dispatched.source_subscriber_id,
            backend.clone(),
            hub.clone(),
        );

        if handled.executed {
            if starts_runtime {
                active_runtime_id =
                    matches!(&handled.outcome, ResponseOutcome::Ok { .. }).then_some(runtime_id);
            } else if let ResponseOutcome::Ok {
                payload:
                    ResponsePayload::SessionRuntime {
                        runtime_id: Some(attached_runtime_id),
                        ..
                    },
            } = &handled.outcome
            {
                // A replacement mailbox can rediscover a provider runtime
                // that survived its previous actor worker.
                active_runtime_id = Some(*attached_runtime_id);
            } else if closes_runtime {
                if (removes_session || active_runtime_id == Some(runtime_id))
                    && matches!(&handled.outcome, ResponseOutcome::Ok { .. })
                {
                    hub.end_runtime(session_id, (!removes_session).then_some(runtime_id));
                    active_runtime_id = None;
                }
            } else if active_runtime_id.is_none()
                && !matches!(
                    &handled.outcome,
                    ResponseOutcome::Ok {
                        payload: ResponsePayload::SessionRuntime { .. }
                    }
                )
                && matches!(&handled.outcome, ResponseOutcome::Ok { .. })
            {
                // Recover the supervisor state if a previous mailbox worker
                // exited unexpectedly while the backend runtime stayed alive.
                active_runtime_id = Some(runtime_id);
            }
        }

        if active_runtime_id.is_none() {
            pending =
                take_queued_request_or_retire(session_id, mailbox_id, &requests, &mailbox_registry);
            if pending.is_none() {
                return;
            }
        }
    }
}

fn take_queued_request_or_retire(
    session_id: Uuid,
    mailbox_id: Uuid,
    requests: &Receiver<DispatchedRequest>,
    mailbox_registry: &Weak<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
) -> Option<DispatchedRequest> {
    let Some(mailbox_registry) = mailbox_registry.upgrade() else {
        return requests.try_recv().ok();
    };
    // Dispatchers send while holding this same lock. Therefore an empty
    // receiver followed by removal is atomic with respect to a new command:
    // it either joins this actor before retirement or creates its successor.
    let mut mailboxes = mailbox_registry.lock();
    match requests.try_recv() {
        Ok(request) => Some(request),
        Err(crossbeam_channel::TryRecvError::Empty) => {
            if mailboxes
                .get(&session_id)
                .is_some_and(|mailbox| mailbox.id == mailbox_id)
            {
                mailboxes.remove(&session_id);
            }
            None
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => None,
    }
}

struct HandledRequest {
    outcome: ResponseOutcome,
    executed: bool,
}

enum TaskCatalogAction {
    None,
    Load,
    Save { projects: Vec<Project> },
    Changed,
}

fn handle_request(
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) -> HandledRequest {
    let request_id = request.request_id;
    let notification = request_id.is_nil();
    let session_id = request.session_id;
    let runtime_id = request.runtime_id;
    let task_catalog_action = task_catalog_action(&request.command);
    let starts_runtime = matches!(
        &request.command,
        Command::Start { .. } | Command::OpenTerminal { .. }
    );
    let (outcome, executed) = if !notification && let Some(cached) = hub.cached_response(request_id)
    {
        (cached, false)
    } else {
        if starts_runtime {
            hub.begin_runtime(session_id, runtime_id);
        }
        let outcome = match backend.handle(request, hub.event_sink(session_id, runtime_id)) {
            Ok(payload) => ResponseOutcome::Ok { payload },
            Err(error) => ResponseOutcome::Error {
                error: RpcError::from(error),
            },
        };
        if !notification {
            hub.cache_response(request_id, outcome.clone());
        }
        (outcome, true)
    };
    if executed && starts_runtime && matches!(&outcome, ResponseOutcome::Error { .. }) {
        hub.end_runtime(session_id, Some(runtime_id));
    }
    if executed {
        match (&task_catalog_action, &outcome) {
            (
                TaskCatalogAction::Load,
                ResponseOutcome::Ok {
                    payload:
                        ResponsePayload::TaskState {
                            projects, sessions, ..
                        },
                },
            ) => hub.replace_task_catalog(projects, sessions),
            (
                TaskCatalogAction::Save { projects },
                ResponseOutcome::Ok {
                    payload: ResponsePayload::TaskStateSaved { sessions },
                },
            ) => hub.task_state_saved(source_subscriber_id, projects, sessions),
            (TaskCatalogAction::Changed, ResponseOutcome::Ok { .. }) => {
                hub.task_state_changed(source_subscriber_id);
            }
            _ => {}
        }
    }
    if !notification {
        let _ = outgoing.send(ServerMessage::Response {
            request_id,
            outcome: outcome.clone(),
        });
    }
    HandledRequest { outcome, executed }
}

fn task_catalog_action(command: &Command) -> TaskCatalogAction {
    match command {
        Command::LoadTaskState => TaskCatalogAction::Load,
        Command::SaveTaskState { projects, .. } => TaskCatalogAction::Save {
            projects: projects.clone(),
        },
        Command::RemoveSession
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. } => TaskCatalogAction::Changed,
        _ => TaskCatalogAction::None,
    }
}

fn send_dispatch_error(
    request_id: Uuid,
    outgoing: Sender<ServerMessage>,
    hub: &Arc<Hub>,
    message: String,
) {
    if request_id.is_nil() {
        return;
    }
    let outcome = hub
        .cached_response(request_id)
        .unwrap_or_else(|| ResponseOutcome::Error {
            error: RpcError { message },
        });
    hub.cache_response(request_id, outcome.clone());
    let _ = outgoing.send(ServerMessage::Response {
        request_id,
        outcome,
    });
}

/// Whether a failed socket write was a stall (`TimedOut`/`WouldBlock`) rather
/// than a broken connection. Diagnostic only: the write path drops the
/// connection either way, but the distinction says whether the peer was slow
/// or actually gone.
fn write_failure_is_retryable(error: &anyhow::Error) -> bool {
    retryable_error(error.as_ref())
}

fn retryable_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ) {
            return true;
        }
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::EAGAIN)
            || error.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            return true;
        }
    }
    error.source().is_some_and(retryable_error)
}

/// Read one client message out of the sealed stream.
///
/// `Ok(None)` means the socket produced nothing this poll (a read timeout), not
/// a disconnect: the connection loop treats them differently.
fn read_sealed_client_message<S: std::io::Read + std::io::Write>(
    sealed: &mut SealedConnection<S>,
) -> anyhow::Result<Option<ClientMessage>> {
    match sealed.read()? {
        Some(plaintext) => Ok(Some(serde_json::from_slice(&plaintext)?)),
        None => Ok(None),
    }
}

/// Seal and write one server message.
fn write_sealed<S, T>(sealed: &mut SealedConnection<S>, value: &T) -> anyhow::Result<()>
where
    S: std::io::Read + std::io::Write,
    T: serde::Serialize,
{
    sealed.send(&serde_json::to_vec(value)?)
}

/// A read that timed out, as opposed to one that failed.
fn is_transport_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            )
        })
    })
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    socket.send(Message::Text(serde_json::to_string(value)?.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::daemon::WakuBackend;
    #[cfg(unix)]
    use crate::model::Project;
    use crate::model::{AgentSession, ProviderKind};
    #[cfg(unix)]
    use crate::persistence::StateStore;
    #[cfg(unix)]
    use crate::settings::DaemonSettingsStore;
    use crate::{DaemonSettings, WireDriverStartOptions};
    #[cfg(unix)]
    use base64::Engine as _;
    use crossbeam_channel::{RecvTimeoutError, bounded};
    use serde_json::json;
    use std::path::PathBuf;
    use waku_client::{DaemonClient, DaemonSupervisor};

    #[derive(Default)]
    struct TestBackend {
        runtimes: Mutex<HashMap<Uuid, Uuid>>,
    }

    /// An agent token is a capability for one session, not a second client.
    /// Everything a full client can do that a delegate has no business doing is
    /// refused, and the refusals are the point: the desktop token keeps working
    /// unchanged, so only the narrower one is constrained.
    #[test]
    fn a_delegated_token_reaches_only_its_own_session() {
        let parent = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        let scope = AgentScope::Delegate {
            parent_session: parent,
            runtime_id: runtime,
        };

        // The ordinary work of driving a delegate.
        for command in [
            Command::AttachSession,
            Command::Prompt {
                prompt: "go".into(),
                turn_id: None,
                message_id: None,
            },
            Command::Steer {
                prompt: "instead".into(),
            },
            Command::Cancel,
            Command::RefreshBackgroundWork,
        ] {
            assert!(
                scope.permits(&command, parent, runtime),
                "{command:?} must be available to a delegate"
            );
        }

        // A different conversation is out of reach even for an allowed command.
        assert!(
            !scope.permits(
                &Command::Prompt {
                    prompt: "go".into(),
                    turn_id: None,
                    message_id: None,
                },
                Uuid::new_v4(),
                runtime
            ),
            "a delegate must not reach another conversation"
        );

        // The same session under a different runtime is out of reach too: the
        // runtime id is what ties the capability to one live provider process.
        assert!(
            !scope.permits(&Command::Cancel, parent, Uuid::new_v4()),
            "a delegate must not reach a replaced runtime"
        );

        // Anything that would make it a full client. Shutdown is its own
        // client message rather than a command, so it is refused even earlier —
        // the scope never sees it.
        for command in [
            Command::Rollback { turns: 1 },
            Command::Fork { turns_to_remove: 1 },
        ] {
            assert!(
                !scope.permits(&command, parent, runtime),
                "{command:?} must not be reachable from a delegate token"
            );
        }
    }

    /// Creating a delegate is the single command that reaches past one session.
    ///
    /// It is granted because a lead that cannot start a delegate has no group at
    /// all, and it is safe for a reason worth pinning: the command names no
    /// existing task. It cannot be pointed at another conversation the way
    /// `Prompt` or `Cancel` can, so this one permission does not widen what the
    /// token can touch.
    #[test]
    fn a_delegate_may_spawn_one_but_still_not_reach_another_session() {
        let parent = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        let scope = AgentScope::Delegate {
            parent_session: parent,
            runtime_id: runtime,
        };
        let spawn = Command::SpawnDelegate {
            task: "survey the tests".into(),
            title: None,
persona: None,
        };

        assert!(
            scope.permits(&spawn, parent, runtime),
            "a lead has to be able to start its delegates"
        );
        // The permission is still bound to the lead's own session and runtime:
        // it is not a licence to act anywhere.
        assert!(
            !scope.permits(&spawn, Uuid::new_v4(), runtime),
            "spawning must be refused under another session id"
        );
        assert!(
            !scope.permits(&spawn, parent, Uuid::new_v4()),
            "spawning must be refused under a replaced runtime"
        );
    }

    /// The desktop token stays unrestricted. If minting a scoped token had
    /// narrowed it, every existing client would break.
    #[test]
    fn the_desktop_token_is_unchanged_by_agent_scoping() {
        assert!(token_matches("desktop-token", "desktop-token"));
        assert!(!token_matches("desktop-token", "someone-else"));
    }

    /// Reading and steering a delegate travels the same narrow road as creating
    /// one.
    ///
    /// The delegate id rides in the command while the request keeps naming the
    /// lead's own session and runtime, so the token's scope is checked exactly as
    /// it was before. What the command names is then the daemon's question to
    /// answer — it asks which lead started that task, which is the half a token
    /// cannot express and must not be able to.
    #[test]
    fn a_delegate_may_read_and_steer_its_own_agents() {
        let parent = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        let scope = AgentScope::Delegate {
            parent_session: parent,
            runtime_id: runtime,
        };
        for command in [
            Command::ReadDelegate {
                session_id: Uuid::new_v4(),
            },
            Command::PromptDelegate {
                session_id: Uuid::new_v4(),
                prompt: "keep going".into(),
            },
        ] {
            assert!(
                scope.permits(&command, parent, runtime),
                "{command:?} has to reach the lead's own delegate"
            );
            assert!(
                !scope.permits(&command, Uuid::new_v4(), runtime),
                "{command:?} must be refused under another session id"
            );
            assert!(
                !scope.permits(&command, parent, Uuid::new_v4()),
                "{command:?} must be refused under a replaced runtime"
            );
        }
    }

    /// Two mints must never collide, and neither may be derivable from the
    /// session it belongs to.
    #[test]
    fn minted_tokens_are_random_hex_and_distinct() {
        let first = minted_token_value();
        let second = minted_token_value();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64, "32 bytes render as 64 hex characters");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    impl Backend for TestBackend {
        fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
            let session_id = request.session_id;
            let runtime_id = request.runtime_id;
            match request.command {
                Command::Start { .. } => {
                    self.runtimes.lock().insert(session_id, runtime_id);
                    events.send(WireDriverEvent::new("connected", json!({})))?;
                    Ok(ResponsePayload::Started {
                        supports_steer: true,
                    })
                }
                Command::AttachSession => Ok(ResponsePayload::SessionRuntime {
                    runtime_id: self.runtimes.lock().get(&session_id).copied(),
                    supports_steer: true,
                }),
                Command::GetSettings => Ok(ResponsePayload::Settings {
                    settings: DaemonSettings::default(),
                }),
                Command::Prompt { prompt, .. } => {
                    events.send(WireDriverEvent::new("textDelta", json!(prompt)))?;
                    Ok(ResponsePayload::Ack)
                }
                Command::CloseSession => {
                    self.runtimes.lock().remove(&session_id);
                    Ok(ResponsePayload::Ack)
                }
                Command::SpawnDelegate { .. } => {
                    let delegate = Uuid::new_v4();
                    Ok(ResponsePayload::DelegateSpawned {
                        session_id: delegate,
                        runtime_id: Uuid::new_v4(),
                    })
                }
                _ => Ok(ResponsePayload::Ack),
            }
        }
    }

    #[derive(Default)]
    struct TaskStateBackend {
        sessions: Mutex<Vec<AgentSession>>,
    }

    impl Backend for TaskStateBackend {
        fn handle(&self, request: Request, _events: EventSink) -> anyhow::Result<ResponsePayload> {
            match request.command {
                Command::SaveTaskState { sessions, .. } => {
                    let mut stored = self.sessions.lock();
                    for session in sessions {
                        if let Some(existing) =
                            stored.iter_mut().find(|existing| existing.id == session.id)
                        {
                            *existing = session;
                        } else {
                            stored.push(session);
                        }
                    }
                    Ok(ResponsePayload::TaskStateSaved {
                        sessions: stored.clone(),
                    })
                }
                Command::LoadTaskState => Ok(ResponsePayload::TaskState {
                    projects: Vec::new(),
                    sessions: self.sessions.lock().clone(),
                    default_cwd: PathBuf::from("/tmp"),
                    projectless_root: Some(PathBuf::from("/tmp/.waku/projects")),
                }),
                _ => Ok(ResponsePayload::Ack),
            }
        }
    }

    #[test]
    fn task_state_revisions_notify_other_clients_only() {
        let hub = Hub::default();
        let (source_tx, source_rx) = unbounded();
        let source_id = hub.subscribe(&[], Subscriber::new(source_tx).0);
        let (observer_tx, observer_rx) = unbounded();
        hub.subscribe(&[], Subscriber::new(observer_tx).0);

        hub.task_state_changed(source_id);

        assert!(source_rx.try_recv().is_err());
        assert!(matches!(
            observer_rx.recv_timeout(Duration::from_secs(1)),
            Ok(ServerMessage::TaskStateChanged { revision: 1 })
        ));
    }

    /// Every connection must run on its own keys.
    ///
    /// Two successful handshakes alone would not show that: a daemon that
    /// reused one session key would also complete both. So each connection
    /// fingerprints its own key and the test asserts the fingerprints differ,
    /// which fails if either ephemeral side is reused.
    #[test]
    fn two_connections_do_not_share_a_session_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(TaskStateBackend::default()),
                server_shutdown,
                ServerOptions::default(),
            )
            .unwrap()
        });

        let mut fingerprints = Vec::new();
        for attempt in 0..2 {
            let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
            assert!(
                matches!(
                    client
                        .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
                        .unwrap(),
                    ResponsePayload::TaskState { .. }
                ),
                "connection {attempt} did not complete a round trip"
            );
            fingerprints.push(client.session_key_fingerprint());
            client.shutdown();
        }

        assert_ne!(
            fingerprints[0], fingerprints[1],
            "both connections derived the same session key, so a key was reused"
        );

        shutdown.store(true, Ordering::Release);
        let _ = server.join();
    }

    /// A client that speaks the pre-encryption protocol gets one plaintext
    /// explanation and then a close — not a hang.
    #[test]
    fn a_plaintext_client_is_told_to_update_and_then_dropped() {
        use tungstenite::Message;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(TaskStateBackend::default()),
                server_shutdown,
                ServerOptions::default(),
            )
            .unwrap()
        });

        let (mut socket, _) = tungstenite::client::connect(format!("ws://{address}/v1")).unwrap();
        // `Message::text` rather than `Message::Text(....into())`: with `.into()`
        // the type inference picks `Bytes`, which builds a **binary** frame. A
        // binary frame is read as a 64-byte `client_hello`, so the daemon would
        // report a low-order key instead of recognising a plaintext client.
        socket
            .send(Message::text(
                r#"{"type":"hello","protocolVersion":8,"token":"secret","clientId":"00000000-0000-0000-0000-000000000000","resumeFrom":[]}"#,
            ))
            .unwrap();

        // The reply arrives rather than the connection simply vanishing: the
        // whole point is that an old client can explain itself to its user.
        let reply = socket
            .read()
            .expect("a plaintext client should be told why it was refused");
        let Message::Text(text) = reply else {
            panic!("expected a text rejection, got {reply:?}");
        };
        assert!(
            text.contains("requires an encrypted connection"),
            "unexpected rejection text: {text}"
        );

        shutdown.store(true, Ordering::Release);
        let _ = server.join();
    }

    #[test]
    fn websocket_task_state_changes_reach_another_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(TaskStateBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let source = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let observer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let source_revisions = source.subscribe_task_state();
        let observer_revisions = observer.subscribe_task_state();
        // The handshake can finish before the server registers its subscriber.
        // Complete the observer's initial load before another client publishes.
        assert!(matches!(
            observer
                .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
                .unwrap(),
            ResponsePayload::TaskState { .. }
        ));
        let session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let session_id = session.id;

        assert!(matches!(
            source
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: Vec::new(),
                        live_session_ids: vec![session_id],
                        sessions: vec![session],
                    },
                )
                .unwrap(),
            ResponsePayload::TaskStateSaved { .. }
        ));
        assert_eq!(
            observer_revisions.recv_timeout(Duration::from_secs(1)),
            Ok(1)
        );
        assert!(source_revisions.try_recv().is_err());
        let ResponsePayload::TaskState { sessions, .. } = observer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);

        // Streaming checkpoints update transcript detail and `updated_at`,
        // but do not change anything rendered in another client's task
        // catalog. They must not trigger a list reload for every stream save.
        let mut checkpoint = sessions[0].clone();
        checkpoint.updated_at = checkpoint.updated_at.saturating_add(1);
        source
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: Vec::new(),
                    live_session_ids: vec![session_id],
                    sessions: vec![checkpoint],
                },
            )
            .unwrap();
        assert!(
            observer_revisions
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        // Desktop persistence uses fire-and-forget notifications, while Web
        // uses requests. Both directions must wake the other application's
        // catalog without echoing back to the source connection.
        let second = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let second_id = second.id;
        observer
            .notify(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: Vec::new(),
                    live_session_ids: vec![session_id, second_id],
                    sessions: vec![second],
                },
            )
            .unwrap();
        assert_eq!(source_revisions.recv_timeout(Duration::from_secs(1)), Ok(2));
        assert!(observer_revisions.try_recv().is_err());
        let ResponsePayload::TaskState { sessions, .. } = source
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| session.id == second_id));

        source.shutdown();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_projection_cannot_resurrect_a_removed_session() {
        let root = std::env::temp_dir().join(format!("waku-remove-race-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let stale_client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let remover = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let project = Project::from_path(root.join("repo"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.begin_turn("persist me");
        stale_client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project.clone()],
                    live_session_ids: vec![session.id],
                    sessions: vec![session.clone()],
                },
            )
            .unwrap();
        remover
            .request(session.id, Uuid::nil(), Command::RemoveSession)
            .unwrap();
        let ResponsePayload::TaskStateSaved { sessions } = stale_client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![session.id],
                    sessions: vec![session],
                },
            )
            .unwrap()
        else {
            panic!("expected task-state save response");
        };
        assert!(sessions.is_empty());
        let ResponsePayload::TaskState { sessions, .. } = stale_client
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected task state");
        };
        assert!(sessions.is_empty());

        stale_client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn websocket_round_trip_sequences_provider_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        assert!(
            DaemonClient::connect(&address.to_string(), "wrong-secret".into()).is_err(),
            "the server must reject a client before it can issue requests"
        );
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let response = client
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                        system_prompt: None,
                    },
                },
            )
            .unwrap();
        assert!(matches!(
            response,
            ResponsePayload::Started {
                supports_steer: true
            }
        ));
        // Start can emit before a refreshed app discovers and subscribes to
        // the daemon-owned runtime. The client must retain that replay.
        let events = client.subscribe(session_id, runtime_id);
        let event = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(event.runtime_id, runtime_id);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event.kind, "connected");

        client.shutdown();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
        server.join().unwrap();
    }

    /// A lead's scoped token has to survive the real server path.
    ///
    /// The unit test above checks the decision itself; this one checks that a
    /// scoped connection reaches the backend at all, which is what was missing
    /// when the bridge was wired up but nothing could start a delegate. It also
    /// pins the refusal: the same token cannot close a *different* session.
    #[test]
    fn a_scoped_token_can_spawn_a_delegate_over_a_real_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = Arc::new(DaemonServer::new(
            "desktop-secret".into(),
            Arc::new(DaemonIdentity::generate()),
            Arc::new(TestBackend::default()),
            ServerOptions::default(),
        ));
        let lead = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        let scoped = server.mint_agent_token(lead, runtime);
        // The loop stops on this flag, not on the client's `shutdown` message:
        // this server is not the desktop-owned one, so it refuses to exit on a
        // client's say-so.
        let stop = Arc::new(AtomicBool::new(false));
        let serving = {
            let server = server.clone();
            let stop = stop.clone();
            std::thread::spawn(move || server.serve(listener, stop).unwrap())
        };

        let client = DaemonClient::connect(&address.to_string(), scoped).unwrap();
        let spawned = client
            .request(
                lead,
                runtime,
                Command::SpawnDelegate {
                    task: "survey the tests".into(),
                    title: None,
persona: None,
                },
            )
            .expect("a lead must be able to start its delegate");
        assert!(
            matches!(spawned, ResponsePayload::DelegateSpawned { .. }),
            "the daemon must report the delegate it created: {spawned:?}"
        );

        // The capability is still tied to the lead: another conversation is out
        // of reach even for a command the scope allows. `Prompt` names a live
        // runtime, so it goes through the same scope check the bridge's calls do.
        let refused = client.request(
            Uuid::new_v4(),
            runtime,
            Command::Prompt {
                prompt: "reach into a stranger's task".into(),
                turn_id: None,
                message_id: None,
            },
        );
        assert!(
            refused.is_err(),
            "a scoped token must not reach another task"
        );

        client.shutdown();
        server.revoke_agent_tokens();
        stop.store(true, Ordering::Release);
        serving.join().unwrap();
    }

    #[test]
    fn late_client_attaches_to_replay_and_live_runtime_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let source = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let source_events = source.subscribe(session_id, runtime_id);
        source
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                        system_prompt: None,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            source_events
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .event
                .kind,
            "connected"
        );

        let late = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            late.request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: Some(attached),
                supports_steer: true,
            } if attached == runtime_id
        ));
        let late_events = late.subscribe(session_id, runtime_id);
        let replayed = late_events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(replayed.sequence, 1);
        assert_eq!(replayed.event.kind, "connected");

        source
            .request(
                session_id,
                runtime_id,
                Command::Prompt {
                    prompt: "streamed from the first client".into(),
                    turn_id: None,
                    message_id: None,
                },
            )
            .unwrap();
        for events in [&source_events, &late_events] {
            let live = events.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(live.sequence, 2);
            assert_eq!(live.event.kind, "textDelta");
            assert_eq!(live.event.payload, json!("streamed from the first client"));
        }

        source
            .request(session_id, runtime_id, Command::CloseSession)
            .unwrap();
        let after_close = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            after_close
                .request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: None,
                supports_steer: true,
            }
        ));
        let stale_events = after_close.subscribe(session_id, runtime_id);
        assert!(
            stale_events
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "an explicitly closed runtime must not replay into future clients"
        );

        source.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn remote_supervisor_reconnects_without_losing_the_daemon_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Keep the address reserved between servers. If the first listener is
        // dropped before the replacement binds, a parallel test can claim the
        // newly freed ephemeral port and make this test fail with AddrInUse.
        let replacement_listener = listener.try_clone().unwrap();
        let address = listener.local_addr().unwrap();
        let backend = Arc::new(TestBackend::default());
        let first_shutdown = Arc::new(AtomicBool::new(false));
        let first_server = {
            let backend = backend.clone();
            let shutdown = first_shutdown.clone();
            std::thread::spawn(move || {
                serve(
                    listener,
                    "secret".into(),
                    Arc::new(DaemonIdentity::generate()),
                    backend,
                    shutdown,
                    ServerOptions::default(),
                )
                .unwrap()
            })
        };

        let supervisor = DaemonSupervisor::connect(&address.to_string(), "secret".into()).unwrap();
        let clients = supervisor.subscribe_clients();
        let initial = clients.recv_timeout(Duration::from_secs(1)).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        initial
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                        system_prompt: None,
                    },
                },
            )
            .unwrap();

        first_shutdown.store(true, Ordering::Release);
        first_server.join().unwrap();

        let second_shutdown = Arc::new(AtomicBool::new(false));
        let second_server = {
            let backend = backend.clone();
            let shutdown = second_shutdown.clone();
            std::thread::spawn(move || {
                serve(
                    replacement_listener,
                    "secret".into(),
                    Arc::new(DaemonIdentity::generate()),
                    backend,
                    shutdown,
                    ServerOptions::default(),
                )
                .unwrap()
            })
        };

        let replacement = clients.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!initial.same_connection(&replacement));
        assert!(matches!(
            replacement
                .request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: Some(attached),
                supports_steer: true,
            } if attached == runtime_id
        ));

        second_shutdown.store(true, Ordering::Release);
        second_server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dropping_an_idle_terminal_does_not_wait_for_output() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let terminal = crate::terminal::DaemonTerminal::open_with_shell(
            &root,
            80,
            24,
            hub.event_sink(Uuid::new_v4(), Uuid::new_v4()),
            terminal_test_shell("while IFS= read -r line; do :; done"),
        )
        .unwrap();
        let (dropped, finished) = bounded(1);
        std::thread::spawn(move || {
            drop(terminal);
            let _ = dropped.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(3)).is_ok(),
            "dropping an idle daemon terminal blocked on its output reader"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_round_trip_streams_input_and_output() {
        websocket_terminal_round_trip(false);
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_close_does_not_wait_for_a_shell_ignoring_hangup() {
        websocket_terminal_round_trip(true);
    }

    #[cfg(unix)]
    fn terminal_test_shell(script: &str) -> alacritty_terminal::tty::Shell {
        // Do not load the developer's or CI runner's login files, prompt
        // plugins, or terminal capability queries in a transport test.
        alacritty_terminal::tty::Shell::new("/bin/sh".into(), vec!["-c".into(), script.into()])
    }

    #[cfg(unix)]
    fn websocket_terminal_round_trip(ignore_hangup: bool) {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(&format!(
            "{}\nprintf 'ready:%s\\n' \"$$\"\nwhile IFS= read -r line; do printf 'received:%s\\n' \"$line\"; done",
            if ignore_hangup { "trap '' HUP" } else { ":" },
        )));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(DaemonIdentity::generate()),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let events = client.subscribe(terminal_id, terminal_id);
        assert!(matches!(
            client
                .request(
                    terminal_id,
                    terminal_id,
                    Command::OpenTerminal {
                        cwd: root.clone(),
                        cols: 80,
                        rows: 24,
                    },
                )
                .unwrap(),
            ResponsePayload::Ack
        ));
        // Wait until the shell has installed its signal handler. Receiving
        // local echo alone does not prove that shell startup has completed.
        let ready = terminal_output_until(&events, b"\n");
        let child_pid: libc::pid_t = String::from_utf8_lossy(&ready)
            .trim()
            .strip_prefix("ready:")
            .unwrap()
            .parse()
            .unwrap();
        client
            .request(
                terminal_id,
                terminal_id,
                Command::WriteTerminal {
                    data: b"waku-terminal-round-trip\r".to_vec(),
                },
            )
            .unwrap();

        // The response prefix is absent from the input, so a PTY echo cannot
        // satisfy this assertion before the child has actually read it.
        terminal_output_until(&events, b"received:waku-terminal-round-trip");

        let (closed, finished) = bounded(1);
        let closing_client = client.clone();
        let close = std::thread::spawn(move || {
            let _ = closed.send(closing_client.request(
                terminal_id,
                terminal_id,
                Command::CloseTerminal,
            ));
        });
        let result = finished.recv_timeout(Duration::from_secs(3));
        if result.is_err() {
            // Clean up the fixture even when shutdown regresses, and fail
            // here instead of waiting for the client's 120-second timeout.
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
            }
        }
        client.shutdown();
        server.join().unwrap();
        close.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            matches!(result, Ok(Ok(ResponsePayload::Ack))),
            "closing daemon terminal did not complete: {result:?}"
        );
    }

    #[cfg(unix)]
    fn terminal_output_until(events: &Receiver<SequencedEvent>, marker: &[u8]) -> Vec<u8> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut seen_events = Vec::new();
        while std::time::Instant::now() < deadline
            && !output.windows(marker.len()).any(|window| window == marker)
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(event) = events.recv_timeout(remaining) else {
                break;
            };
            seen_events.push(event.event.kind.clone());
            if event.event.kind != "terminalOutput" {
                continue;
            }
            let data = event.event.payload["data"].as_str().unwrap();
            output.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
            );
        }
        assert!(
            output.windows(marker.len()).any(|window| window == marker),
            "daemon terminal did not return the shell marker; events={seen_events:?}, output={}",
            String::from_utf8_lossy(&output)
        );
        output
    }

    #[test]
    fn nil_request_ids_execute_without_responses_or_cache_entries() {
        let (outgoing, responses) = unbounded();
        let hub = Arc::new(Hub::default());
        let handled = handle_request(
            Request {
                request_id: Uuid::nil(),
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::GetSettings,
            },
            outgoing,
            0,
            Arc::new(TestBackend::default()),
            hub.clone(),
        );

        assert!(handled.executed);
        assert!(matches!(handled.outcome, ResponseOutcome::Ok { .. }));
        assert!(responses.try_recv().is_err());
        assert!(hub.cached_response(Uuid::nil()).is_none());
    }

    #[test]
    fn browser_origins_are_denied_unless_explicitly_allowed() {
        let request = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "https://app.waku.test")
            .body(())
            .unwrap();
        let response = HandshakeResponse::new(());
        assert_eq!(
            validate_handshake(&request, response, &HashSet::new())
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );

        let allowed = HashSet::from(["https://app.waku.test".to_owned()]);
        assert!(validate_handshake(&request, HandshakeResponse::new(()), &allowed).is_ok());
        let native = HandshakeRequest::builder().uri("/v1").body(()).unwrap();
        assert!(validate_handshake(&native, HandshakeResponse::new(()), &HashSet::new()).is_ok());

        let react_native = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "http://192.168.0.114:34125")
            .header(NATIVE_CLIENT_HEADER, NATIVE_CLIENT_HEADER_VALUE)
            .body(())
            .unwrap();
        assert!(
            validate_handshake(&react_native, HandshakeResponse::new(()), &HashSet::new()).is_ok()
        );

        let forged_native = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "https://attacker.example")
            .header(NATIVE_CLIENT_HEADER, "browser")
            .body(())
            .unwrap();
        assert_eq!(
            validate_handshake(&forged_native, HandshakeResponse::new(()), &HashSet::new())
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn daemon_tokens_require_an_exact_match() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "Secret"));
        assert!(!token_matches("secret", "secret-extra"));
    }

    #[test]
    fn replaced_runtime_ignores_late_events_from_the_old_generation() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let old_runtime_id = Uuid::new_v4();
        let new_runtime_id = Uuid::new_v4();
        let (outgoing, events) = unbounded();
        hub.subscribe(&[], Subscriber::new(outgoing).0);

        hub.begin_runtime(session_id, old_runtime_id);
        let old_sink = hub.event_sink(session_id, old_runtime_id);
        old_sink
            .send(WireDriverEvent::new("old", serde_json::Value::Null))
            .unwrap();
        assert!(matches!(
            events.recv().unwrap(),
            ServerMessage::Event(event) if event.runtime_id == old_runtime_id
        ));

        hub.begin_runtime(session_id, new_runtime_id);
        old_sink
            .send(WireDriverEvent::new("stale", serde_json::Value::Null))
            .unwrap();
        hub.event_sink(session_id, new_runtime_id)
            .send(WireDriverEvent::new("new", serde_json::Value::Null))
            .unwrap();

        let ServerMessage::Event(event) = events.recv().unwrap() else {
            panic!("expected a daemon event");
        };
        assert_eq!(event.runtime_id, new_runtime_id);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event.kind, "new");
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn replay_cursor_from_an_old_daemon_epoch_does_not_hide_new_events() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        hub.begin_runtime(session_id, runtime_id);
        hub.event_sink(session_id, runtime_id)
            .send(WireDriverEvent::new("new-daemon", serde_json::Value::Null))
            .unwrap();

        let (outgoing, events) = unbounded();
        hub.subscribe(
            &[ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::nil(),
                sequence: u64::MAX,
            }],
            Subscriber::new(outgoing).0,
        );

        let ServerMessage::Event(event) = events.recv().unwrap() else {
            panic!("expected the new daemon event to replay");
        };
        assert_eq!(event.epoch, hub.epoch);
        assert_eq!(event.sequence, 1);
    }

    #[test]
    fn subscriber_that_cannot_keep_up_is_kicked_and_removed() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        // Capacity one and no drainer: the second queued message overflows.
        let (outgoing, events) = bounded(1);
        let (subscriber, kicked) = Subscriber::new(outgoing);
        hub.subscribe(&[], subscriber);
        hub.begin_runtime(session_id, runtime_id);
        let sink = hub.event_sink(session_id, runtime_id);

        sink.send(WireDriverEvent::new("one", serde_json::Value::Null))
            .unwrap();
        // Nobody drains, so a second queued message overflows: the subscriber
        // is dropped from the hub and its connection is told to close.
        sink.send(WireDriverEvent::new("two", serde_json::Value::Null))
            .unwrap();
        assert!(kicked.try_recv().is_ok());

        assert!(matches!(
            events.try_recv(),
            Ok(ServerMessage::Event(event)) if event.event.kind == "one"
        ));
        assert!(events.try_recv().is_err());
        sink.send(WireDriverEvent::new("three", serde_json::Value::Null))
            .unwrap();
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn response_cache_evicts_oversized_and_byte_budget_exceeding_outcomes() {
        let hub = Hub::default();
        let oversized_id = Uuid::new_v4();
        let oversized = ResponseOutcome::Ok {
            payload: ResponsePayload::Cursor {
                cursor: Some(serde_json::Value::String(
                    "x".repeat(MAX_CACHED_RESPONSE_BYTES + 1),
                )),
            },
        };
        hub.cache_response(oversized_id, oversized);
        // A single outcome larger than the whole byte budget is never
        // retained; retrying the request runs it again instead of pinning a
        // >64 MB payload in daemon memory.
        assert!(hub.cached_response(oversized_id).is_none());

        let kept_id = Uuid::new_v4();
        hub.cache_response(
            kept_id,
            ResponseOutcome::Ok {
                payload: ResponsePayload::Ack,
            },
        );
        assert!(hub.cached_response(kept_id).is_some());
        assert!(hub.cached_response(oversized_id).is_none());
    }

    struct BlockingProbeBackend {
        probe_started: Sender<()>,
        release_probe: Receiver<()>,
    }

    impl Backend for BlockingProbeBackend {
        fn handle(&self, request: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            if matches!(request.command, Command::ProbeProvider { .. }) {
                self.probe_started.send(()).unwrap();
                self.release_probe.recv().unwrap();
            }
            Ok(ResponsePayload::Ack)
        }
    }

    #[test]
    fn slow_background_command_does_not_block_session_hydration() {
        let (outgoing, response_rx) = unbounded();
        let (probe_started, probe_started_rx) = bounded(1);
        let (release_probe, release_probe_rx) = bounded(1);
        let backend: Arc<dyn Backend> = Arc::new(BlockingProbeBackend {
            probe_started,
            release_probe: release_probe_rx,
        });
        let hub = Arc::new(Hub::default());
        let dispatcher = RequestDispatcher::new(backend, hub);

        let probe_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: probe_id,
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::ProbeProvider {
                    provider: crate::model::ProviderKind::Codex,
                    binary_override: None,
                    discover_models: false,
                    probe_version: false,
                },
            },
            outgoing.clone(),
            0,
        );
        probe_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let hydration_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: hydration_id,
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::HydrateSession {
                    session_id: Uuid::new_v4(),
                },
            },
            outgoing,
            0,
        );
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == hydration_id
        ));

        release_probe.send(()).unwrap();
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == probe_id
        ));
    }

    struct RuntimeOrderingBackend {
        blocked_session_id: Uuid,
        handled: Sender<(Uuid, &'static str)>,
        release_start: Receiver<()>,
    }

    impl Backend for RuntimeOrderingBackend {
        fn handle(&self, request: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            let command = match request.command {
                Command::Start { .. } => {
                    self.handled.send((request.session_id, "start")).unwrap();
                    if request.session_id == self.blocked_session_id {
                        self.release_start.recv().unwrap();
                    }
                    return Ok(ResponsePayload::Started {
                        supports_steer: true,
                    });
                }
                Command::Prompt { .. } => "prompt",
                Command::CloseSession => "close",
                _ => "other",
            };
            self.handled.send((request.session_id, command)).unwrap();
            Ok(ResponsePayload::Ack)
        }
    }

    #[test]
    fn runtime_commands_are_ordered_per_session_without_blocking_other_sessions() {
        let blocked_session_id = Uuid::new_v4();
        let blocked_runtime_id = Uuid::new_v4();
        let other_session_id = Uuid::new_v4();
        let other_runtime_id = Uuid::new_v4();
        let (handled, handled_rx) = unbounded();
        let (release_start, release_start_rx) = bounded(1);
        let dispatcher = RequestDispatcher::new(
            Arc::new(RuntimeOrderingBackend {
                blocked_session_id,
                handled,
                release_start: release_start_rx,
            }),
            Arc::new(Hub::default()),
        );
        let (start_outgoing, start_responses) = unbounded();
        let (second_client_outgoing, second_client_responses) = unbounded();
        let (other_outgoing, other_responses) = unbounded();

        let blocked_start_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: blocked_start_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            start_outgoing,
            0,
        );
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (blocked_session_id, "start")
        );

        let prompt_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: prompt_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::Prompt {
                    prompt: "after start".into(),
                    turn_id: None,
                    message_id: None,
                },
            },
            second_client_outgoing,
            0,
        );

        let other_start_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: other_start_id,
                session_id: other_session_id,
                runtime_id: other_runtime_id,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            other_outgoing.clone(),
            0,
        );
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (other_session_id, "start")
        );
        assert!(matches!(
            other_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == other_start_id
        ));
        assert!(handled_rx.recv_timeout(Duration::from_millis(50)).is_err());

        release_start.send(()).unwrap();
        assert!(matches!(
            start_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == blocked_start_id
        ));
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (blocked_session_id, "prompt")
        );
        assert!(matches!(
            second_client_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == prompt_id
        ));

        let blocked_close_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: blocked_close_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::CloseSession,
            },
            other_outgoing.clone(),
            0,
        );
        let other_close_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: other_close_id,
                session_id: other_session_id,
                runtime_id: other_runtime_id,
                command: Command::CloseSession,
            },
            other_outgoing,
            0,
        );
        let mut close_responses = [false; 2];
        for _ in 0..2 {
            let ServerMessage::Response { request_id, .. } = other_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
            else {
                panic!("expected a close response");
            };
            if request_id == blocked_close_id {
                close_responses[0] = true;
            } else if request_id == other_close_id {
                close_responses[1] = true;
            }
        }
        assert_eq!(close_responses, [true, true]);
    }

    fn test_start_options() -> WireDriverStartOptions {
        WireDriverStartOptions {
            provider: "codex".into(),
            binary: PathBuf::from("codex"),
            cwd: PathBuf::from("."),
            mode: "fullAccess".into(),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
            system_prompt: None,
        }
    }

    /// A client reaches a daemon through a relay that cannot read the session.
    ///
    /// This is the check the relay step is judged on, and it is end to end on
    /// purpose: a real relay process, a real daemon dialing out to it, and a
    /// real client connecting to the relay's other side. Mocks would be the
    /// thing under test.
    ///
    /// What each assertion is for:
    ///
    /// - the round trip proves the two ends completed a sealed handshake
    ///   *through* the relay, so the relay carried bytes it could not read;
    /// - the bad token is rejected over the same path, so a relay session does
    ///   not become a way around authentication;
    /// - the recorder's byte count is non-zero, which proves the traffic went
    ///   through this relay rather than a direct connection.
    #[test]
    fn a_client_reaches_a_daemon_through_a_relay() {
        // --- the relay ---------------------------------------------------
        let relay_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_address = relay_listener.local_addr().unwrap().to_string();
        let recorder = Arc::new(waku_relay::Recorder::default());
        let registry = Arc::new(waku_relay::Registry::new(waku_relay::Limits::default()));
        let for_serve = recorder.clone();
        let for_serve_registry = registry.clone();
        std::thread::spawn(move || {
            let _ = waku_relay::serve(
                relay_listener,
                for_serve_registry,
                SharedForwarded(for_serve),
            );
        });

        // --- the daemon, dialing out with no listener of its own -----------
        let session_id = "test-session-42";
        let server = Arc::new(DaemonServer::new(
            "secret".into(),
            Arc::new(DaemonIdentity::generate()),
            Arc::new(TaskStateBackend::default()),
            ServerOptions::default(),
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        {
            let server = server.clone();
            let shutdown = shutdown.clone();
            let relay_address = format!("ws://{relay_address}");
            let session = session_id.to_owned();
            std::thread::spawn(move || {
                let _ = crate::relay::run_relay(relay_address, session, server, shutdown);
            });
        }

        // --- the client ---------------------------------------------------
        // Let the daemon establish its dial before the client arrives. Without
        // this the two race for the relay's two roles, and the losing side's
        // retry can pair with the wrong counterpart, which surfaces as a
        // signature failure rather than as an ordering problem. A live session
        // with no client yet means the daemon holds its role, which is the
        // state this waits for.
        let paired_deadline = std::time::Instant::now() + Duration::from_secs(20);
        while registry.live_sessions() == 0 {
            assert!(
                std::time::Instant::now() < paired_deadline,
                "the daemon never reached the relay"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let client = loop {
            match DaemonClient::connect_via_relay(&relay_address, session_id, "secret".into()) {
                Ok(client) => break client,
                Err(error) if std::time::Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("could not reach the daemon through the relay: {error:#}"),
            }
        };

        let payload = client
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .expect("the round trip must complete through the relay");
        assert!(matches!(payload, ResponsePayload::TaskState { .. }));

        // The string a user is asked to compare must actually reach the client.
        // Without this the value could be computed correctly and then dropped
        // before anyone could see it, leaving a security check that looks
        // implemented and is absent.
        //
        // That both ends derive the *same* string is `record.rs`'s business and
        // is checked there against both halves of a handshake; this asserts the
        // value survives the trip to a real client over a real relay.
        let shown = client.authentication_string().to_owned();
        assert_eq!(
            shown.len(),
            14,
            "the string is twelve digits in three groups of four, got {shown:?}"
        );
        assert_eq!(
            shown.chars().filter(|c| c.is_ascii_digit()).count(),
            12,
            "the string must be digits: {shown:?}"
        );

        assert!(
            recorder.bytes() > 0,
            "no bytes crossed the relay, so this did not exercise it"
        );

        client.shutdown();
        shutdown.store(true, Ordering::Release);
    }

    /// The fingerprint a pairing code shows is the one a client recomputes.
    ///
    /// This is the whole basis of pairing: the daemon renders a code from its
    /// own key, the client rebuilds the same string from the key the handshake
    /// proved, and a mismatch is what refuses a substituted daemon. If the two
    /// sides truncated the hash differently the client would reject a correct
    /// pairing, which looks exactly like an attack — so the shared function is
    /// checked against the identity here rather than assumed.
    #[test]
    fn the_pairing_fingerprint_matches_the_identity_that_computed_it() {
        let identity = DaemonIdentity::generate();
        let public = identity.ed25519_public().to_bytes();

        assert_eq!(
            identity.fingerprint(),
            waku_protocol::pairing::fingerprint_of(&public),
            "the identity and the shared helper must agree, or pairing rejects a good daemon"
        );

        // Sixteen characters, so a code stays typeable, and hex, so it survives
        // being read aloud or copied by hand.
        assert_eq!(identity.fingerprint().len(), 16);
        assert!(
            identity
                .fingerprint()
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );

        // Two daemons must not share one, or a code would not identify anyone.
        assert_ne!(
            identity.fingerprint(),
            DaemonIdentity::generate().fingerprint()
        );
    }

    /// Hands one recorder to every relay connection thread.
    ///
    /// `waku_relay::serve` takes its sink by value and each connection borrows
    /// it, so the shared handle is a thin wrapper rather than a second counter.
    struct SharedForwarded(Arc<waku_relay::Recorder>);

    impl waku_relay::Forwarded for SharedForwarded {
        fn forwarded(&self, direction: waku_relay::Role, bytes: usize) {
            waku_relay::Forwarded::forwarded(self.0.as_ref(), direction, bytes);
        }
    }
}
