use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where a Claude MCP server came from, as `claude mcp list` reports it.
///
/// Claude Code has three scopes and they are not interchangeable: `user` applies
/// to every project, `project` lives in a committed `.mcp.json`, and `local` is
/// this project on this machine only. Removing the wrong one silently changes
/// behaviour for every other project, so the scope travels with the entry and
/// every mutation carries it back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum McpScope {
    User,
    Project,
    Local,
    /// Supplied by a plugin rather than configured directly.
    ///
    /// `claude mcp get` reports these as "Dynamic config (from command line)",
    /// and they are not the user's to delete here: the plugin owns them, and
    /// removing one would be undone by the plugin on its next load. They are
    /// listed so the page tells the truth about what the agent can reach, with
    /// the remove control withheld.
    Plugin,
}

impl McpScope {
    pub fn label(self) -> String {
        match self {
            Self::User => tr!("claude.scope_user"),
            Self::Project => tr!("claude.scope_project"),
            Self::Local => tr!("claude.scope_local"),
            Self::Plugin => tr!("claude.scope_plugin"),
        }
    }

    /// The `--scope` spelling the CLI accepts.
    ///
    /// `Plugin` has none: those entries have no scope flag because they are not
    /// in any config file the CLI would remove them from.
    pub fn cli_flag(self) -> Option<&'static str> {
        match self {
            Self::User => Some("user"),
            Self::Project => Some("project"),
            Self::Local => Some("local"),
            Self::Plugin => None,
        }
    }

    /// Whether the page may offer to remove this entry.
    pub fn is_removable(self) -> bool {
        !matches!(self, Self::Plugin)
    }
}

/// How a server is reached. Mirrors the four transports `claude mcp add` knows.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
    WebSocket,
}

/// Connection health, which is only ever known after an asynchronous probe.
///
/// `Unknown` is a first-class state, not a placeholder: a probe can time out,
/// the CLI can be missing, or the server may simply not have answered yet. The
/// UI must be able to say "not checked" without implying "broken".
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum McpHealth {
    Connected,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct McpServerEntry {
    pub name: String,
    pub scope: McpScope,
    pub transport: McpTransport,
    /// The command line for stdio transports, or the URL otherwise.
    pub target: String,
    /// Extra arguments, stdio only.
    pub args: Vec<String>,
    /// A project-scoped `.mcp.json` server that has not been approved yet.
    ///
    /// Claude Code does not connect to these, so neither should any control
    /// here pretend they are live.
    pub pending_approval: bool,
    /// `None` until a probe has answered for this name.
    pub health: Option<McpHealth>,
    /// Why the probe failed, when the CLI reported a reason.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct McpCatalog {
    pub servers: Vec<McpServerEntry>,
    /// The CLI could not be run, so the list is empty for a reason the UI must
    /// show rather than presenting an empty configuration.
    pub unavailable: Option<String>,
}

/// One installed Claude Code plugin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    /// `name@marketplace`, the id the CLI takes for enable/disable/uninstall.
    pub id: String,
    pub name: String,
    pub marketplace: String,
    pub version: String,
    pub scope: McpScope,
    pub enabled: bool,
    /// Whether this project specifically has it switched off, which can differ
    /// from `enabled` for a user-scope plugin.
    pub project_enabled: bool,
    pub description: String,
    /// MCP servers the plugin contributes, by name.
    pub mcp_servers: Vec<String>,
    /// Commands, agents, hooks and skills it brings, as its detail page lists.
    pub components: Vec<PluginComponent>,
    pub install_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PluginComponent {
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PluginCatalog {
    pub plugins: Vec<PluginEntry>,
    pub unavailable: Option<String>,
}

/// One marketplace the user has added, as `plugin marketplace list` reports it.
///
/// A marketplace is a catalogue of plugins, not a plugin itself: it has to be
/// added before anything inside it can be installed. It carries no enable state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    pub name: String,
    /// How the marketplace is reached: `github`, `git`, `directory`, `url`.
    pub source: String,
    /// The repository, path or URL behind it, when the CLI names one.
    pub repo: Option<String>,
    /// Where its clone lives on this machine.
    pub install_location: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceCatalog {
    pub marketplaces: Vec<MarketplaceEntry>,
    pub unavailable: Option<String>,
}

/// A plugin one of the added marketplaces offers, installed or not.
///
/// This list is the expensive one: asking for it makes the CLI refresh every
/// marketplace first, which was measured at ~10s against 3916 entries. It is
/// therefore its own command, triggered on purpose rather than on page open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AvailablePlugin {
    /// `name@marketplace`, the id both install and update take.
    pub id: String,
    pub name: String,
    pub marketplace: String,
    pub description: String,
    /// How many other users have it, which is the only ordering signal the CLI
    /// gives. `None` when it did not report one.
    pub install_count: Option<u64>,
    /// Already installed, so the row offers no install control.
    pub installed: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AvailablePluginCatalog {
    pub plugins: Vec<AvailablePlugin>,
    pub unavailable: Option<String>,
}

/// What one plugin ships and what it costs to have it loaded.
///
/// Read from the install path's manifest rather than `claude plugin details`,
/// because that verb only resolves plugins visible from the current directory —
/// a project-scoped plugin installed for another project answers "not found".
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetails {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub homepage: Option<String>,
    /// The MCP servers the plugin contributes, by name.
    pub mcp_servers: Vec<String>,
    /// Other component kinds, in the form `kind: name` the manifest uses, so a
    /// newer manifest with a kind this build does not know still shows up.
    pub components: Vec<PluginComponent>,
    /// Whether `claude plugin update` should be offered.
    ///
    /// Only ever true for an installed plugin from a marketplace: a locally
    /// authored plugin has no newer version to fetch.
    pub can_update: bool,
}
