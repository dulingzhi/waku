//! Windows console code page for the processes the daemon starts.
//!
//! A native Windows tool writes its own output in the console code page, and
//! the console it talks to is the one its ancestors share. Waku's daemon has no
//! console — a GUI subsystem binary gets none, and the supervisor starts it
//! with `CREATE_NO_WINDOW` — so the code page falls back to the system ANSI
//! default. On a Chinese Windows install that is 936, and a tool such as
//! `ping.exe` then emits GBK: bytes that a UTF-8 reader cannot decode. The
//! provider CLI captures those bytes and decodes them as UTF-8, so the text
//! arrives in the transcript already carrying U+FFFD replacement characters.
//! The original bytes are gone by then and no later stage can recover them.
//!
//! The fix therefore has to live in the process that owns the descendants: give
//! the daemon a hidden console and set its code page to UTF-8, so every tool a
//! provider runs emits UTF-8 in the first place. Two details make it work, and
//! both were measured rather than assumed:
//!
//! * A child only adopts the code page by inheriting the console. `DETACHED_PROCESS`
//!   or `CREATE_NO_WINDOW` gives it a console of its own at the system default,
//!   which is the broken behaviour this exists to avoid.
//! * `AllocConsole` shows a window, so it is hidden immediately. The daemon is
//!   long-lived and console-less by design; an empty black window would be a
//!   visible regression even though the encoding was fixed.

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether this process owns a console whose code page is UTF-8.
///
/// [`detach_console`](crate::command_env) reads it to decide how to start a
/// child: a process without such a console keeps `CREATE_NO_WINDOW`, because a
/// console child of the desktop app would flash a window. A process with one
/// must let its children inherit it instead — that inheritance is the whole
/// mechanism, and `CREATE_NO_WINDOW` would silently undo it.
static UTF8_CONSOLE: AtomicBool = AtomicBool::new(false);

/// Whether children of this process will inherit a UTF-8 console.
pub fn console_is_utf8() -> bool {
    UTF8_CONSOLE.load(Ordering::Relaxed)
}

/// Set the process's console to a hidden UTF-8 one.
///
/// Returns whether a console was actually established. A caller cannot do
/// anything useful when this fails — a daemon with no console still works, it
/// just leaves native tools on the system code page — so the result is for
/// logging and tests rather than for error handling.
#[cfg(windows)]
pub fn use_utf8_console() -> bool {
    use windows_sys::Win32::System::Console::{
        AllocConsole, GetConsoleWindow, SetConsoleCP, SetConsoleOutputCP,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    const CP_UTF8: u32 = 65001;

    // A process that already has a console (a daemon started from a terminal)
    // must not allocate a second one — `AllocConsole` would fail and the
    // existing code page would stay as it is. Adopting it is the same
    // behaviour for descendants, which is all this needs.
    // SAFETY: these are plain Win32 calls with no pointers that outlive them.
    unsafe {
        let window = GetConsoleWindow();
        let allocated = window.is_null() && AllocConsole() != 0;
        if allocated {
            // The window exists only to carry the code page; it must not be
            // visible. A failure here leaves the console usable, so it is not
            // worth unwinding over.
            let window = GetConsoleWindow();
            if !window.is_null() {
                ShowWindow(window, SW_HIDE);
            }
        }
        // Both code pages: the output one is what makes tools emit UTF-8, and
        // the input one keeps a tool that reads console input from misreading
        // it in the opposite direction.
        let output_set = SetConsoleOutputCP(CP_UTF8) != 0;
        let input_set = SetConsoleCP(CP_UTF8) != 0;
        let ready = output_set && input_set;
        UTF8_CONSOLE.store(ready, Ordering::Relaxed);
        ready
    }
}

/// No-op on platforms whose child processes already speak UTF-8.
#[cfg(not(windows))]
pub fn use_utf8_console() -> bool {
    false
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Force the console onto a code page that is not UTF-8, so each test starts
    /// from the state the bug actually had instead of from whatever console the
    /// test runner happened to inherit. Without this, a machine whose console is
    /// already UTF-8 makes every assertion below pass for the wrong reason.
    fn start_from_a_non_utf8_console() {
        use windows_sys::Win32::System::Console::SetConsoleOutputCP;

        const CP_GBK: u32 = 936;
        // The test needs a console to change; allocating one is the same step
        // the production path takes.
        let _ = use_utf8_console();
        // SAFETY: a Win32 call with no pointers that outlive it.
        unsafe {
            SetConsoleOutputCP(CP_GBK);
        }
        UTF8_CONSOLE.store(false, Ordering::Relaxed);
    }

    /// The point of the console is the code page its descendants inherit, so
    /// the test asks a real child what code page it sees rather than trusting
    /// that the setter returned success.
    #[test]
    fn a_child_of_this_process_reports_the_utf8_code_page() {
        start_from_a_non_utf8_console();

        assert!(
            use_utf8_console(),
            "the test process should have established a UTF-8 console"
        );
        assert!(console_is_utf8(), "the flag follows the console");

        let output = std::process::Command::new("cmd")
            .args(["/c", "chcp"])
            .output()
            .expect("run chcp");

        let reported = String::from_utf8_lossy(&output.stdout);
        assert!(
            reported.contains("65001"),
            "a child must inherit the UTF-8 code page, but chcp reported {reported:?}"
        );
    }

    /// A native tool must emit UTF-8 once the console carries that code page.
    ///
    /// `echo` carries a Chinese literal on purpose: its bytes are encoded in the
    /// console code page, which is the exact thing under test. `ping` would not
    /// do, because it also switches its own message language, so a passing run
    /// could mean "the output became English" rather than "the bytes became
    /// UTF-8" — a distinction this bug is not willing to forgive.
    #[test]
    fn a_native_tool_emits_utf8_output() {
        start_from_a_non_utf8_console();

        assert!(use_utf8_console(), "the test process needs a UTF-8 console");

        let output = std::process::Command::new("cmd")
            .args(["/c", "echo \u{4e2d}\u{6587}\u{6d4b}\u{8bd5}"])
            .output()
            .expect("run echo");

        let text = std::str::from_utf8(&output.stdout);
        assert!(
            text.is_ok(),
            "a native tool's output must be valid UTF-8, got {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            text.expect("checked above").contains('\u{4e2d}'),
            "the Chinese text must survive the round trip"
        );
    }

    /// The gate the spawn path reads: while this process holds no UTF-8
    /// console, a child keeps `CREATE_NO_WINDOW` (no window flash), and once it
    /// does, the flag has to go or the child would get a console of its own at
    /// the system code page and the fix would silently do nothing.
    #[test]
    fn the_spawn_flag_follows_the_console() {
        start_from_a_non_utf8_console();
        assert!(!console_is_utf8(), "the gate starts closed");

        assert!(use_utf8_console());
        assert!(console_is_utf8(), "the gate opens with the console");
    }

    /// The production shape, end to end: the daemon is started with
    /// `CREATE_NO_WINDOW`, so it begins with no console at all, then allocates
    /// its own. Only then do its children inherit UTF-8. This re-executes the
    /// test binary into exactly that state, because the earlier tests all begin
    /// from a process that already had a console — the one difference the bug
    /// is sensitive to.
    #[test]
    fn a_console_allocated_after_a_consoleless_start_fixes_children() {
        const CHILD_MODE: &str = "WAKU_CONSOLE_CHILD_MODE";

        if std::env::var_os(CHILD_MODE).is_some() {
            // This half runs in the child: no console on entry, exactly as the
            // supervisor starts the daemon.
            let established = use_utf8_console();
            let output = std::process::Command::new("cmd")
                .args(["/c", "echo \u{4e2d}\u{6587}\u{6d4b}\u{8bd5}"])
                .output()
                .expect("run echo");
            let decoded = std::str::from_utf8(&output.stdout).is_ok();
            assert!(
                established,
                "a console-less process must be able to allocate a UTF-8 console"
            );
            assert!(
                decoded,
                "after allocating the console, a child must emit UTF-8, got {:?}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }

        use std::os::windows::process::CommandExt as _;

        let mut command = std::process::Command::new(
            std::env::current_exe().expect("resolve the test executable"),
        );
        command
            .args([
                "--exact",
                "windows_console::tests::a_console_allocated_after_a_consoleless_start_fixes_children",
                "--nocapture",
            ])
            .env(CHILD_MODE, "1")
            // The supervisor's own flag: the daemon starts with no console.
            .creation_flags(0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = command.output().expect("run the console-less child");
        assert!(
            output.status.success(),
            "the console-less child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
