// A GUI build must not open a console window behind the app. Debug builds
// keep the console subsystem so `cargo run` and the dev watcher still see
// stdout and panics.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() {
    // One binary, several jobs. Everything but the GUI is a child process that
    // is launched with a mode word, so it is dispatched before anything touches
    // the window system.
    //
    // Each mode reads its options from the environment and its own arguments
    // after the first word, so `waku --daemon --bind 127.0.0.1:4312` behaves
    // exactly as `waku-daemon --bind 127.0.0.1:4312` does.
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--daemon") => run_daemon(),
        Some("--js-repl") => run_js_repl(),
        Some("--computer-use") => run_computer_use(),
        _ => waku::run(),
    }
}

/// Run the headless daemon, reporting a failure where the parent can see it.
///
/// A GUI-subsystem binary on Windows has no console, so a `println!` would go
/// nowhere. The supervisor reads the child's stderr to explain a daemon that
/// never became ready, and the inherited handle still works there, which is what
/// makes `eprintln!` the right channel even without a console.
///
/// Every mode below shares that channel: the parent that spawned it is the only
/// reader, and inherited handles survive the missing console.
fn run_daemon() {
    if let Err(error) = waku_daemon::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

/// Run the Cua Driver stdio host. The word after `--computer-use` selects the
/// mode, exactly as it does for the standalone `waku_computer_use` binary.
fn run_computer_use() {
    let mode = std::env::args().nth(2);
    if let Err(error) = waku_computer_use::run(mode.as_deref()) {
        eprintln!("Waku Computer Use: {error:#}");
        std::process::exit(1);
    }
}

/// Run the JavaScript REPL MCP server on stdin/stdout.
fn run_js_repl() {
    if let Err(error) = waku::js_repl::serve_stdio() {
        eprintln!("Waku JavaScript REPL: {error:#}");
        std::process::exit(1);
    }
}
