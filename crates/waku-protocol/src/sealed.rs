//! The sealed connection: a WebSocket socket whose every frame after the
//! handshake is an encrypted record.
//!
//! Both ends use this type, so the frame rules live in one place:
//!
//! - Before the handshake returns, nothing but the two hello frames may cross
//!   the wire.
//! - After it, every frame is `Message::Binary` holding one record. A text frame
//!   is a protocol violation and ends the connection rather than being skipped:
//!   a silent skip turns a downgrade attempt into a hang, which is the hardest
//!   failure to diagnose.
//!
//! **Counter order and wire order.** The counter is assigned and the frame is
//! written inside one call, under the caller's connection lock. If two callers
//! could take counters 5 and 6 and then write in the other order, the receiver
//! would see 6 then 5 and reject the connection as reordered — a bug that only
//! appears under concurrency and is invisible to a single-threaded test.

use std::io::{Read, Write};

use crate::record::handshake as wire;
use crate::record::{Direction, Opener, Sealer, SessionKeys};
use anyhow::{Context as _, Result, bail};
use tungstenite::Message;

/// Which end of the connection this is. It decides which key seals and which
/// opens; the two are mirror images.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Client,
    Daemon,
}

/// One end of a sealed connection, after the handshake.
pub struct SealedConnection<S> {
    socket: tungstenite::WebSocket<S>,
    sealer: Sealer,
    opener: Opener,
    keys: SessionKeys,
    side: Side,
}

impl<S: Read + Write> SealedConnection<S> {
    /// Take over an already-upgraded WebSocket as the **client**.
    ///
    /// Sends `client_hello`, reads `server_hello`, and verifies it. When
    /// `expected_ed25519` is set the daemon's signing key must match it, which
    /// is what stops a relay from substituting its own keys on a later
    /// connection.
    pub fn connect_as_client(
        mut socket: tungstenite::WebSocket<S>,
        client_static: x25519_dalek::StaticSecret,
        expected_ed25519: Option<&ed25519_dalek::VerifyingKey>,
    ) -> Result<Self> {
        let (handshake, client_hello) = wire::ClientHandshake::start(client_static);
        socket.send(Message::Binary(client_hello.into()))?;
        let server_hello = match read_binary_frame(&mut socket, "server hello") {
            Ok(hello) => hello,
            Err(FrameError::Plaintext) => {
                bail!(
                    "the daemon answered with a text frame; it does not speak the sealed protocol"
                )
            }
            Err(FrameError::Other(error)) => return Err(error),
        };
        let keys = handshake.finish(&server_hello, expected_ed25519)?;
        Ok(Self::from_keys(socket, keys, Side::Client))
    }

    /// Take over an already-upgraded WebSocket as the **daemon**.
    ///
    /// Reads `client_hello` and answers `server_hello`. Nothing else is read
    /// before this returns, so a client cannot slip a business message into the
    /// gap between the upgrade and the first record.
    ///
    /// A plaintext first frame is returned as [`AcceptError::Plaintext`] **along
    /// with the socket**, because the daemon owes that client one explanation
    /// frame before closing.
    pub fn accept_as_daemon(
        mut socket: tungstenite::WebSocket<S>,
        daemon_x25519: &x25519_dalek::StaticSecret,
        daemon_signing: &ed25519_dalek::SigningKey,
    ) -> Result<Self, Box<AcceptError<S>>> {
        let client_hello = match read_binary_frame(&mut socket, "client hello") {
            Ok(hello) => hello,
            Err(FrameError::Plaintext) => {
                return Err(Box::new(AcceptError::Plaintext(Box::new(socket))));
            }
            Err(FrameError::Other(error)) => {
                return Err(Box::new(AcceptError::Other(error)));
            }
        };
        let (server_hello, keys) = wire::accept(daemon_x25519, daemon_signing, &client_hello)
            .map_err(|error| Box::new(AcceptError::Other(error)))?;
        socket
            .send(Message::Binary(server_hello.into()))
            .map_err(|error| Box::new(AcceptError::Other(error.into())))?;
        Ok(Self::from_keys(socket, keys, Side::Daemon))
    }

    fn from_keys(socket: tungstenite::WebSocket<S>, keys: SessionKeys, side: Side) -> Self {
        let (sealer, opener) = match side {
            Side::Client => keys.client_side(),
            Side::Daemon => keys.daemon_side(),
        };
        Self {
            socket,
            sealer,
            opener,
            keys,
            side,
        }
    }

    /// Seal one message into a record and write it.
    ///
    /// **The counter is taken and the frame written here, with nothing between
    /// them.** A caller that must not interleave these with another caller's
    /// does so by owning this value behind a single lock; the guarantee the
    /// receiver enforces (strictly increasing counters) only holds if the
    /// counter order and the wire order are the same order.
    pub fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        let record = self.sealer.seal(plaintext)?;
        self.socket
            .send(Message::Binary(record.into()))
            .context("could not write a record")?;
        Ok(())
    }

    /// Read one message. `Ok(None)` means "nothing yet or the peer is gone";
    /// the caller decides which from the socket's own state.
    pub fn read(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let message = match self.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(None);
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    // A read timeout is not a disconnect: the caller polls.
                    return Ok(None);
                }
                Err(error) => return Err(error).context("could not read a record"),
            };
            match message {
                Message::Binary(record) => return self.opener.open(&record).map(Some),
                // A text frame here means the peer speaks the pre-encryption
                // protocol, or a middlebox stripped a record. Both end the
                // connection: continuing would mean guessing what it meant.
                Message::Text(_) => bail!(
                    "expected record, got text frame (the peer is not speaking the sealed protocol)"
                ),
                Message::Ping(payload) => self.socket.send(Message::Pong(payload))?,
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    let _ = self.socket.close(frame);
                    return Ok(None);
                }
                Message::Frame(_) => {}
            }
        }
    }

    /// The keys this connection runs on.
    pub fn keys(&self) -> &SessionKeys {
        &self.keys
    }

    pub fn side(&self) -> Side {
        self.side
    }

    /// A fingerprint of the outgoing session key, for tests that must show two
    /// connections did not reuse one.
    ///
    /// **Not the key.** Sealing a fixed plaintext at a known counter yields
    /// bytes that are equal exactly when the key is equal, so two fingerprints
    /// compare without any raw key ever becoming reachable. `SessionKeys`
    /// exposes no key accessor for the same reason: a value that cannot be read
    /// cannot be logged by accident.
    ///
    /// This consumes one counter on a throwaway sealer, so calling it does not
    /// disturb the connection's own counter.
    pub fn outgoing_key_fingerprint(&self) -> [u8; 32] {
        let mut probe = self.sealer_for_fingerprint();
        let record = probe
            .seal(&[0_u8; 32])
            .expect("sealing a 32-byte probe cannot fail");
        let mut out = [0_u8; 32];
        // `counter (8) || ciphertext (32) || tag (16)`: the ciphertext is the
        // key-dependent part, and a fixed plaintext makes it a function of the
        // key alone.
        out.copy_from_slice(&record[8..40]);
        out
    }

    fn sealer_for_fingerprint(&self) -> Sealer {
        let direction = match self.side {
            Side::Client => Direction::ClientToServer,
            Side::Daemon => Direction::ServerToClient,
        };
        self.keys.sealer(direction)
    }

    /// The underlying socket, for setting timeouts and config.
    pub fn socket_mut(&mut self) -> &mut tungstenite::WebSocket<S> {
        &mut self.socket
    }
}

/// Why a daemon could not accept a connection.
///
/// The plaintext case carries the socket back so the daemon can explain itself;
/// every other case is a plain error. That socket is large (it owns a TLS and
/// TCP stream), so the whole enum is boxed by [`SealedConnection::accept_as_daemon`]
/// rather than paid for on every `Ok` path.
pub enum AcceptError<S> {
    /// The peer's first frame was text, so it speaks the pre-encryption
    /// protocol. The socket is returned unmodified.
    Plaintext(Box<tungstenite::WebSocket<S>>),
    Other(anyhow::Error),
}

/// What went wrong reading a handshake frame.
enum FrameError {
    /// A text frame where a binary hello was required.
    Plaintext,
    Other(anyhow::Error),
}

/// Read one binary frame, refusing anything else.
fn read_binary_frame<S: Read + Write>(
    socket: &mut tungstenite::WebSocket<S>,
    what: &str,
) -> Result<Vec<u8>, FrameError> {
    loop {
        let message = socket
            .read()
            .map_err(|error| FrameError::Other(error.into()))?;
        match message {
            Message::Binary(payload) => return Ok(payload.into()),
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .map_err(|error| FrameError::Other(error.into()))?;
            }
            Message::Text(_) => return Err(FrameError::Plaintext),
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => {
                return Err(FrameError::Other(anyhow::anyhow!(
                    "peer closed during the {what} exchange"
                )));
            }
        }
    }
}
