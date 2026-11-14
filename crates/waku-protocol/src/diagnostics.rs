//! Always-on diagnostics written beside the app executable.
//!
//! Waku has no logging framework, so diagnostics are `eprintln!` — and a
//! release build discards them. `waku.exe` is compiled for the Windows GUI
//! subsystem, so it has no console, and `waku-daemon` inherits that dead stderr
//! handle through `Stdio::inherit`. A disconnect therefore left no evidence
//! anywhere, which is why a dropped connection could only be guessed at.
//!
//! Every line is mirrored into `waku.log` next to the app executable, which is
//! where a packaged build keeps everything else. The daemon finds the app
//! through [`APP_EXECUTABLE_ENV`], which the supervisor sets; the app uses its
//! own image path.
//!
//! The location degrades rather than failing: a macOS `.app` bundle is skipped
//! because writing inside it breaks the signature seal, an unwritable directory
//! falls back to the per-user data directory, and finally to the temp
//! directory. `WAKU_LOG_FILE` overrides all of it.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::APP_EXECUTABLE_ENV;

const LOG_FILE_NAME: &str = "waku.log";
/// Cap the file so a long-running session cannot fill the disk. Past this the
/// file is restarted with a marker.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Record a diagnostic line. Mirrors to stderr, which debug builds and the dev
/// watcher already rely on, and appends to the log file.
pub fn log(message: impl AsRef<str>) {
    let message = message.as_ref();
    eprintln!("{message}");
    let Some(path) = log_path() else {
        return;
    };
    // A poisoned lock only means another thread panicked mid-write; the log is
    // diagnostic, so proceed rather than propagate that.
    let _guard = LOG_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let _ = std::fs::write(path, b"--- Waku log restarted ---\n");
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    // The app and its daemon share one file, so name the writer.
    let _ = writeln!(file, "[{}] {message}", std::process::id());
}

/// The resolved log path, or `None` when nothing is writable. Resolved once, so
/// the writability probe does not repeat per line.
fn log_path() -> Option<&'static PathBuf> {
    LOG_PATH.get_or_init(resolve_path).as_ref()
}

fn resolve_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("WAKU_LOG_FILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    // Next to the app, so a packaged build keeps its log beside its binaries.
    if let Some(directory) = app_directory().filter(|directory| is_writable(directory)) {
        return Some(directory.join(LOG_FILE_NAME));
    }
    // A per-user directory always belongs to the user, so it is the reliable
    // fallback when the app lives somewhere read-only.
    if let Some(data) = dirs::data_local_dir() {
        let directory = data.join("waku");
        if std::fs::create_dir_all(&directory).is_ok() {
            return Some(directory.join(LOG_FILE_NAME));
        }
    }
    Some(std::env::temp_dir().join(LOG_FILE_NAME))
}

/// The directory holding the running app's executable. The daemon learns it
/// from [`APP_EXECUTABLE_ENV`]; the app itself uses its own image path.
fn app_directory() -> Option<PathBuf> {
    let executable = std::env::var_os(APP_EXECUTABLE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())?;
    let directory = executable.parent()?.to_path_buf();
    // Writing inside a bundle breaks its signature seal, so treat it as
    // unwritable even though the owner could technically append there.
    if is_inside_app_bundle(&directory) {
        return None;
    }
    Some(directory)
}

fn is_inside_app_bundle(directory: &Path) -> bool {
    let mut components = directory.components().rev();
    // `Foo.app/Contents/MacOS`
    matches!(
        (
            components.next().map(|part| part.as_os_str().to_string_lossy().into_owned()),
            components.next().map(|part| part.as_os_str().to_string_lossy().into_owned()),
            components.next().map(|part| part.as_os_str().to_string_lossy().into_owned()),
        ),
        (Some(bin), Some(contents), Some(bundle))
            if bin == "MacOS" && contents == "Contents" && bundle.ends_with(".app")
    )
}

fn is_writable(directory: &Path) -> bool {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(LOG_FILE_NAME))
        .is_ok()
}
