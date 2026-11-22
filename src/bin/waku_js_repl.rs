//! Standalone JavaScript REPL host for development and diagnostics.
//!
//! A packaged Waku reaches the same code through `waku.exe --js-repl`, so this
//! binary exists to rebuild the MCP server without the desktop app and to give
//! the protocol smoke tests a small target.

fn main() {
    if let Err(error) = waku::js_repl::serve_stdio() {
        eprintln!("Waku JavaScript REPL: {error:#}");
        std::process::exit(1);
    }
}
