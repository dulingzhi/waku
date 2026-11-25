//! Provider backend and driver-event wire translation for `waku-daemon`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::{
    Backend, Command, EventSink, Request, ResponsePayload, WireDriverEvent, WorkspaceOperation,
    WorkspaceResult,
};
use anyhow::{Context as _, anyhow, bail};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::attachments::AttachmentStore;
use crate::computer_use::{ComputerTarget, ComputerUsePhase, ComputerUseState};
use crate::driver::{self, DriverHandle, DriverStartOptions, SessionOptions};
use crate::model::{
    ActivityKind, AgentSession, Checkpoint, CheckpointStatus, DriverEvent, PermissionOption,
    Project, ProviderKind, ProviderResumeCursor, SessionStatus,
};
use crate::persistence::{ComposerDraftStore, PersistedState, StateStore};
use crate::settings::DaemonSettingsStore;
use waku_protocol::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use waku_protocol::{DELEGATE_EVENT_KIND, DelegateEventPayload};

/// How many fully hydrated transcripts the daemon keeps resident.
///
/// Hydration is a cache: consumers reload a released session from the store on
/// demand. Without a cap, a daemon that lives for days adopts the transcript
/// of every session its clients have touched — SaveTaskState pushes, hydrate
/// requests, forks, checkpoints — and resident memory grows without bound.
const RESIDENT_TRANSCRIPT_WINDOW: usize = 24;

/// Releases resident transcripts beyond the recency window after a save.
/// `pinned` names sessions with live runtimes; dirty sessions are skipped
/// inside [`PersistedState::trim_idle_transcripts`] because they hold unsaved
/// work.
fn trim_resident_transcripts(state: &mut PersistedState, pinned: &HashSet<Uuid>) {
    state.trim_idle_transcripts(pinned, RESIDENT_TRANSCRIPT_WINDOW);
}

/// One capture lock per (workspace, session, turn), so the same turn is never
/// captured twice at once.
type CheckpointCaptureLocks = Mutex<HashMap<(PathBuf, Uuid, usize), Arc<Mutex<()>>>>;

/// Mints the scoped capability a delegating provider is launched with.
///
/// The token table belongs to the daemon server, so the backend takes a
/// callback rather than owning the table: one place mints and validates, and a
/// backend constructed without a callback simply never delegates.
pub type AgentBridgeMinter =
    Arc<dyn Fn(Uuid, Uuid) -> String + Send + Sync>;

/// What the daemon remembers about one delegate it started.
///
/// A delegate has no guaranteed client: it is an ordinary task, but the lead
/// runs inside a provider process and may be the only thing watching it. The
/// provider's own event stream is therefore the only place a delegate's reply
/// is guaranteed to pass through, and this is what keeps it: the forwarding
/// thread folds the stream in here as it arrives.
#[derive(Clone)]
struct DelegateRecord {
    /// The conversation that started this delegate. Leads may read and steer
    /// only their own delegates, so this is the authority check — not the
    /// caller's word for which task it is allowed to address.
    lead: Uuid,
    /// The runtime the delegate was started under, so a prompt reaches the
    /// process the lead is actually reading rather than a restarted one.
    runtime_id: Uuid,
    /// The delegate's own event stream, so a turn the daemon opens on the
    /// lead's behalf is announced the way a submitted prompt always is. Without
    /// it a client watching the task would show the answer with no question
    /// above it.
    sink: EventSink,
    status: SessionStatus,
    /// Everything the delegate has said in its current turn. Cleared when a
    /// turn opens, so a steer does not return the previous answer alongside the
    /// new one.
    reply: String,
    /// Why the last turn failed. Never set on a successful turn: a delegate
    /// that worked must not carry a stale complaint into its next report.
    failure: Option<String>,
}

pub struct WakuBackend {
    sessions: Mutex<HashMap<Uuid, (Uuid, DriverHandle)>>,
    /// The delegates this daemon started, by their own session id. Kept in
    /// memory on purpose: a delegate is only addressable while its runtime is
    /// alive, which is the same lifetime as the scoped token that can ask about
    /// it — a daemon restart ends both.
    delegates: Arc<Mutex<HashMap<Uuid, DelegateRecord>>>,
    /// Mints the scoped capability a delegate-driving provider is launched
    /// with. Installed by the daemon once its server exists, because the token
    /// table lives there; a backend without one simply never delegates.
    agent_bridge_minter: Mutex<Option<AgentBridgeMinter>>,
    terminals: Mutex<HashMap<Uuid, (Uuid, crate::terminal::DaemonTerminal)>>,
    #[cfg(all(test, unix))]
    terminal_shell: Option<alacritty_terminal::tty::Shell>,
    settings: DaemonSettingsStore,
    task_store: StateStore,
    task_state: Mutex<PersistedState>,
    removed_session_ids: Mutex<HashSet<Uuid>>,
    composer_drafts: ComposerDraftStore,
    attachments: AttachmentStore,
    usage_scan_cache: Mutex<crate::usage_history::ScanCache>,
    checkpoint_capture_locks: CheckpointCaptureLocks,
    usage_rates_dir: std::path::PathBuf,
    default_cwd: std::path::PathBuf,
}

impl WakuBackend {
    pub fn new(settings: DaemonSettingsStore, task_store: StateStore) -> anyhow::Result<Self> {
        let mut task_state = task_store
            .load()
            .context("could not load Waku task database")?;
        migrate_projectless_state(&task_store, &mut task_state)?;
        let composer_drafts = ComposerDraftStore::for_state_path(task_store.path());
        let attachments = AttachmentStore::new(
            task_store
                .path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("attachments"),
        );
        let usage_rates_dir = task_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            delegates: Arc::new(Mutex::new(HashMap::new())),
            terminals: Mutex::new(HashMap::new()),
            #[cfg(all(test, unix))]
            terminal_shell: None,
            settings,
            task_store,
            task_state: Mutex::new(task_state),
            removed_session_ids: Mutex::new(HashSet::new()),
            composer_drafts,
            attachments,
            usage_scan_cache: Mutex::new(HashMap::new()),
            checkpoint_capture_locks: Mutex::new(HashMap::new()),
            agent_bridge_minter: Mutex::new(None),
            usage_rates_dir,
            default_cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_terminal_shell(mut self, shell: alacritty_terminal::tty::Shell) -> Self {
        self.terminal_shell = Some(shell);
        self
    }

    /// Installs the minter the daemon server provides.
    ///
    /// Done after construction because the token table lives on the server,
    /// which needs the backend first. Until this runs nothing delegates, which
    /// is the safe default: a backend built in a test has no daemon to reach.
    pub fn install_agent_bridge_minter(&self, minter: AgentBridgeMinter) {
        *self.agent_bridge_minter.lock() = Some(minter);
    }

    /// The capability to launch a delegating provider with, or `None` when this
    /// backend has no daemon to mint one.
    ///
    /// The runtime id is the one this `Start` request carries, because that is
    /// what the daemon registers the session under and what a scoped token is
    /// checked against. Minting with anything else would hand out a capability
    /// that its own session then refuses.
    fn agent_bridge_access(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
    ) -> Option<crate::driver::AgentBridgeAccess> {
        let minter = self.agent_bridge_minter.lock().clone()?;
        Some(crate::driver::AgentBridgeAccess {
            address: crate::server::daemon_address()?,
            token: minter(session_id, runtime_id),
            session_id,
            runtime_id,
        })
    }

    /// Starts one delegate session for a lead conversation.
    ///
    /// The new task copies the lead's provider, workspace, model, and runtime
    /// mode, so a delegate is an ordinary session the user can open and read
    /// rather than a private channel only the lead can see. It is started with
    /// no `agent_bridge`, which is what stops a delegate from delegating again
    /// and fanning out without bound.
    fn spawn_delegate(
        &self,
        lead_session: Uuid,
        task: String,
        title: Option<String>,
        persona: Option<String>,
        events: EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let (lead, project_cwd) = {
            let state = self.task_state.lock();
            let index = state
                .sessions
                .iter()
                .position(|session| session.id == lead_session)
                .ok_or_else(|| anyhow!("the lead conversation is unavailable"))?;
            let project_id = state.sessions[index].project_id;
            let project_cwd = state
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .map(|project| project.path.clone());
            (state.sessions[index].clone(), project_cwd)
        };
        let cwd = lead
            .workspace
            .path()
            .or(project_cwd.as_deref())
            .ok_or_else(|| anyhow!("the lead conversation has no workspace"))?
            .to_owned();
        ensure_shell_environment();
        let binary_override = self
            .settings
            .get()
            .provider_binary_overrides
            .get(&lead.provider)
            .cloned();
        let binary = crate::model::provider_probe(lead.provider, binary_override.as_deref())
            .path
            .ok_or_else(|| {
                anyhow!(
                    "{} is not installed",
                    lead.provider.display_name()
                )
            })?;

        let mut delegate = delegated_session(&lead, title);
        let session_id = delegate.id;
        let runtime_id = Uuid::new_v4();

        // The identity is resolved here rather than by the caller, so a spawn
        // can only run as an identity the catalog actually defines. A named
        // identity that no longer exists is an error rather than a silent
        // fallback: a reviewer that quietly started as a plain assistant would
        // edit files it was told not to.
        let identity = match persona.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => {
                let catalog = crate::agents::load_catalog(Some(&cwd));
                Some(catalog.find(id).cloned().ok_or_else(|| {
                    anyhow!("no agent identity named {id} is defined for this project")
                })?)
            }
            None => None,
        };
        if let Some(identity) = identity.as_ref() {
            // The identity's own overrides win over what the lead runs with,
            // which is what lets a cheap reviewer sit beside an expensive
            // implementer.
            if identity.model.is_some() {
                delegate.model = identity.model.clone();
            }
            if identity.effort.is_some() {
                delegate.reasoning_effort = identity.effort.clone();
            }
            delegate.agent_identity = Some(identity.id.clone());
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, event_receiver) = driver::event_channel(wake);
        let options = DriverStartOptions {
            binary,
            cwd,
            mode: delegate.runtime_mode,
            model: delegate.model.clone(),
            reasoning_effort: delegate.reasoning_effort.clone(),
            service_tier: delegate.service_tier.clone(),
            context_window: delegate.context_window.clone(),
            agent_preset: delegate.agent_preset.clone(),
            computer_use_enabled: crate::computer_use::resolve_enabled(false),
            provider_cursor: None,
            // A delegate does not delegate: this is what bounds the fan-out.
            agent_bridge: None,
            system_prompt: identity.as_ref().map(|identity| identity.system_prompt()),
        };
        let handle = driver::start_local(lead.provider, options, event_sender)?;

        let delegate_sink = events.begin_runtime(session_id, runtime_id);
        self.delegates.lock().insert(
            session_id,
            DelegateRecord {
                lead: lead_session,
                runtime_id,
                sink: delegate_sink.clone(),
                status: SessionStatus::Working,
                reply: String::new(),
                failure: None,
            },
        );
        // The map is shared rather than borrowed: the lead asks about this
        // delegate later, from another process, long after `spawn_delegate`
        // returned — and a thread holding `&self` would tie the record's
        // lifetime to a backend that may be dropped first.
        let delegates = self.delegates.clone();
        // The lead's own stream, so a conversation watching the group sees each
        // delegate's reply as it is written. A delegate runs in its own process
        // and publishes only on its own runtime; without this relay the lead's
        // cards could not move until the delegate's turn settled and its
        // transcript was read back.
        let lead_sink = events.clone();
        std::thread::Builder::new()
            .name(format!("waku-daemon-events-{session_id}"))
            .spawn(move || {
                while let Ok(event) = event_receiver.recv() {
                    record_delegate_event(&delegates, session_id, &event);
                    let relayed =
                        delegate_relay_event(&event).and_then(|event| event_to_wire(event).ok());
                    let wire = event_to_wire(event).unwrap_or_else(|error| {
                        WireDriverEvent::new(
                            "error",
                            Value::String(format!("could not encode daemon event: {error}")),
                        )
                    });
                    if delegate_sink.send(wire).is_err() {
                        break;
                    }
                    // Best effort: a lead that closed its window must not stop
                    // the delegate it started.
                    if let Some(inner) = relayed {
                        let payload = serde_json::to_value(DelegateEventPayload {
                            session_id,
                            event: inner,
                        });
                        if let Ok(payload) = payload {
                            let _ =
                                lead_sink.send(WireDriverEvent::new(DELEGATE_EVENT_KIND, payload));
                        }
                    }
                }
            })
            .context("could not start delegate event forwarding thread")?;

        // A task is only stored once it owns a transcript step, so the turn the
        // lead asked for is opened here rather than left for the provider to
        // open later. Without this the delegate would exist only in this
        // process's memory: the user would never see the task the lead started,
        // and a daemon restart would lose it.
        let turn_id = delegate.begin_turn(task.clone());
        let message_id = delegate
            .messages
            .last()
            .map(|message| message.id)
            .unwrap_or_else(Uuid::new_v4);
        self.sessions
            .lock()
            .insert(session_id, (runtime_id, handle.clone()));
        {
            let mut state = self.task_state.lock();
            state.push_session(delegate);
            if let Err(error) = self.task_store.save(&mut state) {
                state.sessions.retain(|session| session.id != session_id);
                drop(state);
                self.sessions.lock().remove(&session_id);
                self.delegates.lock().remove(&session_id);
                return Err(error).context("could not save the delegated task");
            }
        }
        // The ids are the ones the transcript already carries, so the lead's
        // prompt and the provider's own report describe the same turn.
        handle.prompt_with_ids(task, Some(turn_id), Some(message_id));
        Ok(ResponsePayload::DelegateSpawned {
            session_id,
            runtime_id,
        })
    }

    /// Starts one delegate per member of a saved team.
    ///
    /// Each member is an ordinary spawn, so a team is exactly the group its
    /// members are and nothing is shared between them beyond the brief: no
    /// process, no runtime, no transcript. A member that cannot start is
    /// reported in its own slot rather than failing the call — one identity the
    /// user deleted must not cost them the other four agents.
    ///
    /// The team's brief is prepended to every member's task. It is the context
    /// they all need and none of them can see, since a delegate has no view of
    /// the conversation that started it.
    fn spawn_team(
        &self,
        lead_session: Uuid,
        team_id: &str,
        task: &str,
        title: Option<String>,
        events: EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let cwd = {
            let state = self.task_state.lock();
            let lead = state
                .sessions
                .iter()
                .find(|session| session.id == lead_session)
                .ok_or_else(|| anyhow!("the lead conversation is unavailable"))?;
            lead.workspace
                .path()
                .map(Path::to_path_buf)
                .or_else(|| {
                    state
                        .projects
                        .iter()
                        .find(|project| project.id == lead.project_id)
                        .map(|project| project.path.clone())
                })
                .ok_or_else(|| anyhow!("the lead conversation has no workspace"))?
        };
        let catalog = crate::agents::load_catalog(Some(&cwd));
        let team = catalog
            .find_team(team_id)
            .cloned()
            .ok_or_else(|| anyhow!("no agent team named {team_id} is defined for this project"))?;
        if team.members.is_empty() {
            bail!("team {team_id} has no members to start");
        }
        let brief = team.brief.trim();
        let member_task = if brief.is_empty() {
            task.to_owned()
        } else {
            format!("{brief}\n\n{task}")
        };
        let mut spawned = Vec::with_capacity(team.members.len());
        for member in &team.members {
            let member_title = Some(match title.as_deref() {
                Some(title) if !title.trim().is_empty() => format!("{title} · {member}"),
                _ => member.clone(),
            });
            // Each member gets its own sink: `begin_runtime` registers per
            // runtime, so sharing one would route every member's events to
            // whichever delegate registered last.
            let result = self.spawn_delegate(
                lead_session,
                member_task.clone(),
                member_title,
                Some(member.clone()),
                events.clone(),
            );
            match result {
                Ok(ResponsePayload::DelegateSpawned {
                    session_id,
                    runtime_id,
                }) => spawned.push(waku_protocol::SpawnedDelegate {
                    session_id,
                    runtime_id,
                    persona: member.clone(),
                    error: None,
                }),
                Ok(_) => spawned.push(waku_protocol::SpawnedDelegate {
                    session_id: Uuid::nil(),
                    runtime_id: Uuid::nil(),
                    persona: member.clone(),
                    error: Some("the spawn returned an unexpected reply".into()),
                }),
                Err(error) => spawned.push(waku_protocol::SpawnedDelegate {
                    session_id: Uuid::nil(),
                    runtime_id: Uuid::nil(),
                    persona: member.clone(),
                    error: Some(format!("{error:#}")),
                }),
            }
        }
        Ok(ResponsePayload::DelegateTeamSpawned {
            delegates: spawned,
        })
    }

    /// One delegate's live state, checked against the lead that asks.
    ///
    /// The two ids come from different places on purpose: the lead is the one
    /// the caller's token is scoped to, and the delegate is the one the command
    /// names. Only the daemon knows which leads own which delegates, so this is
    /// where that question is answered — a bridge cannot widen its reach by
    /// guessing a session id.
    fn delegate_report(&self, lead: Uuid, delegate: Uuid) -> anyhow::Result<ResponsePayload> {
        let (status, reply, failure) = {
            let delegates = self.delegates.lock();
            let record = delegates
                .get(&delegate)
                .ok_or_else(|| anyhow!("agent {delegate} was not started by this conversation"))?;
            if record.lead != lead {
                bail!("agent {delegate} was not started by this conversation");
            }
            (record.status, record.reply.clone(), record.failure.clone())
        };
        let title = self
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == delegate)
            .map(|session| session.title.clone())
            .unwrap_or_default();
        Ok(ResponsePayload::DelegateReport {
            session_id: delegate,
            title,
            status,
            reply,
            failure,
        })
    }

    /// Adds an instruction to a delegate, letting the daemon decide how.
    ///
    /// A delegate that is still working gets a steer, so a correction lands in
    /// the turn it was meant for; an idle one needs a new turn, because a steer
    /// into a finished turn is refused by every provider. Only the daemon sees
    /// the delegate's live state, so this choice cannot be made by the caller.
    fn prompt_delegate(
        &self,
        lead: Uuid,
        delegate: Uuid,
        prompt: String,
    ) -> anyhow::Result<ResponsePayload> {
        let (runtime_id, running, sink) = {
            let delegates = self.delegates.lock();
            let record = delegates
                .get(&delegate)
                .ok_or_else(|| anyhow!("agent {delegate} was not started by this conversation"))?;
            if record.lead != lead {
                bail!("agent {delegate} was not started by this conversation");
            }
            (
                record.runtime_id,
                record.status.is_busy(),
                record.sink.clone(),
            )
        };
        let driver = {
            let sessions = self.sessions.lock();
            sessions
                .get(&delegate)
                .filter(|(active_runtime_id, _)| *active_runtime_id == runtime_id)
                .map(|(_, driver)| driver.clone())
                .ok_or_else(|| anyhow!("agent {delegate} is no longer running"))?
        };
        if running {
            driver.steer(prompt);
        } else {
            // A turn the daemon opens is announced the way a client's own
            // submission is: every attached viewer mirrors the question from
            // this event, so the transcript they save describes the turn the
            // provider is about to run.
            sink.send(event_to_wire(DriverEvent::PromptSubmitted {
                message: prompt.clone(),
                turn_id: Uuid::new_v4(),
                message_id: Uuid::new_v4(),
            })?)?;
            driver.prompt(prompt);
            // The record turns over here rather than waiting for the provider's
            // own `TurnStarted`, so a report asked for immediately after this
            // call does not answer with the previous turn's text.
            if let Some(record) = self.delegates.lock().get_mut(&delegate) {
                record.status = SessionStatus::Working;
                record.reply.clear();
                record.failure = None;
            }
        }
        Ok(ResponsePayload::DelegatePrompted {
            session_id: delegate,
            steered: running,
        })
    }

    fn open_terminal(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        events: EventSink,
    ) -> anyhow::Result<crate::terminal::DaemonTerminal> {
        #[cfg(all(test, unix))]
        if let Some(shell) = &self.terminal_shell {
            return crate::terminal::DaemonTerminal::open_with_shell(
                cwd,
                cols,
                rows,
                events,
                shell.clone(),
            );
        }
        ensure_shell_environment();
        crate::terminal::DaemonTerminal::open(cwd, cols, rows, events)
    }

    /// Capture and persist one ending checkpoint exactly once per daemon.
    /// Desktop and Web may observe the same turn completion concurrently; a
    /// per-turn lock prevents both clients from running the expensive Git
    /// snapshot while leaving unrelated tasks independent.
    fn capture_turn_checkpoint(
        &self,
        cwd: PathBuf,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<Checkpoint> {
        let key = (cwd.clone(), session_id, turn_count);
        let capture_lock = self
            .checkpoint_capture_locks
            .lock()
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _capture = capture_lock.lock();

        {
            let mut state = self.task_state.lock();
            if let Some(index) = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            {
                self.task_store.hydrate(&mut state.sessions[index])?;
                if let Some(checkpoint) = state.sessions[index]
                    .turns
                    .iter()
                    .find(|turn| turn.turn_count == turn_count)
                    .and_then(|turn| turn.checkpoint.as_ref())
                    .filter(|checkpoint| {
                        matches!(
                            checkpoint.status,
                            CheckpointStatus::Ready | CheckpointStatus::Unavailable
                        )
                    })
                {
                    return Ok(checkpoint.clone());
                }
            }
        }

        let checkpoint = crate::checkpoint::capture_turn(&cwd, session_id, turn_count)?;
        let mut state = self.task_state.lock();
        if let Some(index) = state
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        {
            self.task_store.hydrate(&mut state.sessions[index])?;
            if let Some(turn) = state.sessions[index]
                .turns
                .iter_mut()
                .find(|turn| turn.turn_count == turn_count)
            {
                turn.checkpoint = Some(checkpoint.clone());
                state.mark_session_dirty(session_id);
                self.task_store.save(&mut state)?;
            }
        }
        Ok(checkpoint)
    }
}

/// Storage-layout migrations belong to the daemon because both the database
/// rows and the directories name paths on its host. Persist after each move
/// so a later failure cannot leave an earlier project pointing at its old
/// location in SQLite.
fn migrate_projectless_state(
    task_store: &StateStore,
    task_state: &mut PersistedState,
) -> anyhow::Result<()> {
    let indices = task_state
        .projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            crate::projectless::needs_migration(&project.path).then_some(index)
        })
        .collect::<Vec<_>>();
    for index in indices {
        let old_path = task_state.projects[index].path.clone();
        let workspace = crate::projectless::migrate_workspace(&old_path).with_context(|| {
            format!(
                "could not move projectless workspace {} under ~/.waku/projects",
                old_path.display()
            )
        })?;
        task_state.projects[index].name = crate::model::Project::PROJECTLESS_NAME.to_owned();
        task_state.projects[index].path = workspace.cwd;
        task_store
            .save(task_state)
            .context("could not persist migrated projectless workspace")?;
    }
    Ok(())
}

impl Backend for WakuBackend {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
        let session_id = request.session_id;
        let runtime_id = request.runtime_id;
        match request.command {
            Command::AttachSession => {
                let sessions = self.sessions.lock();
                let Some((runtime_id, driver)) = sessions.get(&session_id) else {
                    return Ok(ResponsePayload::SessionRuntime {
                        runtime_id: None,
                        supports_steer: false,
                    });
                };
                Ok(ResponsePayload::SessionRuntime {
                    runtime_id: Some(*runtime_id),
                    supports_steer: driver.supports_steer(),
                })
            }
            Command::GetSettings => Ok(ResponsePayload::Settings {
                settings: self.settings.get(),
            }),
            Command::UpdateSettings { settings } => {
                self.settings.replace(settings)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ProbeProvider {
                provider,
                binary_override,
                discover_models,
                probe_version,
            } => {
                ensure_shell_environment();
                let mut probe = match binary_override.as_deref() {
                    override_value if discover_models || probe_version => {
                        crate::model::provider_probe(provider, override_value)
                    }
                    override_value => crate::model::cached_provider_probe(provider, override_value),
                };
                let version = probe_version
                    .then(|| {
                        probe
                            .path
                            .as_deref()
                            .and_then(crate::model::probe_provider_version)
                    })
                    .flatten();
                if discover_models {
                    probe = crate::model::discover_provider_models(probe);
                }
                Ok(ResponsePayload::ProviderProbe { probe, version })
            }
            Command::FetchPlanUsage {
                provider,
                binary_override,
                cli_version,
            } => {
                let usage = match provider {
                    crate::model::ProviderKind::Claude => Some(
                        crate::usage::fetch_claude_plan_usage(cli_version.as_deref())?,
                    ),
                    crate::model::ProviderKind::Codex => {
                        Some(crate::usage::fetch_codex_plan_usage()?)
                    }
                    crate::model::ProviderKind::OpenCode => {
                        crate::usage::fetch_opencode_go_plan_usage()?
                    }
                    crate::model::ProviderKind::Grok => {
                        ensure_shell_environment();
                        let probe =
                            crate::model::provider_probe(provider, binary_override.as_deref());
                        let binary = probe.path.ok_or_else(|| anyhow!("grok is not installed"))?;
                        Some(crate::usage::fetch_grok_plan_usage(&binary)?)
                    }
                    _ => bail!("provider has no plan usage fetcher"),
                };
                Ok(ResponsePayload::PlanUsage { usage })
            }
            Command::ProbeComputerPermissions { prompt } => {
                Ok(ResponsePayload::ComputerPermissions {
                    permissions: crate::computer_use::probe_permissions(prompt)?,
                })
            }
            Command::LoadUsageHistory {
                window,
                project_roots,
            } => {
                let rates = crate::usage_history::load_rate_table(&self.usage_rates_dir);
                let history = crate::usage_history::scan(
                    &mut self.usage_scan_cache.lock(),
                    &rates,
                    window,
                    &project_roots,
                );
                Ok(ResponsePayload::UsageHistory { history })
            }
            Command::LoadSkills { projects } => {
                let locations = crate::skills::skill_locations(&projects);
                Ok(ResponsePayload::SkillsCatalog {
                    catalog: crate::skills::scan_skills(&locations),
                })
            }
            Command::SetSkillsEnabled { dirs, enabled } => {
                for dir in dirs {
                    crate::skills::set_skill_enabled(&dir, enabled)
                        .map_err(|error| anyhow!(error))?;
                }
                Ok(ResponsePayload::Ack)
            }
            Command::TrashSkills { dirs } => {
                crate::skills::trash_skills(&dirs).map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::LoadAgents { project } => Ok(ResponsePayload::AgentsCatalog {
                catalog: crate::agents::load_catalog(project.as_deref()),
            }),
            Command::SaveAgentDefinition { definition, project } => {
                crate::agents::save_definition(project.as_deref(), &definition)
                    .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::AgentsCatalog {
                    catalog: crate::agents::load_catalog(project.as_deref()),
                })
            }
            Command::DeleteAgentDefinition { scope, id, project } => {
                crate::agents::delete_definition(project.as_deref(), scope, &id)
                    .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::AgentsCatalog {
                    catalog: crate::agents::load_catalog(project.as_deref()),
                })
            }
            Command::SaveAgentTeam { team, project } => {
                crate::agents::save_team(project.as_deref(), &team)
                    .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::AgentsCatalog {
                    catalog: crate::agents::load_catalog(project.as_deref()),
                })
            }
            Command::DeleteAgentTeam { scope, id, project } => {
                crate::agents::delete_team(project.as_deref(), scope, &id)
                    .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::AgentsCatalog {
                    catalog: crate::agents::load_catalog(project.as_deref()),
                })
            }
            Command::LoadClaudeMcpServers { cwd } => {
                let override_spec = self.claude_binary_override();
                Ok(ResponsePayload::ClaudeMcpCatalog {
                    catalog: crate::claude_config::list_mcp_servers(
                        override_spec.as_deref(),
                        cwd.as_deref(),
                    ),
                })
            }
            Command::RemoveClaudeMcpServer { name, scope, cwd } => {
                let override_spec = self.claude_binary_override();
                crate::claude_config::remove_mcp_server(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &name,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::LoadClaudePlugins => {
                let override_spec = self.claude_binary_override();
                Ok(ResponsePayload::ClaudePluginCatalog {
                    catalog: crate::claude_config::list_plugins(override_spec.as_deref(), None),
                })
            }
            Command::SetClaudePluginEnabled { id, enabled, cwd } => {
                let override_spec = self.claude_binary_override();
                crate::claude_config::set_plugin_enabled(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &id,
                    enabled,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::UninstallClaudePlugin { id, scope, cwd } => {
                let override_spec = self.claude_binary_override();
                crate::claude_config::uninstall_plugin(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &id,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::LoadClaudeMarketplaces => {
                let override_spec = self.claude_binary_override();
                Ok(ResponsePayload::ClaudeMarketplaceCatalog {
                    catalog: crate::claude_config::list_marketplaces(override_spec.as_deref()),
                })
            }
            Command::LoadAvailableClaudePlugins => {
                let override_spec = self.claude_binary_override();
                // The installed list is the reference for "already installed":
                // passing it in keeps the flag on the row consistent with what
                // the Plugins tab shows, rather than trusting the available
                // list's own opinion of itself.
                let installed = crate::claude_config::list_plugins(override_spec.as_deref(), None);
                Ok(ResponsePayload::ClaudeAvailablePluginCatalog {
                    catalog: crate::claude_config::list_available_plugins(
                        override_spec.as_deref(),
                        &installed.plugins,
                    ),
                })
            }
            Command::LoadClaudePluginDetails { id, install_path } => {
                Ok(ResponsePayload::ClaudePluginDetails {
                    details: crate::claude_config::plugin_details(&id, install_path.as_deref()),
                })
            }
            Command::InstallClaudePlugin { id, scope, cwd } => {
                let override_spec = self.claude_binary_override();
                crate::claude_config::install_plugin(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &id,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::UpdateClaudePlugin { id, scope, cwd } => {
                let override_spec = self.claude_binary_override();
                crate::claude_config::update_plugin(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &id,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::AddClaudeMarketplace { source, scope, cwd } => {
                let override_spec = self.claude_binary_override();
                let result = crate::claude_config::add_marketplace(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &source,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::ClaudeAction { result })
            }
            Command::RemoveClaudeMarketplace { name, scope } => {
                let override_spec = self.claude_binary_override();
                let result = crate::claude_config::remove_marketplace(
                    override_spec.as_deref(),
                    &name,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::ClaudeAction { result })
            }
            Command::UpdateClaudeMarketplace { name } => {
                let override_spec = self.claude_binary_override();
                let result = crate::claude_config::update_marketplace(
                    override_spec.as_deref(),
                    name.as_deref(),
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::ClaudeAction { result })
            }
            Command::LoadClaudePluginConfig { id, cwd } => {
                let override_spec = self.claude_binary_override();
                Ok(ResponsePayload::ClaudePluginConfigCatalog {
                    catalog: crate::claude_config::plugin_config(
                        override_spec.as_deref(),
                        cwd.as_deref(),
                        &id,
                    ),
                })
            }
            Command::SetClaudePluginConfig { id, values, cwd } => {
                let override_spec = self.claude_binary_override();
                let pairs: Vec<(String, String)> = values
                    .iter()
                    .filter_map(|pair| {
                        let (key, value) = pair.split_once('=')?;
                        Some((key.to_owned(), value.to_owned()))
                    })
                    .collect();
                let result = crate::claude_config::set_plugin_config(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    &id,
                    &pairs,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::ClaudeAction { result })
            }
            Command::PruneClaudePlugins {
                dry_run,
                scope,
                cwd,
            } => {
                let override_spec = self.claude_binary_override();
                let result = crate::claude_config::prune_plugins(
                    override_spec.as_deref(),
                    cwd.as_deref(),
                    dry_run,
                    scope,
                )
                .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::ClaudeAction { result })
            }
            Command::LoadTaskState => {
                let state = self.task_state.lock();
                Ok(ResponsePayload::TaskState {
                    projects: state.projects.clone(),
                    sessions: state
                        .sessions
                        .iter()
                        .map(AgentSession::list_projection)
                        .collect(),
                    default_cwd: self.default_cwd.clone(),
                    projectless_root: crate::projectless::workspace_root(),
                })
            }
            Command::SaveTaskState {
                projects,
                live_session_ids: _,
                sessions,
            } => {
                let active_runtimes = self
                    .sessions
                    .lock()
                    .iter()
                    .map(|(session_id, (runtime_id, _))| (*session_id, *runtime_id))
                    .collect::<HashMap<_, _>>();
                let mut state = self.task_state.lock();
                let removed_session_ids = self.removed_session_ids.lock();
                for project in projects {
                    if let Some(existing) = state
                        .projects
                        .iter_mut()
                        .find(|existing| existing.id == project.id)
                    {
                        *existing = project;
                    } else {
                        state.projects.push(project);
                    }
                }
                let sessions = sessions
                    .into_iter()
                    .filter(|session| !removed_session_ids.contains(&session.id))
                    .collect::<Vec<_>>();
                drop(removed_session_ids);
                let saved_ids = sessions
                    .iter()
                    .map(|session| session.id)
                    .collect::<Vec<_>>();
                for mut session in sessions {
                    if let Some(existing) = state
                        .sessions
                        .iter_mut()
                        .find(|existing| existing.id == session.id)
                    {
                        if session_projection_precedes(
                            existing,
                            &session,
                            active_runtimes.get(&session.id).copied(),
                        ) {
                            merge_stale_session_metadata(existing, session);
                        } else {
                            preserve_daemon_checkpoints(existing, &mut session);
                            *existing = session;
                        }
                    } else {
                        state.sessions.push(session);
                    }
                }
                let used_project_ids = state
                    .sessions
                    .iter()
                    .map(|session| session.project_id)
                    .collect::<std::collections::HashSet<_>>();
                state.projects.retain(|project| {
                    !project.is_projectless() || used_project_ids.contains(&project.id)
                });
                for session_id in &saved_ids {
                    state.mark_session_dirty(*session_id);
                }
                self.task_store.save(&mut state)?;
                let sessions = saved_ids
                    .into_iter()
                    .filter_map(|session_id| {
                        state
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .cloned()
                    })
                    .collect();
                // The save above can adopt full transcripts for every session
                // the client touched. Keep only the recent window resident;
                // the echoed clones above still carry the saved detail.
                trim_resident_transcripts(&mut state, &active_runtimes.keys().copied().collect());
                Ok(ResponsePayload::TaskStateSaved { sessions })
            }
            Command::RemoveSession => {
                {
                    let mut state = self.task_state.lock();
                    self.removed_session_ids.lock().insert(session_id);
                    let project_id = state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.project_id);
                    state.sessions.retain(|session| session.id != session_id);
                    if let Some(project_id) = project_id {
                        let remove_project = state
                            .projects
                            .iter()
                            .find(|project| project.id == project_id)
                            .is_some_and(Project::is_projectless)
                            && !state
                                .sessions
                                .iter()
                                .any(|session| session.project_id == project_id);
                        if remove_project {
                            state.projects.retain(|project| project.id != project_id);
                        }
                    }
                    self.task_store.save(&mut state)?;
                }
                let removed = self.sessions.lock().remove(&session_id);
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::HydrateSession { session_id } => {
                // Live runtimes stay resident; everything else is trimmed to
                // the recency window once the response is built.
                let pinned = self.sessions.lock().keys().copied().collect();
                let mut state = self.task_state.lock();
                let session = if let Some(session) = state
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    self.task_store.hydrate(session)?;
                    Some(session.clone())
                } else {
                    None
                };
                trim_resident_transcripts(&mut state, &pinned);
                Ok(ResponsePayload::Session { session })
            }
            Command::SearchSessionMessages { query, limit } => {
                let matches = self.task_store.session_message_search(query, limit)()?;
                Ok(ResponsePayload::SessionMessageMatches { matches })
            }
            Command::ListProviderSessions { provider, limit } => {
                const MAX_PROVIDER_SESSIONS: usize = 500;
                let limit = limit.min(MAX_PROVIDER_SESSIONS);
                if limit == 0 {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                }
                ensure_shell_environment();
                let settings = self.settings.get();
                if settings.disabled_providers.contains(&provider) {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                }
                let binary_override = settings
                    .provider_binary_overrides
                    .get(&provider)
                    .map(String::as_str);
                let Some(binary) = crate::model::provider_probe(provider, binary_override).path
                else {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                };
                // Discovery is deliberately provider-scoped. Opening Resume
                // must not start every installed agent CLI, and another
                // provider is queried only after the user explicitly picks it.
                let mut sessions = match provider {
                    ProviderKind::Amp => {
                        crate::amp_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Claude => crate::claude_session::list_provider_sessions(limit)?,
                    ProviderKind::Codex => {
                        crate::codex_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Cursor | ProviderKind::Fx => {
                        crate::acp_session::list_provider_sessions(provider, &binary, &[], limit)?
                    }
                    ProviderKind::OpenCode => {
                        crate::opencode_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::DeepSeek => {
                        crate::deepseek_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Grok => crate::grok_session::list_provider_sessions(limit)?,
                    ProviderKind::Kimi => crate::kimi_session::list_provider_sessions(limit)?,
                    ProviderKind::OhMyPi | ProviderKind::Pi => {
                        crate::pi_session::list_provider_sessions(provider, limit)?
                    }
                };
                sessions.sort_by(|a, b| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then_with(|| a.title.cmp(&b.title))
                });
                let imported = {
                    let state = self.task_state.lock();
                    state
                        .sessions
                        .iter()
                        .filter_map(|session| session.provider_cursor.as_ref())
                        .map(|cursor| (cursor.provider(), cursor.native_id().to_owned()))
                        .collect::<HashSet<_>>()
                };
                sessions.retain(|session| {
                    !imported.contains(&(session.provider(), session.cursor.native_id().to_owned()))
                });
                sessions.truncate(limit);
                Ok(ResponsePayload::ProviderSessions { sessions })
            }
            Command::LoadProviderSession { cursor, cwd } => {
                // Preserve every native turn shell for exact provider turn
                // numbering, but bound imported display text to recent turns.
                const VISIBLE_TURN_LIMIT: usize = 100;
                let history = match &cursor {
                    ProviderResumeCursor::Amp { thread_id, .. } => {
                        let binary = self.provider_binary(ProviderKind::Amp)?;
                        crate::amp_session::provider_session_history(
                            &binary,
                            &cwd,
                            thread_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Claude { session_id, .. } => {
                        self.provider_binary(ProviderKind::Claude)?;
                        crate::claude_session::provider_session_history(
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Codex { thread_id } => {
                        let binary = self.provider_binary(ProviderKind::Codex)?;
                        crate::codex_session::provider_session_history(
                            &binary,
                            thread_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    // OpenCode is not an ACP provider: its history comes from
                    // the adopted background service's own export route.
                    ProviderResumeCursor::OpenCode { session_id, .. } => {
                        let binary = self.provider_binary(ProviderKind::OpenCode)?;
                        crate::opencode_session::provider_session_history(
                            &binary,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Cursor { session_id, .. }
                    | ProviderResumeCursor::Fx { session_id }
                    | ProviderResumeCursor::Grok { session_id }
                    | ProviderResumeCursor::Kimi { session_id } => {
                        let provider = cursor.provider();
                        let binary = self.provider_binary(provider)?;
                        crate::acp_session::provider_session_history(
                            provider,
                            &binary,
                            &cwd,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::DeepSeek { session_id } => {
                        let binary = self.provider_binary(ProviderKind::DeepSeek)?;
                        crate::deepseek_session::provider_session_history(
                            &binary,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::OhMyPi {
                        session_id,
                        session_file,
                    }
                    | ProviderResumeCursor::Pi {
                        session_id,
                        session_file,
                    } => {
                        self.provider_binary(cursor.provider())?;
                        let session_file = session_file.as_deref().ok_or_else(|| {
                            anyhow!(
                                "{} did not report its native session file",
                                cursor.provider().display_name()
                            )
                        })?;
                        crate::pi_session::provider_session_history(
                            cursor.provider(),
                            session_id,
                            session_file,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                };
                Ok(ResponsePayload::ProviderSessionHistory { history })
            }
            Command::LoadComposerDrafts => Ok(ResponsePayload::ComposerDrafts {
                drafts: self.composer_drafts.load()?,
            }),
            Command::SaveComposerDrafts { drafts, generation } => {
                self.composer_drafts.save(drafts, generation)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ApplyComposerDraftChanges { changes } => {
                self.composer_drafts.apply_changes(changes)?;
                Ok(ResponsePayload::Ack)
            }
            Command::StoreBlob { mime_type, bytes } => {
                let reference = self
                    .task_store
                    .blobs()
                    .store_image_bytes(&mime_type, &bytes)?;
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("stored blob has no daemon path"))?;
                Ok(ResponsePayload::BlobStored { reference, path })
            }
            Command::ImportAttachment { name, upload } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import(&name, upload)?,
            }),
            Command::ImportPathAttachment { path } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import_path(&path)?,
            }),
            Command::ReadBlob { reference } => {
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("invalid blob reference"))?;
                Ok(ResponsePayload::BlobData {
                    bytes: std::fs::read(path)?,
                })
            }
            Command::ReadAttachment { reference, path } => Ok(ResponsePayload::BlobData {
                bytes: self.attachments.read_file(&reference, &path)?,
            }),
            Command::SweepBlobs => {
                self.task_store.blob_sweep()();
                Ok(ResponsePayload::Ack)
            }
            Command::ForkSessionFromResponse { turn_count } => {
                let (session, checkpoint_warning) =
                    self.fork_session_from_response(session_id, turn_count)?;
                Ok(ResponsePayload::SessionForked {
                    session,
                    checkpoint_warning,
                })
            }
            Command::RewindSessionToMessage { turn_count } => {
                let (session, cleanup_warning) =
                    self.rewind_session_to_message(session_id, turn_count)?;
                Ok(ResponsePayload::SessionRewound {
                    session,
                    cleanup_warning,
                })
            }
            Command::ForkProviderSession { request } => {
                Ok(ResponsePayload::ProviderSessionForked {
                    result: fork_provider_session(request)?,
                })
            }
            Command::Workspace {
                operation:
                    WorkspaceOperation::CaptureTurn {
                        cwd,
                        session_id,
                        turn_count,
                    },
            } => Ok(ResponsePayload::Workspace {
                result: WorkspaceResult::Checkpoint {
                    checkpoint: self.capture_turn_checkpoint(cwd, session_id, turn_count)?,
                },
            }),
            Command::Workspace { operation } => Ok(ResponsePayload::Workspace {
                result: crate::workspace::execute(operation)?,
            }),
            Command::OpenTerminal { cwd, cols, rows } => {
                let terminal = self.open_terminal(&cwd, cols, rows, events)?;
                // Releases the terminal this one replaces. On unix that drop is
                // what ends the replaced shell; the underscore keeps the binding
                // alive until the end of the arm on every platform.
                let _replaced = self
                    .terminals
                    .lock()
                    .insert(session_id, (runtime_id, terminal));
                Ok(ResponsePayload::Ack)
            }
            Command::WriteTerminal { data } => {
                let terminals = self.terminals.lock();
                let (active_runtime_id, terminal) = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if *active_runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                    );
                }
                terminal.write(data)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ResizeTerminal { cols, rows } => {
                let terminals = self.terminals.lock();
                let (active_runtime_id, terminal) = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if *active_runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                    );
                }
                terminal.resize(cols, rows);
                Ok(ResponsePayload::Ack)
            }
            Command::CloseTerminal => {
                let _removed = {
                    let mut terminals = self.terminals.lock();
                    if let Some((active_runtime_id, _)) = terminals.get(&session_id)
                        && *active_runtime_id != runtime_id
                    {
                        bail!(
                            "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                        );
                    }
                    terminals.remove(&session_id)
                };
                Ok(ResponsePayload::Ack)
            }
            Command::Start { options } => {
                let previous = self.sessions.lock().remove(&session_id);
                drop(previous);
                let provider = decode_enum(&options.provider)?;
                let options = DriverStartOptions {
                    binary: options.binary,
                    cwd: options.cwd,
                    mode: decode_enum(&options.mode)?,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    service_tier: options.service_tier,
                    context_window: options.context_window,
                    agent_preset: options.agent_preset,
                    computer_use_enabled: options.computer_use_enabled,
                    provider_cursor: options
                        .provider_cursor
                        .map(serde_json::from_value)
                        .transpose()
                        .context("daemon received an invalid provider cursor")?,
                    agent_bridge: self.agent_bridge_access(session_id, runtime_id),
                    // The identity a client starts a task as. The daemon has
                    // already resolved it to text through `LoadAgents`, so the
                    // wire carries the prompt rather than an id the daemon
                    // would have to re-read the catalog to interpret.
                    system_prompt: options.system_prompt,
                };
                let (wake, _wake_events) = smol::channel::bounded(1);
                let (event_sender, event_receiver) = driver::event_channel(wake);
                let handle = driver::start_local(provider, options, event_sender)?;
                let supports_steer = handle.supports_steer();
                std::thread::Builder::new()
                    .name(format!("waku-daemon-events-{session_id}"))
                    .spawn(move || {
                        while let Ok(event) = event_receiver.recv() {
                            let wire = event_to_wire(event).unwrap_or_else(|error| {
                                WireDriverEvent::new(
                                    "error",
                                    Value::String(format!(
                                        "could not encode daemon event: {error}"
                                    )),
                                )
                            });
                            if events.send(wire).is_err() {
                                break;
                            }
                        }
                    })
                    .context("could not start daemon event forwarding thread")?;
                self.sessions
                    .lock()
                    .insert(session_id, (runtime_id, handle));
                Ok(ResponsePayload::Started { supports_steer })
            }
            Command::SpawnDelegate {
                task,
                title,
                persona,
            } => self.spawn_delegate(session_id, task, title, persona, events),
            Command::ReadDelegate {
                session_id: delegate,
            } => self.delegate_report(session_id, delegate),
            Command::PromptDelegate {
                session_id: delegate,
                prompt,
            } => self.prompt_delegate(session_id, delegate, prompt),
            Command::SpawnTeam { team, task, title } => {
                self.spawn_team(session_id, &team, &task, title, events)
            }
            Command::CloseSession => {
                let removed = {
                    let mut sessions = self.sessions.lock();
                    sessions
                        .get(&session_id)
                        .is_some_and(|(active_runtime_id, _)| *active_runtime_id == runtime_id)
                        .then(|| sessions.remove(&session_id))
                        .flatten()
                };
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            command => {
                let driver = {
                    let sessions = self.sessions.lock();
                    let (active_runtime_id, driver) = sessions
                        .get(&session_id)
                        .ok_or_else(|| anyhow!("daemon session {session_id} is not running"))?;
                    if *active_runtime_id != runtime_id {
                        bail!(
                            "daemon session {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                        );
                    }
                    driver.clone()
                };
                if let Command::Prompt {
                    prompt,
                    turn_id,
                    message_id,
                } = &command
                {
                    // Publish the submission into the runtime's event stream
                    // before the provider can start the turn. Every attached
                    // client mirrors the user message and its turn from this
                    // event, so the submitting client's own save is no longer
                    // the only record of the prompt — a follower that only
                    // knew the provider's `turnStarted` used to persist a
                    // projection without it, erasing the message for everyone.
                    events.send(event_to_wire(DriverEvent::PromptSubmitted {
                        message: prompt.clone(),
                        turn_id: turn_id.unwrap_or_else(Uuid::new_v4),
                        message_id: message_id.unwrap_or_else(Uuid::new_v4),
                    })?)?;
                }
                handle_driver_command(&driver, command)
            }
        }
    }

    fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock());
        drop(sessions);
        let terminals = std::mem::take(&mut *self.terminals.lock());
        drop(terminals);
    }
}

fn session_projection_precedes(
    existing: &AgentSession,
    incoming: &AgentSession,
    active_runtime_id: Option<Uuid>,
) -> bool {
    let existing_cursor = existing.runtime_event_cursor;
    let incoming_cursor = incoming.runtime_event_cursor;
    if let Some(active_runtime_id) = active_runtime_id {
        let existing_is_active =
            existing_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        let incoming_is_active =
            incoming_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        if existing_is_active != incoming_is_active {
            return existing_is_active;
        }
    }
    match (existing_cursor, incoming_cursor) {
        (Some(existing), Some(incoming))
            if existing.runtime_id == incoming.runtime_id && existing.epoch == incoming.epoch =>
        {
            incoming.sequence < existing.sequence
        }
        (Some(_), None) if existing.status.is_busy() => true,
        _ => incoming.updated_at < existing.updated_at,
    }
}

fn merge_stale_session_metadata(existing: &mut AgentSession, incoming: AgentSession) {
    if incoming.updated_at >= existing.updated_at {
        existing.title = incoming.title;
        existing.project_id = incoming.project_id;
        existing.workspace = incoming.workspace;
        existing.provider = incoming.provider;
        existing.model = incoming.model;
        existing.runtime_mode = incoming.runtime_mode;
        existing.reasoning_effort = incoming.reasoning_effort;
        existing.service_tier = incoming.service_tier;
        existing.context_window = incoming.context_window;
        existing.agent_preset = incoming.agent_preset;
        existing.updated_at = incoming.updated_at;
        existing.last_reply_at = incoming.last_reply_at.or(existing.last_reply_at);
    }
    for queued in incoming.queued_messages {
        if !existing
            .queued_messages
            .iter()
            .any(|candidate| candidate.id == queued.id)
        {
            existing.queued_messages.push(queued);
        }
    }
}

/// Ending checkpoints are produced and stored by the daemon. A second client
/// may still save a projection created just before capture completed; never
/// let that stale projection erase the canonical Git snapshot.
fn preserve_daemon_checkpoints(existing: &AgentSession, incoming: &mut AgentSession) {
    for turn in &mut incoming.turns {
        let Some(checkpoint) = existing
            .turns
            .iter()
            .find(|candidate| candidate.turn_count == turn.turn_count)
            .and_then(|candidate| candidate.checkpoint.as_ref())
            .filter(|checkpoint| {
                matches!(
                    checkpoint.status,
                    CheckpointStatus::Ready | CheckpointStatus::Unavailable
                )
            })
        else {
            continue;
        };
        turn.checkpoint = Some(checkpoint.clone());
    }
}

impl WakuBackend {
    /// Fork a response using only daemon-host state.
    ///
    /// A browser must never reconstruct or persist this operation itself:
    /// provider-native sessions, checkpoint refs, and the task database all
    /// belong to the daemon and may be on another machine.
    fn fork_session_from_response(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd, fork_title) = {
            let mut state = self.task_state.lock();
            let source_index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the source task is unavailable"))?;
            self.task_store
                .hydrate(&mut state.sessions[source_index])
                .context("could not load the source task")?;
            let source = state.sessions[source_index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the source task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            let fork_title = next_response_fork_title(
                source.display_title(),
                state
                    .sessions
                    .iter()
                    .filter(|session| session.project_id == source.project_id)
                    .map(AgentSession::display_title),
            );
            (source, cwd, fork_title)
        };

        validate_response_fork(&source, turn_count)?;
        let provider_turn_count = source
            .turns
            .iter()
            .take(turn_count)
            .filter(|turn| turn.provider_turn_started)
            .count();
        let turns_to_remove = source.provider_turns_after(turn_count);
        let (provider_cursor, message_ids) = self.fork_provider_response(
            &source,
            &cwd,
            &fork_title,
            turn_count,
            provider_turn_count,
            turns_to_remove,
        )?;
        let mut forked = source
            .fork_through_turn(turn_count, provider_cursor, &fork_title)
            .ok_or_else(|| anyhow!("the selected response cannot be copied"))?;
        if !message_ids.is_empty() {
            for turn in &mut forked.turns {
                if let Some(message_id) = turn.provider_resume_at.as_mut()
                    && let Some(remapped) = message_ids.get(message_id)
                {
                    *message_id = remapped.clone();
                }
            }
        }

        let fork_id = forked.id;
        for turn in &mut forked.turns {
            if let Some(checkpoint) = turn.checkpoint.as_mut() {
                checkpoint.git_ref =
                    crate::checkpoint::checkpoint_ref(fork_id, checkpoint.turn_count);
            }
        }
        let checkpoint_warning =
            crate::checkpoint::copy_session_refs(&cwd, source.id, fork_id, turn_count)
                .err()
                .map(|error| error.to_string());

        let pinned = self.sessions.lock().keys().copied().collect();
        let mut state = self.task_state.lock();
        state.push_session(forked.clone());
        if let Err(error) = self.task_store.save(&mut state) {
            state.sessions.retain(|session| session.id != fork_id);
            let _ = crate::checkpoint::delete_all_session_refs(&cwd, fork_id);
            return Err(error).context("could not save the forked task");
        }
        trim_resident_transcripts(&mut state, &pinned);
        Ok((forked, checkpoint_warning))
    }

    /// Restore the daemon-host worktree, provider conversation, and stored
    /// transcript to immediately before one user turn.
    fn rewind_session_to_message(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd) = {
            let mut state = self.task_state.lock();
            let source_index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the task is unavailable"))?;
            self.task_store
                .hydrate(&mut state.sessions[source_index])
                .context("could not load the task")?;
            let source = state.sessions[source_index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            (source, cwd)
        };
        validate_message_rewind(&source, turn_count)?;

        // Resolve the executable before touching the worktree. Even native
        // transcript operations are immediately followed by a replacement
        // prompt, so accepting a rewind that cannot resume would strand the
        // user at a provider state the UI cannot continue.
        let binary = self.provider_binary(source.provider)?;
        let retained_turn_count = turn_count.saturating_sub(1);
        let previous_turn_count = source.turns.len();
        let rollback_turns = source.provider_turns_after(retained_turn_count);
        let provider_turn_count = source
            .turns
            .iter()
            .take(retained_turn_count)
            .filter(|turn| turn.provider_turn_started)
            .count();
        let provider_resume_at = retained_turn_count
            .checked_sub(1)
            .and_then(|index| source.turns.get(index))
            .and_then(|turn| turn.provider_resume_at.clone());

        let turn_start_ref = crate::checkpoint::turn_start_ref(session_id, turn_count);
        let retained_ref = crate::checkpoint::checkpoint_ref(session_id, retained_turn_count);
        let restore_ref = if crate::checkpoint::has_ref(&cwd, &turn_start_ref) {
            turn_start_ref
        } else {
            retained_ref
        };
        if !crate::checkpoint::has_ref(&cwd, &restore_ref) {
            bail!("the checkpoint before this message is unavailable");
        }

        let safety_ref = format!("refs/waku/revert-backup-{session_id}-{}", Uuid::new_v4());
        crate::checkpoint::capture_ref(&cwd, &safety_ref)
            .context("could not create a rewind safety snapshot")?;
        if let Err(error) = crate::checkpoint::restore_ref(&cwd, &restore_ref) {
            return Err(restore_rewind_safety(
                &cwd,
                &safety_ref,
                "could not restore the selected checkpoint",
                error,
            ));
        }

        let provider_rewind = self.rewind_provider_response(
            &source,
            &cwd,
            &binary,
            retained_turn_count,
            rollback_turns,
            provider_turn_count,
            provider_resume_at,
        );
        let (provider_cursor, message_ids, reset_native_session) = match provider_rewind {
            Ok(result) => result,
            Err(error) => {
                return Err(restore_rewind_safety(
                    &cwd,
                    &safety_ref,
                    "the provider rejected the rewind",
                    error,
                ));
            }
        };

        let _ = crate::checkpoint::delete_ref(&cwd, &safety_ref);
        let cleanup_warning = crate::checkpoint::delete_turn_refs_after(
            &cwd,
            session_id,
            retained_turn_count,
            previous_turn_count,
        )
        .err()
        .map(|error| error.to_string());

        // Every provider resumes from the newly stored cursor on the next
        // prompt. Dropping a resident source driver also prevents its late
        // events from racing the rewound transcript.
        let removed = self.sessions.lock().remove(&session_id);
        drop(removed);

        let mut rewound = source.clone();
        if !message_ids.is_empty() {
            for turn in rewound.turns.iter_mut().take(retained_turn_count) {
                if let Some(remapped) = turn
                    .provider_resume_at
                    .as_ref()
                    .and_then(|message_id| message_ids.get(message_id))
                    .cloned()
                {
                    turn.provider_resume_at = Some(remapped);
                }
            }
        }
        if reset_native_session {
            rewound.provider_cursor = None;
        } else if let Some(cursor) = provider_cursor {
            rewound.provider_cursor = Some(cursor);
        }
        rewound.truncate_after_turn(retained_turn_count);
        rewound.status = SessionStatus::Idle;

        let pinned = self.sessions.lock().keys().copied().collect();
        let mut state = self.task_state.lock();
        let existing = state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow!("the task was removed while it was being rewound"))?;
        *existing = rewound.clone();
        state.mark_session_dirty(session_id);
        self.task_store
            .save(&mut state)
            .context("could not save the rewound task")?;
        trim_resident_transcripts(&mut state, &pinned);
        Ok((rewound, cleanup_warning))
    }

    fn fork_provider_response(
        &self,
        source: &AgentSession,
        cwd: &Path,
        fork_title: &str,
        turn_count: usize,
        provider_turn_count: usize,
        turns_to_remove: usize,
    ) -> anyhow::Result<(ProviderResumeCursor, HashMap<String, String>)> {
        match source.provider {
            ProviderKind::Claude => {
                let Some(ProviderResumeCursor::Claude { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Claude's native session is unavailable");
                };
                let resume_at = source
                    .turns
                    .get(turn_count.saturating_sub(1))
                    .and_then(|turn| turn.provider_resume_at.clone());
                let fork = fork_provider_session(ProviderSessionForkRequest::Claude {
                    session_id: session_id.clone(),
                    resume_at,
                    turn_count: provider_turn_count,
                    title: fork_title.to_owned(),
                })?;
                Ok((fork.cursor, fork.message_ids))
            }
            ProviderKind::Codex
            | ProviderKind::DeepSeek
            | ProviderKind::OhMyPi
            | ProviderKind::Pi => Ok((
                self.fork_response_with_driver(source, cwd, turns_to_remove)?,
                HashMap::new(),
            )),
            ProviderKind::Cursor => {
                let fork = fork_provider_session(ProviderSessionForkRequest::Cursor {
                    source: source.clone(),
                    turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Amp => {
                let Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context,
                }) = source.provider_cursor.as_ref()
                else {
                    bail!("Amp's native thread is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Amp {
                    binary: self.provider_binary(ProviderKind::Amp)?,
                    cwd: cwd.to_owned(),
                    thread_id: thread_id.clone(),
                    fork_context: fork_context.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::OpenCode => {
                let Some(ProviderResumeCursor::OpenCode { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("OpenCode's native session is unavailable");
                };
                // No cwd: a session carries its own `location`, so there is no
                // server working directory to fork against.
                let fork = fork_provider_session(ProviderSessionForkRequest::OpenCode {
                    binary: self.provider_binary(ProviderKind::OpenCode)?,
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Grok => {
                let Some(ProviderResumeCursor::Grok { session_id }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Grok Build's native session is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Grok {
                    binary: self.provider_binary(ProviderKind::Grok)?,
                    cwd: cwd.to_owned(),
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            // Unreachable through the UI, which hides branching for providers
            // that answer `supports_conversation_fork` with false.
            ProviderKind::Fx | ProviderKind::Kimi => {
                bail!(
                    "{} cannot branch a conversation at a turn",
                    source.provider.display_name()
                )
            }
        }
    }

    fn fork_response_with_driver(
        &self,
        source: &AgentSession,
        cwd: &Path,
        turns_to_remove: usize,
    ) -> anyhow::Result<ProviderResumeCursor> {
        if let Some(driver) = self
            .sessions
            .lock()
            .get(&source.id)
            .map(|(_, driver)| driver.clone())
        {
            return driver.fork(turns_to_remove);
        }

        match source.provider {
            ProviderKind::Codex
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::Codex { .. })
                ) =>
            {
                bail!("Codex's native thread is unavailable");
            }
            ProviderKind::DeepSeek
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::DeepSeek { .. })
                ) =>
            {
                bail!("DeepSeek Harness's native session is unavailable");
            }
            ProviderKind::Pi
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::Pi {
                        session_file: Some(_),
                        ..
                    })
                ) =>
            {
                bail!("Pi's native session file is unavailable");
            }
            ProviderKind::OhMyPi
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::OhMyPi {
                        session_file: Some(_),
                        ..
                    })
                ) =>
            {
                bail!("Oh My Pi's native session file is unavailable");
            }
            _ => {}
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, _event_receiver) = driver::event_channel(wake);
        let driver = driver::start_local(
            source.provider,
            DriverStartOptions {
                binary: self.provider_binary(source.provider)?,
                cwd: cwd.to_owned(),
                mode: source.runtime_mode,
                model: source.model.clone(),
                reasoning_effort: source.reasoning_effort.clone(),
                service_tier: source.service_tier.clone(),
                context_window: source.context_window.clone(),
                agent_preset: source.agent_preset.clone(),
                computer_use_enabled: false,
                provider_cursor: source.provider_cursor.clone(),
                agent_bridge: None,
                // A fork reproduces an existing conversation, so the identity
                // it ran under is already stated in its transcript.
                // Re-injecting the prompt would state it twice.
                system_prompt: None,
            },
            event_sender,
        )?;
        driver.fork(turns_to_remove)
    }

    #[allow(clippy::too_many_arguments)]
    fn rewind_provider_response(
        &self,
        source: &AgentSession,
        cwd: &Path,
        binary: &Path,
        retained_turn_count: usize,
        rollback_turns: usize,
        provider_turn_count: usize,
        provider_resume_at: Option<String>,
    ) -> anyhow::Result<(Option<ProviderResumeCursor>, HashMap<String, String>, bool)> {
        if rollback_turns == 0 {
            return Ok((None, HashMap::new(), false));
        }
        let reset_native_session = retained_turn_count == 0
            && matches!(
                source.provider,
                ProviderKind::Claude | ProviderKind::Cursor | ProviderKind::Grok
            );
        if reset_native_session {
            return Ok((None, HashMap::new(), true));
        }

        match source.provider {
            ProviderKind::Claude => {
                let Some(ProviderResumeCursor::Claude { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Claude's native session is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Claude {
                    session_id: session_id.clone(),
                    resume_at: provider_resume_at,
                    turn_count: provider_turn_count,
                    title: format!("{} (rewind)", source.display_title()),
                })?;
                Ok((Some(fork.cursor), fork.message_ids, false))
            }
            ProviderKind::OpenCode => {
                let cursor = if let Some(driver) = self
                    .sessions
                    .lock()
                    .get(&source.id)
                    .map(|(_, driver)| driver.clone())
                {
                    driver
                        .rollback(rollback_turns)?
                        .ok_or_else(|| anyhow!("OpenCode returned no rewound-session cursor"))?
                } else {
                    let Some(ProviderResumeCursor::OpenCode { session_id, .. }) =
                        source.provider_cursor.as_ref()
                    else {
                        bail!("OpenCode's native session is unavailable");
                    };
                    fork_provider_session(ProviderSessionForkRequest::OpenCode {
                        binary: binary.to_owned(),
                        session_id: session_id.clone(),
                        turn_count: provider_turn_count,
                    })?
                    .cursor
                };
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Amp => {
                let Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context,
                }) = source.provider_cursor.as_ref()
                else {
                    bail!("Amp's native thread is unavailable");
                };
                let cursor = fork_provider_session(ProviderSessionForkRequest::Amp {
                    binary: binary.to_owned(),
                    cwd: cwd.to_owned(),
                    thread_id: thread_id.clone(),
                    fork_context: fork_context.clone(),
                    turn_count: provider_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Cursor => {
                let cursor = fork_provider_session(ProviderSessionForkRequest::Cursor {
                    source: source.clone(),
                    turn_count: retained_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Grok => {
                let Some(ProviderResumeCursor::Grok { session_id }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Grok Build's native session is unavailable");
                };
                let cursor = fork_provider_session(ProviderSessionForkRequest::Grok {
                    binary: binary.to_owned(),
                    cwd: cwd.to_owned(),
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Codex
            | ProviderKind::DeepSeek
            | ProviderKind::OhMyPi
            | ProviderKind::Pi => Ok((
                self.rollback_response_with_driver(source, cwd, binary, rollback_turns)?,
                HashMap::new(),
                false,
            )),
            // Unreachable through the UI, which hides rewinding for providers
            // that answer `supports_conversation_rollback` with false.
            ProviderKind::Fx | ProviderKind::Kimi => {
                bail!(
                    "{} cannot rewind a conversation to a turn",
                    source.provider.display_name()
                )
            }
        }
    }

    fn rollback_response_with_driver(
        &self,
        source: &AgentSession,
        cwd: &Path,
        binary: &Path,
        rollback_turns: usize,
    ) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if let Some(driver) = self
            .sessions
            .lock()
            .get(&source.id)
            .map(|(_, driver)| driver.clone())
        {
            return driver.rollback(rollback_turns);
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, _event_receiver) = driver::event_channel(wake);
        let driver = driver::start_local(
            source.provider,
            DriverStartOptions {
                binary: binary.to_owned(),
                cwd: cwd.to_owned(),
                mode: source.runtime_mode,
                model: source.model.clone(),
                reasoning_effort: source.reasoning_effort.clone(),
                service_tier: source.service_tier.clone(),
                context_window: source.context_window.clone(),
                agent_preset: source.agent_preset.clone(),
                computer_use_enabled: false,
                provider_cursor: source.provider_cursor.clone(),
                agent_bridge: None,
                // A rewind restores a conversation that already ran; its
                // identity is part of what was said, not of the restart.
                system_prompt: None,
            },
            event_sender,
        )?;
        driver.rollback(rollback_turns)
    }

    fn provider_binary(&self, provider: ProviderKind) -> anyhow::Result<PathBuf> {
        ensure_shell_environment();
        let settings = self.settings.get();
        let binary_override = settings
            .provider_binary_overrides
            .get(&provider)
            .map(String::as_str);
        crate::model::provider_probe(provider, binary_override)
            .path
            .ok_or_else(|| anyhow!("{} is not installed on the daemon", provider.display_name()))
    }

    /// The user's configured `claude` path, when there is one.
    ///
    /// Claude's MCP and plugin commands must run against the same binary the
    /// sessions run, or a user who pointed Waku at a specific install would be
    /// editing a different Claude Code's configuration.
    fn claude_binary_override(&self) -> Option<String> {
        ensure_shell_environment();
        self.settings
            .get()
            .provider_binary_overrides
            .get(&ProviderKind::Claude)
            .cloned()
    }
}

fn validate_message_rewind(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before editing a prior message");
    }
    let Some(turn) = source
        .turns
        .iter()
        .find(|turn| turn.turn_count == turn_count)
    else {
        bail!("the selected message is unavailable");
    };
    if !source.messages.iter().any(|message| {
        message.turn_id == Some(turn.id) && message.role == crate::model::MessageRole::User
    }) {
        bail!("the selected user message is unavailable");
    }
    let rollback_turns = source.provider_turns_after(turn_count.saturating_sub(1));
    if rollback_turns > 0 && source.provider_cursor.is_none() {
        bail!("the provider conversation is unavailable");
    }
    Ok(())
}

fn restore_rewind_safety(
    cwd: &Path,
    safety_ref: &str,
    context: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match crate::checkpoint::restore_ref(cwd, safety_ref) {
        Ok(()) => {
            let _ = crate::checkpoint::delete_ref(cwd, safety_ref);
            anyhow!("{context}: {error}; the original worktree was restored")
        }
        Err(restore_error) => anyhow!(
            "{context}: {error}; restoring the safety snapshot also failed: {restore_error}; snapshot: {safety_ref}"
        ),
    }
}

fn validate_response_fork(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before forking a response");
    }
    let cursor = source
        .provider_cursor
        .as_ref()
        .ok_or_else(|| anyhow!("the provider conversation is unavailable"))?;
    if cursor.provider() != source.provider {
        bail!("the provider conversation does not match this task");
    }
    if source
        .turns
        .get(turn_count.saturating_sub(1))
        .is_none_or(|turn| turn.turn_count != turn_count || !turn.provider_turn_started)
    {
        bail!("the selected response cannot be forked");
    }
    Ok(())
}

fn numbered_title_suffix(title: &str) -> Option<(&str, usize)> {
    let (base, suffix) = title.rsplit_once(" (")?;
    let number = suffix.strip_suffix(')')?.parse().ok()?;
    (!base.is_empty() && number >= 2).then_some((base, number))
}

fn next_response_fork_title<'a>(
    source_title: &str,
    existing_titles: impl IntoIterator<Item = &'a str>,
) -> String {
    let existing_titles = existing_titles.into_iter().collect::<Vec<_>>();
    let base = numbered_title_suffix(source_title)
        .filter(|(base, _)| existing_titles.iter().any(|title| title == base))
        .map_or(source_title, |(base, _)| base);
    let highest_number = existing_titles
        .iter()
        .filter_map(|title| {
            if *title == base {
                Some(1)
            } else {
                numbered_title_suffix(title)
                    .filter(|(candidate_base, _)| *candidate_base == base)
                    .map(|(_, number)| number)
            }
        })
        .max()
        .unwrap_or(1);
    format!("{base} ({})", highest_number.saturating_add(1).max(2))
}

fn fork_provider_session(
    request: ProviderSessionForkRequest,
) -> anyhow::Result<ProviderSessionFork> {
    use crate::model::ProviderResumeCursor;

    let (cursor, message_ids, source_resume_at) = match request {
        ProviderSessionForkRequest::Claude {
            session_id,
            resume_at,
            turn_count,
            title,
        } => {
            let source_resume_at = resume_at.map(Ok).unwrap_or_else(|| {
                crate::claude_session::message_id_for_turn(&session_id, turn_count)
            })?;
            let fork =
                crate::claude_session::fork_session_at(&session_id, &source_resume_at, &title)?;
            let fork_resume_at = fork
                .message_ids
                .get(&source_resume_at)
                .cloned()
                .ok_or_else(|| anyhow!("Claude fork did not include its target message"))?;
            (
                ProviderResumeCursor::Claude {
                    session_id: fork.session_id,
                    resume_at: Some(fork_resume_at),
                },
                fork.message_ids,
                Some(source_resume_at),
            )
        }
        ProviderSessionForkRequest::Amp {
            binary,
            cwd,
            thread_id,
            fork_context,
            turn_count,
        } => (
            crate::amp_session::fork_session_at_turn(
                &binary,
                &cwd,
                &thread_id,
                fork_context.as_deref(),
                turn_count,
            )?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Cursor { source, turn_count } => (
            crate::cursor_session::fork_session_at_turn(&source, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::OpenCode {
            binary,
            session_id,
            turn_count,
        } => (
            crate::opencode_session::fork_session_at_turn(&binary, &session_id, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Grok {
            binary,
            cwd,
            session_id,
            turn_count,
        } => (
            crate::grok_session::fork_session_at_turn(&binary, &cwd, &session_id, turn_count)?,
            HashMap::new(),
            None,
        ),
    };
    Ok(ProviderSessionFork {
        cursor,
        message_ids,
        source_resume_at,
    })
}

/// Builds the task record for a delegate of `lead`.
///
/// Pulled out of [`WakuBackend::spawn_delegate`] so the rules that make a
/// What of a delegate's stream its lead's conversation also receives.
///
/// Deliberately four kinds, not a mirror of everything. A lead's cards show what
/// the delegate is saying and whether it is done; the delegate's tool calls,
/// reasoning, permissions and checkpoints belong to the delegate's own task,
/// where opening it reads the full transcript. Forwarding those too would put a
/// second process's internals into the lead's UI for no reader.
fn delegate_relay_event(event: &DriverEvent) -> Option<DriverEvent> {
    match event {
        DriverEvent::TextDelta(text) => Some(DriverEvent::TextDelta(text.clone())),
        DriverEvent::TurnStarted => Some(DriverEvent::TurnStarted),
        DriverEvent::TurnFinished { success, summary } => Some(DriverEvent::TurnFinished {
            success: *success,
            summary: summary.clone(),
        }),
        DriverEvent::Error(error) => Some(DriverEvent::Error(error.clone())),
        _ => None,
    }
}

/// Folds one provider event into what the daemon remembers about a delegate.
///
/// Only three things matter to a lead reading a delegate: whether it is still
/// working, what it has said in this turn, and how it failed. Tool calls,
/// reasoning and checkpoints belong to the transcript, which whoever opens the
/// task projects for itself.
///
/// The success/failure decision lives here and nowhere else, so a successful
/// turn can never leave an earlier turn's complaint attached to a delegate that
/// is now fine.
fn record_delegate_event(
    delegates: &Mutex<HashMap<Uuid, DelegateRecord>>,
    session_id: Uuid,
    event: &DriverEvent,
) {
    let mut delegates = delegates.lock();
    let Some(record) = delegates.get_mut(&session_id) else {
        return;
    };
    match event {
        DriverEvent::TurnStarted => {
            record.status = SessionStatus::Working;
            record.reply.clear();
            record.failure = None;
        }
        DriverEvent::TextDelta(text) => record.reply.push_str(text),
        DriverEvent::TurnFinished { success, summary } => {
            if *success {
                record.status = SessionStatus::Idle;
                record.failure = None;
            } else {
                record.status = SessionStatus::Failed;
                record.failure = Some(
                    summary
                        .clone()
                        .unwrap_or_else(|| "the delegate's turn failed".to_owned()),
                );
            }
        }
        DriverEvent::Error(error) => {
            record.status = SessionStatus::Failed;
            record.failure = Some(error.clone());
        }
        // A delegate that dies mid-turn must stop reporting as working; one
        // whose turn had already settled is simply finished.
        DriverEvent::ProcessExited if record.status.is_busy() => {
            record.status = SessionStatus::Failed;
            record.failure = Some("the delegate's process exited".to_owned());
        }
        _ => {}
    }
}

fn delegated_session(lead: &AgentSession, title: Option<String>) -> AgentSession {
    let mut delegate = AgentSession::new(lead.project_id, lead.provider);
    delegate.title = title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| AgentSession::DEFAULT_TITLE.to_owned());
    delegate.model = lead.model.clone();
    delegate.runtime_mode = lead.runtime_mode;
    delegate.reasoning_effort = lead.reasoning_effort.clone();
    delegate.service_tier = lead.service_tier.clone();
    delegate.context_window = lead.context_window.clone();
    delegate.agent_preset = lead.agent_preset.clone();
    // The relationship is part of the delegate, not a side table: the sidebar
    // reads this field to decide whether a task is something the user opened or
    // something the lead created, and the user has to reach the delegate from
    // the lead. Recording it here — where the session is built — keeps a delegate
    // a delegate after the daemon that created it is gone.
    delegate.parent_session_id = Some(lead.id);
    delegate
}

fn handle_driver_command(
    driver: &DriverHandle,
    command: Command,
) -> anyhow::Result<ResponsePayload> {
    match command {
        Command::Prompt { prompt, .. } => driver.prompt(prompt),
        Command::Steer { prompt } => driver.steer(prompt),
        Command::Cancel => driver.cancel(),
        Command::CancelComputerUse => driver.cancel_computer_use(),
        Command::RefreshBackgroundWork => driver.refresh_background_work(),
        Command::StopBackgroundWork { key, control_id } => {
            driver.stop_background_work(
                serde_json::from_value(key).context("invalid background-work key")?,
                control_id,
            );
        }
        Command::Respond {
            request_id,
            option_id,
        } => driver.respond(request_id, option_id),
        Command::RespondUserInput {
            request_id,
            answers,
        } => driver.respond_user_input(request_id, answers),
        Command::Goal { operation } => driver.goal(operation),
        Command::RunComputerTool { request } => {
            driver.run_computer_tool(crate::computer_use::ComputerToolRequest {
                call_id: request.call_id,
                tool: request.tool,
                arguments: request.arguments,
            });
        }
        Command::RejectComputerTool { request, reason } => {
            driver.reject_computer_tool(
                crate::computer_use::ComputerToolRequest {
                    call_id: request.call_id,
                    tool: request.tool,
                    arguments: request.arguments,
                },
                reason,
            );
        }
        Command::ApplyOptions { options } => {
            return Ok(ResponsePayload::OptionsApplied {
                applied: driver.apply_options(SessionOptions {
                    mode: decode_enum(&options.mode)?,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    service_tier: options.service_tier,
                    context_window: options.context_window,
                }),
            });
        }
        Command::Rollback { turns } => {
            let cursor = driver
                .rollback(turns)?
                .map(serde_json::to_value)
                .transpose()?;
            return Ok(ResponsePayload::Cursor { cursor });
        }
        Command::Fork { turns_to_remove } => {
            let cursor = Some(serde_json::to_value(driver.fork(turns_to_remove)?)?);
            return Ok(ResponsePayload::Cursor { cursor });
        }
        Command::AttachSession
        | Command::Start { .. }
        | Command::SpawnDelegate { .. }
        | Command::SpawnTeam { .. }
        | Command::ReadDelegate { .. }
        | Command::PromptDelegate { .. }
        | Command::GetSettings
        | Command::UpdateSettings { .. }
        | Command::ProbeProvider { .. }
        | Command::FetchPlanUsage { .. }
        | Command::ProbeComputerPermissions { .. }
        | Command::LoadUsageHistory { .. }
        | Command::LoadSkills { .. }
        | Command::SetSkillsEnabled { .. }
        | Command::TrashSkills { .. }
        | Command::LoadAgents { .. }
        | Command::SaveAgentDefinition { .. }
        | Command::DeleteAgentDefinition { .. }
        | Command::SaveAgentTeam { .. }
        | Command::DeleteAgentTeam { .. }
        | Command::LoadClaudeMcpServers { .. }
        | Command::RemoveClaudeMcpServer { .. }
        | Command::LoadClaudePlugins
        | Command::SetClaudePluginEnabled { .. }
        | Command::UninstallClaudePlugin { .. }
        | Command::LoadClaudeMarketplaces
        | Command::LoadAvailableClaudePlugins
        | Command::LoadClaudePluginDetails { .. }
        | Command::InstallClaudePlugin { .. }
        | Command::UpdateClaudePlugin { .. }
        | Command::AddClaudeMarketplace { .. }
        | Command::RemoveClaudeMarketplace { .. }
        | Command::UpdateClaudeMarketplace { .. }
        | Command::LoadClaudePluginConfig { .. }
        | Command::SetClaudePluginConfig { .. }
        | Command::PruneClaudePlugins { .. }
        | Command::LoadTaskState
        | Command::SaveTaskState { .. }
        | Command::RemoveSession
        | Command::HydrateSession { .. }
        | Command::SearchSessionMessages { .. }
        | Command::ListProviderSessions { .. }
        | Command::LoadProviderSession { .. }
        | Command::LoadComposerDrafts
        | Command::SaveComposerDrafts { .. }
        | Command::ApplyComposerDraftChanges { .. }
        | Command::StoreBlob { .. }
        | Command::ImportAttachment { .. }
        | Command::ImportPathAttachment { .. }
        | Command::ReadBlob { .. }
        | Command::ReadAttachment { .. }
        | Command::SweepBlobs
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. }
        | Command::ForkProviderSession { .. }
        | Command::Workspace { .. }
        | Command::OpenTerminal { .. }
        | Command::WriteTerminal { .. }
        | Command::ResizeTerminal { .. }
        | Command::CloseTerminal
        | Command::CloseSession => {
            bail!("daemon received a command in the wrong dispatch path")
        }
    }
    Ok(ResponsePayload::Ack)
}

fn ensure_shell_environment() {
    static REFRESHED: OnceLock<()> = OnceLock::new();
    REFRESHED.get_or_init(|| {
        crate::command_env::refresh_from_default_shell();
    });
}

fn decode_enum<T: DeserializeOwned>(value: &str) -> anyhow::Result<T> {
    serde_json::from_value(Value::String(value.to_owned()))
        .with_context(|| format!("invalid protocol enum value {value:?}"))
}

pub fn encode_enum<T: Serialize>(value: T) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("protocol enum did not serialize as a string"))
}

fn event_to_wire(event: DriverEvent) -> anyhow::Result<WireDriverEvent> {
    let (kind, payload) = match event {
        DriverEvent::RuntimeEventCursorAdvanced(_) => {
            bail!("client-only runtime cursors cannot be sent by the daemon")
        }
        // Received by a client, never produced by the daemon: the relay below
        // builds its wire form directly from the delegate's own encoded event.
        DriverEvent::DelegateStreamed { .. } => {
            bail!("a relayed delegate event is not sent again as one")
        }
        DriverEvent::Connected { provider_cursor } => {
            ("connected", serde_json::to_value(provider_cursor)?)
        }
        DriverEvent::AgentPresetSelected(preset) => {
            ("agentPresetSelected", serde_json::to_value(preset)?)
        }
        DriverEvent::AutoTitleUpdated(title) => ("autoTitleUpdated", serde_json::to_value(title)?),
        DriverEvent::AvailableCommands(commands) => {
            ("availableCommands", serde_json::to_value(commands)?)
        }
        DriverEvent::TurnStarted => ("turnStarted", Value::Null),
        DriverEvent::TurnParked => ("turnParked", Value::Null),
        DriverEvent::TextDelta(text) => ("textDelta", Value::String(text)),
        DriverEvent::ReasoningDelta(text) => ("reasoningDelta", Value::String(text)),
        DriverEvent::Activity {
            id,
            kind,
            title,
            detail,
            complete,
        } => (
            "activity",
            json!({
                "id": id,
                "kind": kind,
                "title": title,
                "detail": detail,
                "complete": complete,
            }),
        ),
        DriverEvent::RichActivity(activity) => ("richActivity", serde_json::to_value(activity)?),
        DriverEvent::BackgroundWork(work) => ("backgroundWork", serde_json::to_value(work)?),
        DriverEvent::Permission {
            request_id,
            title,
            detail,
            options,
        } => (
            "permission",
            json!({
                "requestId": request_id,
                "title": title,
                "detail": detail,
                "options": options,
            }),
        ),
        DriverEvent::UserInputRequested {
            request_id,
            questions,
        } => (
            "userInputRequested",
            json!({
                "requestId": request_id,
                "questions": questions,
            }),
        ),
        DriverEvent::ComputerUseUpdated(state) => (
            "computerUseUpdated",
            serde_json::to_value(ComputerUseWire {
                target: state.target,
                phase: state.phase,
                visible: state.visible,
                image_url: state.image_url,
            })?,
        ),
        DriverEvent::PromptSubmitted {
            message,
            turn_id,
            message_id,
        } => (
            "promptSubmitted",
            json!({ "message": message, "turnId": turn_id, "messageId": message_id }),
        ),
        DriverEvent::SteerAccepted { message } => ("steerAccepted", json!({ "message": message })),
        DriverEvent::SteerRejected { message, reason } => (
            "steerRejected",
            json!({ "message": message, "reason": reason }),
        ),
        DriverEvent::UsageUpdated {
            context_tokens,
            context_window,
        } => (
            "usageUpdated",
            json!({
                "contextTokens": context_tokens,
                "contextWindow": context_window,
            }),
        ),
        DriverEvent::PlanUsageUpdated(usage) => ("planUsageUpdated", serde_json::to_value(usage)?),
        DriverEvent::GoalUpdated(goal) => ("goalUpdated", serde_json::to_value(goal)?),
        DriverEvent::TurnFinished { success, summary } => (
            "turnFinished",
            json!({ "success": success, "summary": summary }),
        ),
        DriverEvent::Error(error) => ("error", Value::String(error)),
        DriverEvent::ProcessExited => ("processExited", Value::Null),
    };
    Ok(WireDriverEvent::new(kind, payload))
}

pub fn event_from_wire(event: WireDriverEvent) -> anyhow::Result<DriverEvent> {
    let payload = event.payload;
    Ok(match event.kind.as_str() {
        "connected" => DriverEvent::Connected {
            provider_cursor: serde_json::from_value(payload)?,
        },
        "agentPresetSelected" => DriverEvent::AgentPresetSelected(serde_json::from_value(payload)?),
        "autoTitleUpdated" => DriverEvent::AutoTitleUpdated(serde_json::from_value(payload)?),
        "availableCommands" => DriverEvent::AvailableCommands(serde_json::from_value(payload)?),
        "turnStarted" => DriverEvent::TurnStarted,
        "turnParked" => DriverEvent::TurnParked,
        "textDelta" => DriverEvent::TextDelta(serde_json::from_value(payload)?),
        "reasoningDelta" => DriverEvent::ReasoningDelta(serde_json::from_value(payload)?),
        "activity" => {
            let activity: ActivityWire = serde_json::from_value(payload)?;
            DriverEvent::Activity {
                id: activity.id,
                kind: activity.kind,
                title: activity.title,
                detail: activity.detail,
                complete: activity.complete,
            }
        }
        "richActivity" => DriverEvent::RichActivity(serde_json::from_value(payload)?),
        "backgroundWork" => DriverEvent::BackgroundWork(serde_json::from_value(payload)?),
        "permission" => {
            let permission: PermissionWire = serde_json::from_value(payload)?;
            DriverEvent::Permission {
                request_id: permission.request_id,
                title: permission.title,
                detail: permission.detail,
                options: permission.options,
            }
        }
        "userInputRequested" => {
            let request: UserInputWire = serde_json::from_value(payload)?;
            DriverEvent::UserInputRequested {
                request_id: request.request_id,
                questions: request.questions,
            }
        }
        "computerUseUpdated" => {
            let state: ComputerUseWire = serde_json::from_value(payload)?;
            DriverEvent::ComputerUseUpdated(ComputerUseState {
                target: state.target,
                phase: state.phase,
                visible: state.visible,
                image_url: state.image_url,
            })
        }
        "promptSubmitted" => {
            let submitted: SubmittedPromptWire = serde_json::from_value(payload)?;
            DriverEvent::PromptSubmitted {
                message: submitted.message,
                turn_id: submitted.turn_id,
                message_id: submitted.message_id,
            }
        }
        "steerAccepted" => {
            let steer: AcceptedSteerWire = serde_json::from_value(payload)?;
            DriverEvent::SteerAccepted {
                message: steer.message,
            }
        }
        "steerRejected" => {
            let steer: RejectedSteerWire = serde_json::from_value(payload)?;
            DriverEvent::SteerRejected {
                message: steer.message,
                reason: steer.reason,
            }
        }
        "usageUpdated" => {
            let usage: UsageWire = serde_json::from_value(payload)?;
            DriverEvent::UsageUpdated {
                context_tokens: usage.context_tokens,
                context_window: usage.context_window,
            }
        }
        "planUsageUpdated" => DriverEvent::PlanUsageUpdated(serde_json::from_value(payload)?),
        "goalUpdated" => DriverEvent::GoalUpdated(serde_json::from_value(payload)?),
        "turnFinished" => {
            let finished: TurnFinishedWire = serde_json::from_value(payload)?;
            DriverEvent::TurnFinished {
                success: finished.success,
                summary: finished.summary,
            }
        }
        "error" => DriverEvent::Error(serde_json::from_value(payload)?),
        "processExited" => DriverEvent::ProcessExited,
        kind => bail!("daemon sent an unsupported driver event {kind:?}"),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmittedPromptWire {
    message: String,
    turn_id: Uuid,
    message_id: Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityWire {
    id: Option<String>,
    kind: ActivityKind,
    title: String,
    detail: Option<String>,
    complete: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionWire {
    request_id: String,
    title: String,
    detail: String,
    options: Vec<PermissionOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInputWire {
    request_id: String,
    questions: Vec<crate::model::UserInputQuestion>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComputerUseWire {
    target: Option<ComputerTarget>,
    phase: ComputerUsePhase,
    visible: bool,
    image_url: Option<String>,
}

#[derive(Deserialize)]
struct AcceptedSteerWire {
    message: String,
}

#[derive(Deserialize)]
struct RejectedSteerWire {
    message: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageWire {
    context_tokens: Option<u64>,
    context_window: Option<u64>,
}

#[derive(Deserialize)]
struct TurnFinishedWire {
    success: bool,
    summary: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lead's conversation receives a delegate's reply as it is written, and
    /// nothing else about it.
    ///
    /// The four relayed kinds are the ones a reader follows: the text, the turn
    /// opening and closing, and an error. Everything else a delegate's process
    /// reports — tool calls, reasoning, permission prompts, checkpoints — belongs
    /// to the delegate's own task. Forwarding those would put a second process's
    /// internals in the lead's UI, and the transcript is where they are read.
    #[test]
    fn a_lead_receives_its_delegates_words_but_not_its_internals() {
        assert!(matches!(
            delegate_relay_event(&DriverEvent::TextDelta("half an answer".into())),
            Some(DriverEvent::TextDelta(text)) if text == "half an answer"
        ));
        assert!(matches!(
            delegate_relay_event(&DriverEvent::TurnStarted),
            Some(DriverEvent::TurnStarted)
        ));
        assert!(matches!(
            delegate_relay_event(&DriverEvent::TurnFinished {
                success: true,
                summary: None
            }),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));
        assert!(matches!(
            delegate_relay_event(&DriverEvent::Error("boom".into())),
            Some(DriverEvent::Error(error)) if error == "boom"
        ));

        for internal in [
            DriverEvent::ReasoningDelta("thinking".into()),
            DriverEvent::Activity {
                id: None,
                kind: ActivityKind::Tool,
                title: "ran the tests".into(),
                detail: None,
                complete: true,
            },
            DriverEvent::UsageUpdated {
                context_tokens: Some(1),
                context_window: Some(2),
            },
            DriverEvent::ProcessExited,
        ] {
            assert!(
                delegate_relay_event(&internal).is_none(),
                "{internal:?} belongs to the delegate's own task, not the lead's UI"
            );
        }
    }

    /// A delegate has to be an ordinary task of the same project and provider,
    /// carrying the lead's options, so the user can open it and read what it
    /// did. If it were a different provider or mode it would not be the work the
    /// lead asked for.
    #[test]
    fn a_delegate_inherits_the_leads_provider_workspace_and_options() {
        let mut lead = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        lead.model = Some("opus".into());
        lead.runtime_mode = crate::model::RuntimeMode::Ask;

        let delegate = delegated_session(&lead, Some("Survey the tests".into()));

        assert_eq!(delegate.provider, lead.provider);
        assert_eq!(delegate.project_id, lead.project_id);
        assert_eq!(delegate.model.as_deref(), Some("opus"));
        assert_eq!(delegate.runtime_mode, lead.runtime_mode);
        assert_eq!(delegate.title, "Survey the tests");
        // The relationship is what lets the sidebar hide it and the lead reach
        // it; a delegate built without this is just an unrelated task.
        assert_eq!(delegate.parent_session_id, Some(lead.id));
        assert!(delegate.is_delegate());
    }

    /// A conversation the user starts is not a delegate of anything.
    ///
    /// The sidebar's filter reads this, so a false positive would hide a task
    /// the user opened — the opposite failure, and the more damaging one.
    #[test]
    fn an_ordinary_conversation_is_not_a_delegate() {
        let session = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        assert_eq!(session.parent_session_id, None);
        assert!(!session.is_delegate());
    }

    /// A delegate starts empty. Copying the lead's transcript would make the
    /// user's history appear inside a task that never ran it, and the new
    /// provider process would be resumed into a conversation it does not own.
    ///
    /// It also starts without a bridge, which is the only thing bounding the
    /// fan-out: a delegate that could delegate again would let one prompt
    /// multiply tasks without limit.
    #[test]
    fn a_delegate_starts_with_no_transcript_and_no_provider_cursor() {
        let mut lead = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        lead.push_message(crate::model::MessageRole::Assistant, "lead's own words");

        let delegate = delegated_session(&lead, None);

        assert!(delegate.messages.is_empty());
        assert!(delegate.turns.is_empty());
        assert!(delegate.provider_cursor.is_none());
        assert_eq!(delegate.title, AgentSession::DEFAULT_TITLE);
    }

    /// A spawn for a conversation the daemon does not know must say so.
    ///
    /// This drives the real [`WakuBackend`] through its real handler — the same
    /// path a scoped bridge request takes — without launching a provider. An
    /// unknown lead is the case worth pinning because it is what a bridge with a
    /// stale session id would send, and silently reporting success there would
    /// leave a lead waiting on a delegate that was never created.
    #[test]
    fn spawning_from_an_unknown_conversation_is_refused() {
        let root = std::env::temp_dir().join(format!("waku-delegate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new(root.join("app.db"));
        let settings = DaemonSettingsStore::open(root.join("daemon.json")).unwrap();
        let backend = WakuBackend::new(settings, store).unwrap();

        let refused = Backend::handle(
            &backend,
            Request {
                request_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                runtime_id: Uuid::new_v4(),
                command: Command::SpawnDelegate {
                    task: "go".into(),
                    title: None,
persona: None,
                },
            },
            // A sink for the runtime this would have created; the spawn must be
            // refused before anything uses it.
            EventSink::for_test(),
        );

        let error = refused.expect_err("an unknown lead cannot start a delegate");
        assert!(
            format!("{error}").contains("unavailable"),
            "the refusal has to name the reason: {error}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    fn a_delegate_started_by(lead: Uuid) -> (Arc<Mutex<HashMap<Uuid, DelegateRecord>>>, Uuid) {
        let delegates = Arc::new(Mutex::new(HashMap::new()));
        let delegate = Uuid::new_v4();
        delegates.lock().insert(
            delegate,
            DelegateRecord {
                lead,
                runtime_id: Uuid::new_v4(),
                sink: EventSink::for_test(),
                status: SessionStatus::Working,
                reply: String::new(),
                failure: None,
            },
        );
        (delegates, delegate)
    }

    fn recorded(
        delegates: &Mutex<HashMap<Uuid, DelegateRecord>>,
        delegate: Uuid,
    ) -> DelegateRecord {
        delegates.lock().get(&delegate).cloned().unwrap()
    }

    /// What a lead reads is the text the delegate's own stream produced.
    ///
    /// The daemon is the only observer a delegate is guaranteed to have, so this
    /// record is where its words exist between the provider emitting them and
    /// somebody opening the task. Assembling them out of deltas — rather than
    /// keeping the last one — is what makes a streamed answer readable at all.
    #[test]
    fn a_delegate_report_carries_what_the_provider_streamed() {
        let (delegates, delegate) = a_delegate_started_by(Uuid::new_v4());

        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TextDelta("Tuesday".into()),
        );
        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TextDelta(", the 25th".into()),
        );

        let report = recorded(&delegates, delegate);
        assert_eq!(report.reply, "Tuesday, the 25th");
        assert_eq!(report.status, SessionStatus::Working);
        assert!(report.failure.is_none());

        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TurnFinished {
                success: true,
                summary: Some("answered the question".into()),
            },
        );

        let report = recorded(&delegates, delegate);
        assert_eq!(report.status, SessionStatus::Idle);
        assert_eq!(report.reply, "Tuesday, the 25th");
    }

    /// A second turn must not hand back the first turn's answer.
    ///
    /// A lead that steers a finished delegate and then reads it again would
    /// otherwise see the old answer appended to the new one, which reads as if
    /// the delegate had answered the new instruction twice.
    #[test]
    fn a_new_turn_clears_what_the_lead_already_read() {
        let (delegates, delegate) = a_delegate_started_by(Uuid::new_v4());
        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TextDelta("first".into()),
        );
        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TurnFinished {
                success: true,
                summary: None,
            },
        );

        record_delegate_event(&delegates, delegate, &DriverEvent::TurnStarted);
        assert_eq!(recorded(&delegates, delegate).reply, "");
        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TextDelta("second".into()),
        );
        assert_eq!(recorded(&delegates, delegate).reply, "second");
    }

    /// Only a failed turn carries a reason, and success clears the old one.
    ///
    /// The rule is enforced by the one function that writes the record, because
    /// a delegate that recovered must not drag an earlier complaint into every
    /// later report — a lead would read it as a current failure.
    #[test]
    fn a_failed_turn_keeps_its_reason_and_a_good_one_drops_it() {
        let (delegates, delegate) = a_delegate_started_by(Uuid::new_v4());

        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TurnFinished {
                success: false,
                summary: Some("context window exhausted".into()),
            },
        );
        let failed = recorded(&delegates, delegate);
        assert_eq!(failed.status, SessionStatus::Failed);
        assert_eq!(failed.failure.as_deref(), Some("context window exhausted"));

        record_delegate_event(&delegates, delegate, &DriverEvent::TurnStarted);
        record_delegate_event(
            &delegates,
            delegate,
            &DriverEvent::TurnFinished {
                success: true,
                summary: Some("recovered".into()),
            },
        );
        let recovered = recorded(&delegates, delegate);
        assert_eq!(recovered.status, SessionStatus::Idle);
        assert!(
            recovered.failure.is_none(),
            "a successful turn must not keep an earlier failure attached"
        );
    }

    /// A delegate whose process dies mid-turn must stop reporting as working.
    ///
    /// Otherwise `wait_agent` would poll a delegate that can never answer until
    /// its own deadline, and the lead would learn nothing it could act on.
    #[test]
    fn a_delegate_whose_process_exits_is_reported_as_failed() {
        let (delegates, delegate) = a_delegate_started_by(Uuid::new_v4());
        record_delegate_event(&delegates, delegate, &DriverEvent::TextDelta("half".into()));

        record_delegate_event(&delegates, delegate, &DriverEvent::ProcessExited);

        let report = recorded(&delegates, delegate);
        assert_eq!(report.status, SessionStatus::Failed);
        assert_eq!(report.reply, "half");
        assert!(report.failure.is_some(), "the lead has to learn why");
    }

    /// Events for a session the daemon never delegated are ignored, not
    /// attributed to whichever record happens to be nearby.
    #[test]
    fn events_for_an_unknown_session_touch_no_record() {
        let (delegates, delegate) = a_delegate_started_by(Uuid::new_v4());
        record_delegate_event(
            &delegates,
            Uuid::new_v4(),
            &DriverEvent::TextDelta("nope".into()),
        );
        assert_eq!(recorded(&delegates, delegate).reply, "");
    }

    fn a_backend_in(root: &std::path::Path) -> WakuBackend {
        let store = StateStore::new(root.join("app.db"));
        let settings = DaemonSettingsStore::open(root.join("daemon.json")).unwrap();
        WakuBackend::new(settings, store).unwrap()
    }

    /// One lead cannot read another lead's delegate.
    ///
    /// The delegate id travels in the command while the token stays scoped to
    /// the lead, so this check is the only thing standing between a bridge and
    /// every other task the daemon knows. A guessable id must not be enough.
    #[test]
    fn reading_another_leads_delegate_is_refused() {
        let root = std::env::temp_dir().join(format!("waku-delegate-read-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = a_backend_in(&root);

        let other_lead = Uuid::new_v4();
        let delegate = Uuid::new_v4();
        backend.delegates.lock().insert(
            delegate,
            DelegateRecord {
                lead: other_lead,
                runtime_id: Uuid::new_v4(),
                sink: EventSink::for_test(),
                status: SessionStatus::Working,
                reply: "someone else's work".into(),
                failure: None,
            },
        );

        let refused = backend
            .delegate_report(Uuid::new_v4(), delegate)
            .expect_err("a delegate belongs to the lead that started it");
        assert!(
            format!("{refused}").contains("not started by this conversation"),
            "the refusal has to say what was wrong: {refused}"
        );

        // The owner still reads it: the check must not refuse everyone.
        let owns = backend
            .delegate_report(other_lead, delegate)
            .expect("the lead that started it may read it");
        let ResponsePayload::DelegateReport { reply, .. } = owns else {
            panic!("the daemon did not answer with a report");
        };
        assert_eq!(reply, "someone else's work");

        std::fs::remove_dir_all(root).ok();
    }

    /// An id the daemon never handed out must be reported as unknown rather
    /// than as a delegate with nothing to say.
    #[test]
    fn reading_an_agent_this_daemon_never_started_is_refused() {
        let root = std::env::temp_dir().join(format!("waku-delegate-unknown-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = a_backend_in(&root);

        let refused = backend
            .delegate_report(Uuid::new_v4(), Uuid::new_v4())
            .expect_err("an unknown agent has no state to report");
        assert!(
            format!("{refused}").contains("not started by this conversation"),
            "the refusal has to name the reason: {refused}"
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stale_runtime_projection_keeps_newer_transcript_cursor() {        let runtime_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.status = SessionStatus::Working;
        existing.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 10,
        });
        existing.push_message(crate::model::MessageRole::Assistant, "complete so far");

        let mut stale = existing.clone();
        stale.title = "Renamed elsewhere".into();
        stale.messages.clear();
        stale.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 7,
        });

        assert!(session_projection_precedes(
            &existing,
            &stale,
            Some(runtime_id)
        ));
        merge_stale_session_metadata(&mut existing, stale);
        assert_eq!(existing.title, "Renamed elsewhere");
        assert_eq!(existing.messages.len(), 1);
        assert_eq!(existing.runtime_event_cursor.unwrap().sequence, 10);
    }

    #[test]
    fn client_projection_cannot_replace_a_daemon_checkpoint() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.begin_turn("change it");
        existing.finish_active_turn(crate::model::TurnStatus::Completed, None);
        let checkpoint = Checkpoint {
            turn_count: 1,
            git_ref: "refs/waku/canonical".into(),
            status: CheckpointStatus::Ready,
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            created_at: 1,
        };
        existing.turns[0].checkpoint = Some(checkpoint.clone());

        let mut incoming = existing.clone();
        incoming.turns[0].checkpoint = Some(Checkpoint {
            git_ref: "refs/waku/stale-client".into(),
            ..checkpoint.clone()
        });
        preserve_daemon_checkpoints(&existing, &mut incoming);

        assert_eq!(incoming.turns[0].checkpoint.as_ref(), Some(&checkpoint));
    }

    #[test]
    fn response_fork_titles_follow_one_numbered_sequence() {
        assert_eq!(
            next_response_fork_title("Fix the bug", ["Fix the bug"]),
            "Fix the bug (2)"
        );
        assert_eq!(
            next_response_fork_title(
                "Fix the bug (2)",
                ["Fix the bug", "Fix the bug (2)", "Fix the bug (4)"]
            ),
            "Fix the bug (5)"
        );
        assert_eq!(
            next_response_fork_title("Plan (2026)", ["Plan (2026)"]),
            "Plan (2026) (2)"
        );
    }

    #[test]
    fn message_rewind_requires_a_settled_user_turn_and_provider_cursor() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.begin_turn("change it");
        session.mark_active_turn_provider_started();
        session.provider_cursor = Some(ProviderResumeCursor::Codex {
            thread_id: "thread".into(),
        });
        session.finish_active_turn(crate::model::TurnStatus::Completed, None);

        assert!(validate_message_rewind(&session, 1).is_ok());

        let mut busy = session.clone();
        busy.status = SessionStatus::Working;
        assert!(validate_message_rewind(&busy, 1).is_err());

        let mut missing_cursor = session.clone();
        missing_cursor.provider_cursor = None;
        assert!(validate_message_rewind(&missing_cursor, 1).is_err());

        let mut missing_message = session;
        missing_message.messages.clear();
        assert!(validate_message_rewind(&missing_message, 1).is_err());
    }

    #[test]
    fn wire_event_round_trip_preserves_ordered_delta_payload() {
        let wire = event_to_wire(DriverEvent::TextDelta("hello".into())).unwrap();
        assert_eq!(wire.kind, "textDelta");
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::TextDelta(text) if text == "hello"
        ));
    }

    #[test]
    fn wire_event_round_trip_preserves_prompt_submission_identity() {
        let turn_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let wire = event_to_wire(DriverEvent::PromptSubmitted {
            message: "ship it".into(),
            turn_id,
            message_id,
        })
        .unwrap();
        assert_eq!(wire.kind, "promptSubmitted");
        assert_eq!(wire.payload["message"], "ship it");
        assert_eq!(wire.payload["turnId"], turn_id.to_string());
        assert_eq!(wire.payload["messageId"], message_id.to_string());
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::PromptSubmitted { message, turn_id: decoded_turn, message_id: decoded_message }
                if message == "ship it" && decoded_turn == turn_id && decoded_message == message_id
        ));
    }

    /// A delegate spawn without an identity has to stay valid: it is what every
    /// install with no identities defined does, and what a lead does when it
    /// decides a task needs no particular agent.
    #[test]
    fn a_spawn_without_an_identity_deserializes() {
        let command: Command =
            serde_json::from_value(serde_json::json!({"type": "spawnDelegate", "task": "go"}))
                .expect("an anonymous spawn has to decode");
        assert!(matches!(
            command,
            Command::SpawnDelegate {
                persona: None,
                ..
            }
        ));
    }

    /// The identity travels as an id, and an older client that never sends the
    /// field must not produce a malformed command.
    #[test]
    fn a_spawn_naming_an_identity_carries_its_id() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "type": "spawnDelegate",
            "task": "review the diff",
            "persona": "reviewer",
        }))
        .expect("a named spawn has to decode");
        assert!(matches!(
            command,
            Command::SpawnDelegate {
                persona: Some(ref id),
                ..
            } if id == "reviewer"
        ));
    }

    /// A team spawn has to decode from what a bridge actually sends: a team id,
    /// the shared task, and an optional title.
    #[test]
    fn a_team_spawn_decodes_from_its_wire_form() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "type": "spawnTeam",
            "team": "review-crew",
            "task": "audit the release",
        }))
        .expect("a team spawn has to decode");
        assert!(matches!(
            command,
            Command::SpawnTeam {
                ref team,
                ref task,
                title: None,
            } if team == "review-crew" && task == "audit the release"
        ));
    }

    /// A member that failed carries its own reason, so the caller can tell
    /// which roles it actually has running.
    #[test]
    fn a_failed_team_member_reports_its_own_reason() {
        let delegate = waku_protocol::SpawnedDelegate {
            session_id: Uuid::nil(),
            runtime_id: Uuid::nil(),
            persona: "reviewer".into(),
            error: Some("no agent identity named reviewer is defined for this project".into()),
        };
        assert_eq!(delegate.persona, "reviewer");
        assert!(
            delegate
                .error
                .as_deref()
                .is_some_and(|error| error.contains("reviewer")),
            "the report has to name the member that failed: {delegate:?}"
        );
    }
}

