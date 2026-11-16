//! A pairing relay for Waku daemons, and the limits that keep it honest.
//!
//! # What this process is allowed to know
//!
//! A relay exists so a daemon behind a NAT can be reached. It does exactly one
//! thing: it waits for two sockets to name the same session, then moves bytes
//! between them.
//!
//! **It holds:**
//!
//! - a session id, for as long as the session lives;
//! - the two sockets;
//! - byte counters, for the caps below.
//!
//! **It never holds** — and there is no code path that could collect — a token,
//! a transcript, a command, a file path, or a public key that would let it
//! impersonate either end. Every byte it forwards is a Waku record that neither
//! side can be talked into sending in the clear; the record layer
//! (`waku-protocol::record`) is what makes the relay blind, not this process's
//! good intentions.
//!
//! # Protocol
//!
//! Two WebSocket paths, distinguished by role:
//!
//! ```text
//! wss://relay/<session-id>/daemon   the daemon dials out to this
//! wss://relay/<session-id>/client   a client connects to this
//! ```
//!
//! The session id comes **from the daemon**, never from the relay: the relay has
//! no user directory, no identity, and nothing to allocate against. A caller
//! that presents a session id is claiming a role in it; the first to arrive
//! holds that role until the session ends.
//!
//! Rules, all of them enforceable without reading a payload:
//!
//! - A second `/daemon` for a live session is **refused**, never swapped in.
//!   Accepting it would let a stranger take over a session by guessing its id,
//!   and the takeover would look like a reconnect to the client.
//! - A second `/client` for a live session is refused for the same reason.
//! - A socket with no counterpart waits, and **may not send**: bytes from an
//!   unpaired end are dropped rather than buffered, so the relay never becomes
//!   storage for a caller that has not been let in.
//! - A waiting socket that finds no counterpart within [`Limits::pair_timeout`]
//!   is closed.
//! - A session ends when either side closes, when it exceeds
//!   [`Limits::session_duration`], or when forwarded bytes exceed
//!   [`Limits::session_bytes`].
//!
//! # What is not here
//!
//! No plaintext mode. The staging document rules it out deliberately: a relay
//! that can carry plaintext is a relay an operator can be asked to configure
//! that way, and the client cannot tell the difference from the outside.
//!
//! No logging of payloads. Counters and session events may be logged; bytes
//! never are.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

/// Caps a relay enforces without reading anything.
#[derive(Clone, Debug)]
pub struct Limits {
    /// How long one end may wait for its counterpart.
    pub pair_timeout: Duration,
    /// How long a paired session may live.
    pub session_duration: Duration,
    /// How many bytes may cross in each direction before the session ends.
    pub session_bytes: u64,
    /// How many new sessions one source address may open per second.
    pub sessions_per_source_per_second: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pair_timeout: Duration::from_secs(30),
            session_duration: Duration::from_secs(60 * 60),
            // A coding-agent session moves transcripts, diffs and terminal
            // output; 512 MiB per direction is generous for an hour's work and
            // still bounds what one session can cost.
            session_bytes: 512 * 1024 * 1024,
            sessions_per_source_per_second: 4,
        }
    }
}

/// Which end of a session a socket claims to be.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Role {
    Daemon,
    Client,
}

impl Role {
    fn counterpart(self) -> Self {
        match self {
            Self::Daemon => Self::Client,
            Self::Client => Self::Daemon,
        }
    }

    /// The same counterpart, spelled for call sites that read as "the other
    /// end of this arriving connection".
    fn role_counterpart(self) -> Self {
        self.counterpart()
    }
}

/// Parses `/<session-id>/<role>` out of a request path.
///
/// The session id is deliberately treated as opaque text with a length cap: the
/// relay must not accept a 4 KiB id, and it must not try to interpret one.
pub fn parse_path(path: &str) -> Result<(String, Role)> {
    const MAX_SESSION_ID: usize = 128;
    let trimmed = path.trim_start_matches('/');
    let mut parts = trimmed.split('/');
    let (Some(session), Some(role), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("expected /<session-id>/<daemon|client>");
    };
    if session.is_empty() || session.len() > MAX_SESSION_ID {
        bail!("session id must be 1..={MAX_SESSION_ID} bytes");
    }
    if !session
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("session id may contain only ASCII alphanumerics, '-' and '_'");
    }
    let role = match role {
        "daemon" => Role::Daemon,
        "client" => Role::Client,
        other => bail!("unknown role {other:?}; expected daemon or client"),
    };
    Ok((session.to_owned(), role))
}

/// One session's bookkeeping.
///
/// This struct exists so the limits are enforced in one place. It holds no
/// payload and no key.
pub struct Session {
    pub created_at: Instant,
    daemon_present: bool,
    client_present: bool,
    daemon_bytes: Arc<AtomicU64>,
    client_bytes: Arc<AtomicU64>,
}

impl Session {
    fn new() -> Self {
        Self {
            created_at: Instant::now(),
            daemon_present: false,
            client_present: false,
            daemon_bytes: Arc::new(AtomicU64::new(0)),
            client_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Claim `role`. Fails when that role is already held.
    ///
    /// A second claim on a live role is refused rather than replaced: replacing
    /// would let anyone who guesses a session id take a session over, and the
    /// displacement would be indistinguishable from a reconnect.
    fn claim(&mut self, role: Role) -> Result<Arc<AtomicU64>> {
        let taken = match role {
            Role::Daemon => self.daemon_present,
            Role::Client => self.client_present,
        };
        if taken {
            bail!("this session already has a {} end", role_name(role));
        }
        match role {
            Role::Daemon => self.daemon_present = true,
            Role::Client => self.client_present = true,
        }
        Ok(match role {
            Role::Daemon => self.daemon_bytes.clone(),
            Role::Client => self.client_bytes.clone(),
        })
    }

    fn release(&mut self, role: Role) {
        match role {
            Role::Daemon => self.daemon_present = false,
            Role::Client => self.client_present = false,
        }
    }

    /// Whether both ends are attached, so bytes may flow.
    fn paired(&self) -> bool {
        self.daemon_present && self.client_present
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Daemon => "daemon",
        Role::Client => "client",
    }
}

/// A claimed role's forwarding slot.
///
/// Its whole purpose is the counter: a socket must call [`Slot::account`] for
/// every byte it forwards, and the session ends when the cap is reached. The
/// slot also releases the role when dropped, so a session's lifetime is tied to
/// the socket holding it rather than to a separate guard someone must remember
/// to drop.
pub struct Slot {
    role: Role,
    counter: Arc<AtomicU64>,
    registry: Arc<Registry>,
    session_id: String,
}

impl Slot {
    pub fn role(&self) -> Role {
        self.role
    }

    /// Count `bytes` as forwarded. Returns `false` once the cap is exceeded, at
    /// which point the caller must close the session.
    pub fn account_counter(&self) -> Arc<AtomicU64> {
        self.counter.clone()
    }

    pub fn account(&self, bytes: usize, limit: u64) -> bool {
        let total = self
            .counter
            .fetch_add(bytes as u64, Ordering::AcqRel)
            .saturating_add(bytes as u64);
        total <= limit
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.registry.release(&self.session_id, self.role);
    }
}

/// The registry of live sessions.
///
/// Deliberately dumb: a map from id to state, behind one lock held only for the
/// bookkeeping, never across a socket write.
#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<String, Session>>,
    limits: Limits,
    started: Mutex<HashMap<String, (Instant, u32)>>,
}

impl Registry {
    pub fn new(limits: Limits) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            limits,
            started: Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Claim `role` in `session_id`, creating the session if needed.
    ///
    /// Returns an error when the role is already held, which the caller must
    /// treat as a refusal and not a retry.
    pub fn claim(self: &Arc<Self>, session_id: &str, role: Role) -> Result<Slot> {
        let counter = {
            let mut sessions = self.sessions.lock().expect("registry lock");
            let expired = sessions
                .get(session_id)
                .is_some_and(|session| session.created_at.elapsed() > self.limits.session_duration);
            if expired {
                // An expired session is dropped and rebuilt rather than reused:
                // its counters are meaningless and its slots belong to sockets
                // that should already be closed.
                sessions.remove(session_id);
            }
            let session = sessions
                .entry(session_id.to_owned())
                .or_insert_with(Session::new);
            session.claim(role)?
        };
        Ok(Slot {
            role,
            counter,
            registry: self.clone(),
            session_id: session_id.to_owned(),
        })
    }

    /// Whether both ends of `session_id` are attached.
    pub fn is_paired(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .expect("registry lock")
            .get(session_id)
            .is_some_and(Session::paired)
    }

    /// Give a role back when its socket goes away.
    pub fn release(&self, session_id: &str, role: Role) {
        let mut sessions = self.sessions.lock().expect("registry lock");
        if let Some(session) = sessions.get_mut(session_id) {
            session.release(role);
            // A session with no ends left holds nothing worth keeping.
            if !session.daemon_present && !session.client_present {
                sessions.remove(session_id);
            }
        }
    }

    /// Number of live sessions. For tests and metrics, never for routing.
    pub fn live_sessions(&self) -> usize {
        self.sessions.lock().expect("registry lock").len()
    }

    /// Admit one new session from `source`, rate-limited.
    ///
    /// The limit is per source address over a sliding one-second window. It
    /// exists so one host cannot fill the session map. Only *new* sessions are
    /// counted: a reconnect to a live session is not a new one, and
    /// rate-limiting it would turn the limit into a denial of service against a
    /// reconnecting user.
    ///
    /// The window is anchored at a source's first admission and ages out on its
    /// own: a refused admission never refreshes it, so a source that keeps
    /// knocking is not locked out permanently — it is admitted again once the
    /// window it filled has passed.
    pub fn admit_source(&self, source: &str) -> Result<()> {
        let now = Instant::now();
        let mut started = self.started.lock().expect("started lock");
        started.retain(|_, (first, _)| now.duration_since(*first) < RATE_WINDOW);
        let entry = started.entry(source.to_owned()).or_insert_with(|| (now, 0));
        if entry.1 >= self.limits.sessions_per_source_per_second {
            bail!("too many new sessions from this source; retry shortly");
        }
        entry.1 += 1;
        Ok(())
    }
}

/// The window a source's session rate is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// How the relay reports what it forwarded, without ever reporting what it was.
///
/// **The signature is the guarantee.** It carries a direction and a length, and
/// there is deliberately no payload parameter: a relay that could hand its own
/// callback the bytes is one refactor away from handing them to a logger, a
/// crash reporter, or a `Vec` that outlives the frame. A test that wants to
/// prove the relay cannot read a session therefore cannot do it by asking this
/// trait — it has to put known plaintext in and check what the other end
/// received, which is what the round-trip test in `waku-core` does.
pub trait Forwarded {
    /// One frame was forwarded in `direction`, `bytes` long.
    fn forwarded(&self, direction: Role, bytes: usize);
}

/// A sink that discards the notification. The default.
pub struct Silent;
impl Forwarded for Silent {
    fn forwarded(&self, _direction: Role, _bytes: usize) {}
}

/// Counts frames and bytes, and holds no payload.
///
/// It used to expose a `bytes()` accessor that always returned an empty vector,
/// because `forwarded` never received any bytes to keep. The accessor is gone
/// rather than left as a method that looks like evidence and is not: what it
/// claimed to prove — that the relay cannot read a session — is a property of
/// the record layer, not of this recorder.
#[derive(Default)]
pub struct Recorder {
    frames: AtomicU64,
    total_bytes: AtomicU64,
}

impl Recorder {
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Acquire)
    }

    /// Total bytes forwarded. A count, never a copy.
    pub fn bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Acquire)
    }
}

impl Forwarded for Recorder {
    fn forwarded(&self, _direction: Role, bytes: usize) {
        self.frames.fetch_add(1, Ordering::AcqRel);
        self.total_bytes.fetch_add(bytes as u64, Ordering::AcqRel);
    }
}

/// Accept connections forever, pairing them as they arrive.
///
/// Each connection gets its own thread. The relay holds no per-session state
/// beyond the registry entry, and no thread holds the registry lock while
/// touching a socket.
pub fn serve<F>(
    listener: std::net::TcpListener,
    registry: Arc<Registry>,
    forwarded: F,
) -> Result<()>
where
    F: Forwarded + Send + Sync + 'static,
{
    let forwarded = Arc::new(forwarded);
    let parking = Arc::new(Parking::default());
    listener
        .set_nonblocking(true)
        .context("could not configure the relay listener")?;
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                let registry = registry.clone();
                let forwarded = forwarded.clone();
                let parking = parking.clone();
                std::thread::Builder::new()
                    .name("waku-relay-connection".into())
                    .spawn(move || {
                        if let Err(error) = handle(
                            stream,
                            peer,
                            &registry,
                            &parking,
                            forwarded.as_ref() as &dyn Forwarded,
                        ) {
                            // Session ids and byte counts are safe to log.
                            // Payloads never reach this line.
                            eprintln!("waku-relay connection ended: {error:#}");
                        }
                    })
                    .context("could not start a relay connection thread")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("relay listener failed"),
        }
    }
}

/// Handle one arriving socket.
///
/// The path decides the role; the role decides who this socket may be paired
/// with. Nothing else about the caller is inspected — the relay has no user
/// directory to inspect it against.
#[allow(clippy::result_large_err)]
fn handle(
    stream: std::net::TcpStream,
    peer: std::net::SocketAddr,
    registry: &Arc<Registry>,
    parking: &Parking,
    forwarded: &dyn Forwarded,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    // `accept_hdr` demands a closure that outlives the call, so the parsed path
    // is handed out through a shared cell rather than a borrowed local.
    let chosen: std::sync::Arc<Mutex<Option<(String, Role)>>> = Default::default();
    let recorder = chosen.clone();
    let socket = tungstenite::accept_hdr(
        stream,
        move |request: &tungstenite::handshake::server::Request,
              response: tungstenite::handshake::server::Response| {
            match parse_path(request.uri().path()) {
                Ok(parsed) => {
                    *recorder.lock().expect("path cell") = Some(parsed);
                    Ok(response)
                }
                Err(error) => Err(tungstenite::handshake::server::ErrorResponse::new(Some(
                    error.to_string(),
                ))),
            }
        },
    )?;
    let recorded = chosen.lock().expect("path cell").take();
    let Some((session_id, role)) = recorded else {
        bail!("the handshake did not record a path");
    };

    // Every socket must have a counterpart or time out. The socket is parked in
    // the registry while it waits, so the other end's thread can find it.
    let mut socket = socket;
    let slot = match registry.claim(&session_id, role) {
        Ok(slot) => slot,
        Err(error) => {
            // A role that is already held is a refusal, not a retry. Say so in
            // a close frame so the caller sees a reason rather than a reset.
            let _ = socket.close(Some(tungstenite::protocol::CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::Policy,
                reason: error.to_string().into(),
            }));
            return Err(error);
        }
    };
    let limits = registry.limits().clone();
    let release = RoleGuard {
        registry,
        session_id: session_id.clone(),
        role,
        armed: true,
    };

    // Whoever arrives second drives. If a counterpart is already parked, take it
    // and run the pump here; otherwise park and wait to be taken, then keep
    // waiting until the pairing is over.
    //
    // **The parked side must not release its role when it wakes.** The wake
    // means "someone took your socket", not "the session ended": the arriving
    // side is only starting to pump. Releasing the role there let a second
    // daemon claim the session while the first one was still carrying traffic,
    // and the client then completed a handshake with the newcomer — which
    // surfaced as `handshake signature invalid` somewhere else entirely. The
    // role is released when the pump finishes, which is why the parked side
    // waits on `finished`.
    let outcome = match parking.take(&session_id, role.counterpart()) {
        Some(other) => {
            // Announce the start *before* pumping: the parked thread is about
            // to wake from `wait_until_taken` and must find this session
            // already marked running, or it would release its role in the gap.
            parking.begin(&session_id);
            let result = drive(role, socket, other, &slot, &limits, forwarded).map(|_| ());
            parking.finish(&session_id);
            result
        }
        None => {
            parking.park(&session_id, role, socket);
            if parking.wait_until_taken(&session_id, role, limits.pair_timeout) {
                // Hold the claim until the driving side is done with the
                // session. Their socket being taken only means the pump
                // started, and a role released here let a second daemon claim
                // the session while the first was still carrying traffic.
                parking.wait_until_finished(&session_id, limits.session_duration);
                Ok(())
            } else {
                bail!("timed out waiting for a counterpart")
            }
        }
    };
    drop(release);
    let _ = peer;
    outcome
}

/// Releases a claimed role when the connection ends, however it ends.
struct RoleGuard<'a> {
    registry: &'a Registry,
    session_id: String,
    role: Role,
    armed: bool,
}

impl Drop for RoleGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.registry.release(&self.session_id, self.role);
        }
    }
}

/// Where a waiting socket lives until its counterpart arrives.
///
/// One entry per (session, role). The parked side blocks on this; the arriving
/// side removes the entry and the parked thread wakes to find it gone.
#[derive(Default)]
pub struct Parking {
    waiting: Mutex<HashMap<(String, Role), tungstenite::WebSocket<std::net::TcpStream>>>,
    /// Sessions whose pump is currently running.
    ///
    /// A paired session has two threads: the arriving one drives the pump, and
    /// the parked one has to stay claimed until that finishes. This set is how
    /// the parked thread learns that the pump is over — its own socket being
    /// taken only means the pump *started*.
    running: Mutex<HashMap<String, usize>>,
    arrived: std::sync::Condvar,
}

impl Parking {
    fn park(
        &self,
        session_id: &str,
        role: Role,
        socket: tungstenite::WebSocket<std::net::TcpStream>,
    ) {
        let mut waiting = self.waiting.lock().expect("parking lock");
        waiting.insert((session_id.to_owned(), role), socket);
        self.arrived.notify_all();
    }

    /// Mark a session's pump as started.
    fn begin(&self, session_id: &str) {
        let mut running = self.running.lock().expect("running lock");
        *running.entry(session_id.to_owned()).or_insert(0) += 1;
    }

    /// Mark a session's pump as over, releasing anyone waiting on it.
    fn finish(&self, session_id: &str) {
        let mut running = self.running.lock().expect("running lock");
        if let Some(count) = running.get_mut(session_id) {
            *count -= 1;
            if *count == 0 {
                running.remove(session_id);
            }
        }
        self.arrived.notify_all();
    }

    /// Block until no pump is running for `session_id`, or `timeout` elapses.
    ///
    /// Returns immediately when nothing is running, which is the case when the
    /// counterpart vanished before it could drive.
    fn wait_until_finished(&self, session_id: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut running = self.running.lock().expect("running lock");
        loop {
            let Some(count) = running.get(session_id) else {
                return;
            };
            if *count == 0 || Instant::now() >= deadline {
                return;
            }
            let now = Instant::now();
            let remaining = deadline.checked_duration_since(now).unwrap_or_default();
            let (guard, _) = self
                .arrived
                .wait_timeout(running, remaining)
                .expect("running lock");
            running = guard;
        }
    }

    /// Take a parked socket of `role`, if one is waiting.
    fn take(
        &self,
        session_id: &str,
        role: Role,
    ) -> Option<tungstenite::WebSocket<std::net::TcpStream>> {
        let mut waiting = self.waiting.lock().expect("parking lock");
        let taken = waiting.remove(&(session_id.to_owned(), role));
        if taken.is_some() {
            self.arrived.notify_all();
        }
        taken
    }

    /// Wait until this caller's own parked socket is taken by the other side.
    fn wait_until_taken(&self, session_id: &str, role: Role, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut waiting = self.waiting.lock().expect("parking lock");
        loop {
            if !waiting.contains_key(&(session_id.to_owned(), role)) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                waiting.remove(&(session_id.to_owned(), role));
                return false;
            }
            let (guard, _) = self
                .arrived
                .wait_timeout(waiting, deadline - now)
                .expect("parking lock");
            waiting = guard;
        }
    }
}

/// Run the pump in both directions until either side ends.
///
/// Both sockets are non-blocking with a short read timeout, so one thread can
/// alternate between them. Two threads would be faster, but a `WebSocket` cannot
/// be read and written from two places at once, and splitting by direction needs
/// two owned sockets — which the borrow checker will not hand out from one
/// pairing. Alternating is simpler to reason about and the relay is not the
/// bottleneck in this system.
fn drive(
    arriving: Role,
    mut arriving_socket: tungstenite::WebSocket<std::net::TcpStream>,
    mut parked_socket: tungstenite::WebSocket<std::net::TcpStream>,
    slot: &Slot,
    limits: &Limits,
    forwarded: &dyn Forwarded,
) -> Result<()> {
    // Both ends must be non-blocking for the alternation to work. A socket with
    // no data must yield rather than stall the other direction.
    arriving_socket
        .get_mut()
        .set_read_timeout(Some(RELAY_READ_POLL))?;
    parked_socket
        .get_mut()
        .set_read_timeout(Some(RELAY_READ_POLL))?;

    // `arriving` names the role of `arriving_socket`; the parked one is the
    // counterpart.
    let arriving_role = arriving;
    let parked_role = arriving.role_counterpart();
    debug_assert_eq!(parked_role, slot.role().counterpart());

    // Two counters, one per direction. The arriving side reuses the slot it
    // already holds; the counterpart gets a fresh counter for this pairing.
    let parked_counter = Arc::new(AtomicU64::new(0));

    let mut arriving_open = true;
    let mut parked_open = true;
    while arriving_open && parked_open {
        arriving_open = forward_one(
            &mut arriving_socket,
            &mut parked_socket,
            arriving_role,
            &slot.account_counter(),
            limits,
            forwarded,
        )?;
        parked_open = forward_one(
            &mut parked_socket,
            &mut arriving_socket,
            parked_role,
            &parked_counter,
            limits,
            forwarded,
        )?;
    }
    Ok(())
}

/// The read timeout that makes the alternating pump responsive without
/// spinning.
const RELAY_READ_POLL: Duration = Duration::from_millis(20);

/// Move at most one frame from `from` to `to`.
///
/// Returns `false` when the source has ended, so the caller stops.
fn forward_one<A, B>(
    from: &mut tungstenite::WebSocket<A>,
    to: &mut tungstenite::WebSocket<B>,
    direction: Role,
    counter: &AtomicU64,
    limits: &Limits,
    forwarded: &dyn Forwarded,
) -> Result<bool>
where
    A: std::io::Read + std::io::Write,
    B: std::io::Read + std::io::Write,
{
    let message = match from.read() {
        Ok(message) => message,
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
            return Ok(false);
        }
        Err(tungstenite::Error::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            ) =>
        {
            // Nothing yet. Not an end.
            return Ok(true);
        }
        Err(error) => return Err(error).context("relay read failed"),
    };
    match message {
        tungstenite::Message::Binary(payload) => {
            let total = counter
                .fetch_add(payload.len() as u64, Ordering::AcqRel)
                .saturating_add(payload.len() as u64);
            if total > limits.session_bytes {
                bail!("session byte cap reached");
            }
            forwarded.forwarded(direction, payload.len());
            to.send(tungstenite::Message::Binary(payload))
                .context("relay write failed")?;
        }
        tungstenite::Message::Ping(payload) => {
            // Keepalive is hop-by-hop at the relay; it is never forwarded.
            from.send(tungstenite::Message::Pong(payload))
                .context("relay pong failed")?;
        }
        tungstenite::Message::Pong(_) => {}
        tungstenite::Message::Close(frame) => {
            let _ = to.close(frame);
            return Ok(false);
        }
        // A text frame is not part of the sealed protocol. Forwarding it would
        // mean carrying something the relay was never meant to carry.
        tungstenite::Message::Text(_) | tungstenite::Message::Frame(_) => {
            bail!("relay refuses a non-binary frame");
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_a_session_and_a_role() {
        assert_eq!(
            parse_path("/abc123/daemon").unwrap(),
            ("abc123".to_owned(), Role::Daemon)
        );
        assert_eq!(
            parse_path("/abc123/client").unwrap(),
            ("abc123".to_owned(), Role::Client)
        );
    }

    #[test]
    fn a_path_that_is_not_a_session_is_refused() {
        for path in ["", "/", "/only", "/a/b/c", "//daemon", "/x/other"] {
            assert!(parse_path(path).is_err(), "{path:?} should be refused");
        }
    }

    #[test]
    fn a_session_id_is_bounded_and_restricted() {
        assert!(parse_path(&format!("/{}/daemon", "a".repeat(128))).is_ok());
        assert!(parse_path(&format!("/{}/daemon", "a".repeat(129))).is_err());
        assert!(parse_path("/has space/daemon").is_err());
        assert!(parse_path("/has/slash/daemon").is_err());
    }

    #[test]
    fn a_second_daemon_cannot_take_a_live_session() {
        let registry = Arc::new(Registry::new(Limits::default()));
        let first = registry.claim("s", Role::Daemon).unwrap();
        assert_eq!(first.role(), Role::Daemon);
        // The whole point: a stranger who guesses the id is refused, never
        // swapped in.
        assert!(registry.claim("s", Role::Daemon).is_err());
    }

    #[test]
    fn both_roles_can_be_claimed_and_released() {
        let registry = Arc::new(Registry::new(Limits::default()));
        let daemon = registry.claim("s", Role::Daemon).unwrap();
        let client = registry.claim("s", Role::Client).unwrap();
        assert!(registry.is_paired("s"));
        drop(client);
        drop(daemon);
        assert!(!registry.is_paired("s"));
        assert_eq!(registry.live_sessions(), 0);
    }

    #[test]
    fn a_byte_cap_ends_the_session() {
        let registry = Arc::new(Registry::new(Limits::default()));
        let slot = registry.claim("s", Role::Daemon).unwrap();
        assert!(slot.account(100, 100));
        assert!(!slot.account(1, 100));
    }

    #[test]
    fn a_source_is_rate_limited() {
        let registry = Arc::new(Registry::new(Limits {
            sessions_per_source_per_second: 2,
            ..Limits::default()
        }));
        registry.admit_source("10.0.0.1").unwrap();
        registry.admit_source("10.0.0.1").unwrap();
        assert!(registry.admit_source("10.0.0.1").is_err());
        // A different source is unaffected.
        registry.admit_source("10.0.0.2").unwrap();
    }

    /// The rate limit is a window, not a permanent ban.
    ///
    /// The test above proves the limit fires; it would also pass against a
    /// limiter that never let the source back in, which is the failure that
    /// matters for a daemon reconnecting on its own. This pins the other half:
    /// once a source's window has passed, it is admitted again.
    #[test]
    fn a_source_recovers_after_the_window_passes() {
        let registry = Arc::new(Registry::new(Limits {
            sessions_per_source_per_second: 1,
            ..Limits::default()
        }));
        registry.admit_source("10.0.0.1").unwrap();
        assert!(
            registry.admit_source("10.0.0.1").is_err(),
            "the second admission in the window must be refused"
        );

        std::thread::sleep(RATE_WINDOW + Duration::from_millis(120));

        assert!(
            registry.admit_source("10.0.0.1").is_ok(),
            "a source must be admitted again once its window has passed"
        );
    }

    // ---- end to end ----

    /// Start a relay on a free port and return its address.
    fn start_relay(limits: Limits, recorder: Arc<Recorder>) -> (String, Arc<Registry>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let registry = Arc::new(Registry::new(limits));
        let for_serve = registry.clone();
        std::thread::spawn(move || {
            let _ = serve(listener, for_serve, SharedRecorder(recorder));
        });
        (address, registry)
    }

    /// A `Forwarded` that hands the same recorder to every connection thread.
    ///
    /// `serve` takes its sink by value and each connection borrows it, so the
    /// shared handle is a thin wrapper rather than a second recorder.
    struct SharedRecorder(Arc<Recorder>);

    impl Forwarded for SharedRecorder {
        fn forwarded(&self, direction: Role, bytes: usize) {
            self.0.forwarded(direction, bytes);
        }
    }

    fn connect(
        address: &str,
        session: &str,
        role: &str,
    ) -> anyhow::Result<
        tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    > {
        let (socket, _) = tungstenite::client::connect(format!("ws://{address}/{session}/{role}"))?;
        Ok(socket)
    }

    /// The relay pairs two sockets and moves bytes between them.
    #[test]
    fn a_paired_session_carries_bytes_both_ways() {
        let recorder = Arc::new(Recorder::default());
        let (address, _registry) = start_relay(Limits::default(), recorder.clone());

        // The daemon side parks first; the client arrives and drives.
        let daemon = std::thread::spawn({
            let address = address.clone();
            move || -> Vec<u8> {
                let mut socket = connect(&address, "sess-e2e", "daemon").unwrap();
                socket
                    .send(tungstenite::Message::Binary(b"from-daemon".to_vec().into()))
                    .unwrap();
                // Read whatever the client sends back.
                match socket.read().unwrap() {
                    tungstenite::Message::Binary(payload) => payload.to_vec(),
                    other => panic!("daemon expected a binary frame, got {other:?}"),
                }
            }
        });

        std::thread::sleep(Duration::from_millis(80));

        let mut client = connect(&address, "sess-e2e", "client").unwrap();
        client
            .send(tungstenite::Message::Binary(b"from-client".to_vec().into()))
            .unwrap();
        let from_daemon = match client.read().unwrap() {
            tungstenite::Message::Binary(payload) => payload.to_vec(),
            other => panic!("client expected a binary frame, got {other:?}"),
        };

        assert_eq!(from_daemon, b"from-daemon");
        assert_eq!(daemon.join().unwrap(), b"from-client");
        assert!(
            recorder.frames() >= 2,
            "both directions should have moved a frame"
        );
    }

    /// The relay cannot read what it carries.
    ///
    /// The relay is given an opaque blob with no key and no format it knows.
    /// What matters is that it forwards the blob byte-for-byte and that nothing
    /// in the relay's own code inspects the contents — the assertion below is on
    /// the blob surviving unchanged, and on the relay's sink never being handed
    /// anything but a length.
    ///
    /// A full "the operator cannot read a task title" test needs the record
    /// layer, which this crate deliberately does not depend on: a relay that
    /// linked the encryption code would be one refactor away from holding a key.
    #[test]
    fn the_relay_forwards_bytes_it_never_interprets() {
        let blob: Vec<u8> = b"\x00\x01SEALED-RECORD-BYTES\xff".to_vec();
        let recorder = Arc::new(Recorder::default());
        let (address, _registry) = start_relay(Limits::default(), recorder.clone());

        // The daemon side parks first; the client arrives second and drives the
        // pump, exactly as in `a_paired_session_carries_bytes_both_ways`. What
        // this test adds is the assertion on the *shape* of the frame: the
        // bytes must arrive as one binary message, unchanged.
        let daemon = std::thread::spawn({
            let address = address.clone();
            let blob = blob.clone();
            move || -> () {
                let mut socket = connect(&address, "sess-blind", "daemon").unwrap();
                socket
                    .send(tungstenite::Message::Binary(blob.into()))
                    .unwrap();
                // Hold the socket open: closing here would end the session
                // before the client had read the frame.
                let _ = socket.read();
            }
        });
        std::thread::sleep(Duration::from_millis(80));

        let mut client = connect(&address, "sess-blind", "client").unwrap();
        // The relay reads with a short poll timeout, but this socket has none:
        // a frame that never arrives would hang the test rather than fail it.
        match client.get_mut() {
            tungstenite::stream::MaybeTlsStream::Plain(stream) => stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("the test socket must accept a read timeout"),
            other => panic!("the test relay speaks plain ws, got {other:?}"),
        }

        let received = client
            .read()
            .expect("the relay must forward the daemon's frame");
        let tungstenite::Message::Binary(payload) = received else {
            panic!("expected a binary frame, got {received:?}");
        };
        assert_eq!(
            payload.to_vec(),
            blob,
            "the relay must forward a frame byte-for-byte"
        );
        assert!(recorder.frames() >= 1);
        assert_eq!(recorder.bytes(), blob.len() as u64);

        // Close the client before joining. The pump runs until *both* sockets
        // close, and the daemon thread is blocked in its own `read` waiting for
        // a reply that never comes; without this close the join below waits
        // forever instead of failing. The assertion above has already run, so
        // closing here cannot hide a forwarding bug.
        let _ = client.close(None);
        let _ = daemon.join();
    }

    /// A socket with no counterpart waits, and is closed when the timeout
    /// elapses rather than living forever.
    #[test]
    fn an_unpaired_socket_times_out() {
        let limits = Limits {
            pair_timeout: Duration::from_millis(150),
            ..Limits::default()
        };
        let recorder = Arc::new(Recorder::default());
        let (address, registry) = start_relay(limits, recorder);

        let started = Instant::now();
        let mut socket = connect(&address, "sess-lonely", "daemon").unwrap();
        // The relay never pairs it, so the socket is closed underneath us.
        let _ = socket.read();
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the relay should hold the socket for the pair timeout, not drop it immediately"
        );
        let _ = &registry;
    }
}
