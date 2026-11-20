//! Claude Code's MCP servers and plugins, read and mutated through its CLI.
//!
//! Waku never rewrites `.claude.json` itself. That file carries duplicate keys
//! that differ only in letter case (`d:/Projects/X` next to `D:/Projects/X`),
//! so a round-trip through any single parser loses entries — a whole-file
//! rewrite would quietly delete a user's configuration. Every mutation here
//! therefore shells out to `claude mcp …` / `claude plugin …` and lets Claude
//! Code own its own file format.
//!
//! Reads are layered the same way: the plugin catalog comes from the one
//! command with machine-readable output (`--json`), and MCP entries are parsed
//! from `claude mcp list`, whose text format is stable enough to read but which
//! also performs network health checks — so listing and probing are separate
//! operations, and only the second one pays for a connection attempt.

use std::path::{Path, PathBuf};

use waku_protocol::claude::{
    AvailablePlugin, AvailablePluginCatalog, ClaudeActionResult, MarketplaceCatalog,
    MarketplaceEntry, MarketplaceScope, McpCatalog, McpHealth, McpScope, McpServerEntry,
    McpTransport, PluginCatalog, PluginComponent, PluginConfigCatalog, PluginConfigOption,
    PluginDetails, PluginEntry,
};

use crate::command_env;

fn claude_binary(override_spec: Option<&str>) -> Option<PathBuf> {
    if let Some(spec) = override_spec
        && let Some(path) = command_env::resolve_binary_override(spec)
    {
        return Some(path);
    }
    command_env::find_executable("claude")
}

fn run_claude(binary: &Path, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut command = command_env::plain_command(binary);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command_env::output(&mut command).map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if stderr.is_empty() {
            format!("claude exited with {}", output.status)
        } else {
            stderr
        };
        return Err(detail);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a `--json` verb and read its result line.
///
/// These verbs print a human line or two and then one JSON object as their
/// last line of stdout — even when they fail, which is why a failure still has
/// a `message` worth showing. A non-zero exit is not treated as an error here:
/// the CLI reports its own reason inside that object, and that reason is more
/// precise than the exit status.
fn run_claude_action(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<ClaudeActionResult, String> {
    let mut command = command_env::plain_command(binary);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command_env::output(&mut command).map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = stdout.lines().rev().find_map(parse_action_line) {
        return Ok(line);
    }
    // No JSON line: fall back to the raw diagnostics rather than inventing a
    // result the CLI never reported.
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let message = if stderr.is_empty() {
        stdout.trim().to_owned()
    } else {
        stderr
    };
    Err(if message.is_empty() {
        format!("claude exited with {}", output.status)
    } else {
        message
    })
}

/// Turn one `{"command":…,"outcome":…,"message":…}` line into a result.
///
/// Anything that is not a JSON object with an `outcome` is not a result line,
/// so a progress message that happens to start with `{` cannot be mistaken for
/// one.
fn parse_action_line(line: &str) -> Option<ClaudeActionResult> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let outcome = value.get("outcome")?.as_str()?;
    Some(ClaudeActionResult {
        ok: outcome == "ok",
        message: value
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_default()
            .to_owned(),
        failure_code: value
            .get("failureCode")
            .and_then(|code| code.as_str())
            .map(str::to_owned),
        summary: None,
    })
}

// ── MCP ────────────────────────────────────────────────────────────────────

/// Every configured MCP server, with its scope and connection state.
///
/// Two passes, because neither CLI view is sufficient alone:
///
/// * `claude mcp list` is the only command that enumerates everything, and it
///   health-checks as it goes — but it never says which scope an entry is in.
/// * `claude mcp get <name>` states the scope, and costs one process per name.
///
/// The list pass runs first so the set of names is known; the `get` pass then
/// resolves the scope each one is in. Both are needed before any remove, since
/// removing without a scope takes the entry from whichever scope happens to
/// have it.
pub fn list_mcp_servers(override_spec: Option<&str>, cwd: Option<&Path>) -> McpCatalog {
    let Some(binary) = claude_binary(override_spec) else {
        return McpCatalog {
            servers: Vec::new(),
            unavailable: Some(tr!("claude.cli_not_found")),
        };
    };

    let stdout = match run_claude(&binary, &["mcp", "list"], cwd) {
        Ok(stdout) => stdout,
        Err(error) => {
            return McpCatalog {
                servers: Vec::new(),
                unavailable: Some(error),
            };
        }
    };

    let mut servers = parse_mcp_list(&stdout);
    // The `get` pass resolves each entry's scope. It used to run one process at
    // a time, which measured ~9.9s for eight servers — the single biggest cost
    // on the page, larger than the health-checking `list` it follows. The
    // processes are independent, so they run concurrently; the same eight took
    // ~3.6s that way. Results are collected by index, not by completion order,
    // so a slow server cannot shuffle the list under the cursor.
    let scopes = describe_mcp_scopes(&binary, cwd, &servers);
    for (server, scope) in servers.iter_mut().zip(scopes) {
        if let Some(scope) = scope {
            server.scope = scope;
        }
    }
    McpCatalog {
        servers,
        unavailable: None,
    }
}

/// Resolve every server's scope concurrently, keeping the input order.
///
/// A bounded number of workers rather than one thread per server: these are
/// child processes, and a machine with dozens of configured servers should not
/// fork all of them at once.
fn describe_mcp_scopes(
    binary: &Path,
    cwd: Option<&Path>,
    servers: &[McpServerEntry],
) -> Vec<Option<McpScope>> {
    const WORKERS: usize = 8;
    let workers = WORKERS.min(servers.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(vec![None; servers.len()]);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(server) = servers.get(index) else {
                        break;
                    };
                    let Some(stdout) = run_claude(binary, &["mcp", "get", &server.name], cwd).ok()
                    else {
                        continue;
                    };
                    let (resolved, _, _) = parse_mcp_get(&stdout);
                    if let Ok(mut results) = results.lock() {
                        results[index] = Some(resolved);
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

/// One line per server: `name: target (transport) - status`.
///
/// Health is parsed out here rather than by a second command because `list`
/// has already paid for the connection attempt. A line whose leading token is
/// followed by `:` is a server; the `Checking …` preamble and blank lines are
/// not.
fn parse_mcp_list(stdout: &str) -> Vec<McpServerEntry> {
    let mut servers = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Checking") {
            continue;
        }
        let Some((name, rest)) = line.split_once(": ") else {
            continue;
        };
        // Reject prose that happens to contain a colon, e.g. the trailing
        // hint lines the CLI sometimes prints.
        if name.contains(' ') {
            continue;
        }
        let (health, detail) = parse_health(rest);
        let (target, transport, args) = parse_target_and_transport(rest);
        servers.push(McpServerEntry {
            name: name.to_owned(),
            // `list` does not say which scope an entry came from, so the scope
            // is resolved against the config files below.
            scope: McpScope::User,
            transport,
            target,
            args,
            pending_approval: rest.contains("Pending approval"),
            health,
            detail,
        });
    }
    servers
}

fn parse_health(rest: &str) -> (Option<McpHealth>, Option<String>) {
    if rest.contains("Pending approval") {
        return (None, None);
    }
    if rest.contains("Connected") && !rest.contains("Failed") {
        return (Some(McpHealth::Connected), None);
    }
    if let Some(index) = rest.find("Failed to connect") {
        let reason = rest[index..]
            .trim_start_matches("Failed to connect")
            .trim_start_matches([' ', '—', '-'])
            .trim()
            .to_owned();
        let detail = (!reason.is_empty()).then_some(reason);
        return (Some(McpHealth::Failed), detail);
    }
    (None, None)
}

/// Split `name: <target> (<transport>) - <status>` into its parts.
fn parse_target_and_transport(rest: &str) -> (String, McpTransport, Vec<String>) {
    // The status suffix starts at the last ` - ` the CLI prints before it.
    let body = match rest.rfind(" - ") {
        Some(index) => &rest[..index],
        None => rest,
    };
    let body = body.trim();

    let (transport, without_transport) = if let Some(stripped) = body.strip_suffix(" (HTTP)") {
        (McpTransport::Http, stripped)
    } else if let Some(stripped) = body.strip_suffix(" (SSE)") {
        (McpTransport::Sse, stripped)
    } else if let Some(stripped) = body.strip_suffix(" (WebSocket)") {
        (McpTransport::WebSocket, stripped)
    } else {
        (McpTransport::Stdio, body)
    };

    let without_transport = without_transport.trim();
    match transport {
        McpTransport::Stdio => {
            let mut parts = without_transport.split_whitespace();
            let command = parts.next().unwrap_or_default().to_owned();
            let args = parts.map(str::to_owned).collect::<Vec<_>>();
            (command, transport, args)
        }
        _ => (without_transport.to_owned(), transport, Vec::new()),
    }
}

/// Remove one server. The scope is passed through so a project-level entry is
/// not deleted from the user config by mistake.
pub fn remove_mcp_server(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    name: &str,
    scope: McpScope,
) -> Result<(), String> {
    // A plugin-owned server has no scope flag to pass, and no config file to
    // remove it from: the plugin would put it back. Refusing here keeps the
    // CLI from answering with a confusing "not found" for a server the page
    // just listed.
    let Some(flag) = scope.cli_flag() else {
        return Err(tr!("claude.plugin_server_not_removable"));
    };
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    run_claude(&binary, &["mcp", "remove", name, "--scope", flag], cwd).map(|_| ())
}

/// Re-read one server through `claude mcp get`, which — unlike `list` — states
/// the scope it came from.
///
/// This is what makes a remove safe: `remove` without `--scope` deletes "from
/// whichever scope has it", which is exactly the ambiguity that would let a
/// project entry vanish from the user config.
pub fn describe_mcp_server(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    name: &str,
) -> Option<(McpScope, Option<McpHealth>, Option<String>)> {
    let binary = claude_binary(override_spec)?;
    let stdout = run_claude(&binary, &["mcp", "get", name], cwd).ok()?;
    Some(parse_mcp_get(&stdout))
}

/// Parse `claude mcp get`'s block:
///
/// ```text
/// codegraph:
///   Scope: User config (available in all your projects)
///   Status: √ Connected
///   Type: stdio
///   Command: codegraph
///   Args: serve --mcp
/// ```
fn parse_mcp_get(stdout: &str) -> (McpScope, Option<McpHealth>, Option<String>) {
    let mut scope = McpScope::Local;
    let mut health = None;
    let mut detail = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Scope:") {
            let value = value.trim();
            // Plugin-contributed servers report "Dynamic config (from command
            // line)". They are matched first because the word "config" would
            // otherwise be read as a user-scope entry, and the page would then
            // offer to delete something no config file holds.
            scope = if value.starts_with("Dynamic") {
                McpScope::Plugin
            } else if value.starts_with("User") {
                McpScope::User
            } else if value.starts_with("Project") {
                McpScope::Project
            } else {
                McpScope::Local
            };
        } else if let Some(value) = line.strip_prefix("Status:") {
            let value = value.trim();
            if value.contains("Connected") && !value.contains("Failed") {
                health = Some(McpHealth::Connected);
            } else if value.contains("Failed") {
                health = Some(McpHealth::Failed);
                detail = Some(value.to_owned());
            } else if value.contains("Pending approval") {
                health = None;
            }
        }
    }
    (scope, health, detail)
}

// ── Plugins ────────────────────────────────────────────────────────────────

pub fn list_plugins(override_spec: Option<&str>, cwd: Option<&Path>) -> PluginCatalog {
    let Some(binary) = claude_binary(override_spec) else {
        return PluginCatalog {
            plugins: Vec::new(),
            unavailable: Some(tr!("claude.cli_not_found")),
        };
    };
    match run_claude(&binary, &["plugin", "list", "--json"], cwd) {
        Ok(stdout) => match parse_plugin_list(&stdout) {
            Ok(plugins) => PluginCatalog {
                plugins,
                unavailable: None,
            },
            Err(error) => PluginCatalog {
                plugins: Vec::new(),
                unavailable: Some(error),
            },
        },
        Err(error) => PluginCatalog {
            plugins: Vec::new(),
            unavailable: Some(error),
        },
    }
}

/// The CLI's `--json` shape. Unknown fields are ignored so a newer CLI that
/// adds keys does not break the page.
pub(crate) fn parse_plugin_list(stdout: &str) -> Result<Vec<PluginEntry>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|error| error.to_string())?;
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    Ok(items.iter().filter_map(parse_plugin_entry).collect())
}

fn parse_plugin_entry(value: &serde_json::Value) -> Option<PluginEntry> {
    let id = value.get("id")?.as_str()?.to_owned();
    let (name, marketplace) = match id.split_once('@') {
        Some((name, marketplace)) => (name.to_owned(), marketplace.to_owned()),
        None => (id.clone(), String::new()),
    };
    let mcp_servers = value
        .get("mcpServers")
        .and_then(|servers| servers.as_object())
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default();
    let components = value
        .get("components")
        .and_then(|components| components.as_array())
        .map(|components| {
            components
                .iter()
                .filter_map(|component| {
                    Some(PluginComponent {
                        kind: component.get("type")?.as_str()?.to_owned(),
                        name: component.get("name")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(PluginEntry {
        id,
        name,
        marketplace,
        version: value
            .get("version")
            .and_then(|version| version.as_str())
            .unwrap_or_default()
            .to_owned(),
        scope: match value.get("scope").and_then(|scope| scope.as_str()) {
            Some("project") => McpScope::Project,
            Some("local") => McpScope::Local,
            _ => McpScope::User,
        },
        enabled: value
            .get("enabled")
            .and_then(|enabled| enabled.as_bool())
            .unwrap_or(false),
        project_enabled: value
            .get("projectEnabled")
            .and_then(|enabled| enabled.as_bool())
            .unwrap_or(false),
        description: value
            .get("description")
            .and_then(|description| description.as_str())
            .unwrap_or_default()
            .to_owned(),
        mcp_servers,
        components,
        install_path: value
            .get("installPath")
            .and_then(|path| path.as_str())
            .map(str::to_owned),
    })
}

pub fn set_plugin_enabled(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    id: &str,
    enabled: bool,
) -> Result<(), String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    let verb = if enabled { "enable" } else { "disable" };
    run_claude(&binary, &["plugin", verb, id], cwd).map(|_| ())
}

pub fn uninstall_plugin(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    id: &str,
    scope: McpScope,
) -> Result<(), String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    // `plugin uninstall` defaults to user scope, so a project-scoped plugin
    // must name its scope or the uninstall targets the wrong install — and
    // reports success while the plugin is still there.
    let Some(flag) = scope.cli_flag() else {
        return Err(tr!("claude.plugin_server_not_removable"));
    };
    run_claude(&binary, &["plugin", "uninstall", id, "--scope", flag], cwd).map(|_| ())
}

// ── Marketplaces and installing ────────────────────────────────────────────

/// The marketplaces the user has added.
///
/// Cheap (~1s) and part of what the Plugins tab shows, because a plugin cannot
/// be installed from a marketplace that is not listed here.
pub fn list_marketplaces(override_spec: Option<&str>) -> MarketplaceCatalog {
    let Some(binary) = claude_binary(override_spec) else {
        return MarketplaceCatalog {
            marketplaces: Vec::new(),
            unavailable: Some(tr!("claude.cli_not_found")),
        };
    };
    match run_claude(&binary, &["plugin", "marketplace", "list", "--json"], None) {
        Ok(stdout) => match parse_marketplace_list(&stdout) {
            Ok(marketplaces) => MarketplaceCatalog {
                marketplaces,
                unavailable: None,
            },
            Err(error) => MarketplaceCatalog {
                marketplaces: Vec::new(),
                unavailable: Some(error),
            },
        },
        Err(error) => MarketplaceCatalog {
            marketplaces: Vec::new(),
            unavailable: Some(error),
        },
    }
}

fn parse_marketplace_list(stdout: &str) -> Result<Vec<MarketplaceEntry>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|error| error.to_string())?;
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(MarketplaceEntry {
                name: item.get("name")?.as_str()?.to_owned(),
                source: item
                    .get("source")
                    .and_then(|source| source.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                repo: item
                    .get("repo")
                    .and_then(|repo| repo.as_str())
                    .map(str::to_owned),
                install_location: item
                    .get("installLocation")
                    .and_then(|path| path.as_str())
                    .map(str::to_owned),
            })
        })
        .collect())
}

/// Everything the added marketplaces offer, installed or not.
///
/// This is the one expensive read in the whole page: it makes the CLI refresh
/// every marketplace before answering, measured on this machine at ~10s for
/// 3916 entries and 2.4 MB of JSON. It is therefore a command of its own that
/// the page runs only when asked, never as part of opening the tab.
pub fn list_available_plugins(
    override_spec: Option<&str>,
    installed: &[PluginEntry],
) -> AvailablePluginCatalog {
    let Some(binary) = claude_binary(override_spec) else {
        return AvailablePluginCatalog {
            plugins: Vec::new(),
            unavailable: Some(tr!("claude.cli_not_found")),
        };
    };
    match run_claude(&binary, &["plugin", "list", "--available", "--json"], None) {
        Ok(stdout) => match parse_available_plugins(&stdout, installed) {
            Ok(plugins) => AvailablePluginCatalog {
                plugins,
                unavailable: None,
            },
            Err(error) => AvailablePluginCatalog {
                plugins: Vec::new(),
                unavailable: Some(error),
            },
        },
        Err(error) => AvailablePluginCatalog {
            plugins: Vec::new(),
            unavailable: Some(error),
        },
    }
}

/// `--available` answers an object with `installed` and `available` arrays,
/// unlike plain `list`, which answers a bare array. Both shapes are accepted so
/// a CLI that drops the wrapper does not empty the page.
fn parse_available_plugins(
    stdout: &str,
    installed: &[PluginEntry],
) -> Result<Vec<AvailablePlugin>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|error| error.to_string())?;
    let items = match value.get("available").and_then(|list| list.as_array()) {
        Some(items) => items.clone(),
        None => match value.as_array() {
            Some(items) => items.clone(),
            None => return Ok(Vec::new()),
        },
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("pluginId").and_then(|id| id.as_str())?.to_owned();
            let (name, marketplace) = match id.split_once('@') {
                Some((name, marketplace)) => (name.to_owned(), marketplace.to_owned()),
                None => (id.clone(), String::new()),
            };
            Some(AvailablePlugin {
                installed: installed.iter().any(|plugin| plugin.id == id),
                id,
                name,
                marketplace,
                description: item
                    .get("description")
                    .and_then(|description| description.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                install_count: item.get("installCount").and_then(|count| count.as_u64()),
            })
        })
        .collect())
}

pub fn install_plugin(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    id: &str,
    scope: McpScope,
) -> Result<(), String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    // `plugin install` defaults to user scope, which would install a plugin
    // into every project when the page meant this one.
    let Some(flag) = scope.cli_flag() else {
        return Err(tr!("claude.plugin_server_not_removable"));
    };
    // `--json` keeps the result one line instead of a progress display. `-y`
    // is required because this runs without a TTY: without it the CLI refuses
    // a plugin whose marketplace declares an install command, which is the
    // safe default but has to be answered deliberately, not silently.
    run_claude(
        &binary,
        &["plugin", "install", id, "--scope", flag, "--json", "-y"],
        cwd,
    )
    .map(|_| ())
}

pub fn update_plugin(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    id: &str,
    scope: Option<McpScope>,
) -> Result<(), String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    let mut args = vec!["plugin", "update", id, "--json", "-y"];
    // `update` auto-detects its scope, unlike `install` and `uninstall`, so the
    // flag is only passed when the caller knows it. A plugin scope has no flag.
    if let Some(flag) = scope.and_then(McpScope::cli_flag) {
        args.push("--scope");
        args.push(flag);
    }
    run_claude(&binary, &args, cwd).map(|_| ())
}

// ── Marketplace writes ─────────────────────────────────────────────────────

/// Declare a marketplace from a repo, URL or path.
///
/// The CLI validates the source and says why it refused, so the source is
/// passed through untouched rather than pattern-matched here: `owner/repo`, an
/// https or git URL, and a local path are all valid and only the CLI knows
/// which forms it currently accepts.
pub fn add_marketplace(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    source: &str,
    scope: MarketplaceScope,
) -> Result<ClaudeActionResult, String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    run_claude_action(
        &binary,
        &[
            "plugin",
            "marketplace",
            "add",
            source,
            "--scope",
            scope.cli_flag(),
            "--json",
        ],
        cwd,
    )
}

/// Remove a marketplace declaration. `scope: None` removes it from every layer,
/// which is what the CLI does when `--scope` is omitted.
pub fn remove_marketplace(
    override_spec: Option<&str>,
    name: &str,
    scope: Option<MarketplaceScope>,
) -> Result<ClaudeActionResult, String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    let mut args = vec!["plugin", "marketplace", "remove", name, "--json"];
    if let Some(scope) = scope {
        args.push("--scope");
        args.push(scope.cli_flag());
    }
    run_claude_action(&binary, &args, None)
}

/// Refresh one marketplace, or all of them when no name is given.
///
/// Refreshing all of them prints no JSON line (`--json` only applies to a named
/// marketplace), so that form collects the text output instead of pretending to
/// have a structured result.
pub fn update_marketplace(
    override_spec: Option<&str>,
    name: Option<&str>,
) -> Result<ClaudeActionResult, String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    match name {
        Some(name) => run_claude_action(
            &binary,
            &["plugin", "marketplace", "update", name, "--json"],
            None,
        ),
        None => {
            let stdout = run_claude(&binary, &["plugin", "marketplace", "update"], None)?;
            Ok(ClaudeActionResult {
                ok: true,
                message: stdout.trim().to_owned(),
                failure_code: None,
                summary: None,
            })
        }
    }
}

// ── Plugin configuration ───────────────────────────────────────────────────

/// Read the options a plugin declares and which are set.
///
/// `configure --json` answers a schema per option plus `configured` and
/// `unconfigured` key lists. Both are needed: the schema says what can be set,
/// the lists say what already is.
pub fn plugin_config(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    id: &str,
) -> PluginConfigCatalog {
    let empty = |error: String| PluginConfigCatalog {
        plugin_id: id.to_owned(),
        options: Vec::new(),
        unavailable: Some(error),
        orphaned: Vec::new(),
    };
    let Some(binary) = claude_binary(override_spec) else {
        return empty(tr!("claude.cli_not_found"));
    };
    // `configure` resolves the plugin against the current directory, so a
    // project-scoped plugin is only reachable from its own project. Running
    // there is the only way to read its options.
    match run_claude(&binary, &["plugin", "configure", id, "--json"], cwd) {
        Ok(stdout) => match parse_plugin_config(id, &stdout) {
            Ok(catalog) => catalog,
            Err(error) => empty(error),
        },
        Err(error) => empty(error),
    }
}

fn parse_plugin_config(id: &str, stdout: &str) -> Result<PluginConfigCatalog, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|error| error.to_string())?;
    let configured: Vec<String> = value
        .get("configured")
        .and_then(|list| list.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let unconfigured: Vec<String> = value
        .get("unconfigured")
        .and_then(|list| list.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let mut options = Vec::new();
    if let Some(schema) = value.get("schema").and_then(|schema| schema.as_object()) {
        for (key, spec) in schema {
            options.push(parse_config_option(key, spec, &configured, &unconfigured));
        }
    }
    // A key the schema no longer declares but the CLI still holds a value for.
    // It is usually the reason a plugin behaves oddly, so it is reported
    // instead of being dropped on the floor.
    let known: Vec<&str> = options.iter().map(|option| option.key.as_str()).collect();
    let orphaned = configured
        .iter()
        .filter(|key| !known.contains(&key.as_str()))
        .cloned()
        .collect();

    Ok(PluginConfigCatalog {
        plugin_id: id.to_owned(),
        options,
        unavailable: None,
        orphaned,
    })
}

fn parse_config_option(
    key: &str,
    spec: &serde_json::Value,
    configured: &[String],
    unconfigured: &[String],
) -> PluginConfigOption {
    let schema_type = spec.get("type").and_then(|kind| kind.as_str());
    let choices: Vec<String> = spec
        .get("enum")
        .and_then(|choices| choices.as_array())
        .map(|choices| {
            choices
                .iter()
                .map(|choice| match choice {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    // The CLI reports which keys are set, not their values, and a secret is
    // never echoed back. So the saved value is carried as a set/unset marker
    // rather than invented: an option in `configured` shows as saved, and one
    // the schema still lists among `unconfigured` shows as unset.
    let is_set = configured.iter().any(|saved| saved == key)
        && !unconfigured.iter().any(|unsaved| unsaved == key);
    PluginConfigOption {
        key: key.to_owned(),
        title: spec
            .get("title")
            .and_then(|title| title.as_str())
            .unwrap_or(key)
            .to_owned(),
        description: spec
            .get("description")
            .and_then(|description| description.as_str())
            .unwrap_or_default()
            .to_owned(),
        kind: if !choices.is_empty() {
            "enum".to_owned()
        } else {
            schema_type.unwrap_or("string").to_owned()
        },
        required: spec
            .get("required")
            .and_then(|required| required.as_bool())
            .unwrap_or(false),
        choices,
        value: None,
        secret: spec
            .get("secret")
            .and_then(|secret| secret.as_bool())
            .unwrap_or(false),
        is_set,
    }
}

/// Save plugin options through `plugin configure --values-stdin`.
///
/// Values go over stdin, not the command line: an option can be a token, and an
/// argument is visible in the process list on this machine.
pub fn set_plugin_config(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    id: &str,
    values: &[(String, String)],
) -> Result<ClaudeActionResult, String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    let mut object = serde_json::Map::new();
    for (key, value) in values {
        object.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    let stdin = serde_json::to_string(&serde_json::Value::Object(object))
        .map_err(|error| error.to_string())?;
    let mut command = command_env::plain_command(&binary);
    command.args(["plugin", "configure", id, "--values-stdin", "--json"]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command_env::output_with_stdin(&mut command, stdin.as_bytes())
        .map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(result) = stdout.lines().rev().find_map(parse_action_line) {
        return Ok(result);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if stderr.is_empty() {
        stdout.trim().to_owned()
    } else {
        stderr
    })
}

/// Remove auto-installed plugin dependencies that are no longer needed.
pub fn prune_plugins(
    override_spec: Option<&str>,
    cwd: Option<&Path>,
    dry_run: bool,
    scope: MarketplaceScope,
) -> Result<ClaudeActionResult, String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    let mut args = vec!["plugin", "prune", "--scope", scope.cli_flag(), "-y"];
    if dry_run {
        args.push("--dry-run");
    }
    let stdout = run_claude(&binary, &args, cwd)?;
    Ok(ClaudeActionResult {
        ok: true,
        message: stdout.trim().to_owned(),
        failure_code: None,
        summary: None,
    })
}

///
/// `claude plugin details` would be the obvious source, but it resolves plugins
/// against the current directory: a project-scoped plugin installed for another
/// project answers "not found" from here, and this page lists every scope. The
/// manifest is a file both cases have, so it is read directly — a read only,
/// which is why it does not break the "never rewrite Claude's config" rule.
pub fn plugin_details(id: &str, install_path: Option<&Path>) -> PluginDetails {
    let mut details = PluginDetails {
        id: id.to_owned(),
        name: id.split_once('@').map_or(id, |(name, _)| name).to_owned(),
        version: String::new(),
        description: String::new(),
        author: None,
        homepage: None,
        mcp_servers: Vec::new(),
        components: Vec::new(),
        // Installed from a marketplace is exactly "has an id with @ and a
        // version to move forward from".
        can_update: id.contains('@'),
    };
    let Some(root) = install_path else {
        details.can_update = false;
        return details;
    };
    let manifest = root.join(".claude-plugin").join("plugin.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        // A missing manifest is not an error worth a page: the plugin is still
        // listed, and the summary carries whatever `plugin list` already knew.
        details.can_update = false;
        return details;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        details.can_update = false;
        return details;
    };
    if let Some(name) = value.get("name").and_then(|name| name.as_str()) {
        details.name = name.to_owned();
    }
    if let Some(version) = value.get("version").and_then(|version| version.as_str()) {
        details.version = version.to_owned();
    }
    if let Some(description) = value.get("description").and_then(|text| text.as_str()) {
        details.description = description.to_owned();
    }
    // `author` is either a string or an object with a `name`, depending on the
    // manifest version, so both shapes are read rather than assuming one.
    details.author = match value.get("author") {
        Some(serde_json::Value::String(name)) => Some(name.clone()),
        Some(author) => author
            .get("name")
            .and_then(|name| name.as_str())
            .map(str::to_owned),
        None => None,
    };
    details.homepage = value
        .get("homepage")
        .and_then(|homepage| homepage.as_str())
        .map(str::to_owned);
    if let Some(servers) = value
        .get("mcpServers")
        .and_then(|servers| servers.as_object())
    {
        details.mcp_servers = servers.keys().cloned().collect();
    }
    // Every other component kind the manifest declares, in the `kind: name`
    // form the page shows. Unknown kinds are kept so a newer manifest still
    // reports what it ships instead of looking empty.
    for kind in [
        "skills",
        "agents",
        "commands",
        "hooks",
        "lspServers",
        "outputStyles",
    ] {
        let Some(entries) = value.get(kind) else {
            continue;
        };
        match entries {
            // A list of names.
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(name) = item.as_str() {
                        details.components.push(PluginComponent {
                            kind: kind.to_owned(),
                            name: name.to_owned(),
                        });
                    }
                }
            }
            // An object keyed by name, which is how hooks and LSP servers are
            // declared.
            serde_json::Value::Object(map) => {
                for name in map.keys() {
                    details.components.push(PluginComponent {
                        kind: kind.to_owned(),
                        name: name.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    details
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_list_lines_split_into_name_target_and_transport() {
        let stdout = "\
Checking MCP server health…

codegraph: codegraph serve --mcp - √ Connected
dbg-mcp: http://127.0.0.1:13337/mcp (HTTP) - × Failed to connect — ECONNREFUSED
";
        let servers = parse_mcp_list(stdout);
        assert_eq!(servers.len(), 2);

        assert_eq!(servers[0].name, "codegraph");
        assert_eq!(servers[0].transport, McpTransport::Stdio);
        assert_eq!(servers[0].target, "codegraph");
        assert_eq!(servers[0].args, ["serve", "--mcp"]);
        assert_eq!(servers[0].health, Some(McpHealth::Connected));

        assert_eq!(servers[1].name, "dbg-mcp");
        assert_eq!(servers[1].transport, McpTransport::Http);
        assert_eq!(servers[1].target, "http://127.0.0.1:13337/mcp");
        assert_eq!(servers[1].health, Some(McpHealth::Failed));
        assert!(
            servers[1]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("ECONNREFUSED"))
        );
    }

    #[test]
    fn the_health_preamble_is_not_a_server() {
        let servers = parse_mcp_list("Checking MCP server health…\n\n");
        assert!(servers.is_empty());
    }

    #[test]
    fn a_pending_approval_server_reports_no_health_rather_than_failed() {
        let servers =
            parse_mcp_list("remote: https://example.test/mcp (HTTP) - ⏸ Pending approval\n");
        assert_eq!(servers.len(), 1);
        assert!(servers[0].pending_approval);
        // Not connected is not the same as broken.
        assert_eq!(servers[0].health, None);

        // The same line with the marker *and* a failure word must still read
        // as pending: a server nobody approved was never attempted, so it must
        // not be reported as having failed to connect.
        let servers =
            parse_mcp_list("remote: https://example.test/mcp (HTTP) - ⏸ Pending approval\n");
        assert_ne!(servers[0].health, Some(McpHealth::Failed));
    }

    #[test]
    fn plugin_json_is_parsed_into_entries() {
        let stdout = r#"[
          {
            "id": "ida-pro-mcp@mrexodia",
            "version": "0.1.0",
            "scope": "user",
            "enabled": true,
            "projectEnabled": false,
            "installPath": "C:\\plugins\\ida",
            "mcpServers": {"idalib": {"command": "uv"}}
          }
        ]"#;
        let plugins = parse_plugin_list(stdout).unwrap();
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        assert_eq!(plugin.name, "ida-pro-mcp");
        assert_eq!(plugin.marketplace, "mrexodia");
        assert_eq!(plugin.scope, McpScope::User);
        assert!(plugin.enabled);
        assert_eq!(plugin.mcp_servers, ["idalib"]);
    }

    #[test]
    fn an_unparsable_plugin_list_is_an_error_not_an_empty_catalog() {
        // Reporting "no plugins installed" for a broken CLI would hide the
        // problem behind a plausible-looking empty page.
        assert!(parse_plugin_list("not json").is_err());
    }

    #[test]
    fn a_plugin_id_without_a_marketplace_still_parses() {
        let plugins = parse_plugin_list(r#"[{"id": "solo", "enabled": true}]"#).unwrap();
        assert_eq!(plugins[0].name, "solo");
        assert_eq!(plugins[0].marketplace, "");
    }

    #[test]
    fn mcp_get_resolves_the_scope_that_list_omits() {
        let stdout = "\
codegraph:
  Scope: User config (available in all your projects)
  Status: √ Connected
  Type: stdio
  Command: codegraph
  Args: serve --mcp
";
        let (scope, health, detail) = parse_mcp_get(stdout);
        assert_eq!(scope, McpScope::User);
        assert_eq!(health, Some(McpHealth::Connected));
        assert!(detail.is_none());
    }

    #[test]
    fn mcp_get_reports_a_project_scope_and_a_failure_reason() {
        let stdout = "\
dbg: 
  Scope: Project config (shared through .mcp.json)
  Status: × Failed to connect — ECONNREFUSED
";
        let (scope, health, detail) = parse_mcp_get(stdout);
        assert_eq!(scope, McpScope::Project);
        assert_eq!(health, Some(McpHealth::Failed));
        assert!(detail.is_some());
    }

    #[test]
    fn an_unknown_scope_word_falls_back_to_local_not_user() {
        // Guessing `user` would widen the blast radius of a remove: it names a
        // config shared by every project.
        let (scope, _, _) = parse_mcp_get("x:\n  Scope: Something new\n");
        assert_eq!(scope, McpScope::Local);
    }

    /// A real observation from this machine: a plugin's server reads
    /// "Scope: Dynamic config (from command line)". Before this was handled it
    /// was classified `Local`, which offered a delete that no config file backs
    /// and that the plugin would undo.
    #[test]
    fn a_plugin_contributed_server_is_recognised_and_not_removable() {
        let stdout = "\
plugin:ida-pro-mcp:idalib:
  Scope: Dynamic config (from command line)
  Status: √ Connected
  Type: stdio
";
        let (scope, health, _) = parse_mcp_get(stdout);
        assert_eq!(scope, McpScope::Plugin);
        assert_eq!(health, Some(McpHealth::Connected));
        assert!(scope.cli_flag().is_none());
        assert!(!scope.is_removable());
    }

    #[test]
    fn directly_configured_scopes_remain_removable() {
        for scope in [McpScope::User, McpScope::Project, McpScope::Local] {
            assert!(scope.is_removable());
            assert!(scope.cli_flag().is_some());
        }
    }

    fn installed_plugin(id: &str) -> PluginEntry {
        PluginEntry {
            id: id.into(),
            name: id.split_once('@').map_or(id, |(name, _)| name).into(),
            marketplace: id.split_once('@').map_or("", |(_, market)| market).into(),
            version: "1.0.0".into(),
            scope: McpScope::User,
            enabled: true,
            project_enabled: false,
            description: String::new(),
            mcp_servers: Vec::new(),
            components: Vec::new(),
            install_path: None,
        }
    }

    /// `--available` wraps its payload in `{installed, available}`, unlike the
    /// bare array plain `list` returns. Reading only the wrapped shape would
    /// silently show nothing if the CLI ever dropped the wrapper.
    #[test]
    fn available_plugins_read_the_wrapped_and_bare_shapes() {
        let wrapped = r#"{"installed":[],"available":[
            {"pluginId":"a@m","name":"a","description":"first","marketplaceName":"m","installCount":12}
        ]}"#;
        let plugins = parse_available_plugins(wrapped, &[]).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "a@m");
        assert_eq!(plugins[0].install_count, Some(12));
        assert!(!plugins[0].installed);

        let bare = r#"[{"pluginId":"a@m","name":"a","installCount":12}]"#;
        assert_eq!(parse_available_plugins(bare, &[]).unwrap().len(), 1);
    }

    /// The installed flag decides whether the row offers Install at all, so it
    /// must come from the installed catalog rather than from the available
    /// list's own opinion of itself.
    #[test]
    fn an_available_plugin_already_installed_is_marked_so() {
        let json = r#"[{"pluginId":"a@m","name":"a"}]"#;
        let plugins = parse_available_plugins(json, &[installed_plugin("a@m")]).unwrap();
        assert!(plugins[0].installed);
    }

    #[test]
    fn marketplaces_carry_their_source_and_location() {
        let json = r#"[
            {"name":"mrexodia","source":"github","repo":"mrexodia/claude-marketplace",
             "installLocation":"C:\\cache\\mrexodia"}
        ]"#;
        let marketplaces = parse_marketplace_list(json).unwrap();
        assert_eq!(marketplaces.len(), 1);
        assert_eq!(marketplaces[0].name, "mrexodia");
        assert_eq!(
            marketplaces[0].repo.as_deref(),
            Some("mrexodia/claude-marketplace")
        );
        assert_eq!(
            marketplaces[0].install_location.as_deref(),
            Some("C:\\cache\\mrexodia")
        );
    }

    fn write_manifest(dir: &Path, body: &str) {
        let manifest_dir = dir.join(".claude-plugin");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(manifest_dir.join("plugin.json"), body).unwrap();
    }

    /// `author` is a plain string in some manifests and `{name}` in others, and
    /// both are in use on this machine. Reading only one shape would drop the
    /// author from half the plugins.
    #[test]
    fn plugin_details_read_both_author_shapes() {
        let root = std::env::temp_dir().join("waku-plugin-details-author");
        let _ = std::fs::remove_dir_all(&root);
        write_manifest(&root, r#"{"name":"p","author":"A Person"}"#);
        assert_eq!(
            plugin_details("p@m", Some(&root)).author.as_deref(),
            Some("A Person")
        );

        write_manifest(
            &root,
            r#"{"name":"p","author":{"name":"Anthropic","email":"x@y.z"}}"#,
        );
        assert_eq!(
            plugin_details("p@m", Some(&root)).author.as_deref(),
            Some("Anthropic")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The manifest lists most component kinds as arrays but hooks and LSP
    /// servers as objects keyed by name; a parser that only handles arrays
    /// reports a plugin with hooks as having none.
    #[test]
    fn plugin_details_collect_every_component_kind() {
        let root = std::env::temp_dir().join("waku-plugin-details-kinds");
        let _ = std::fs::remove_dir_all(&root);
        write_manifest(
            &root,
            r#"{
                "name":"p","version":"2.1.0","description":"d",
                "mcpServers":{"srv":{"command":"x"}},
                "skills":["one","two"],
                "commands":["ship"],
                "hooks":{"PreToolUse":[]},
                "lspServers":{"rust":{}}
            }"#,
        );
        let details = plugin_details("p@m", Some(&root));
        assert_eq!(details.version, "2.1.0");
        assert_eq!(details.mcp_servers, ["srv"]);
        let kinds: Vec<(&str, &str)> = details
            .components
            .iter()
            .map(|component| (component.kind.as_str(), component.name.as_str()))
            .collect();
        assert!(kinds.contains(&("skills", "one")));
        assert!(kinds.contains(&("skills", "two")));
        assert!(kinds.contains(&("commands", "ship")));
        assert!(kinds.contains(&("hooks", "PreToolUse")));
        assert!(kinds.contains(&("lspServers", "rust")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing manifest must not crash or claim updatability: the plugin is
    /// still listed, it just has nothing to read and nothing to update from.
    #[test]
    fn plugin_details_survive_a_missing_manifest() {
        let root = std::env::temp_dir().join("waku-plugin-details-missing");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let details = plugin_details("p@m", Some(&root));
        assert_eq!(details.name, "p");
        assert!(!details.can_update);
        assert!(details.components.is_empty());

        let none = plugin_details("p@m", None);
        assert_eq!(none.name, "p");
        assert!(!none.can_update);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The concurrent scope pass must put each answer on the server it belongs
    /// to, even when the servers finish in the opposite order.
    ///
    /// Workers take arbitrary indices, so filing by completion order would hand
    /// every row some other row's scope — and the scope is what decides which
    /// `--scope` a remove uses. A mis-filed `user` where a `project` belonged
    /// would delete from the wrong config.
    ///
    /// The stub makes the *first* server the slow one and every later one fast,
    /// so completion order is the reverse of input order and a shifted result
    /// cannot pass by luck. The stub is a `.cmd` on Windows and a shell script
    /// elsewhere, so the test runs on both.
    #[test]
    fn concurrent_scopes_stay_filed_against_their_own_server() {
        let dir = std::env::temp_dir().join("waku-scope-filing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        #[cfg(windows)]
        let script = {
            // `%1`=mcp `%2`=get `%3`=name. `ping` is the portable Windows sleep:
            // it waits about a second without needing another binary.
            let script = dir.join("fake-claude.cmd");
            let body = "\
@echo off\n\
if \"%3\"==\"server-0\" (ping -n 2 127.0.0.1 >nul & echo Scope: Project config & exit /b 0)\n\
if \"%3\"==\"server-1\" (echo Scope: Local config & exit /b 0)\n\
if \"%3\"==\"server-2\" (echo Scope: Dynamic config & exit /b 0)\n\
if \"%3\"==\"server-3\" (echo Scope: User config & exit /b 0)\n\
exit /b 0\n";
            std::fs::write(&script, body).unwrap();
            script
        };

        #[cfg(unix)]
        let script = {
            use std::io::Write as _;
            let script = dir.join("fake-claude");
            let mut file = std::fs::File::create(&script).unwrap();
            write!(
                file,
                "#!/bin/sh\n\
                 case \"$3\" in\n\
                 server-0) sleep 1; echo 'Scope: Project config' ;;\n\
                 server-1) echo 'Scope: Local config' ;;\n\
                 server-2) echo 'Scope: Dynamic config' ;;\n\
                 server-3) echo 'Scope: User config' ;;\n\
                 esac\n"
            )
            .unwrap();
            drop(file);
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            script
        };

        let servers: Vec<McpServerEntry> = (0..4)
            .map(|index| McpServerEntry {
                name: format!("server-{index}"),
                scope: McpScope::User,
                transport: McpTransport::Stdio,
                target: "cmd".into(),
                args: Vec::new(),
                pending_approval: false,
                health: None,
                detail: None,
            })
            .collect();
        let results = describe_mcp_scopes(&script, None, &servers);
        assert_eq!(
            results,
            vec![
                Some(McpScope::Project),
                Some(McpScope::Local),
                Some(McpScope::Plugin),
                Some(McpScope::User),
            ],
            "server-0 answers last, so filing by completion order swaps these"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A result line is the last JSON object on stdout, even on failure.
    ///
    /// The CLI prints human progress first and the object last, so a parser
    /// that took the first brace it saw would read a progress message.
    #[test]
    fn an_action_result_is_read_off_the_last_json_line() {
        let stdout = "Refreshing marketplace cache (timeout: 120s)…\n\
                      {\"command\":\"marketplace-update\",\"outcome\":\"ok\",\"marketplace\":\"mrexodia\",\"message\":\"Successfully updated\"}\n";
        let result = stdout.lines().rev().find_map(parse_action_line).unwrap();
        assert!(result.ok);
        assert_eq!(result.message, "Successfully updated");

        let failed = "{\"command\":\"marketplace-add\",\"outcome\":\"failed\",\"message\":\"Invalid marketplace source format.\",\"failureCode\":\"invalid_source\"}";
        let result = parse_action_line(failed).unwrap();
        assert!(!result.ok);
        assert_eq!(result.failure_code.as_deref(), Some("invalid_source"));
        assert!(result.message.contains("Invalid marketplace source"));
    }

    /// Prose that merely starts with `{` is not a result line.
    #[test]
    fn non_json_lines_are_not_mistaken_for_results() {
        assert!(parse_action_line("{not json").is_none());
        assert!(parse_action_line("plain progress text").is_none());
        // Valid JSON without `outcome` is not one of these results either.
        assert!(parse_action_line(r#"{"command":"x"}"#).is_none());
    }

    /// The schema says what may be set, the two lists say what is.
    #[test]
    fn plugin_config_reports_options_and_their_set_state() {
        let stdout = r#"{
            "pluginId":"p@m",
            "schema":{
                "token":{"type":"string","description":"API token","secret":true},
                "mode":{"title":"Mode","enum":["fast","safe"]},
                "level":{"type":"number","required":true}
            },
            "configured":["token"],
            "unconfigured":["mode","level"]
        }"#;
        let catalog = parse_plugin_config("p@m", stdout).unwrap();
        assert!(catalog.unavailable.is_none());
        let by_key = |key: &str| {
            catalog
                .options
                .iter()
                .find(|option| option.key == key)
                .unwrap()
        };
        assert!(by_key("token").secret);
        assert!(by_key("token").is_set);
        assert_eq!(by_key("mode").kind, "enum");
        assert_eq!(by_key("mode").choices, ["fast", "safe"]);
        assert!(!by_key("mode").is_set);
        assert!(by_key("level").required);
        assert!(catalog.orphaned.is_empty());
    }

    /// A saved key the schema no longer declares is reported, not hidden: it is
    /// usually why a plugin misbehaves.
    #[test]
    fn a_saved_value_without_a_schema_entry_is_reported_as_orphaned() {
        let stdout = r#"{
            "pluginId":"p@m",
            "schema":{"mode":{"enum":["a"]}},
            "configured":["mode","removed-key"],
            "unconfigured":[]
        }"#;
        let catalog = parse_plugin_config("p@m", stdout).unwrap();
        assert_eq!(catalog.orphaned, ["removed-key"]);
    }

    /// An empty schema is a real answer, not a failure: the plugin simply has
    /// nothing to configure.
    #[test]
    fn an_empty_schema_is_not_an_error() {
        let stdout = r#"{"pluginId":"p@m","schema":{},"inputs":{},"choices":{},"configured":[],"unconfigured":[]}"#;
        let catalog = parse_plugin_config("p@m", stdout).unwrap();
        assert!(catalog.unavailable.is_none());
        assert!(catalog.options.is_empty());
    }

    /// A plugin the CLI cannot resolve must stay an error, not an empty form.
    ///
    /// `configure` resolves against the current directory, and a project-scoped
    /// plugin outside its project answers "No installed plugin has the id…".
    /// Reporting that as "no options" would tell the user the plugin simply has
    /// nothing to configure, which is a different and wrong statement.
    ///
    /// The CLI itself is not invoked here: that would make the test depend on
    /// what is installed. What this pins is the split between "read, no
    /// options" and "could not read", which is the decision the page renders
    /// differently.
    #[test]
    fn an_unresolvable_plugin_is_not_reported_as_having_no_options() {
        // An unparsable answer stands in for any failure to read.
        let catalog = parse_plugin_config("p@m", "No installed plugin has the id");
        assert!(
            catalog.is_err(),
            "an answer that is not JSON is a read failure, not an empty schema"
        );
    }

    /// A probe that cannot run must leave its own slot empty.
    #[test]
    fn a_failed_scope_probe_leaves_its_slot_empty() {
        let servers: Vec<McpServerEntry> = (0..3)
            .map(|index| McpServerEntry {
                name: format!("server-{index}"),
                scope: McpScope::User,
                transport: McpTransport::Stdio,
                target: "cmd".into(),
                args: Vec::new(),
                pending_approval: false,
                health: None,
                detail: None,
            })
            .collect();
        let results =
            describe_mcp_scopes(Path::new("definitely-not-a-real-binary"), None, &servers);
        assert_eq!(results.len(), servers.len());
        assert!(
            results.iter().all(Option::is_none),
            "a failed probe must leave its own slot empty"
        );
    }
}
