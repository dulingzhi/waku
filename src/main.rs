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
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match Mode::parse(&arguments) {
        Mode::Daemon => run_daemon(),
        Mode::JsRepl => run_js_repl(),
        Mode::AgentBridge => run_agent_bridge(),
        Mode::TodoBridge => run_todo_bridge(),
        // The word after `--computer-use` is the helper's own mode.
        Mode::ComputerUse(mode) => run_computer_use(mode),
        Mode::Gui => waku::run(),
    }
}

/// Which job this process was started for.
///
/// Dispatch is a value rather than a chain of `if`s so the rule that decides it
/// can be tested: a package ships one executable, and a helper whose mode word
/// was lost would otherwise start the window system instead of failing.
#[derive(Debug, Eq, PartialEq)]
enum Mode<'a> {
    Gui,
    Daemon,
    JsRepl,
    AgentBridge,
    TodoBridge,
    ComputerUse(Option<&'a str>),
}

impl<'a> Mode<'a> {
    fn parse(arguments: &'a [String]) -> Self {
        match arguments.first().map(String::as_str) {
            Some("--daemon") => Self::Daemon,
            Some("--js-repl") => Self::JsRepl,
            Some("--agent-bridge") => Self::AgentBridge,
            Some("--todo-bridge") => Self::TodoBridge,
            Some("--computer-use") => Self::ComputerUse(arguments.get(1).map(String::as_str)),
            _ => Self::Gui,
        }
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
fn run_computer_use(mode: Option<&str>) {
    if let Err(error) = waku_computer_use::run(mode) {
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

/// Run the agent bridge MCP server on stdin/stdout.
///
/// A provider launches this itself, so the only thing that says which daemon to
/// drive is the environment the daemon handed down. Running it by hand outside a
/// Waku session therefore fails loudly instead of driving the wrong daemon.
fn run_agent_bridge() {
    if let Err(error) = waku::agent_bridge::serve_stdio() {
        eprintln!("Waku agent bridge: {error:#}");
        std::process::exit(1);
    }
}

/// Run the task-list MCP server on stdin/stdout. It needs no daemon credential:
/// the model's tool call is read back from the provider transcript instead.
fn run_todo_bridge() {
    if let Err(error) = waku::todo_bridge::serve_stdio() {
        eprintln!("Waku todo bridge: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// A package ships one executable, so the mode word is the only thing that
    /// keeps a helper from starting the GUI instead. Losing it would make an
    /// agent's MCP server fail to launch while a window opened behind it, which
    /// is why each mode is pinned here rather than only exercised by hand.
    #[test]
    fn each_mode_word_selects_its_job_and_nothing_else_does() {
        assert_eq!(Mode::parse(&[]), Mode::Gui);
        assert_eq!(Mode::parse(&arguments(&["--daemon"])), Mode::Daemon);
        assert_eq!(
            Mode::parse(&arguments(&["--daemon", "--bind", "127.0.0.1:0"])),
            Mode::Daemon
        );
        assert_eq!(Mode::parse(&arguments(&["--js-repl"])), Mode::JsRepl);
        // The helper's own mode travels after the flag, and is optional.
        assert_eq!(
            Mode::parse(&arguments(&["--computer-use"])),
            Mode::ComputerUse(None)
        );
        assert_eq!(
            Mode::parse(&arguments(&["--computer-use", "mcp"])),
            Mode::ComputerUse(Some("mcp"))
        );
        assert_eq!(
            Mode::parse(&arguments(&["--computer-use", "list-tools"])),
            Mode::ComputerUse(Some("list-tools"))
        );
        // Anything else is the GUI, including an unknown flag.
        assert_eq!(Mode::parse(&arguments(&["--unknown"])), Mode::Gui);
    }
}
