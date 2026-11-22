use std::io::Write as _;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use waku_protocol::{DAEMON_TOKEN_ENV, DaemonReady, PROTOCOL_VERSION};

/// Run the daemon until it is stopped.
///
/// This is what `waku --daemon` and the standalone `waku-daemon` binary both
/// call: one implementation, so the two cannot drift apart.
pub fn run() -> anyhow::Result<()> {
    // `--daemon` is the mode selector the desktop passes when it spawns its own
    // executable; it says nothing about how to run, so it is stripped here and
    // the rest of the options parse identically either way. Without this the
    // standalone binary and `waku --daemon` would not accept the same command
    // line, which is the whole point of sharing one body.
    let arguments = Arguments::parse(
        std::env::args()
            .skip(1)
            .filter(|argument| argument != "--daemon"),
    )?;
    let token =
        std::env::var(DAEMON_TOKEN_ENV).context("Waku daemon authentication token is missing")?;
    // The bearer capability belongs only to this server process. Remove it
    // before any provider or workspace subprocess can inherit the daemon's
    // environment.
    unsafe { std::env::remove_var(DAEMON_TOKEN_ENV) };
    let listener = TcpListener::bind(&arguments.bind)
        .with_context(|| format!("could not bind Waku daemon to {}", arguments.bind))?;
    let address = listener.local_addr()?;
    ensure_bind_allowed(address, arguments.allow_non_loopback)?;

    // The identity is loaded before readiness is announced, and a failure here
    // aborts startup. A daemon that ran on a broken identity would accept
    // connections it can never prove itself on, which shows up as a client that
    // cannot connect with no reason given. Failing now puts the real error in
    // the log of the process that was actually misconfigured.
    let identity_path = waku_core::daemon_identity::default_path();
    let identity = waku_core::daemon_identity::DaemonIdentity::load_or_generate(&identity_path)
        .with_context(|| {
            format!(
                "could not load the daemon identity at {}",
                identity_path.display()
            )
        })?;

    // Loaded here, before readiness, because the relay decision rides in the
    // readiness line. Loaded once: the store moves into the backend later, so a
    // second open would mean reading the same document twice and leaving the
    // two copies to disagree.
    let task_path = waku_core::persistence::StateStore::default_path();
    let settings = waku_core::DaemonSettingsStore::open_with_legacy(
        waku_core::DaemonSettings::default_path(),
        [task_path.with_file_name("settings.json")],
    )
    .context("could not load daemon settings")?;

    // The relay decision is made before readiness is announced because the
    // pairing code travels *in* the readiness line. The readiness reader stops
    // after one line, so a code printed separately would be read by nobody.
    let configured = settings.get();
    let relay_address = configured.relay_url.clone();
    let relay_keep_listening = configured.relay_keep_listening; // Generated here, not configured. A user copies a relay address from an
    // operator; making them also invent a session id would mean one they can
    // mistype with no way to tell a typo from a relay outage. It changes on
    // every start, which is what a pairing code is for.
    let session_id = relay_address
        .as_ref()
        .map(|_| waku_core::relay::new_session_id());
    let pairing = relay_address.as_ref().map(|relay| {
        let code = waku_protocol::pairing::PairingCode {
            relay: relay.clone(),
            session_id: session_id.clone().expect("generated alongside the relay"),
            fingerprint: identity.fingerprint(),
        };
        waku_protocol::DaemonPairing {
            code: code.render(),
            relay: relay.clone(),
            session_id: session_id.clone().expect("generated alongside the relay"),
        }
    });

    // A configured address that already carries a scheme or a path produces a
    // pairing code nobody can parse, and the daemon would start looking healthy
    // while being unusable. Failing here puts the error in the log of the
    // process that was misconfigured, instead of in a client that cannot tell a
    // bad code from a dead relay.
    if let Some(relay) = relay_address.as_deref()
        && (relay.contains("://") || relay.contains('/'))
    {
        return Err(anyhow::anyhow!(
            "relay_url must be host:port with no scheme and no path, but is {relay:?}"
        ));
    }

    let ready = DaemonReady {
        address: address.to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        x25519_public: identity.x25519_public_base64(),
        ed25519_public: identity.ed25519_public_base64(),
        pairing,
    };
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    waku_protocol::diagnostics::log(format!(
        "waku-daemon started pid={} address={address} protocol={PROTOCOL_VERSION}",
        std::process::id()
    ));
    // Only the fingerprint is logged: it is what a user compares against a
    // pairing code, and it is short enough to scan. Neither public key in full
    // nor any secret belongs in a log file that survives the process.
    waku_protocol::diagnostics::log(format!(
        "waku-daemon identity ready fingerprint={}",
        identity.fingerprint()
    ));
    if let Some(parent_pid) = arguments.parent_pid {
        let monitor_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("waku-daemon-parent".into())
            .spawn(move || {
                while !monitor_shutdown.load(Ordering::Acquire) {
                    if !process_is_alive(parent_pid) {
                        // This is the one shutdown Waku performs on its own. If
                        // the parent probe is wrong, a live app silently loses
                        // its daemon, so name the decision in the log.
                        waku_protocol::diagnostics::log(format!(
                            "waku-daemon shutting down: parent pid {parent_pid} is not alive"
                        ));
                        monitor_shutdown.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })?;
    }

    let task_store = waku_core::persistence::StateStore::daemon(task_path);

    // The identity is the one loaded above, before readiness was announced.
    // Loading it a second time here used to be a separate call, which meant two
    // `load_or_generate` calls racing on a first run: both could find the file
    // missing and both could try to create it, and the `DaemonReady` line could
    // publish keys from a different identity than the one the server ran on.
    let identity = Arc::new(identity);

    // One `DaemonServer` serves both modes, so a client that reaches the daemon
    // through the relay sees the same sessions, subscriptions and replay cursors
    // as one that connects directly. Two servers would mean two hubs.
    let server = Arc::new(waku_core::DaemonServer::new(
        token.clone(),
        identity.clone(),
        Arc::new(waku_core::daemon::WakuBackend::new(settings, task_store)?),
        waku_core::ServerOptions {
            allowed_origins: arguments.allowed_origins.into_iter().collect(),
            allow_shutdown: arguments.parent_pid.is_some(),
        },
    ));

    let Some(relay_address) = relay_address else {
        // No relay configured: listen and nothing else, which is the default
        // and the mode the desktop app expects.
        return server.serve(listener, shutdown);
    };
    let session_id = session_id.expect("generated whenever a relay is configured");

    // A relay is configured. Dial out, and keep listening only if asked: a
    // relayed daemon usually sits behind the very NAT that made the relay
    // necessary, so an extra listener is surface without a user.
    if relay_keep_listening {
        let for_listener = server.clone();
        let for_listener_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("waku-daemon-listener".into())
            .spawn(move || {
                if let Err(error) = for_listener.serve(listener, for_listener_shutdown) {
                    waku_protocol::diagnostics::log(format!(
                        "waku-daemon listener stopped: {error:#}"
                    ));
                }
            })
            .context("could not start the Waku daemon listener")?;
    } else {
        drop(listener);
    }

    waku_core::relay::run_relay(relay_address, session_id, server, shutdown)
}

fn ensure_bind_allowed(address: SocketAddr, allow_non_loopback: bool) -> anyhow::Result<()> {
    if address.ip().is_loopback() || allow_non_loopback {
        return Ok(());
    }
    bail!(
        "refusing non-loopback daemon bind {address}; pass --allow-non-loopback only after configuring authentication and exact browser origins"
    )
}

struct Arguments {
    bind: String,
    parent_pid: Option<u32>,
    allowed_origins: Vec<String>,
    allow_non_loopback: bool,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut bind = "127.0.0.1:0".to_owned();
        let mut parent_pid = None;
        let mut allowed_origins = Vec::new();
        let mut allow_non_loopback = false;
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--bind" => {
                    bind = arguments
                        .next()
                        .ok_or_else(|| anyhow!("--bind requires an address"))?;
                }
                "--parent-pid" => {
                    parent_pid = Some(
                        arguments
                            .next()
                            .ok_or_else(|| anyhow!("--parent-pid requires a process id"))?
                            .parse()
                            .context("--parent-pid is not a valid process id")?,
                    );
                }
                "--allow-origin" => {
                    let origin = arguments
                        .next()
                        .filter(|origin| !origin.trim().is_empty())
                        .ok_or_else(|| anyhow!("--allow-origin requires an origin"))?;
                    allowed_origins.push(origin);
                }
                "--allow-non-loopback" => {
                    allow_non_loopback = true;
                }
                "--help" | "-h" => {
                    // The program name comes from how this was started, not from
                    // a compile-time constant: the same body runs as
                    // `waku-daemon` and as `waku --daemon`, and the usage line
                    // has to name the one the user actually typed.
                    let program = std::env::args()
                        .next()
                        .and_then(|path| {
                            std::path::Path::new(&path)
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| "waku-daemon".to_owned());
                    println!(
                        "usage: {program} [--bind ADDRESS] [--allow-non-loopback] [--parent-pid PID] [--allow-origin ORIGIN]..."
                    );
                    std::process::exit(0);
                }
                unknown => bail!("unknown argument {unknown:?}"),
            }
        }
        Ok(Self {
            bind,
            parent_pid,
            allowed_origins,
            allow_non_loopback,
        })
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows reuses process ids, so the handle is opened for the narrowest
/// right that answers the question and closed immediately. A pid that no
/// longer exists fails to open; one that has exited but is still held open by
/// another handle reports an exit code instead of `STILL_ACTIVE`.
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Only a pid that does not exist justifies shutting down. Access
            // denied, handle exhaustion, or any other transient failure leaves
            // the parent's state unknown, and outliving a live app is the safer
            // error than abandoning it mid-session.
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut exit_code = 0_u32;
        let read = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        // A failed read leaves the parent's state unknown; outliving the app
        // is the safer error than shutting a live daemon down.
        read == 0 || exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests;
