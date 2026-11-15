//! Windows daemon terminals, hosted on ConPTY.
//!
//! Same contract as the unix backend: the daemon owns the shell, its cwd and
//! the pseudoconsole, and streams raw bytes to whichever client is attached.
//!
//! The two backends differ in how they learn about three things, and each
//! difference is load-bearing:
//!
//! - **Reading.** Unix hands out a `File` that reads and blocks. Windows'
//!   `Pty` exposes `EventedReadWrite::reader()`, an `UnblockedReader` whose
//!   `Read` impl returns `Ok(0)` when no bytes are ready — it never signals
//!   EOF that way. So a read of zero here means "nothing yet", and exit is
//!   detected only through `next_child_event`.
//! - **Exiting.** Unix reads `EIO` off the pty. On Windows the
//!   `ChildExitWatcher` posts a `ChildEvent::Exited`, which is the single
//!   source of truth for "the shell is gone".
//! - **Termination.** Unix sends `SIGHUP` and escalates to `SIGKILL`.
//!   ConPTY has no such signal, and closing the pseudoconsole blocks until
//!   `conout` is drained. See [`DaemonTerminal::drop`].

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use alacritty_terminal::event::{OnResize as _, WindowSize};
use alacritty_terminal::tty::{self, EventedPty as _, EventedReadWrite as _, Shell};
use anyhow::{Context as _, bail};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::json;

use crate::{EventSink, WireDriverEvent};

const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;
const MIN_COLUMNS: u16 = 2;
const MIN_ROWS: u16 = 1;
/// How long the output reader sleeps when the pipe has nothing buffered.
/// `UnblockedReader` reports emptiness as a zero-length read rather than
/// blocking, so the loop must yield on its own.
const READ_POLL_INTERVAL: Duration = Duration::from_millis(4);

pub struct DaemonTerminal {
    pty: Arc<Mutex<tty::Pty>>,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl DaemonTerminal {
    pub fn open(
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
        events: EventSink,
    ) -> anyhow::Result<Self> {
        let shell = default_shell();
        Self::open_with_shell(cwd, cols, rows, events, shell)
    }

    fn open_with_shell(
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
        events: EventSink,
        shell: Shell,
    ) -> anyhow::Result<Self> {
        if !cwd.is_dir() {
            bail!(
                "terminal working directory does not exist: {}",
                cwd.display()
            );
        }

        let mut options = tty::Options {
            shell: Some(shell),
            working_directory: Some(cwd.to_owned()),
            drain_on_exit: false,
            ..Default::default()
        };
        for (name, value) in crate::command_env::shell_environment() {
            options.env.insert(
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }
        options.env.insert("TERM".into(), "xterm-256color".into());
        options.env.insert("COLORTERM".into(), "truecolor".into());

        let size = window_size(cols, rows);
        let pty = tty::new(&options, size, 0)
            .with_context(|| format!("spawn terminal in {}", cwd.display()))?;
        let pty = Arc::new(Mutex::new(pty));
        let stopped = Arc::new(AtomicBool::new(false));
        let reader_pty = pty.clone();
        let reader_stopped = stopped.clone();
        let reader = std::thread::Builder::new()
            .name("waku-daemon-terminal-output".into())
            .spawn(move || read_output(reader_pty, reader_stopped, events))
            .context("start terminal output thread")?;

        Ok(Self {
            pty,
            stopped,
            reader: Some(reader),
        })
    }

    pub fn write(&self, data: Vec<u8>) -> anyhow::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut pty = self.pty.lock();
        pty.writer()
            .write_all(&data)
            .context("write terminal input")?;
        pty.writer().flush().context("flush terminal input")
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        self.pty.lock().on_resize(window_size(cols, rows));
    }
}

/// Pump the pseudoconsole's output into the driver event stream.
///
/// A zero-length read means the pipe has nothing buffered yet, not EOF: this
/// exits only when the child process reports it is gone, or when the terminal
/// is being torn down. Holding `pty` while reading is deliberate — the reader
/// *is* `pty.reader()`, so the lock is the borrow, and a write from another
/// thread waits for one chunk's worth of copying rather than interleaving
/// with the read half.
fn read_output(pty: Arc<Mutex<tty::Pty>>, stopped: Arc<AtomicBool>, events: EventSink) {
    let mut buffer = [0_u8; 32 * 1024];
    while !stopped.load(Ordering::Acquire) {
        let mut pty = pty.lock();
        let read = pty.reader().read(&mut buffer);
        let exited = pty.next_child_event().is_some();
        drop(pty);

        // A zero-length read means "nothing buffered", not EOF, so it is not
        // an exit condition and the loop yields below instead.
        let idle = match read {
            Ok(0) => true,
            Ok(read) => {
                let data = base64::engine::general_purpose::STANDARD.encode(&buffer[..read]);
                let _ = events.send_ephemeral(WireDriverEvent::new(
                    "terminalOutput",
                    json!({ "data": data }),
                ));
                false
            }
            // Errors are fatal for this stream rather than something to retry.
            Err(error) => {
                let _ = events.send_ephemeral(WireDriverEvent::new(
                    "terminalError",
                    serde_json::Value::String(error.to_string()),
                ));
                return;
            }
        };

        if exited {
            let _ = events.send_ephemeral(WireDriverEvent::new(
                "terminalExited",
                serde_json::Value::Null,
            ));
            return;
        }
        if idle {
            std::thread::sleep(READ_POLL_INTERVAL);
        }
    }
}

impl Drop for DaemonTerminal {
    /// Tear the terminal down without deadlocking the pseudoconsole.
    ///
    /// ConPTY's `ClosePseudoConsole` blocks until `conout` is drained, so the
    /// output pipe must stay readable until the child is gone — closing it
    /// first hangs the close. What this does *not* do is kill the shell: the
    /// daemon has no handle-level way to signal a console process group the
    /// way `SIGHUP` does, and guessing at a `TerminateProcess` risks killing a
    /// process that is not ours. The shell therefore outlives the terminal
    /// entry and ends with the daemon, which is the conservative choice: no
    /// terminal is left reachable, and nothing is killed by mistake.
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// The user's configured shell, matching the platform order the unix backend
/// resolves through `command_env`.
fn default_shell() -> Shell {
    let shell = crate::command_env::default_terminal_shell();
    let args = crate::command_env::default_terminal_shell_args(&shell);
    Shell::new(shell.to_string_lossy().into_owned(), args)
}

fn window_size(cols: u16, rows: u16) -> WindowSize {
    WindowSize {
        num_lines: rows.max(MIN_ROWS),
        num_cols: cols.max(MIN_COLUMNS),
        cell_width: CELL_WIDTH,
        cell_height: CELL_HEIGHT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_sized_terminal_is_clamped_to_a_usable_grid() {
        let size = window_size(0, 0);
        assert_eq!(size.num_cols, MIN_COLUMNS);
        assert_eq!(size.num_lines, MIN_ROWS);
    }

    #[test]
    fn a_sized_terminal_is_passed_through() {
        let size = window_size(120, 40);
        assert_eq!(size.num_cols, 120);
        assert_eq!(size.num_lines, 40);
        assert_eq!(size.cell_width, CELL_WIDTH);
        assert_eq!(size.cell_height, CELL_HEIGHT);
    }

    /// The shell the daemon picks must be an executable that exists, since
    /// ConPTY's `CreateProcessW` fails outright otherwise.
    #[test]
    fn the_default_shell_resolves_to_a_real_program() {
        let shell = crate::command_env::default_terminal_shell();
        // A bare name such as `cmd.exe` is resolved through PATHEXT by
        // `CreateProcessW`, so only an absolute path can be checked here.
        if shell.is_absolute() {
            assert!(shell.is_file(), "unresolvable shell: {}", shell.display());
        } else {
            assert!(!shell.as_os_str().is_empty());
        }
    }
}
