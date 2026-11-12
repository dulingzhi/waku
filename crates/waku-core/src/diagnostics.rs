//! Opt-in file diagnostics.
//!
//! Waku has no logging framework: diagnostics are `eprintln!`, and a release
//! build discards them. `waku.exe` is compiled for the Windows GUI subsystem
//! (see `src/main.rs`), so it has no console, and `waku-daemon.exe` inherits
//! that dead stderr handle through `Stdio::inherit()`. Piping a GUI binary from
//! a shell captures nothing, which leaves a disconnect with no evidence.
//!
//! Setting `WAKU_LOG_FILE` to a path mirrors those lines into that file, so a
//! packaged build can be diagnosed. Unset means no file is opened and the
//! behavior is exactly as before.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Keep the file from growing without bound during a long session. Once past
/// this, the file is truncated and restarted with a marker.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> Option<&'static PathBuf> {
    LOG_PATH
        .get_or_init(|| {
            let path = std::env::var_os("WAKU_LOG_FILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)?;
            if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(parent);
            }
            Some(path)
        })
        .as_ref()
}

/// Record a diagnostic line. Always mirrors to stderr, which is what a debug
/// build and the dev watcher already rely on, and additionally appends to
/// `WAKU_LOG_FILE` when one is configured.
pub fn log(message: impl AsRef<str>) {
    let message = message.as_ref();
    eprintln!("{message}");
    let Some(path) = log_path() else {
        return;
    };
    // A poisoned lock would only mean another thread panicked mid-write; the
    // log is diagnostic, so proceed rather than propagate that.
    let _guard = LOG_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let _ = std::fs::write(path, b"--- Waku daemon log truncated ---\n");
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{message}");
}
