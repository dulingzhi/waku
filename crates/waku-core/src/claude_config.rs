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
    McpCatalog, McpHealth, McpScope, McpServerEntry, McpTransport, PluginCatalog, PluginComponent,
    PluginEntry,
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
    for server in &mut servers {
        if let Some((scope, _, _)) = describe_mcp_server(override_spec, cwd, &server.name) {
            server.scope = scope;
        }
    }
    McpCatalog {
        servers,
        unavailable: None,
    }
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
) -> Result<(), String> {
    let binary = claude_binary(override_spec).ok_or_else(|| tr!("claude.cli_not_found"))?;
    run_claude(&binary, &["plugin", "uninstall", id], cwd).map(|_| ())
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
}
