//! Desktop ownership of the Waku daemon process.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

pub fn start_process() -> anyhow::Result<waku_client::DaemonSupervisor> {
    let address = std::env::var(waku_client::DAEMON_ADDRESS_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let token = std::env::var(waku_client::DAEMON_TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    match (address, token) {
        (Some(address), Some(token)) => {
            return waku_client::DaemonSupervisor::connect(address.trim(), token);
        }
        (Some(_), None) => bail!(
            "{} is set but {} is missing",
            waku_client::DAEMON_ADDRESS_ENV,
            waku_client::DAEMON_TOKEN_ENV
        ),
        (None, Some(_)) => bail!(
            "{} is set but {} is missing",
            waku_client::DAEMON_TOKEN_ENV,
            waku_client::DAEMON_ADDRESS_ENV
        ),
        (None, None) => {}
    }
    let app_settings = waku_client::persistence::load_or_create_app_settings()
        .context("could not load desktop daemon settings")?;
    // The desktop spawns its own executable with `--daemon`, so shipping one
    // binary is enough: the same file is the GUI and the daemon.
    let (executable, leading) = daemon_launch_target()?;
    waku_client::DaemonSupervisor::spawn_configured_with(
        &executable,
        cfg!(debug_assertions),
        app_settings.daemon_exposure,
        &leading,
    )
}

/// Resolve the local host name once during app construction. Settings can
/// then show a useful LAN URL without touching the OS from a render frame.
pub fn local_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buffer = [0_u8; 256];
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let length = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            let hostname = String::from_utf8_lossy(&buffer[..length]).trim().to_owned();
            if !hostname.is_empty() {
                return Some(hostname);
            }
        }
    }
    // `COMPUTERNAME` is the Windows equivalent and is always set; `HOSTNAME`
    // covers the shells that export it.
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|hostname| hostname.trim().to_owned())
        .find(|hostname| !hostname.is_empty())
}

/// The program to launch as the daemon, and the arguments that select that mode.
///
/// Normally this is the running executable plus `--daemon`: one file holds the
/// GUI and the daemon, so a package ships a single program.
///
/// A separate `waku-daemon` binary still wins when one is present, and
/// `WAKU_DAEMON_PATH` overrides both. That escape hatch is what lets a
/// development build run the daemon from Cargo's `debug` directory — the
/// watcher relinks that file and the supervisor swaps only the daemon — and it
/// is also how a host that wants the daemon under a service manager points at
/// it explicitly.
fn daemon_launch_target() -> anyhow::Result<(PathBuf, Vec<&'static str>)> {
    if let Some(path) = std::env::var_os("WAKU_DAEMON_PATH").filter(|path| !path.is_empty()) {
        // An explicit path names a daemon binary, which needs no mode flag. If
        // it *is* this executable, `--daemon` is still required.
        let path: PathBuf = path.into();
        let leading = if same_file(&path)? {
            vec!["--daemon"]
        } else {
            Vec::new()
        };
        return Ok((path, leading));
    }
    let executable = format!("waku-daemon{}", std::env::consts::EXE_SUFFIX);
    let current = std::env::current_exe().context("could not locate the Waku executable")?;

    // Development keeps the daemon beside Cargo's debug artifacts rather than
    // inside Waku Debug.app. The supervisor watches this file and swaps only
    // the daemon when the development watcher relinks it.
    #[cfg(debug_assertions)]
    if let Some(debug_directory) = current
        .ancestors()
        .find(|candidate| candidate.file_name().is_some_and(|name| name == "debug"))
    {
        let external = debug_directory.join(&executable);
        if external.is_file() {
            return Ok((external, Vec::new()));
        }
    }

    if let Some(directory) = current.parent() {
        let sibling = directory.join(&executable);
        if sibling.is_file() {
            return Ok((sibling, Vec::new()));
        }
    }
    // No separate daemon on disk: spawn this executable in daemon mode.
    Ok((current, vec!["--daemon"]))
}

/// Whether `path` names the executable currently running.
///
/// Compared by canonical path rather than by name: a daemon binary and the app
/// binary can share a directory, and only the app needs the mode flag.
fn same_file(path: &Path) -> anyhow::Result<bool> {
    let current = std::env::current_exe().context("could not locate the Waku executable")?;
    let current = std::fs::canonicalize(&current).unwrap_or(current);
    let other = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(current == other)
}
