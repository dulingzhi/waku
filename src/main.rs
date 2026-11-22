// A GUI build must not open a console window behind the app. Debug builds
// keep the console subsystem so `cargo run` and the dev watcher still see
// stdout and panics.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() {
    // One binary, several jobs. The daemon is the only mode that is not the GUI:
    // it is spawned by the desktop as a child process and may also be started by
    // hand on a headless host, so it is dispatched before anything touches the
    // window system.
    //
    // The daemon reads its options from the environment and its own arguments
    // after the first word, so `waku --daemon --bind 127.0.0.1:4312` behaves
    // exactly as `waku-daemon --bind 127.0.0.1:4312` does.
    if std::env::args().nth(1).as_deref() == Some("--daemon") {
        run_daemon();
        return;
    }
    waku::run();
}

/// Run the headless daemon, reporting a failure where the parent can see it.
///
/// A GUI-subsystem binary on Windows has no console, so a `println!` would go
/// nowhere. The supervisor reads the child's stderr to explain a daemon that
/// never became ready, and the inherited handle still works there, which is what
/// makes `eprintln!` the right channel even without a console.
fn run_daemon() {
    if let Err(error) = waku_daemon::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
