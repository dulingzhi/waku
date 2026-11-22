//! Standalone Cua Driver host for development and diagnostics.
//!
//! A packaged Waku reaches the same code through `waku.exe --computer-use
//! <mode>`, so this binary exists to rebuild one helper without the desktop
//! app and to give the SDK smoke tests a small target.

fn main() {
    let mode = std::env::args().nth(1);
    if let Err(error) = waku_computer_use::run(mode.as_deref()) {
        eprintln!("Waku Computer Use: {error:#}");
        std::process::exit(1);
    }
}
