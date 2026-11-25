use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

use crate::attachments::{AttachmentUpload, StoredAttachment};
use crate::computer_use::ComputerPermissions;
use crate::model::{
    AgentSession, GoalOperation, Project, ProviderKind, ProviderProbe, ProviderResumeCursor,
    ProviderSessionHistory, ProviderSessionSummary, SessionStatus, UserInputAnswer,
};
use crate::persistence::{ComposerDraftChange, ComposerDrafts, SessionMessageMatch};
use crate::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use crate::settings::DaemonSettings;
use crate::skills::SkillsCatalog;
use crate::agents::{AgentDefinition, AgentDefinitionScope, AgentsCatalog, AgentTeam};
use crate::usage::PlanUsage;
use crate::usage_history::{UsageHistory, UsageWindow};
use crate::workspace::{WorkspaceOperation, WorkspaceResult};

use crate::claude::{
    AvailablePluginCatalog, ClaudeActionResult, MarketplaceCatalog, MarketplaceScope, McpCatalog,
    McpScope, PluginCatalog, PluginConfigCatalog, PluginDetails,
};

/// Bumped because the new delegate commands below change the command surface:
/// a client that sends them to an older daemon would get an error it cannot
/// distinguish from a real failure.
///
/// 15 adds the agent-identity and team commands, plus the `persona` a spawn may
/// name. A client built before this cannot describe which identity a delegate
/// should run as, and one built after it expects a `persona` field an older
/// daemon never sends back.
///
/// 14 relays a delegate's own stream to its lead's conversation under a new
/// event kind. A client built before this cannot decode that kind, so it would
/// read the delegate's live text as an invalid daemon event rather than seeing it.
///
/// 13 gives a session the conversation that spawned it as a delegate. A client
/// built before this would list a group run's subagents as tasks of their own,
/// and one built after it would expect a field an older daemon never sends.
///
/// 12 adds `readDelegate` and `promptDelegate`, the two commands an agent
/// bridge uses to read back and steer the delegates it started.
///
/// 10 adds the marketplace, available-plugin, plugin-detail, install and
/// update commands, which extend the same surface for the same reason.
pub const PROTOCOL_VERSION: u32 = 15;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 48 * 1024 * 1024;
pub const DAEMON_TOKEN_ENV: &str = "WAKU_DAEMON_TOKEN";
pub const DAEMON_ADDRESS_ENV: &str = "WAKU_DAEMON_ADDRESS";
/// The session a delegating provider was launched for.
///
/// The scoped token already proves this, but the bridge cannot read a token's
/// scope, and every request it sends has to name a session. Passing the id
/// alongside keeps the bridge from guessing: a wrong id is refused by the
/// daemon's own scope check rather than being silently accepted.
pub const DAEMON_SESSION_ENV: &str = "WAKU_DAEMON_SESSION";
/// The runtime a delegating provider was launched for.
///
/// A scoped token is checked against both halves of its scope, so a bridge that
/// guessed the runtime would be refused by the daemon's own check — as it should
/// be. The pair travels together for exactly that reason.
pub const DAEMON_RUNTIME_ENV: &str = "WAKU_DAEMON_RUNTIME";
pub const APP_EXECUTABLE_ENV: &str = "WAKU_APP_EXECUTABLE";

/// The single line the daemon prints on stdout once it is accepting
/// connections, which is how a client learns where to connect and whether the
/// build is compatible.
///
/// The two public keys are the daemon's long-lived identity
/// (`waku_core::daemon_identity`). They are published here so a client can pin
/// them on first contact and verify later connections. They are public keys
/// only: the secrets never leave the daemon process.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReady {
    pub address: String,
    pub protocol_version: u32,
    pub pid: u32,
    /// Base64 of the 32-byte X25519 public key, used for key agreement.
    pub x25519_public: String,
    /// Base64 of the 32-byte Ed25519 public key, used to verify signatures.
    pub ed25519_public: String,
    /// Present only when the daemon dials a relay.
    ///
    /// It lives here rather than on a second stdout line because the readiness
    /// reader stops after one line: a daemon that printed its pairing code
    /// separately would have that line read by nobody, and the UI would show
    /// nothing while looking implemented.
    ///
    /// Absent — not empty — when no relay is configured, so a daemon that only
    /// listens cannot be mistaken for a relayed one with a blank code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<DaemonPairing>,
}

/// What a client needs to reach a relayed daemon.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DaemonPairing {
    /// The daemon's `waku://` code, ready to display.
    pub code: String,
    /// The relay the daemon dialed, for display next to the code.
    pub relay: String,
    /// The session the daemon is holding.
    pub session_id: String,
}

// Versioned wire type: variants differ in size by design, and boxing one would
// change the union `ts-rs` generates for every client. Messages move one at a
// time, so the spread costs nothing in practice.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        token: String,
        client_id: Uuid,
        #[serde(default)]
        resume_from: Vec<ReplayCursor>,
    },
    Request(Request),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCursor {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Identifies the daemon process that assigned `sequence`.
    pub epoch: Uuid,
    pub sequence: u64,
}

// Versioned wire type: `ts-rs` derives the browser client's union from these
// variants, so boxing one would change the generated TypeScript shape.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// Resolve the daemon-owned provider runtime for an existing task.
    ///
    /// Clients use this after reconnecting or opening the same daemon from a
    /// second app. It observes the session actor without starting, replacing,
    /// or otherwise mutating the provider process.
    AttachSession,
    Start {
        options: WireDriverStartOptions,
    },
    Prompt {
        prompt: String,
        /// The ids the submitting client already gave this turn and its user
        /// message. The daemon republishes them with the submission so every
        /// other client attached to the runtime mirrors the same rows instead
        /// of minting its own; older clients omit them and the daemon mints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<Uuid>,
    },
    Steer {
        prompt: String,
    },
    Cancel,
    CancelComputerUse,
    RefreshBackgroundWork,
    StopBackgroundWork {
        key: Value,
        control_id: String,
    },
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    /// Ask the live provider runtime to read or mutate its persisted thread
    /// goal. Fire-and-forget: the outcome arrives as a `goalUpdated` driver
    /// event, or an `error` event when the provider refuses.
    Goal {
        operation: GoalOperation,
    },
    RunComputerTool {
        request: WireComputerToolRequest,
    },
    RejectComputerTool {
        request: WireComputerToolRequest,
        reason: String,
    },
    ApplyOptions {
        options: WireSessionOptions,
    },
    Rollback {
        turns: usize,
    },
    Fork {
        turns_to_remove: usize,
    },
    GetSettings,
    UpdateSettings {
        settings: DaemonSettings,
    },
    ProbeProvider {
        provider: ProviderKind,
        binary_override: Option<String>,
        discover_models: bool,
        probe_version: bool,
    },
    FetchPlanUsage {
        provider: ProviderKind,
        binary_override: Option<String>,
        cli_version: Option<String>,
    },
    ProbeComputerPermissions {
        prompt: bool,
    },
    LoadUsageHistory {
        window: UsageWindow,
        project_roots: Vec<PathBuf>,
    },
    LoadSkills {
        projects: Vec<(String, PathBuf)>,
    },
    /// Every agent identity and team template this daemon can see, with the
    /// problems it found reading them.
    LoadAgents {
        /// The project whose `.waku/agents` root to include. `None` lists the
        /// built-ins and the user's own identities only.
        #[ts(type = "string | null")]
        project: Option<PathBuf>,
    },
    /// Write one identity, creating its directory.
    SaveAgentDefinition {
        definition: AgentDefinition,
        #[ts(type = "string | null")]
        project: Option<PathBuf>,
    },
    DeleteAgentDefinition {
        scope: AgentDefinitionScope,
        id: String,
        #[ts(type = "string | null")]
        project: Option<PathBuf>,
    },
    SaveAgentTeam {
        team: AgentTeam,
        #[ts(type = "string | null")]
        project: Option<PathBuf>,
    },
    DeleteAgentTeam {
        scope: AgentDefinitionScope,
        id: String,
        #[ts(type = "string | null")]
        project: Option<PathBuf>,
    },
    SetSkillsEnabled {
        dirs: Vec<PathBuf>,
        enabled: bool,
    },
    TrashSkills {
        dirs: Vec<PathBuf>,
    },
    /// Every MCP server Claude Code knows about, with its scope and the health
    /// `claude mcp list` reports. One command, because that CLI verb both
    /// enumerates and health-checks; there is no cheap configuration-only view
    /// to split out.
    LoadClaudeMcpServers {
        /// The project whose `local` and `project` scopes to include, when a
        /// task is open. `None` lists user scope only.
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    /// Remove an MCP server. There is deliberately no `Set…Enabled` command:
    /// Claude Code has no CLI verb for it — disabling is the per-project
    /// `disabledMcpServers` key inside `.claude.json`, which this build refuses
    /// to rewrite (see `waku_core::claude_config` for why).
    RemoveClaudeMcpServer {
        name: String,
        #[ts(type = "string")]
        scope: McpScope,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    LoadClaudePlugins,
    SetClaudePluginEnabled {
        id: String,
        enabled: bool,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    UninstallClaudePlugin {
        id: String,
        /// The scope the plugin is installed into. `plugin uninstall` defaults
        /// to `user`, so this has to be explicit or a project-scoped plugin is
        /// reported uninstalled while still installed.
        #[ts(type = "string")]
        scope: McpScope,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    /// The marketplaces the user has added, cheap enough to read with the page.
    LoadClaudeMarketplaces,
    /// Everything the added marketplaces offer, installed or not.
    ///
    /// Separate from `LoadClaudePlugins` on purpose: answering it makes the CLI
    /// refresh every marketplace, measured at ~10s for 3916 entries, so it must
    /// never run as part of opening the page.
    LoadAvailableClaudePlugins,
    /// Read one installed plugin's manifest. Reads a file, not the CLI, because
    /// `plugin details` only resolves plugins visible from the current directory.
    LoadClaudePluginDetails {
        id: String,
        #[ts(type = "string | null")]
        install_path: Option<PathBuf>,
    },
    InstallClaudePlugin {
        id: String,
        /// The scope to install into. The CLI defaults to `user`, which would
        /// put a project plugin into every project without saying so.
        #[ts(type = "string")]
        scope: McpScope,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    UpdateClaudePlugin {
        id: String,
        #[ts(type = "string | null")]
        scope: Option<McpScope>,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    /// Declare a marketplace from a repo, URL or path.
    AddClaudeMarketplace {
        /// `owner/repo`, an https/git URL, or a path on the daemon host.
        source: String,
        #[ts(type = "string")]
        scope: MarketplaceScope,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    /// Remove a marketplace declaration. `scope: None` removes it everywhere.
    RemoveClaudeMarketplace {
        name: String,
        #[ts(type = "string | null")]
        scope: Option<MarketplaceScope>,
    },
    /// Refresh one marketplace, or every one when no name is given.
    UpdateClaudeMarketplace {
        #[ts(type = "string | null")]
        name: Option<String>,
    },
    /// Read the options a plugin declares and which of them are set.
    ///
    /// Carries `cwd` because `plugin configure` resolves the plugin against the
    /// current directory: a project-scoped plugin is only reachable from its own
    /// project, exactly like `plugin details`.
    LoadClaudePluginConfig {
        id: String,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    /// Save plugin options. Values are the CLI's own `key=value` pairs.
    SetClaudePluginConfig {
        id: String,
        values: Vec<String>,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    /// Remove auto-installed plugin dependencies that are no longer needed.
    PruneClaudePlugins {
        /// Report what would be removed without removing it.
        dry_run: bool,
        scope: MarketplaceScope,
        #[ts(type = "string | null")]
        cwd: Option<PathBuf>,
    },
    LoadTaskState,
    SaveTaskState {
        projects: Vec<Project>,
        live_session_ids: Vec<Uuid>,
        sessions: Vec<AgentSession>,
    },
    /// Explicitly remove one daemon-owned task. Ordinary state saves are
    /// merge-only so a stale client snapshot cannot delete tasks another
    /// client just created.
    RemoveSession,
    HydrateSession {
        session_id: Uuid,
    },
    SearchSessionMessages {
        query: String,
        limit: usize,
    },
    /// List one provider's resumable CLI conversations on the daemon host.
    ListProviderSessions {
        provider: ProviderKind,
        limit: usize,
    },
    /// Load the user-visible transcript for one provider-native conversation.
    LoadProviderSession {
        cursor: ProviderResumeCursor,
        cwd: PathBuf,
    },
    LoadComposerDrafts,
    SaveComposerDrafts {
        drafts: ComposerDrafts,
        generation: u64,
    },
    ApplyComposerDraftChanges {
        changes: Vec<ComposerDraftChange>,
    },
    StoreBlob {
        mime_type: String,
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ImportAttachment {
        name: String,
        upload: AttachmentUpload,
    },
    ImportPathAttachment {
        #[ts(type = "string")]
        path: PathBuf,
    },
    ReadBlob {
        reference: String,
    },
    ReadAttachment {
        reference: String,
        path: PathBuf,
    },
    SweepBlobs,
    /// Fork a persisted task through one completed provider turn.
    ///
    /// This is intentionally a daemon-owned operation: provider-native
    /// conversation state, Git checkpoint refs, and SQLite all live on the
    /// daemon host and must move together for remote clients.
    ForkSessionFromResponse {
        turn_count: usize,
    },
    /// Restore a task and its provider conversation to immediately before a
    /// prior user message. The client can then submit the edited replacement
    /// as an ordinary new turn.
    RewindSessionToMessage {
        turn_count: usize,
    },
    ForkProviderSession {
        request: ProviderSessionForkRequest,
    },
    Workspace {
        operation: WorkspaceOperation,
    },
    OpenTerminal {
        #[ts(type = "string")]
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    WriteTerminal {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        data: Vec<u8>,
    },
    ResizeTerminal {
        cols: u16,
        rows: u16,
    },
    /// Start one delegate session for the calling lead conversation.
    ///
    /// This exists because a lead cannot do it with the ordinary commands: it
    /// is not allowed to save task state or to start a runtime under an id of
    /// its own choosing, and both would make an agent token a second full
    /// client. The daemon copies the lead's own provider, workspace, and
    /// options onto a new task, starts it, and reports the pair back — so the
    /// delegate is an ordinary Waku session the user can open and read, while
    /// the capability that created it stays narrow.
    SpawnDelegate {
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The identity to start this delegate as, by id from the catalog the
        /// lead was given. `None` starts an anonymous delegate that inherits
        /// the lead's provider and model, which is what an install with no
        /// identities defined has.
        ///
        /// The id travels rather than resolved instructions so the daemon
        /// stays the authority on which identities exist: a bridge cannot
        /// invent one by writing its own prompt into a spawn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persona: Option<String>,
    },
    /// Start one delegate per member of a saved team, so a whole group can be
    /// set up in one call.
    ///
    /// The team is named by id and resolved by the daemon, for the same reason a
    /// single spawn names its identity rather than its prompt: the daemon stays
    /// the authority on which teams and identities exist, and a bridge cannot
    /// invent a group by describing one.
    SpawnTeam {
        team: String,
        /// The task every member is started on. A team is a set of roles, not a
        /// set of tasks, so the caller supplies the one job the group shares;
        /// each member applies its own identity to it.
        task: String,
        /// A short label prefixed onto each member's session title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// What one delegate the calling lead started is doing, and what it has
    /// said in its current turn.
    ///
    /// Named by the delegate rather than the conversation, because a lead
    /// addresses a delegate's *task* while its own token stays scoped to the
    /// lead's conversation. The daemon is the authority on which delegates
    /// belong to which lead, so it checks the pair rather than trusting the
    /// caller to name a task it may see.
    ReadDelegate {
        session_id: Uuid,
    },
    /// Add an instruction to a delegate the calling lead started.
    ///
    /// The daemon chooses how it lands: a delegate still working has it steered
    /// into the running turn, and an idle one starts a new turn. The lead does
    /// not get to pick, because only the daemon knows which of the two is
    /// possible at the moment the message arrives.
    PromptDelegate {
        session_id: Uuid,
        prompt: String,
    },
    CloseTerminal,
    CloseSession,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverStartOptions {
    pub provider: String,
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub agent_preset: Option<String>,
    pub computer_use_enabled: bool,
    pub provider_cursor: Option<Value>,
    /// Text this session starts as, resolved by the daemon from an identity the
    /// client named. Sent as the resolved prompt rather than an id so a client
    /// starting a task cannot make the daemon read a catalog it chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireSessionOptions {
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireComputerToolRequest {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverEvent {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl WireDriverEvent {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }
}

/// The event kind that carries a delegate's stream to its lead's conversation.
///
/// A delegate runs as its own task with its own provider process, so its text
/// arrives on its own runtime and nowhere else. The lead's conversation is where
/// a user watches a group work, and it can show a delegate live only if that
/// text also reaches the lead's stream. Rather than widen every event with an
/// optional subject, the daemon republishes the delegate's own events under this
/// one kind, with the delegate's id beside the original event.
///
/// Only what a reader watches travels this way — text, the turn's start and
/// finish, and an error. Tool calls, reasoning and checkpoints stay on the
/// delegate's runtime, where opening the task reads them from its transcript.
pub const DELEGATE_EVENT_KIND: &str = "delegateEvent";

/// The payload of a [`DELEGATE_EVENT_KIND`] event.
///
/// `sessionId` names the delegate the inner `event` happened to.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DelegateEventPayload {
    pub session_id: Uuid,
    pub event: WireDriverEvent,
}

/// One delegate a team spawn started.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SpawnedDelegate {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// The identity it was started as, so the caller can report which members
    /// actually started instead of assuming the whole team did.
    pub persona: String,
    /// Why this member could not start. A team is worth starting when most of
    /// it can: one bad identity must not cost the caller the other four, so a
    /// per-member failure is reported beside the successes rather than as one
    /// error for the whole call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SequencedEvent {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Changes whenever the daemon restarts, so a reused runtime id can begin
    /// again at sequence one without being mistaken for an old event.
    pub epoch: Uuid,
    pub sequence: u64,
    pub event: WireDriverEvent,
}

// Versioned wire type: `Event` carries a whole `SequencedEvent` while the rest
// are small markers; boxing it would change the generated TypeScript union.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        daemon_version: String,
    },
    Rejected {
        message: String,
    },
    Response {
        request_id: Uuid,
        outcome: ResponseOutcome,
    },
    Event(SequencedEvent),
    /// The daemon-owned project/task catalog changed through another client.
    /// Clients should invalidate their lightweight task-state snapshot; live
    /// runtime events continue through [`Self::Event`].
    TaskStateChanged {
        revision: u64,
    },
    ShuttingDown,
}

// Versioned wire type: both variants are already the flattened forms clients
// read, and boxing either would change the generated TypeScript union.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponseOutcome {
    Ok { payload: ResponsePayload },
    Error { error: RpcError },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponsePayload {
    Ack,
    SessionRuntime {
        runtime_id: Option<Uuid>,
        supports_steer: bool,
    },
    Started {
        supports_steer: bool,
    },
    OptionsApplied {
        applied: bool,
    },
    Cursor {
        cursor: Option<Value>,
    },
    Settings {
        settings: DaemonSettings,
    },
    ProviderProbe {
        probe: ProviderProbe,
        version: Option<String>,
    },
    PlanUsage {
        usage: Option<PlanUsage>,
    },
    ComputerPermissions {
        permissions: ComputerPermissions,
    },
    UsageHistory {
        history: UsageHistory,
    },
    SkillsCatalog {
        catalog: SkillsCatalog,
    },
    AgentsCatalog {
        catalog: AgentsCatalog,
    },
    ClaudeMcpCatalog {
        catalog: McpCatalog,
    },
    ClaudePluginCatalog {
        catalog: PluginCatalog,
    },
    ClaudeMarketplaceCatalog {
        catalog: MarketplaceCatalog,
    },
    ClaudeAvailablePluginCatalog {
        catalog: AvailablePluginCatalog,
    },
    ClaudePluginDetails {
        details: PluginDetails,
    },
    ClaudeAction {
        result: ClaudeActionResult,
    },
    ClaudePluginConfigCatalog {
        catalog: PluginConfigCatalog,
    },
    TaskState {
        projects: Vec<Project>,
        sessions: Vec<AgentSession>,
        default_cwd: PathBuf,
        projectless_root: Option<PathBuf>,
    },
    TaskStateSaved {
        sessions: Vec<AgentSession>,
    },
    Session {
        session: Option<AgentSession>,
    },
    SessionMessageMatches {
        matches: Vec<SessionMessageMatch>,
    },
    ProviderSessions {
        sessions: Vec<ProviderSessionSummary>,
    },
    ProviderSessionHistory {
        history: ProviderSessionHistory,
    },
    ComposerDrafts {
        drafts: ComposerDrafts,
    },
    BlobStored {
        reference: String,
        path: PathBuf,
    },
    AttachmentStored {
        attachment: StoredAttachment,
    },
    BlobData {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ProviderSessionForked {
        result: ProviderSessionFork,
    },
    SessionForked {
        session: AgentSession,
        checkpoint_warning: Option<String>,
    },
    SessionRewound {
        session: AgentSession,
        cleanup_warning: Option<String>,
    },
    Workspace {
        result: WorkspaceResult,
    },
    /// The task and runtime a [`Command::SpawnDelegate`] created, so the lead
    /// can address the delegate with the ordinary prompt and steer commands.
    DelegateSpawned {
        session_id: Uuid,
        runtime_id: Uuid,
    },
    /// One delegate per member of a team, each with its own identity.
    DelegateTeamSpawned {
        delegates: Vec<SpawnedDelegate>,
    },
    /// One delegate's live state, as the daemon sees it.
    ///
    /// `reply` is the delegate's own text for its current turn, taken from the
    /// provider's stream. It is the only place that text is guaranteed to
    /// exist: a delegate may have no client attached at all, so nothing else is
    /// guaranteed to have projected its transcript yet.
    DelegateReport {
        session_id: Uuid,
        title: String,
        status: SessionStatus,
        reply: String,
        /// Why the last turn failed, when it did. `None` on success, and also
        /// while the delegate is still working.
        failure: Option<String>,
    },
    /// How a [`Command::PromptDelegate`] was delivered.
    DelegatePrompted {
        session_id: Uuid,
        /// `true` when the message joined a turn already in progress, `false`
        /// when it opened a new one.
        steered: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct RpcError {
    pub message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

/// Carry a byte field as a base64 JSON string.
///
/// A `Vec<u8>` would otherwise serialise as an array of numbers — several times
/// the size, and unreadable in a log. Public because more than one message
/// module carries bytes; the encoding has to be the same in all of them or a
/// reader would decode one and not the other.
pub mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_payloads_use_base64_json_strings() {
        let payload = ResponsePayload::BlobData {
            bytes: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["bytes"], "AAEC/w==");
        let ResponsePayload::BlobData { bytes } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected payload variant");
        };
        assert_eq!(bytes, vec![0, 1, 2, 255]);

        let command = Command::WriteTerminal {
            data: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "writeTerminal");
        assert_eq!(json["data"], "AAEC/w==");
        let Command::WriteTerminal { data } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(data, vec![0, 1, 2, 255]);
    }

    #[test]
    fn response_fork_command_uses_stable_camel_case_fields() {
        let json =
            serde_json::to_value(Command::ForkSessionFromResponse { turn_count: 7 }).unwrap();

        assert_eq!(json["type"], "forkSessionFromResponse");
        assert_eq!(json["turnCount"], 7);
        assert_eq!(PROTOCOL_VERSION, 15);
    }

    #[test]
    fn message_rewind_command_uses_stable_camel_case_fields() {
        let json = serde_json::to_value(Command::RewindSessionToMessage { turn_count: 4 }).unwrap();

        assert_eq!(json["type"], "rewindSessionToMessage");
        assert_eq!(json["turnCount"], 4);
        assert_eq!(PROTOCOL_VERSION, 15);
    }

    #[test]
    fn provider_session_commands_use_stable_wire_fields() {
        let list = serde_json::to_value(Command::ListProviderSessions {
            provider: ProviderKind::Codex,
            limit: 250,
        })
        .unwrap();
        assert_eq!(list["type"], "listProviderSessions");
        assert_eq!(list["provider"], "codex");
        assert_eq!(list["limit"], 250);

        let load = serde_json::to_value(Command::LoadProviderSession {
            cursor: ProviderResumeCursor::Codex {
                thread_id: "01900000-0000-7000-8000-000000000001".into(),
            },
            cwd: PathBuf::from("/tmp/project"),
        })
        .unwrap();
        assert_eq!(load["type"], "loadProviderSession");
        assert_eq!(load["cursor"]["provider"], "codex");
        assert_eq!(
            load["cursor"]["threadId"],
            "01900000-0000-7000-8000-000000000001"
        );
        assert_eq!(load["cwd"], "/tmp/project");
    }

    #[test]
    fn handshake_and_replay_field_names_are_stable() {
        let session_id = Uuid::nil();
        let runtime_id = Uuid::from_u128(1);
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: "secret".into(),
            client_id: Uuid::from_u128(2),
            resume_from: vec![ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::from_u128(3),
                sequence: 9,
            }],
        };
        let json = serde_json::to_value(message).unwrap();

        assert_eq!(json["type"], "hello");
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["resumeFrom"][0]["sessionId"], session_id.to_string());
        assert_eq!(json["resumeFrom"][0]["runtimeId"], runtime_id.to_string());
        assert_eq!(
            json["resumeFrom"][0]["epoch"],
            Uuid::from_u128(3).to_string()
        );
        assert!(json.get("protocol_version").is_none());
    }

    #[test]
    fn composer_draft_changes_have_stable_wire_keys() {
        let project_id = Uuid::from_u128(7);
        let command = Command::ApplyComposerDraftChanges {
            changes: vec![ComposerDraftChange {
                target: crate::persistence::ComposerDraftTarget::NewSession { project_id },
                draft: Some(crate::persistence::ComposerDraft {
                    text: "unfinished".into(),
                    attachments: Vec::new(),
                }),
            }],
        };
        let json = serde_json::to_value(command).unwrap();

        assert_eq!(json["type"], "applyComposerDraftChanges");
        assert_eq!(json["changes"][0]["target"]["type"], "newSession");
        assert_eq!(
            json["changes"][0]["target"]["projectId"],
            project_id.to_string()
        );
        assert_eq!(json["changes"][0]["draft"]["text"], "unfinished");
    }

    /// The daemon publishes its identity on the readiness line, so these field
    /// names are part of what a client pins. They are camelCase on the wire and
    /// the two keys are plain base64 strings.
    #[test]
    fn a_relayed_daemon_publishes_its_pairing_code_in_the_readiness_line() {
        // The readiness reader stops after one line, so the code has to travel
        // here. A daemon that printed it separately would have that line read
        // by nobody and the UI would show nothing while looking implemented.
        let ready = DaemonReady {
            address: "127.0.0.1:4570".into(),
            protocol_version: PROTOCOL_VERSION,
            pid: 7,
            x25519_public: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into(),
            ed25519_public: "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=".into(),
            pairing: Some(DaemonPairing {
                code: "waku://relay.test:443/abc123#d258dc0e8595ed66".into(),
                relay: "relay.test:443".into(),
                session_id: "abc123".into(),
            }),
        };
        let json = serde_json::to_value(&ready).unwrap();

        assert_eq!(
            json["pairing"]["code"],
            "waku://relay.test:443/abc123#d258dc0e8595ed66"
        );
        assert_eq!(json["pairing"]["relay"], "relay.test:443");
        assert_eq!(json["pairing"]["sessionId"], "abc123");

        // And it must survive a round trip: the desktop reads this line from a
        // child process's stdout, not from a value it built itself.
        let parsed: DaemonReady = serde_json::from_value(json).unwrap();
        let pairing = parsed
            .pairing
            .expect("a relayed daemon must report pairing");
        assert_eq!(
            waku_protocol_pairing_round_trip(&pairing.code),
            ("relay.test".to_owned() + ":443", "abc123".to_owned())
        );
    }

    /// Parse a code and return its relay and session, so this test checks the
    /// published code is one a client could actually use.
    fn waku_protocol_pairing_round_trip(code: &str) -> (String, String) {
        let parsed =
            crate::pairing::PairingCode::parse(code).expect("the published code must parse");
        (parsed.relay, parsed.session_id)
    }

    #[test]
    fn the_readiness_line_carries_the_daemon_public_keys() {
        let ready = DaemonReady {
            address: "127.0.0.1:4567".into(),
            protocol_version: PROTOCOL_VERSION,
            pid: 4242,
            x25519_public: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into(),
            ed25519_public: "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=".into(),
            // Absent, which is what a daemon that only listens sends.
            pairing: None,
        };
        let json = serde_json::to_value(&ready).unwrap();

        // A daemon with no relay must not carry the key at all. An explicit
        // `null` would make "no relay" and "a relay whose code is missing"
        // different shapes for the client to tell apart, and only one of them
        // is worth handling.
        assert!(
            json.get("pairing").is_none(),
            "a listening-only daemon must omit pairing entirely"
        );

        assert_eq!(json["address"], "127.0.0.1:4567");
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["pid"], 4242);
        assert_eq!(
            json["x25519Public"],
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="
        );
        assert_eq!(
            json["ed25519Public"],
            "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8="
        );
        // No snake_case escape: the wire shape is the camelCase one only.
        assert!(json.get("x25519_public").is_none());
        assert!(json.get("ed25519_public").is_none());

        // Both fields are required of a daemon that speaks this build: a
        // readiness line without them is rejected here, not silently accepted
        // with empty keys.
        let incomplete = serde_json::json!({
            "address": "127.0.0.1:4567",
            "protocolVersion": PROTOCOL_VERSION,
            "pid": 4242,
        });
        assert!(
            serde_json::from_value::<DaemonReady>(incomplete).is_err(),
            "a readiness line without the keys is not a complete DaemonReady"
        );

        // The reverse direction — a newer daemon read by an older client — is
        // what the added fields have to survive, and it does: `DaemonReady` has
        // no `deny_unknown_fields`, so the extra keys are ignored. That is the
        // whole compatibility argument for not bumping `PROTOCOL_VERSION`.
        let with_extra = serde_json::json!({
            "address": "127.0.0.1:4567",
            "protocolVersion": PROTOCOL_VERSION,
            "pid": 4242,
            "x25519Public": "AA==",
            "ed25519Public": "AA==",
            "futureField": 1,
        });
        assert!(serde_json::from_value::<DaemonReady>(with_extra).is_ok());
    }

    /// A relayed delegate event survives the wire with its subject intact.
    ///
    /// The envelope has to name the delegate *and* carry the delegate's own
    /// event unchanged: the lead's card is keyed by that id, and dropping it
    /// would leave the text with no card to land in. This is the contract the
    /// daemon writes and the desktop reads, so it is pinned on both sides of the
    /// JSON.
    #[test]
    fn a_relayed_delegate_event_names_its_delegate_and_keeps_its_event() {
        let delegate = Uuid::new_v4();
        let envelope = WireDriverEvent::new(
            DELEGATE_EVENT_KIND,
            serde_json::to_value(DelegateEventPayload {
                session_id: delegate,
                event: WireDriverEvent::new("textDelta", serde_json::json!("half an answer")),
            })
            .unwrap(),
        );

        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["kind"], DELEGATE_EVENT_KIND);

        let decoded: DelegateEventPayload = serde_json::from_value(envelope.payload).unwrap();
        assert_eq!(decoded.session_id, delegate);
        assert_eq!(decoded.event.kind, "textDelta");
        assert_eq!(decoded.event.payload, serde_json::json!("half an answer"));
    }
}
