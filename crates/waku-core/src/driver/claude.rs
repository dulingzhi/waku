//! Claude Code's streaming-input session.
//!
//! `claude` accepts a realtime stream of user messages on stdin and answers on
//! stdout, which is the same transport the Claude Agent SDK's `query()` drives —
//! the SDK is a wrapper around these flags, not a separate capability. One
//! process serves the whole conversation, and `--permission-prompt-tool stdio`
//! makes it ask the host before running a tool instead of deciding alone.
//! A user message written mid-turn is folded into the running turn at the next
//! model call rather than queued as a turn of its own, which is what makes
//! steering a plain write.
//!
//! Flags and payloads here were read off the real CLI and the SDK's own
//! invocation, not guessed. `--permission-prompt-tool` in particular is absent
//! from `claude --help`.

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::Read;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, BackgroundWorkEvent, BackgroundWorkItem, BackgroundWorkKey, BackgroundWorkKind,
    BackgroundWorkStatus, DriverEvent, PermissionOption, ProviderResumeCursor, RuntimeMode,
    UserInputAnswer, UserInputOption, UserInputQuestion, unix_time_millis,
};

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        input: Value,
        answers: Vec<UserInputAnswer>,
    },
    Options(SessionOptions),
    StopBackgroundWork {
        key: BackgroundWorkKey,
        control_id: String,
    },
    Shutdown,
}

/// The stream-json user message both prompts and steers are delivered as.
fn user_message_payload(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        },
        "parent_tool_use_id": null
    })
}

fn stop_task_request(request_id: u64, task_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": format!("waku-{request_id}"),
        "request": {"subtype": "stop_task", "task_id": task_id}
    })
}

pub struct ClaudeDriver {
    commands: Sender<CommandMessage>,
    pending_user_inputs: Arc<Mutex<HashMap<String, Value>>>,
    mode: RuntimeMode,
}

/// The permission posture Claude is launched with.
fn permission_mode(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Ask => "default",
        RuntimeMode::AutoAcceptEdits => "acceptEdits",
        RuntimeMode::Auto => "auto",
        RuntimeMode::FullAccess => "bypassPermissions",
    }
}

/// The model id to hand the CLI for a session's context-window choice.
///
/// Claude Code serves 200K by default and reaches its 1M window through a
/// `[1m]` suffix on the model id rather than a flag, so the window travels with
/// `--model` and with mid-session `set_model` requests.
fn wire_model(model: Option<&str>, context_window: Option<&str>) -> Option<String> {
    let model = model?;
    if context_window == Some("1m") && !model.ends_with("[1m]") {
        Some(format!("{model}[1m]"))
    } else {
        Some(model.to_owned())
    }
}

/// Claude Code titles the session itself, on Haiku, in a request it fires
/// alongside the first turn's own first model call, so the title reaches the
/// native transcript about three seconds in — long before the turn it belongs
/// to settles. Reading it only when `result` arrives, as the turn-end pass
/// does, leaves an agentic first turn showing the truncated prompt for its
/// whole run and an interrupted one showing it forever. One look after five
/// seconds catches it, with a second as insurance; a schedule that runs dry
/// re-arms on the next prompt, and the turn-end pass still owns later
/// retitles and the rewind cursor.
fn start_claude_title_refresh(
    title_refresh: &super::title_refresh::NativeTitleRefresh,
    session_id: &str,
    events: &DriverEventSender,
) {
    let session_id = session_id.to_owned();
    title_refresh.start(
        "waku-claude-title",
        vec![Duration::from_secs(5), Duration::from_secs(10)],
        events.clone(),
        move || crate::claude_session::session_metadata(&session_id).map(|native| native.title),
    );
}

/// The MCP server name Claude sees for Waku's delegation tools.
const AGENT_BRIDGE_SERVER_NAME: &str = "waku_agents";

/// Registers Waku's agent bridge for this one Claude session.
///
/// Claude has no global config this build can rewrite per task — its MCP
/// servers live in a shared user file — so the bridge travels as inline
/// `--mcp-config` JSON instead. Inline rather than a temp file because the
/// config is rebuilt for every launch and a file would need its own cleanup
/// path; version 2.1.330 accepts the JSON form directly.
///
/// The credential is **not** in that JSON. Claude passes its own environment to
/// the servers it starts, so the daemon's address and the scoped token ride in
/// the provider's environment, where they never appear in a process listing the
/// way an argv entry would.
fn configure_agent_bridge(command: &mut Command, bridge: &crate::driver::AgentBridgeAccess) {
    // `waku --agent-bridge` rather than a standalone helper: a package ships one
    // executable, which is the same rule the Computer Use helpers follow.
    //
    // Resolved through the Computer Use helper path rather than `current_exe`,
    // because this runs inside the daemon: a packaged build launches the daemon
    // as `waku --daemon`, but a development build runs a separate `waku-daemon`
    // binary whose own path has no `--agent-bridge` mode at all. The daemon
    // passes `APP_EXECUTABLE_ENV` for exactly this reason, and it is the only
    // thing that points at the program that can serve the bridge.
    let Ok(executable) = crate::computer_use::host_executable_path() else {
        // Without a path to that executable there is nothing to point Claude at.
        // Failing the whole launch would strand a session over an optional tool,
        // so the bridge is simply absent.
        return;
    };
    let config = serde_json::json!({
        "mcpServers": {
            AGENT_BRIDGE_SERVER_NAME: {
                "command": executable.display().to_string(),
                "args": ["--agent-bridge"],
            }
        }
    });
    command
        .args(["--mcp-config", &config.to_string()])
        .env(waku_protocol::DAEMON_ADDRESS_ENV, &bridge.address)
        .env(waku_protocol::DAEMON_TOKEN_ENV, &bridge.token)
        // Both ids come from the capability, never from this driver's own
        // session id: that one is Claude's provider conversation, and a bridge
        // told to drive *that* is refused by the daemon on every call. Taking
        // them from one place is what makes the mistake impossible to repeat.
        .env(
            waku_protocol::DAEMON_SESSION_ENV,
            bridge.session_id.to_string(),
        )
        .env(
            waku_protocol::DAEMON_RUNTIME_ENV,
            bridge.runtime_id.to_string(),
        );
}

fn configure_stream_command(command: &mut Command, mode: RuntimeMode) {
    command.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        // Newer Claude models omit readable thinking text unless the caller
        // explicitly asks for a summary. Raw reasoning remains provider-private.
        "--thinking-display",
        "summarized",
        // Echoes each user message back, which is how a queued prompt is
        // distinguished from one the agent has started on.
        "--replay-user-messages",
        // Without this, a subagent's stream carries only its tool_use and
        // tool_result blocks — enough to see what it ran, never what it said.
        // With it, its text and thinking blocks arrive too, tagged with the
        // `parent_tool_use_id` of the call that started it, at every nesting
        // depth. That is what makes a subagent's conversation readable, and
        // readable is the precondition for steering one.
        "--forward-subagent-text",
        // Undocumented, and the whole reason Supervised can mean supervised:
        // without it the CLI decides permissions itself and only reports
        // denials after the fact.
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        permission_mode(mode),
    ]);
    if mode == RuntimeMode::FullAccess {
        command.arg("--dangerously-skip-permissions");
    }
}

impl ClaudeDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window,
            agent_preset: _,
            agent_bridge,
            computer_use_enabled: _,
            provider_cursor,
            system_prompt,
        } = options;
        let (resume_session_id, resume_at) = match provider_cursor {
            Some(ProviderResumeCursor::Claude {
                session_id,
                resume_at,
            }) => ((!session_id.is_empty()).then_some(session_id), resume_at),
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume Claude Code from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            None => (None, None),
        };
        // Claude accepts a caller-chosen session id, so the cursor exists before
        // the first turn does and a rewind has something to point at.
        let session_id = resume_session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut command = crate::command_env::command(&binary);
        command.current_dir(&cwd);
        configure_stream_command(&mut command, mode);
        let launch_model = wire_model(model.as_deref(), context_window.as_deref());
        if let Some(model) = launch_model.as_deref() {
            command.args(["--model", model]);
        }
        if let Some(effort) = reasoning_effort.as_deref() {
            command.args(["--effort", effort]);
        }
        if resume_session_id.is_some() {
            command.args(["--resume", &session_id]);
        } else {
            command.args(["--session-id", &session_id]);
        }
        // Only a lead conversation is launched with a bridge: the daemon mints
        // the capability, so an ordinary task has none and never offers these
        // tools at all.
        if let Some(bridge) = agent_bridge.as_ref() {
            configure_agent_bridge(&mut command, bridge);
        }
        // An identity rides on the provider's own system prompt, which is the
        // only place a rule is stated to Claude rather than proposed to it.
        if let Some(prompt) = system_prompt.as_deref()
            && !prompt.trim().is_empty()
        {
            command.args(["--append-system-prompt", prompt]);
        }

        let command = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .context("failed to start `claude` in streaming-input mode")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Claude stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Claude stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Claude stderr unavailable"))?;

        // The cursor is known up front, so a rewind can address this session
        // before it has produced anything.
        let _ = events.send(DriverEvent::Connected {
            provider_cursor: Some(ProviderResumeCursor::Claude {
                session_id: session_id.clone(),
                resume_at,
            }),
        });

        let (commands, command_rx) = unbounded();
        let auto_approve = mode != RuntimeMode::Ask;
        let turn_active = Arc::new(Mutex::new(false));
        let pending_task_stops = Arc::new(Mutex::new(HashMap::<String, BackgroundWorkKey>::new()));
        let pending_user_inputs = Arc::new(Mutex::new(HashMap::<String, Value>::new()));

        let reader_events = events.clone();
        let reader_commands = commands.clone();
        let reader_turn = turn_active.clone();
        let reader_session = session_id.clone();
        let reader_pending_task_stops = pending_task_stops.clone();
        let reader_pending_user_inputs = pending_user_inputs.clone();
        let reader_thread = thread::Builder::new()
            .name("waku-claude-reader".into())
            .spawn(move || {
                let mut state = ClaudeStreamState {
                    pending_task_stops: reader_pending_task_stops,
                    pending_user_inputs: reader_pending_user_inputs,
                    ..ClaudeStreamState::default()
                };
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    handle_message(
                        &value,
                        &reader_session,
                        &reader_events,
                        &reader_commands,
                        &reader_turn,
                        auto_approve,
                        &mut state,
                    );
                }
            })?;

        let writer_events = events.clone();
        let writer_turn = turn_active;
        let writer_pending_task_stops = pending_task_stops;
        let writer_title_refresh = super::title_refresh::NativeTitleRefresh::default();
        let title_session_id = session_id;
        thread::Builder::new()
            .name("waku-claude-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut next_request_id = 0_u64;
                let mut current_model = launch_model;
                while let Ok(message) = command_rx.recv() {
                    let written = match message {
                        CommandMessage::Prompt(text) => {
                            *writer_turn.lock() = true;
                            start_claude_title_refresh(
                                &writer_title_refresh,
                                &title_session_id,
                                &writer_events,
                            );
                            let _ = writer_events.send(DriverEvent::TurnStarted);
                            write_line(&mut stdin, &user_message_payload(&text))
                        }
                        CommandMessage::Steer(text) => {
                            // A mid-turn user message is held by the CLI and
                            // folded into the running turn at the next model
                            // call — one `result` settles everything, and the
                            // isReplay echo marks the moment it is absorbed.
                            // Verified against the real CLI (2.1.223). Unlike
                            // Amp, no marker is needed: folding is the default.
                            if !*writer_turn.lock() {
                                let _ = writer_events.send(DriverEvent::SteerRejected {
                                    message: text,
                                    reason: tr!(
                                        "errors.provider_no_active_turn",
                                        provider = "Claude"
                                    ),
                                });
                                continue;
                            }
                            // No TurnStarted and no turn re-arm: the turn the
                            // message joins is already running.
                            let written = write_line(&mut stdin, &user_message_payload(&text));
                            match &written {
                                Ok(()) => {
                                    let _ = writer_events
                                        .send(DriverEvent::SteerAccepted { message: text });
                                }
                                Err(error) => {
                                    let _ = writer_events.send(DriverEvent::SteerRejected {
                                        message: text,
                                        reason: tr!(
                                            "errors.provider_transport_write",
                                            provider = "Claude",
                                            error = error
                                        ),
                                    });
                                }
                            }
                            // A failed write still falls through to the shared
                            // transport-failure path: the running turn cannot
                            // settle once stdin is gone.
                            written
                        }
                        CommandMessage::Cancel => {
                            next_request_id += 1;
                            write_line(
                                &mut stdin,
                                &json!({
                                    "type": "control_request",
                                    "request_id": format!("waku-{next_request_id}"),
                                    "request": {"subtype": "interrupt"}
                                }),
                            )
                        }
                        CommandMessage::Respond {
                            request_id,
                            option_id,
                        } => {
                            let decision = if option_id == "deny" {
                                json!({
                                    "behavior": "deny",
                                    "message": "The user denied this tool call."
                                })
                            } else {
                                json!({"behavior": "allow"})
                            };
                            write_line(
                                &mut stdin,
                                &json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "success",
                                        "request_id": request_id,
                                        "response": decision
                                    }
                                }),
                            )
                        }
                        CommandMessage::RespondUserInput {
                            request_id,
                            input,
                            answers,
                        } => {
                            let mut answer_values = serde_json::Map::new();
                            for answer in answers {
                                let multi_select = input
                                    .get("questions")
                                    .and_then(Value::as_array)
                                    .into_iter()
                                    .flatten()
                                    .find(|question| {
                                        question.get("question").and_then(Value::as_str)
                                            == Some(answer.question_id.as_str())
                                    })
                                    .and_then(|question| question.get("multiSelect"))
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                let value = if multi_select {
                                    json!(answer.answers)
                                } else {
                                    Value::String(
                                        answer.answers.into_iter().next().unwrap_or_default(),
                                    )
                                };
                                answer_values.insert(answer.question_id, value);
                            }
                            write_line(
                                &mut stdin,
                                &json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "success",
                                        "request_id": request_id,
                                        "response": {
                                            "behavior": "allow",
                                            "updatedInput": {
                                                "questions": input.get("questions").cloned().unwrap_or(Value::Array(Vec::new())),
                                                "answers": answer_values
                                            }
                                        }
                                    }
                                }),
                            )
                        }
                        CommandMessage::Options(options) => {
                            // The window rides on the model id, so switching it
                            // is the same `set_model` round trip as switching
                            // models — verified accepted by the CLI (2.1.228).
                            let next_model = wire_model(
                                options.model.as_deref(),
                                options.context_window.as_deref(),
                            );
                            if next_model == current_model {
                                continue;
                            }
                            current_model = next_model;
                            let Some(model) = current_model.as_deref() else {
                                continue;
                            };
                            next_request_id += 1;
                            write_line(
                                &mut stdin,
                                &json!({
                                    "type": "control_request",
                                    "request_id": format!("waku-{next_request_id}"),
                                    "request": {"subtype": "set_model", "model": model}
                                }),
                            )
                        }
                        CommandMessage::StopBackgroundWork { key, control_id } => {
                            next_request_id += 1;
                            let request_id = format!("waku-{next_request_id}");
                            writer_pending_task_stops
                                .lock()
                                .insert(request_id, key.clone());
                            write_line(&mut stdin, &stop_task_request(next_request_id, &control_id))
                        }
                        CommandMessage::Shutdown => break,
                    };
                    if let Err(error) = written {
                        let _ = writer_events.send(DriverEvent::Error(tr!(
                            "errors.provider_transport_write",
                            provider = "Claude",
                            error = error
                        )));
                        // Nothing will settle a turn whose prompt never landed.
                        if std::mem::take(&mut *writer_turn.lock()) {
                            let _ = writer_events.send(DriverEvent::TurnFinished {
                                success: false,
                                summary: Some(tr!(
                                    "errors.provider_receive_prompt",
                                    provider = "Claude"
                                )),
                            });
                        }
                        break;
                    }
                }
            })?;

        let last_visible_stderr = Arc::new(Mutex::new(None::<String>));
        let stderr_last_error = last_visible_stderr.clone();
        let stderr_events = events.clone();
        let stderr_thread = thread::Builder::new()
            .name("waku-claude-stderr".into())
            .spawn(move || {
                let lines = BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>();
                if let Some(message) = super::support::provider_stderr_error(lines) {
                    let error = format!("Claude Code: {message}");
                    *stderr_last_error.lock() = Some(error.clone());
                    let _ = stderr_events.send(DriverEvent::Error(error));
                }
            })?;

        thread::Builder::new()
            .name("waku-claude-process".into())
            .spawn(move || {
                let status = child.wait();
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                if let Ok(status) = status
                    && !status.success()
                    && last_visible_stderr.lock().is_none()
                {
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.provider_exited",
                        provider = "Claude Code",
                        status = status
                    )));
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            commands,
            pending_user_inputs,
            mode,
        })
    }
}

impl DriverControl for ClaudeDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn supports_steer(&self) -> bool {
        true
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn stop_background_work(&self, key: BackgroundWorkKey, control_id: String) {
        let _ = self
            .commands
            .send(CommandMessage::StopBackgroundWork { key, control_id });
    }

    fn respond(&self, request_id: String, option_id: String) {
        let _ = self.commands.send(CommandMessage::Respond {
            request_id,
            option_id,
        });
    }

    fn respond_user_input(&self, request_id: String, answers: Vec<UserInputAnswer>) {
        let Some(input) = self.pending_user_inputs.lock().remove(&request_id) else {
            return;
        };
        let _ = self.commands.send(CommandMessage::RespondUserInput {
            request_id,
            input,
            answers,
        });
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        // The model has a setter; the permission posture is a launch flag, and
        // changing what a running agent may touch deserves a fresh session.
        if options.mode != self.mode {
            return false;
        }
        self.commands.send(CommandMessage::Options(options)).is_ok()
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        Err(anyhow!(
            "conversation rollback is not supported by this provider transport"
        ))
    }
}

impl Drop for ClaudeDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

fn write_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Default)]
struct ClaudeStreamState {
    saw_text_delta: bool,
    saw_reasoning_delta: bool,
    /// Tool-use id → transcript presentation plus the full shell command.
    /// Claude's later `task_started` event links back by id but does not repeat
    /// the command, so keep it here until the matching tool result arrives.
    tools: HashMap<String, (ActivityKind, String, String, Option<String>)>,
    background_task_kinds: HashMap<String, BackgroundWorkKind>,
    /// Detached, non-ambient tasks the CLI will wake this session for when
    /// they settle. Replaced wholesale by each `background_tasks_changed`
    /// level signal and kept current by the task bookends between them.
    live_tasks: HashSet<String>,
    /// The prompted turn's reply ended while `live_tasks` was non-empty: the
    /// turn is held open for the wake instead of being reported finished.
    parked: bool,
    /// Task tool-use id → task id, so a subagent's own messages — they arrive
    /// on the main channel with `parent_tool_use_id` set — can be routed into
    /// that task's output pane instead of this session's transcript.
    subagent_tasks: HashMap<String, String>,
    /// The item each task last reported through its own bookends.
    ///
    /// `background_tasks_changed` says only which tasks can still wake the
    /// session: it repeats no status, no origin and no title. It also keeps
    /// arriving after a task settles. The bookends own those fields, so this
    /// is what the level signal reuses instead of rebuilding an entry from
    /// scratch — which used to reopen settled subagents and drop the tool call
    /// that ties a card to its transcript.
    live_task_items: HashMap<String, BackgroundWorkItem>,
    /// The description each task started with. Progress events reuse the
    /// `description` field for the agent's current activity line, which
    /// belongs in the detail row, never the title.
    task_descriptions: HashMap<String, String>,
    /// Tasks whose output pane already carries streamed transcript; the
    /// settle notification's summary would only duplicate it.
    streamed_task_output: HashSet<String>,
    /// Stop handles for native Bash output files currently being tailed.
    task_output_tails: ClaudeTaskOutputTails,
    pending_task_stops: Arc<Mutex<HashMap<String, BackgroundWorkKey>>>,
    pending_user_inputs: Arc<Mutex<HashMap<String, Value>>>,
    /// Model of the latest main-thread assistant message, so the settled
    /// turn's `modelUsage` map can be read for that model's context window
    /// rather than a subagent's.
    last_assistant_model: Option<String>,
    /// Last title copied from Claude's native transcript metadata.
    last_auto_title: Option<String>,
}

#[derive(Default)]
struct ClaudeTaskOutputTails(HashMap<String, Arc<AtomicBool>>);

impl Drop for ClaudeTaskOutputTails {
    fn drop(&mut self) {
        for stop in self.0.values() {
            stop.store(true, Ordering::Release);
        }
    }
}

#[cfg(unix)]
const CLAUDE_TASK_OUTPUT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Claude keeps live Bash output outside its JSON stream. The installed CLI
/// writes it under `/tmp/claude-<uid>/<workspace>/<session>/tasks/<id>.output`;
/// locate the workspace component instead of reproducing Claude's private cwd
/// escaping rules.
#[cfg(unix)]
fn claude_task_output_path(session_id: &str, task_id: &str) -> Option<PathBuf> {
    // SAFETY: `geteuid` has no preconditions and does not retain pointers.
    let uid = unsafe { libc::geteuid() };
    claude_task_output_path_in(
        &Path::new("/tmp").join(format!("claude-{uid}")),
        session_id,
        task_id,
    )
}

#[cfg(unix)]
fn claude_task_output_path_in(root: &Path, session_id: &str, task_id: &str) -> Option<PathBuf> {
    if task_id.is_empty()
        || task_id.contains('/')
        || task_id.contains('\\')
        || session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
    {
        return None;
    }
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| {
            entry
                .path()
                .join(session_id)
                .join("tasks")
                .join(format!("{task_id}.output"))
        })
        .find(|path| path.is_file())
}

#[cfg(unix)]
fn drain_utf8_output(bytes: &mut Vec<u8>, final_read: bool) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => String::from_utf8(std::mem::take(bytes)).ok(),
        Err(error) => {
            let valid = error.valid_up_to();
            let invalid = error.error_len().is_some();
            if invalid || final_read {
                let text = String::from_utf8_lossy(bytes).into_owned();
                bytes.clear();
                return Some(text);
            }
            (valid > 0).then(|| {
                String::from_utf8(bytes.drain(..valid).collect())
                    .expect("the UTF-8 validator marked this prefix valid")
            })
        }
    }
}

#[cfg(unix)]
fn stream_claude_task_output(
    path: PathBuf,
    key: BackgroundWorkKey,
    events: DriverEventSender,
    stop: Arc<AtomicBool>,
) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let mut pending_utf8 = Vec::new();
    let mut final_pass = false;
    loop {
        let mut chunk = Vec::new();
        if file.read_to_end(&mut chunk).is_err() {
            break;
        }
        pending_utf8.extend_from_slice(&chunk);
        let stopping = stop.load(Ordering::Acquire);
        if let Some(delta) = drain_utf8_output(&mut pending_utf8, stopping && final_pass)
            && !delta.is_empty()
        {
            let _ = events.send(DriverEvent::BackgroundWork(
                BackgroundWorkEvent::OutputDelta {
                    key: key.clone(),
                    delta,
                },
            ));
        }
        if stopping {
            if final_pass {
                break;
            }
            // The notification and the output-file close are adjacent but
            // originate on different tasks. One final poll closes that race.
            final_pass = true;
        }
        thread::sleep(CLAUDE_TASK_OUTPUT_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn stream_claude_task_output_when_ready(
    session_id: String,
    task_id: String,
    key: BackgroundWorkKey,
    events: DriverEventSender,
    stop: Arc<AtomicBool>,
) {
    loop {
        // Check for the file before honoring stop: if Claude creates and
        // completes a short task between polls, the final pass still captures
        // its output for the retained background-process entry.
        if let Some(path) = claude_task_output_path(&session_id, &task_id) {
            stream_claude_task_output(path, key, events, stop);
            return;
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(CLAUDE_TASK_OUTPUT_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn start_claude_task_output_tail(
    session_id: &str,
    item: &BackgroundWorkItem,
    events: &DriverEventSender,
    state: &mut ClaudeStreamState,
) {
    if item.key.kind != BackgroundWorkKind::Process
        || state
            .task_output_tails
            .0
            .contains_key(&item.key.provider_id)
    {
        return;
    }
    let session_id = session_id.to_owned();
    let task_id = item.key.provider_id.clone();
    let key = item.key.clone();
    let events = events.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_task_id = task_id.clone();
    let spawned = thread::Builder::new()
        .name("waku-claude-task-output".into())
        .spawn(move || {
            stream_claude_task_output_when_ready(
                session_id,
                thread_task_id,
                key,
                events,
                thread_stop,
            )
        });
    if spawned.is_ok() {
        state.task_output_tails.0.insert(task_id, stop);
    }
}

#[cfg(not(unix))]
fn start_claude_task_output_tail(
    _session_id: &str,
    _item: &BackgroundWorkItem,
    _events: &DriverEventSender,
    _state: &mut ClaudeStreamState,
) {
}

/// The context window of the model that served this turn, from the result
/// message's per-model usage map. Falls back to the largest reported window
/// when the model key does not line up.
fn context_window_from_result(value: &Value, last_model: Option<&str>) -> Option<u64> {
    let models = value.get("modelUsage")?.as_object()?;
    if let Some(model) = last_model {
        for (key, entry) in models {
            if key == model || entry.get("canonicalModel").and_then(Value::as_str) == Some(model) {
                return entry.get("contextWindow").and_then(Value::as_u64);
            }
        }
    }
    models
        .values()
        .filter_map(|entry| entry.get("contextWindow").and_then(Value::as_u64))
        .max()
}

/// Housekeeping the CLI runs for itself — `skip_transcript` tasks and the
/// live-update watchers it arms on its own. The wire marks them `ambient`
/// and asks hosts to keep them out of activity indicators; letting one into
/// the registry would also park the session on work the user never started.
fn claude_task_is_ambient(value: &Value) -> bool {
    value.get("ambient").and_then(Value::as_bool) == Some(true)
}

fn claude_task_id(value: &Value) -> Option<&str> {
    value
        .get("task_id")
        .or_else(|| value.get("taskId"))
        .or_else(|| value.pointer("/task/id"))
        .and_then(Value::as_str)
}

fn claude_task_status(value: &Value) -> BackgroundWorkStatus {
    let status = value
        .get("status")
        .or_else(|| value.pointer("/task/status"))
        .or_else(|| value.pointer("/patch/status"))
        .and_then(Value::as_str)
        .unwrap_or("running")
        .to_ascii_lowercase();
    match status.as_str() {
        "pending" | "starting" => BackgroundWorkStatus::Starting,
        "completed" | "complete" | "succeeded" | "success" => BackgroundWorkStatus::Completed,
        "failed" | "errored" | "error" => BackgroundWorkStatus::Failed,
        "killed" | "cancelled" | "canceled" | "stopped" | "interrupted" => {
            BackgroundWorkStatus::Stopped
        }
        _ => BackgroundWorkStatus::Running,
    }
}

fn claude_task_kind(value: &Value, state: &ClaudeStreamState) -> BackgroundWorkKind {
    if let Some(task_id) = claude_task_id(value)
        && let Some(kind) = state.background_task_kinds.get(task_id)
    {
        return *kind;
    }
    let task_type = value
        .get("task_type")
        .or_else(|| value.get("taskType"))
        .or_else(|| value.get("subagent_type"))
        .or_else(|| value.get("subagentType"))
        .or_else(|| value.get("agentType"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if task_type == "monitor" {
        return BackgroundWorkKind::Monitor;
    }
    if task_type.contains("agent")
        || task_type.contains("teammate")
        || task_type.contains("workflow")
        || task_type == "remote"
    {
        return BackgroundWorkKind::Subagent;
    }
    let tool_use_id = value
        .get("tool_use_id")
        .or_else(|| value.get("toolUseId"))
        .and_then(Value::as_str);
    if tool_use_id
        .and_then(|id| state.tools.get(id))
        .is_some_and(|(_, _, wire_name, _)| wire_name.eq_ignore_ascii_case("monitor"))
    {
        BackgroundWorkKind::Monitor
    } else {
        BackgroundWorkKind::Process
    }
}

/// Progress summaries and errors may span paragraphs; the detail row under a
/// task is a one-liner.
fn one_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut short = line.chars().take(160).collect::<String>();
    if short.len() < line.len() {
        short.push('…');
    }
    Some(short)
}

fn claude_task_item(
    subtype: &str,
    value: &Value,
    state: &ClaudeStreamState,
) -> Option<BackgroundWorkItem> {
    let task_id = claude_task_id(value)?.to_owned();
    if claude_task_is_ambient(value) {
        return None;
    }
    let kind = claude_task_kind(value, state);
    let wire_description = value
        .get("description")
        .or_else(|| value.get("subject"))
        .or_else(|| value.get("workflow_name"))
        .or_else(|| value.get("workflowName"))
        .or_else(|| value.pointer("/task/description"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    // Only the start names a task; on later events `description` is the
    // agent's current activity line ("Running …") and `summary` can be the
    // whole final report. An empty title keeps the stored one on upsert.
    let title = if subtype == "task_started" {
        wire_description
            .map(str::to_owned)
            .unwrap_or_else(|| match kind {
                BackgroundWorkKind::Subagent => tr!("background.subagent"),
                BackgroundWorkKind::Monitor => tr!("background.monitor"),
                BackgroundWorkKind::Process => tr!("background.process"),
            })
    } else {
        value
            .pointer("/patch/description")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .unwrap_or_default()
    };
    let mut status = claude_task_status(value);
    if status == BackgroundWorkStatus::Running && kind == BackgroundWorkKind::Monitor {
        status = BackgroundWorkStatus::Monitoring;
    }
    let mut item = BackgroundWorkItem::new(kind, task_id.clone(), title, status);
    item.background = value
        .get("is_backgrounded")
        .or_else(|| value.get("isBackgrounded"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    item.can_stop = status.is_live();
    item.origin_activity_id = value
        .get("tool_use_id")
        .or_else(|| value.get("toolUseId"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    item.role = value
        .get("subagent_type")
        .or_else(|| value.get("subagentType"))
        .or_else(|| value.get("agentType"))
        .or_else(|| value.get("task_type"))
        .or_else(|| value.get("taskType"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    item.model = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Subagent prompts ride `task_started`, but detached Bash tasks only link
    // back to the originating tool call by id. Preserve either as the detail
    // surface's Prompt/Command value.
    item.command = value
        .get("prompt")
        .or_else(|| value.get("command"))
        .or_else(|| value.pointer("/task/command"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            item.origin_activity_id
                .as_deref()
                .and_then(|id| state.tools.get(id))
                .and_then(|(_, _, _, command)| command.clone())
        });
    // The settle notification's summary is the subagent's final report and
    // belongs in the output pane — unless the live transcript already
    // streamed there.
    if subtype == "task_notification"
        && kind == BackgroundWorkKind::Subagent
        && !state.streamed_task_output.contains(&task_id)
    {
        item.output = value
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
    }
    let progress_line = (subtype != "task_notification")
        .then(|| value.get("summary").and_then(Value::as_str))
        .flatten()
        .filter(|text| !text.is_empty())
        .or_else(|| {
            (subtype != "task_started")
                .then_some(wire_description)
                .flatten()
                .filter(|text| {
                    state
                        .task_descriptions
                        .get(&task_id)
                        .is_none_or(|original| original != text)
                })
        });
    item.detail = progress_line
        .or_else(|| {
            value
                .get("last_tool_name")
                .or_else(|| value.get("lastToolName"))
                .or_else(|| value.get("error"))
                .or_else(|| value.pointer("/patch/error"))
                .or_else(|| value.get("output_file"))
                .or_else(|| value.get("outputFile"))
                .and_then(Value::as_str)
        })
        .and_then(one_line);
    item.control_id = Some(task_id);
    item.duration_ms = value
        .pointer("/usage/duration_ms")
        .or_else(|| value.get("duration_ms"))
        .and_then(Value::as_u64);
    item.updated_at_ms = unix_time_millis();
    Some(item)
}

fn handle_claude_system(
    value: &Value,
    session_id: &str,
    events: &DriverEventSender,
    state: &mut ClaudeStreamState,
) {
    let subtype = value.get("subtype").and_then(Value::as_str);
    if subtype == Some("background_tasks_changed") {
        let tasks = value
            .get("background_tasks")
            .or_else(|| value.get("backgroundTasks"))
            .or_else(|| value.get("tasks"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // The level signal is authoritative for what can still wake the
        // session; ambient housekeeping never parks a turn.
        state.live_tasks = tasks
            .iter()
            .filter(|entry| !claude_task_is_ambient(entry))
            .filter_map(|entry| {
                entry
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| claude_task_id(entry).map(str::to_owned))
            })
            .collect();
        let items = tasks
            .iter()
            .filter_map(|entry| {
                if claude_task_is_ambient(entry) {
                    return None;
                }
                let bare_id = entry.as_str();
                let task_id = bare_id
                    .map(str::to_owned)
                    .or_else(|| claude_task_id(entry).map(str::to_owned))?;
                let known_kind = state.background_task_kinds.get(&task_id).copied();
                // The level signal normally precedes task_started. Bare IDs
                // carry no kind, so wait for that edge instead of briefly
                // inventing a Process entry beside the real Subagent entry.
                let kind = known_kind
                    .or_else(|| bare_id.is_none().then(|| claude_task_kind(entry, state)))?;
                // This signal only says *what can still wake the session*. It
                // repeats neither a status nor the originating tool call, and
                // it keeps arriving after a task settles, so building each
                // entry from scratch would reopen settled subagents on every
                // signal and drop the tool call that ties a card to its
                // transcript. The bookends own the status; this owns presence.
                let known = state
                    .live_task_items
                    .get(&task_id)
                    .filter(|known| known.key.kind == kind);
                let status = known
                    .map(|known| known.status)
                    .unwrap_or_else(|| match kind {
                        BackgroundWorkKind::Monitor => BackgroundWorkStatus::Monitoring,
                        BackgroundWorkKind::Process | BackgroundWorkKind::Subagent => {
                            BackgroundWorkStatus::Running
                        }
                    });
                // The description carries the task's own label, so keep it
                // rather than replacing a real title with the kind's name.
                let title = known
                    .map(|known| known.title.clone())
                    .filter(|title| !title.is_empty())
                    .or_else(|| {
                        entry
                            .get("description")
                            .or_else(|| entry.get("subject"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| match kind {
                        BackgroundWorkKind::Subagent => tr!("background.subagent"),
                        BackgroundWorkKind::Monitor => tr!("background.monitor"),
                        BackgroundWorkKind::Process => tr!("background.process"),
                    });
                let mut item = BackgroundWorkItem::new(kind, task_id.clone(), title, status);
                item.background = true;
                item.can_stop = status.is_stoppable();
                item.control_id = Some(task_id.clone());
                item.origin_activity_id = known
                    .and_then(|known| known.origin_activity_id.clone())
                    .or_else(|| {
                        entry
                            .get("tool_use_id")
                            .or_else(|| entry.get("toolUseId"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                item.role = known.and_then(|known| known.role.clone()).or_else(|| {
                    entry
                        .get("subagent_type")
                        .or_else(|| entry.get("subagentType"))
                        .or_else(|| entry.get("task_type"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
                item.model = known.and_then(|known| known.model.clone());
                item.command = known.and_then(|known| known.command.clone());
                item.updated_at_ms = unix_time_millis();
                Some(item)
            })
            .collect();
        let _ = events.send(DriverEvent::BackgroundWork(
            BackgroundWorkEvent::ReconcileLive { items },
        ));
        return;
    }
    if let Some(subtype @ ("task_started" | "task_progress" | "task_updated" | "task_notification")) =
        subtype
        && let Some(item) = claude_task_item(subtype, value, state)
    {
        let task_id = item.key.provider_id.clone();
        state
            .background_task_kinds
            .insert(task_id.clone(), item.key.kind);
        // Remember what this task last reported so a later level signal can
        // reuse it: that signal carries none of these fields.
        match state.live_task_items.get_mut(&task_id) {
            Some(known) => {
                if !item.title.is_empty() {
                    known.title.clone_from(&item.title);
                }
                known.status = item.status;
                known.can_stop = item.can_stop;
                if item.detail.is_some() {
                    known.detail.clone_from(&item.detail);
                }
                if item.origin_activity_id.is_some() {
                    known
                        .origin_activity_id
                        .clone_from(&item.origin_activity_id);
                }
                if item.role.is_some() {
                    known.role.clone_from(&item.role);
                }
                if item.model.is_some() {
                    known.model.clone_from(&item.model);
                }
                if item.command.is_some() {
                    known.command.clone_from(&item.command);
                }
                known.updated_at_ms = item.updated_at_ms;
            }
            None => {
                state.live_task_items.insert(task_id.clone(), item.clone());
            }
        }
        if subtype == "task_started"
            && let Some(tool_use_id) = item.origin_activity_id.clone()
        {
            state.subagent_tasks.insert(tool_use_id, task_id.clone());
        }
        // The bookends keep the wake set current between level signals: a
        // detached start joins it, a settle leaves it, and a foreground task
        // joins when its patch backgrounds it (Ctrl-B's wire equivalent).
        let backgrounded_by_patch = value
            .pointer("/patch/is_backgrounded")
            .and_then(Value::as_bool)
            == Some(true);
        if !item.status.is_live() {
            state.live_tasks.remove(&task_id);
        } else if (subtype == "task_started" && item.background) || backgrounded_by_patch {
            state.live_tasks.insert(task_id.clone());
        }
        let output_tail_item = (subtype == "task_started"
            && item.key.kind == BackgroundWorkKind::Process)
            .then(|| item.clone());
        let stop_output_tail = !item.status.is_live();
        // Remember what the task is called: `task_started` names it, and a
        // `task_updated` patch is an explicit rename. Later activity lines
        // that merely repeat this stay out of the detail row.
        let described = match subtype {
            "task_started" => value.get("description"),
            _ => value.pointer("/patch/description"),
        };
        if let Some(description) = described
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            state
                .task_descriptions
                .insert(task_id.clone(), description.to_owned());
        }
        let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
            item,
        )));
        // Establish the registry item before its tail thread can enqueue the
        // first delta; an output-before-start race would otherwise discard it.
        if let Some(item) = output_tail_item {
            start_claude_task_output_tail(session_id, &item, events, state);
        } else if stop_output_tail && let Some(stop) = state.task_output_tails.0.remove(&task_id) {
            stop.store(true, Ordering::Release);
        }
    }
}

/// Renders a subagent message into its task's output pane: narrative text
/// as-is, tool calls as single `› tool · subject` lines.
fn forward_subagent_transcript(
    parent_tool_use_id: &str,
    value: &Value,
    events: &impl DriverEventSink,
    state: &mut ClaudeStreamState,
) {
    let Some(task_id) = state.subagent_tasks.get(parent_tool_use_id).cloned() else {
        return;
    };
    let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    let mut delta = String::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    delta.push_str(text);
                    delta.push_str("\n\n");
                }
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = block.get("input");
                let subject = activity::input_title(input).or_else(|| {
                    input.and_then(|input| {
                        [
                            "command",
                            "file_path",
                            "path",
                            "pattern",
                            "query",
                            "url",
                            "description",
                        ]
                        .into_iter()
                        .find_map(|key| input.get(key).and_then(Value::as_str))
                        .and_then(one_line)
                    })
                });
                match subject {
                    Some(subject) => delta.push_str(&format!("› {name} · {subject}\n")),
                    None => delta.push_str(&format!("› {name}\n")),
                }
            }
            _ => {}
        }
    }
    if delta.is_empty() {
        return;
    }
    let kind = state
        .background_task_kinds
        .get(&task_id)
        .copied()
        .unwrap_or(BackgroundWorkKind::Subagent);
    state.streamed_task_output.insert(task_id.clone());
    let _ = events.send(DriverEvent::BackgroundWork(
        BackgroundWorkEvent::OutputDelta {
            key: BackgroundWorkKey::new(kind, task_id),
            delta,
        },
    ));
}

/// Renders the main conversation's `SendMessage` into the addressed
/// subagent's pane, marked as a relay.
///
/// A relayed message has no wire echo of its own: the CLI folds it into the
/// subagent's next tool round, and the resumed turn's messages keep the
/// original `parent_tool_use_id`, so nothing on the subagent's side says these
/// words came from the parent. The parent's own `SendMessage` tool call is the
/// only place it is observable, which is why the mark has to be written here
/// rather than read off the subagent's transcript.
///
/// Returns whether the call addressed a known subagent. A message to the main
/// conversation — `/`, or a teammate outside this task registry — is left
/// alone.
fn forward_parent_relay(
    tool_use: &Value,
    events: &impl DriverEventSink,
    state: &ClaudeStreamState,
) -> bool {
    if tool_use.get("name").and_then(Value::as_str) != Some("SendMessage") {
        return false;
    }
    let Some(target) = tool_use.pointer("/input/to").and_then(Value::as_str) else {
        return false;
    };
    let Some(kind) = state
        .background_task_kinds
        .get(target)
        .copied()
        .filter(|kind| *kind == BackgroundWorkKind::Subagent)
    else {
        return false;
    };
    let Some(message) = tool_use
        .pointer("/input/message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return false;
    };
    // The heading sits on its own line and every relayed line is indented, so
    // a multi-line message cannot be mistaken for the subagent's own words.
    let mut delta = format!("{}\n", tr!("background.relay_heading"));
    for line in message.lines() {
        delta.push_str("  ");
        delta.push_str(line);
        delta.push('\n');
    }
    delta.push('\n');
    let _ = events.send(DriverEvent::BackgroundWork(
        BackgroundWorkEvent::OutputDelta {
            key: BackgroundWorkKey::new(kind, target),
            delta,
        },
    ));
    true
}

/// Claude Code wakes itself. When detached work — a backgrounded shell
/// command, a subagent, a monitor — settles after the model's reply ended,
/// the CLI folds its `<task-notification>` into a fresh model call without
/// any prompt from the host. Verified against 2.1.258: after the settling
/// `task_notification`, stdout carries a new `init`, the partial stream, the
/// assistant message and a `result`, and no user message is echoed. Model
/// output is the marker rather than `init`, so a resume handshake can never
/// open an empty turn.
///
/// A parked turn (see the `result` branch) is continued by its wake: the
/// host learns the model is running again through `TurnStarted`, on the turn
/// it already holds open. A wake that finds no turn at all — the host settled
/// the parked one itself — opens a fresh one the same way.
fn begin_unprompted_turn(
    turn_active: &Mutex<bool>,
    state: &mut ClaudeStreamState,
    events: &DriverEventSender,
) {
    let mut active = turn_active.lock();
    if *active && !state.parked {
        return;
    }
    *active = true;
    state.parked = false;
    let _ = events.send(DriverEvent::TurnStarted);
}

#[allow(clippy::too_many_arguments)]
fn handle_message(
    value: &Value,
    session_id: &str,
    events: &DriverEventSender,
    commands: &Sender<CommandMessage>,
    turn_active: &Mutex<bool>,
    auto_approve: bool,
    state: &mut ClaudeStreamState,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("system") => {
            // The init handshake carries the CLI's own command registry —
            // built-ins, custom commands, plugins and skills alike.
            if value.get("subtype").and_then(Value::as_str) == Some("init")
                && let Some(commands) = value.get("slash_commands").and_then(Value::as_array)
            {
                let commands = commands
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| crate::model::ReportedCommand {
                        name: name.to_owned(),
                        description: String::new(),
                    })
                    .collect::<Vec<_>>();
                if !commands.is_empty() {
                    let _ = events.send(DriverEvent::AvailableCommands(commands));
                }
            }
            handle_claude_system(value, session_id, events, state);
        }
        Some("control_request") => {
            if value.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool") {
                begin_unprompted_turn(turn_active, state, events);
                if !request_user_input(value, events, state) {
                    request_permission(value, events, commands, auto_approve);
                }
            }
        }
        Some("control_response") => {
            let Some(request_id) = value
                .pointer("/response/request_id")
                .or_else(|| value.get("request_id"))
                .and_then(Value::as_str)
            else {
                return;
            };
            let Some(key) = state.pending_task_stops.lock().remove(request_id) else {
                return;
            };
            let subtype = value.pointer("/response/subtype").and_then(Value::as_str);
            let stop_status = value
                .pointer("/response/response/status")
                .or_else(|| value.pointer("/response/status"))
                .or_else(|| value.pointer("/response/response"))
                .and_then(Value::as_str);
            if subtype == Some("error") || matches!(stop_status, Some("not_found" | "not_running"))
            {
                let message = value
                    .pointer("/response/error")
                    .or_else(|| value.pointer("/response/response/message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| tr!("background.stop_not_running"));
                let _ = events.send(DriverEvent::BackgroundWork(
                    BackgroundWorkEvent::StopFailed { key, message },
                ));
            } else {
                let mut item = BackgroundWorkItem::new(
                    key.kind,
                    key.provider_id.clone(),
                    "",
                    BackgroundWorkStatus::Stopped,
                );
                item.key = key;
                item.background = true;
                let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
                    item,
                )));
            }
        }
        Some("stream_event") => {
            // A subagent's partial stream must not feed — or re-arm — the
            // main message; its transcript lands via the completed messages.
            if value
                .get("parent_tool_use_id")
                .and_then(Value::as_str)
                .is_some()
            {
                return;
            }
            begin_unprompted_turn(turn_active, state, events);
            let event = value.get("event").unwrap_or(&Value::Null);
            // Each assistant message re-arms the delta fallback.
            if event.get("type").and_then(Value::as_str) == Some("message_start") {
                state.saw_text_delta = false;
                state.saw_reasoning_delta = false;
            }
            let delta = event.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(text) = delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        state.saw_text_delta = true;
                        let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                    }
                }
                Some("thinking_delta") => {
                    if let Some(text) = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        state.saw_reasoning_delta = true;
                        let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
                    }
                }
                _ => {}
            }
        }
        Some("assistant") => {
            // A subagent message (a set `parent_tool_use_id`) runs in its own
            // context and belongs to its task's output pane, not this
            // session's transcript or usage meter.
            if let Some(parent) = value.get("parent_tool_use_id").and_then(Value::as_str) {
                forward_subagent_transcript(parent, value, events, state);
                return;
            }
            begin_unprompted_turn(turn_active, state, events);
            if let Some(usage) = value.pointer("/message/usage") {
                if let Some(model) = value.pointer("/message/model").and_then(Value::as_str) {
                    state.last_assistant_model = Some(model.to_owned());
                }
                if let Some(tokens) = super::support::claude_context_tokens(usage) {
                    let _ = events.send(DriverEvent::UsageUpdated {
                        context_tokens: Some(tokens),
                        context_window: None,
                    });
                }
            }
            let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
                return;
            };
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") if !state.saw_text_delta => {
                        if let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                        }
                    }
                    Some("thinking") if !state.saw_reasoning_delta => {
                        if let Some(text) = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
                        }
                    }
                    Some("tool_use") => {
                        // A relay to a known subagent is the only record of
                        // that message: the subagent's own turn carries no
                        // marker saying its words came from the parent. The
                        // call still belongs in this session's transcript, so
                        // it is mirrored rather than diverted.
                        forward_parent_relay(block, events, state);
                        let id = block.get("id").and_then(Value::as_str).map(str::to_owned);
                        let wire_title = block
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| tr!("activity.tool"));
                        let kind = super::support::classify_tool(&wire_title);
                        // The Agent tool names its work in `description`;
                        // without this the row reads as a bare "Agent".
                        let title = activity::input_title(block.get("input"))
                            .or_else(|| {
                                (wire_title.eq_ignore_ascii_case("task")
                                    || wire_title.eq_ignore_ascii_case("agent"))
                                .then(|| {
                                    block
                                        .pointer("/input/description")
                                        .and_then(Value::as_str)
                                        .map(str::trim)
                                        .filter(|text| !text.is_empty())
                                        .map(str::to_owned)
                                })
                                .flatten()
                            })
                            .unwrap_or_else(|| wire_title.clone());
                        if let Some(id) = &id {
                            let command = block
                                .pointer("/input/command")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|command| !command.is_empty())
                                .map(str::to_owned);
                            state.tools.insert(
                                id.clone(),
                                (kind, title.clone(), wire_title.clone(), command),
                            );
                        }
                        let _ = events.send(DriverEvent::RichActivity(
                            activity::tool_activity(
                                id,
                                kind,
                                title,
                                block.get("input"),
                                None,
                                None,
                                false,
                                false,
                            )
                            .with_tool_name(block.get("name").and_then(Value::as_str)),
                        ));
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            // A subagent's tool results echo on the main channel too; its
            // transcript lives in the task's output pane, not here.
            if value
                .get("parent_tool_use_id")
                .and_then(Value::as_str)
                .is_some()
            {
                return;
            }
            // `--replay-user-messages` echoes Waku's own prompts back; they are
            // an acknowledgement, not transcript content.
            if value.get("isReplay").and_then(Value::as_bool) == Some(true) {
                return;
            }
            let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
                return;
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let (kind, title, wire_name, _) = id
                    .as_ref()
                    .and_then(|id| state.tools.remove(id))
                    .unwrap_or((ActivityKind::Tool, "Tool".to_owned(), String::new(), None));
                let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
                // The result text of an edit is only a confirmation sentence.
                // The positioned hunks Claude actually applied ride alongside
                // it, so hand them over as the activity's source and the diff
                // lands in the transcript with real line numbers.
                let patch = (kind == ActivityKind::FileChange)
                    .then(|| value.get("tool_use_result"))
                    .flatten()
                    .filter(|result| !result.is_null());
                let item = activity::tool_activity(
                    id,
                    kind,
                    title,
                    None,
                    block.get("content"),
                    block.get("content"),
                    failed,
                    true,
                )
                .with_activity_source(patch)
                .with_tool_name(Some(&wire_name));
                let _ = events.send(DriverEvent::RichActivity(item));
            }
        }
        Some("result") => {
            let failed = value.get("is_error").and_then(Value::as_bool) == Some(true);
            if failed && let Some(result) = value.get("result").and_then(Value::as_str) {
                let _ = events.send(DriverEvent::Error(result.to_owned()));
            }
            // Ahead of TurnFinished, whose forced save should include it.
            if let Some(window) =
                context_window_from_result(value, state.last_assistant_model.as_deref())
            {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: None,
                    context_window: Some(window),
                });
            }
            if !*turn_active.lock() {
                return;
            }
            // Claude writes its generated title and rewind checkpoint to the
            // same native transcript as the turn settles. Read it once, ahead
            // of TurnFinished, so that event's forced save includes both.
            if let Ok(metadata) = crate::claude_session::session_metadata(session_id) {
                if let Some(title) = metadata.title
                    && state.last_auto_title.as_deref() != Some(title.as_str())
                {
                    state.last_auto_title = Some(title.clone());
                    let _ = events.send(DriverEvent::AutoTitleUpdated(Some(title)));
                }
                if let Some(message_id) = metadata.latest_message_id {
                    let _ = events.send(DriverEvent::Connected {
                        provider_cursor: Some(ProviderResumeCursor::Claude {
                            session_id: session_id.to_owned(),
                            resume_at: Some(message_id),
                        }),
                    });
                }
            }
            // "I'll report back when it finishes" is not the end of the turn:
            // the CLI wakes the model itself once the detached work settles,
            // so the turn is parked and its wake continues it. Only a reply
            // with nothing left to wait for settles it.
            if !failed && !state.live_tasks.is_empty() {
                state.parked = true;
                let _ = events.send(DriverEvent::TurnParked);
                return;
            }
            state.parked = false;
            *turn_active.lock() = false;
            let _ = events.send(DriverEvent::TurnFinished {
                success: !failed,
                summary: None,
            });
        }
        // `system` status/thinking-token notices and `rate_limit_event` are not
        // transcript content.
        _ => {}
    }
}

fn request_user_input(
    value: &Value,
    events: &impl DriverEventSink,
    state: &ClaudeStreamState,
) -> bool {
    let request = value.get("request").unwrap_or(&Value::Null);
    if request.get("tool_name").and_then(Value::as_str) != Some("AskUserQuestion") {
        return false;
    }
    let Some(request_id) = value.get("request_id").and_then(Value::as_str) else {
        return true;
    };
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, question)| {
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if text.is_empty() {
                return None;
            }
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = option.get("label").and_then(Value::as_str)?.trim();
                    (!label.is_empty()).then(|| UserInputOption {
                        label: label.to_owned(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|description| !description.is_empty())
                            .map(str::to_owned),
                    })
                })
                .collect();
            Some(UserInputQuestion {
                // Claude's SDK resolves answers by the complete question text.
                id: text.to_owned(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .filter(|header| !header.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Question {}", index + 1)),
                question: text.to_owned(),
                options,
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    if questions.is_empty() {
        return true;
    }
    state
        .pending_user_inputs
        .lock()
        .insert(request_id.to_owned(), input);
    let _ = events.send(DriverEvent::UserInputRequested {
        request_id: request_id.to_owned(),
        questions,
    });
    true
}

fn request_permission(
    value: &Value,
    events: &impl DriverEventSink,
    commands: &Sender<CommandMessage>,
    auto_approve: bool,
) {
    let Some(request_id) = value.get("request_id").and_then(Value::as_str) else {
        return;
    };
    if auto_approve {
        let _ = commands.send(CommandMessage::Respond {
            request_id: request_id.to_owned(),
            option_id: "allow".into(),
        });
        return;
    }

    let request = value.get("request").unwrap_or(&Value::Null);
    let tool = request
        .get("display_name")
        .or_else(|| request.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| tr!("permission.a_tool"));
    // The agent says why it is asking; that reason is what the answer rests on.
    let detail = request
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            request
                .get("blocked_path")
                .and_then(Value::as_str)
                .map(|path| tr!("permission.blocked_path", path = path))
        })
        .unwrap_or_else(|| tr!("permission.agent_wants_to_run", tool = tool.as_str()));
    let _ = events.send(DriverEvent::Permission {
        request_id: request_id.to_owned(),
        title: activity::input_title(request.get("input"))
            .unwrap_or_else(|| tr!("permission.run_tool", tool = tool.as_str())),
        detail,
        options: vec![
            PermissionOption {
                id: "allow".into(),
                label: tr!("permission.allow_once"),
                allow: true,
            },
            PermissionOption {
                id: "deny".into(),
                label: tr!("common.deny"),
                allow: false,
            },
        ],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_command_requests_readable_reasoning_summary() {
        let mut command = Command::new("/usr/bin/true");
        configure_stream_command(&mut command, RuntimeMode::AutoAcceptEdits);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            arguments
                .windows(2)
                .any(|arguments| { arguments == ["--thinking-display", "summarized"] })
        );
    }

    /// A subagent's conversation is only readable with this flag.
    ///
    /// Without it the stream carries a subagent's tool_use and tool_result
    /// blocks alone: enough to see what it ran, never what it said. Its text and
    /// thinking blocks arrive only when this is passed, and those are what the
    /// subagent panel shows and what a steering message would be answering.
    #[test]
    fn streaming_command_forwards_subagent_text() {
        let mut command = Command::new("/usr/bin/true");
        configure_stream_command(&mut command, RuntimeMode::AutoAcceptEdits);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            arguments.iter().any(|arg| arg == "--forward-subagent-text"),
            "subagent text and thinking blocks need this flag: {arguments:?}"
        );
    }

    /// An identity has to reach Claude as a system rule, not as a first user
    /// turn.
    ///
    /// Measured on CLI 2.1.330: with `--append-system-prompt "You are Auditor,
    /// the RV…"` a fresh session answered "Auditor, the RV", and a `--resume`
    /// of that session still reported the identity exactly once — so a resumed
    /// delegate does not accumulate copies of its own prompt.
    #[test]
    fn an_identity_reaches_the_session_through_the_system_prompt() {
        let mut command = Command::new("/usr/bin/true");
        let prompt = "You are Auditor, the RV.\n\nBoundaries you must not cross:\nNever edit files.\n";
        command.args(["--append-system-prompt", prompt]);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--append-system-prompt", prompt]),
            "the identity text itself has to be the flag's value: {arguments:?}"
        );
        assert!(
            !arguments.iter().any(|arg| arg == "--system-prompt"),
            "replacing Claude's own prompt would drop the rules it ships with"
        );
    }

    /// A lead conversation gets the delegation tools; an ordinary task does not.
    ///
    /// This is the whole wiring: the daemon mints a scoped token and hands it
    /// over as `AgentBridgeAccess`, and the driver has to turn that into a real
    /// MCP server registration. Before this, the field was accepted and dropped,
    /// so `spawn_agent` was unreachable from any session.
    #[test]
    fn a_delegating_session_registers_the_agent_bridge() {
        let mut command = Command::new("/usr/bin/true");
        configure_agent_bridge(
            &mut command,
            &crate::driver::AgentBridgeAccess {
                address: "127.0.0.1:4312".into(),
                token: "scoped-token".into(),
                session_id: uuid::Uuid::new_v4(),
                runtime_id: uuid::Uuid::new_v4(),
            },
        );

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let flag = arguments
            .iter()
            .position(|argument| argument == "--mcp-config")
            .expect("Claude only learns about the bridge through this flag");
        let config: Value = serde_json::from_str(&arguments[flag + 1]).unwrap();
        let server = &config["mcpServers"]["waku_agents"];
        assert_eq!(server["args"][0], "--agent-bridge");
        assert!(
            server["command"]
                .as_str()
                .is_some_and(|command| !command.is_empty()),
            "the bridge has to name an executable: {server:?}"
        );
    }

    /// The bridge must be told the *Waku* task it belongs to.
    ///
    /// Claude has its own session id, and it is a different UUID with a different
    /// meaning. Handing that one to the bridge made every `spawn_agent` fail with
    /// "this agent token may not perform that request": the daemon scopes the
    /// token to the task, so a request naming Claude's conversation is refused
    /// before it reaches the backend. The ids here are made distinct on purpose,
    /// because the version of this test that passed used one string for both.
    ///
    /// This drives the *whole driver* rather than the helper, because the bug was
    /// at the call site: the helper was told which id to use, and the caller
    /// passed Claude's. Only launching the real thing can catch that.
    #[test]
    fn the_bridge_is_told_the_waku_session_not_claudes_own() {
        let waku_session = uuid::Uuid::new_v4();
        let runtime_id = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("waku-claude-bridge-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let dump = root.join("env.txt");
        let stub = fake_claude(&root, &dump);

        let (events, _event_rx) = crate::driver::test_event_channel();
        let driver = ClaudeDriver::start(
            DriverStartOptions {
                binary: stub,
                cwd: root.clone(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                system_prompt: None,
                agent_bridge: Some(crate::driver::AgentBridgeAccess {
                    address: "127.0.0.1:4312".into(),
                    token: "scoped-token".into(),
                    session_id: waku_session,
                    runtime_id,
                }),
            },
            events,
        )
        .expect("the driver should launch with a bridge");
        // The stub writes its environment and exits, so the driver's reader ends.
        drop(driver);

        let written = wait_for_file(&dump);
        let environment = written
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            environment
                .get(waku_protocol::DAEMON_SESSION_ENV)
                .map(String::as_str),
            Some(waku_session.to_string().as_str()),
            "the bridge has to address the task the token was minted for, \
             not Claude's own conversation"
        );
        assert_eq!(
            environment
                .get(waku_protocol::DAEMON_RUNTIME_ENV)
                .map(String::as_str),
            Some(runtime_id.to_string().as_str())
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// A stub that dumps its environment to `dump` and exits.
    ///
    /// It has to behave like a program Claude is launched as — the driver spawns
    /// it and reads its stdout — so on Windows it is a batch file and elsewhere a
    /// shell script, both made executable for this test only.
    fn fake_claude(root: &std::path::Path, dump: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        {
            let path = root.join("stub.cmd");
            let script = format!(
                "@echo off\r\nset > \"{}\"\r\n",
                dump.display()
            );
            std::fs::write(&path, script).unwrap();
            path
        }
        #[cfg(not(windows))]
        {
            let path = root.join("stub.sh");
            let script = format!("#!/bin/sh\nenv > '{}'\n", dump.display());
            std::fs::write(&path, script).unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    fn wait_for_file(path: &std::path::Path) -> String {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the stub never wrote {}", path.display());
    }

    fn environment_of(
        command: &Command,
    ) -> std::collections::HashMap<String, Option<String>> {        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// The capability must never travel in argv.
    ///
    /// A command line is readable by every process owner on the host, so a token
    /// pasted into `--mcp-config` would hand the daemon to anyone who can list
    /// processes. Claude passes its own environment to the servers it starts,
    /// which is why the credential rides there instead.
    #[test]
    fn the_bridge_credential_travels_in_the_environment_not_the_command_line() {
        let mut command = Command::new("/usr/bin/true");
        let runtime_id = uuid::Uuid::new_v4();
        let session_id = uuid::Uuid::new_v4();
        configure_agent_bridge(
            &mut command,
            &crate::driver::AgentBridgeAccess {
                address: "127.0.0.1:4312".into(),
                token: "scoped-token".into(),
                session_id,
                runtime_id,
            },
        );

        let rendered = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !rendered.contains("scoped-token"),
            "the token must not appear in argv: {rendered}"
        );

        let environment = environment_of(&command);
        assert_eq!(
            environment
                .get(waku_protocol::DAEMON_TOKEN_ENV)
                .cloned()
                .flatten()
                .as_deref(),
            Some("scoped-token")
        );
        assert_eq!(
            environment
                .get(waku_protocol::DAEMON_ADDRESS_ENV)
                .cloned()
                .flatten()
                .as_deref(),
            Some("127.0.0.1:4312")
        );
        assert_eq!(
            environment
                .get(waku_protocol::DAEMON_SESSION_ENV)
                .cloned()
                .flatten()
                .as_deref(),
            Some(session_id.to_string().as_str())
        );
        // The runtime is half of the scope the daemon checks, so a bridge that
        // did not receive it would be refused on every call it made.
        assert_eq!(
            environment
                .get(waku_protocol::DAEMON_RUNTIME_ENV)
                .cloned()
                .flatten()
                .as_deref(),
            Some(runtime_id.to_string().as_str())
        );
    }

    fn harness() -> (        DriverEventSender,
        crossbeam_channel::Receiver<DriverEvent>,
        Sender<CommandMessage>,
        crossbeam_channel::Receiver<CommandMessage>,
        Mutex<bool>,
        ClaudeStreamState,
    ) {
        let (events, event_rx) = crate::driver::test_event_channel();
        let (commands, command_rx) = unbounded();
        (
            events,
            event_rx,
            commands,
            command_rx,
            Mutex::new(true),
            ClaudeStreamState::default(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn locates_claudes_native_task_output_across_workspace_slugs() {
        let root = std::env::temp_dir().join(format!("waku-claude-output-test-{}", Uuid::new_v4()));
        let output = root
            .join("-Users-egoist-dev-waku")
            .join("session-live")
            .join("tasks")
            .join("task-live.output");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "first\n").unwrap();

        assert_eq!(
            claude_task_output_path_in(&root, "session-live", "task-live"),
            Some(output)
        );
        assert_eq!(
            claude_task_output_path_in(&root, "session-live", "../escape"),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn live_task_output_preserves_split_utf8() {
        let bytes = "first 界".as_bytes();
        let split = bytes.len() - 1;
        let mut pending = bytes[..split].to_vec();

        assert_eq!(
            drain_utf8_output(&mut pending, false).as_deref(),
            Some("first ")
        );
        pending.extend_from_slice(&bytes[split..]);
        assert_eq!(
            drain_utf8_output(&mut pending, false).as_deref(),
            Some("界")
        );
        assert!(pending.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn native_task_output_streams_before_completion() {
        let root = std::env::temp_dir().join(format!("waku-claude-tail-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("task.output");
        std::fs::write(&output, "first\n").unwrap();
        let (events, event_rx) = crate::driver::test_event_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "task-live");
        let tail_stop = stop.clone();
        let tail = thread::spawn(move || {
            stream_claude_task_output(output, key, events, tail_stop);
        });

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta { delta, .. })
                if delta == "first\n"
        ));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("task.output"))
            .unwrap();
        file.write_all("second 界\n".as_bytes()).unwrap();
        file.flush().unwrap();
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta { delta, .. })
                if delta == "second 界\n"
        ));

        stop.store(true, Ordering::Release);
        tail.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The real `waku` program beside this test binary.
    ///
    /// `cargo test` puts test binaries in `target/<profile>/deps` and the program
    /// one directory up, so this is the same artifact the driver would ship with.
    fn test_waku_executable() -> std::path::PathBuf {
        let mut path = std::env::current_exe().expect("the test binary has a path");
        path.pop();
        if path.file_name().is_some_and(|name| name == "deps") {
            path.pop();
        }
        let executable = path.join(if cfg!(windows) { "waku.exe" } else { "waku" });
        assert!(
            executable.is_file(),
            "the waku program must be built for this probe: {}",
            executable.display()
        );
        executable
    }

    /// The whole delegation path against the real CLI.
    ///
    /// This is the only test that can prove what a user actually does: a real
    /// Claude process, launched by the real driver, calling `spawn_agent` through
    /// the MCP server the driver registered for it, and the daemon turning that
    /// into a second task. Every cheaper test stops short of one of those links —
    /// the env-only test cannot see the model, and the bridge-only test cannot see
    /// the driver — which is exactly how a wrong session id survived two rounds.
    ///
    /// Ignored by default: needs the CLI installed, credentials, and the network.
    #[test]
    #[ignore = "requires an installed, authenticated claude"]
    fn a_real_claude_session_starts_a_delegate_through_the_real_daemon() {
        use crate::persistence::StateStore;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let binary =
            crate::command_env::find_executable("claude").expect("claude is not installed");
        // The driver resolves the bridge program from `APP_EXECUTABLE_ENV` first,
        // which is how the daemon tells a provider where Waku lives. Under
        // `cargo test` the current executable is the test harness, so without
        // this the bridge would be launched as `cargo_test --agent-bridge` and
        // Claude would see no tools at all.
        let waku = test_waku_executable();
        unsafe { std::env::set_var(crate::APP_EXECUTABLE_ENV, &waku) };
        let root = std::env::temp_dir().join(format!("waku-claude-delegate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        // A private state directory holding one lead task, so the daemon has a
        // conversation to derive a delegate from.
        let store = StateStore::new(root.join("app.db"));
        let mut state = store.load().unwrap();
        let project = crate::model::Project::from_path(root.clone());
        let mut lead =
            crate::model::AgentSession::new(project.id, crate::model::ProviderKind::Claude);
        lead.title = "lead".into();
        lead.begin_turn("open the group");
        let lead_id = lead.id;
        state.projects.push(project);
        state.push_session(lead);
        store.save(&mut state).unwrap();

        let settings =
            crate::DaemonSettingsStore::open(root.join("daemon.json")).unwrap();
        let backend = Arc::new(crate::daemon::WakuBackend::new(settings, store).unwrap());

        // A real daemon, and a real capability minted for the lead task.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        crate::server::publish_daemon_address(&address.to_string());
        let server = Arc::new(crate::DaemonServer::new(
            "desktop-token".into(),
            Arc::new(crate::daemon_identity::DaemonIdentity::generate()),
            backend.clone(),
            crate::ServerOptions::default(),
        ));
        let runtime_id = Uuid::new_v4();
        let scoped = server.mint_agent_token(lead_id, runtime_id);
        let stop = Arc::new(AtomicBool::new(false));
        let serving = {
            let server = server.clone();
            let stop = stop.clone();
            std::thread::spawn(move || server.serve(listener, stop).unwrap())
        };

        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = ClaudeDriver::start(
            DriverStartOptions {
                binary,
                cwd: root.clone(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                system_prompt: None,
                // The capability the daemon would hand a lead in the app.
                agent_bridge: Some(crate::driver::AgentBridgeAccess {
                    address: address.to_string(),
                    token: scoped,
                    session_id: lead_id,
                    runtime_id,
                }),
            },
            events,
        )
        .expect("the lead should start with a bridge");

        driver.prompt(
            "Call the waku_agents tool spawn_agent with task 'Reply with exactly: DELEGATE_OK' \
             and title 'probe'. Then report the tool's exact result. Do not do the task yourself."
                .to_owned(),
        );

        let mut reply = String::new();
        while let Ok(event) = event_rx.recv_timeout(std::time::Duration::from_secs(240)) {
            match event {
                DriverEvent::TextDelta(delta) => reply.push_str(&delta),
                DriverEvent::TurnFinished { .. } => break,
                DriverEvent::Error(error) => panic!("the CLI reported: {error}"),
                _ => {}
            }
        }
        assert!(
            !reply.contains("may not perform that request"),
            "the lead was refused, so the bridge addressed the wrong session: {reply}"
        );
        eprintln!("LEAD REPLY: {reply}");

        // The delegate has to be a real second task, not an id string.
        let listed = crate::Backend::handle(
            backend.as_ref(),
            crate::Request {
                request_id: Uuid::new_v4(),
                session_id: lead_id,
                runtime_id,
                command: crate::Command::LoadTaskState,
            },
            server.event_sink_for(lead_id, runtime_id),
        )
        .unwrap();
        let crate::ResponsePayload::TaskState { sessions, .. } = listed else {
            panic!("the daemon did not return its task list");
        };
        assert!(
            sessions.iter().any(|session| session.id != lead_id),
            "the lead never created a delegate: {sessions:?}"
        );

        drop(driver);
        stop.store(true, std::sync::atomic::Ordering::Release);
        serving.join().unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    /// Drives the real CLI through the actual driver, including a second turn
    /// on the same process — the whole point of the transport. Ignored by
    /// default: needs the CLI installed, credentials, and the network.
    #[test]
    #[ignore = "requires an installed, authenticated claude"]    fn claude_streaming_session_against_the_real_cli() {
        let binary =
            crate::command_env::find_executable("claude").expect("claude is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = ClaudeDriver::start(
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                agent_bridge: None,
                system_prompt: None,
            },
            events,
        )
        .expect("the streaming session should start");

        assert!(matches!(
            event_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("the driver should report its session"),
            DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Claude { .. })
            }
        ));

        let collect = |driver: &ClaudeDriver, prompt: &str| -> String {
            driver.prompt(prompt.to_owned());
            let mut text = String::new();
            while let Ok(event) = event_rx.recv_timeout(std::time::Duration::from_secs(180)) {
                match event {
                    DriverEvent::TextDelta(delta) => text.push_str(&delta),
                    DriverEvent::TurnFinished { success, .. } => {
                        assert!(success, "the turn should settle successfully");
                        return text;
                    }
                    DriverEvent::Error(error) => panic!("the CLI reported: {error}"),
                    _ => {}
                }
            }
            panic!("the turn never settled");
        };

        let first = collect(&driver, "Reply with exactly: BANANA. Use no tools.");
        assert!(first.contains("BANANA"), "expected a reply, got {first:?}");

        // The second turn proves one process is serving the conversation and
        // kept its context — a per-turn spawn could not answer this.
        let second = collect(
            &driver,
            "What word did I just ask you to reply with? Answer with that word only.",
        );
        assert!(
            second.contains("BANANA"),
            "the session should retain context across turns, got {second:?}"
        );
    }

    /// Proves steering through the actual driver: the message injected while
    /// the Bash tool sleeps lands inside the same turn — one SteerAccepted,
    /// one TurnFinished, and a reply that honors both instructions. Ignored by
    /// default: needs the CLI installed, credentials, and the network.
    #[test]
    #[ignore = "requires an installed, authenticated claude"]
    fn claude_steering_folds_a_mid_turn_message_into_the_running_turn() {
        let binary =
            crate::command_env::find_executable("claude").expect("claude is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = ClaudeDriver::start(
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: Some("claude-haiku-4-5-20251001".into()),
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                agent_bridge: None,
                system_prompt: None,
            },
            events,
        )
        .expect("the streaming session should start");

        driver.prompt(
            "Use the Bash tool to run exactly `sleep 6` (nothing else). \
             After the command completes, reply with exactly: FIRST DONE"
                .into(),
        );

        let mut text = String::new();
        let mut steered = false;
        let mut steer_accepted = false;
        let mut turns_finished = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while std::time::Instant::now() < deadline {
            let Ok(event) = event_rx.recv_timeout(std::time::Duration::from_secs(5)) else {
                // Quiet after the turn settled means no second turn is coming.
                if turns_finished == 1 {
                    break;
                }
                continue;
            };
            match event {
                DriverEvent::RichActivity(item) if !steered && !item.complete => {
                    // The tool is running: the turn is unambiguously live.
                    steered = true;
                    driver.steer(
                        "ADDITIONAL INSTRUCTION: end your very next reply \
                         with the word BANANA."
                            .into(),
                    );
                }
                DriverEvent::SteerAccepted { message } => {
                    assert!(message.contains("BANANA"));
                    steer_accepted = true;
                }
                DriverEvent::SteerRejected { reason, .. } => {
                    panic!("the steer should be accepted, got rejection: {reason}");
                }
                DriverEvent::TextDelta(delta) => text.push_str(&delta),
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "the turn should settle successfully");
                    turns_finished += 1;
                }
                DriverEvent::Error(error) => panic!("the CLI reported: {error}"),
                _ => {}
            }
        }

        assert!(steered, "the probe never saw the tool start");
        assert!(steer_accepted, "the driver should acknowledge the steer");
        assert_eq!(
            turns_finished, 1,
            "a steered message must not settle a second turn"
        );
        assert!(
            text.contains("BANANA"),
            "the steered instruction should shape the same turn's reply, got {text:?}"
        );
    }

    #[test]
    fn steering_is_advertised_and_rides_the_command_channel() {
        let (commands, command_rx) = unbounded();
        let driver = ClaudeDriver {
            commands,
            pending_user_inputs: Arc::new(Mutex::new(HashMap::new())),
            mode: RuntimeMode::FullAccess,
        };

        assert!(driver.supports_steer());
        driver.steer("Focus on the failing tests first".into());
        match command_rx.try_recv() {
            Ok(CommandMessage::Steer(text)) => {
                assert_eq!(text, "Focus on the failing tests first");
            }
            Ok(_) => panic!("expected a steer command"),
            Err(_) => panic!("no command was sent"),
        }
    }

    #[test]
    fn stop_task_uses_claudes_native_control_request() {
        assert_eq!(
            stop_task_request(7, "agent-42"),
            json!({
                "type": "control_request",
                "request_id": "waku-7",
                "request": {"subtype": "stop_task", "task_id": "agent-42"}
            })
        );
    }

    #[test]
    fn stop_task_control_errors_restore_the_live_item() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let key = BackgroundWorkKey::new(BackgroundWorkKind::Subagent, "agent-42");
        state
            .pending_task_stops
            .lock()
            .insert("waku-8".into(), key.clone());
        handle_message(
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "waku-8",
                    "response": {"status": "not_running"}
                }
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(DriverEvent::BackgroundWork(BackgroundWorkEvent::StopFailed {
                key: failed_key,
                ..
            })) if failed_key == key
        ));
    }

    #[test]
    fn background_bash_task_keeps_its_originating_command() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({
                "type": "assistant",
                "message": {"content": [{
                    "type": "tool_use",
                    "id": "toolu-bash",
                    "name": "Bash",
                    "input": {
                        "command": "cargo check && cargo test --bin waku",
                        "description": "Type-check and run full suite"
                    }
                }]}
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(DriverEvent::RichActivity(_))
        ));

        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "bash-42",
                "tool_use_id": "toolu-bash",
                "task_type": "local_bash",
                "description": "Type-check and run full suite",
                "is_backgrounded": true
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(started)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_started should surface a background item");
        };
        assert_eq!(started.key.kind, BackgroundWorkKind::Process);
        assert_eq!(
            started.command.as_deref(),
            Some("cargo check && cargo test --bin waku")
        );
    }

    #[test]
    fn task_lifecycle_surfaces_subagents_independently_of_the_turn() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let tool = json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "toolu-agent",
                "name": "Task",
                "input": {"description": "Inspect the parser"}
            }]}
        });
        handle_message(&tool, "s", &events, &commands, &turn, true, &mut state);
        let _ = event_rx.try_recv().unwrap();

        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "agent-42",
                "tool_use_id": "toolu-agent",
                "task_type": "local_agent",
                "subagent_type": "Explore",
                "description": "Inspect the parser"
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(started)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_started should surface a background item");
        };
        assert_eq!(started.key.kind, BackgroundWorkKind::Subagent);
        assert_eq!(started.key.provider_id, "agent-42");
        assert_eq!(started.origin_activity_id.as_deref(), Some("toolu-agent"));
        assert_eq!(started.status, BackgroundWorkStatus::Running);

        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_updated",
                "task_id": "agent-42",
                "patch": {"status": "completed"}
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(completed)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_updated should settle the background item");
        };
        assert_eq!(completed.key.kind, BackgroundWorkKind::Subagent);
        assert_eq!(completed.status, BackgroundWorkStatus::Completed);
    }

    /// Captured from CLI 2.1.330 with `--include-partial-messages`, which is
    /// what Waku passes: the CLI emits `background_tasks_changed` repeatedly,
    /// and its `tasks` entries carry **no status field**. Reading that as
    /// "running" would reopen an already-settled subagent on every level
    /// signal and replace its description with a generic label — the card then
    /// sits on "working" forever with no idea which agent it is.
    #[test]
    fn a_level_signal_does_not_reopen_a_settled_subagent() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({
                "type": "system", "subtype": "task_started",
                "task_id": "ab178c2413e4e5a2c",
                "tool_use_id": "call_x",
                "description": "List 1 file",
                "subagent_type": "general-purpose",
                "is_backgrounded": false,
                "task_type": "local_agent"
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let _ = event_rx.try_recv().unwrap();

        handle_message(
            &json!({
                "type": "system", "subtype": "task_updated",
                "task_id": "ab178c2413e4e5a2c",
                "patch": {"status": "completed"}
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let _ = event_rx.try_recv().unwrap();

        // The level signal still lists it — the CLI does not always drop it
        // immediately — and carries only its identity, never a status.
        handle_message(
            &json!({
                "type": "system", "subtype": "background_tasks_changed",
                "tasks": [{
                    "task_id": "ab178c2413e4e5a2c",
                    "task_type": "local_agent",
                    "subagent_type": "general-purpose",
                    "description": "List 1 file"
                }]
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );

        let mut last = None;
        while let Ok(event) = event_rx.try_recv() {
            if let DriverEvent::BackgroundWork(BackgroundWorkEvent::ReconcileLive { items }) = event
            {
                last = Some(items);
            }
        }
        let items = last.expect("the level signal must reconcile");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].status,
            BackgroundWorkStatus::Completed,
            "a level signal must not reopen a settled subagent"
        );
        assert_eq!(
            items[0].title, "List 1 file",
            "the level signal's own description belongs on the card"
        );
    }

    /// The level signal has to keep using what the bookends learned about a
    /// task, because it does not repeat the kind or the originating tool call.
    #[test]
    fn a_level_signal_keeps_the_kind_and_origin_the_bookends_established() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({
                "type": "system", "subtype": "task_started",
                "task_id": "ab178c2413e4e5a2c",
                "tool_use_id": "call_x",
                "description": "List 1 file",
                "subagent_type": "general-purpose",
                "task_type": "local_agent",
                "is_backgrounded": false
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let _ = event_rx.try_recv().unwrap();
        handle_message(
            &json!({
                "type": "system", "subtype": "background_tasks_changed",
                "tasks": [{"task_id": "ab178c2413e4e5a2c"}]
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let mut items = None;
        while let Ok(event) = event_rx.try_recv() {
            if let DriverEvent::BackgroundWork(BackgroundWorkEvent::ReconcileLive {
                items: found,
            }) = event
            {
                items = Some(found);
            }
        }
        let item = &items.expect("level signal")[0];
        assert_eq!(
            item.key.kind,
            BackgroundWorkKind::Subagent,
            "a bare id keeps the kind its task_started established"
        );
        assert_eq!(
            item.origin_activity_id.as_deref(),
            Some("call_x"),
            "the level signal must not strip the originating tool call"
        );
    }

    /// Captured from CLI 2.1.330 running three Task subagents at once: every
    /// subagent message arrives with `parent_tool_use_id` set to the `Task`
    /// tool call, and the CLI sends a bare `thinking` block before each one.
    /// The pane must show the narrative and tool lines, and must not be
    /// emptied by the thinking-only messages that precede them.
    #[test]
    fn a_captured_subagent_stream_fills_its_task_pane() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let transcript = [
            json!({
                "type": "system", "subtype": "task_started",
                "task_id": "ad33119efe684b244",
                "tool_use_id": "call_a8d088d35bb94b55af6a7237",
                "description": "List one file",
                "subagent_type": "general-purpose",
                "is_backgrounded": false,
                "task_type": "local_agent"
            }),
            json!({"type": "assistant", "parent_tool_use_id": "call_a8d088d35bb94b55af6a7237",
                "message": {"content": [{"type": "thinking", "thinking": "Which files exist?"}]}}),
            json!({"type": "assistant", "parent_tool_use_id": "call_a8d088d35bb94b55af6a7237",
                "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash",
                    "input": {"command": "ls"}}]}}),
            json!({"type": "assistant", "parent_tool_use_id": "call_a8d088d35bb94b55af6a7237",
                "message": {"content": [{"type": "thinking", "thinking": "Two files."}]}}),
            json!({"type": "assistant", "parent_tool_use_id": "call_a8d088d35bb94b55af6a7237",
                "message": {"content": [{"type": "text", "text": "The directory has two files."}]}}),
        ];
        let mut deltas = Vec::new();
        for message in &transcript {
            handle_message(message, "s", &events, &commands, &turn, true, &mut state);
            while let Ok(event) = event_rx.try_recv() {
                if let DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta {
                    key,
                    delta,
                }) = event
                {
                    assert_eq!(key.provider_id, "ad33119efe684b244");
                    deltas.push(delta);
                }
            }
        }

        assert!(
            !deltas.is_empty(),
            "a subagent's transcript must reach its pane, not be dropped"
        );
        let joined = deltas.join("");
        assert!(
            joined.contains("two files."),
            "the narrative reaches the pane: {joined:?}"
        );
        assert!(
            joined.contains("Bash") && joined.contains("ls"),
            "the tool call reaches the pane: {joined:?}"
        );
    }

    /// A subagent message whose `parent_tool_use_id` is not a known `Task`
    /// call must not be silently reinterpreted: the pane it belongs to is
    /// unknown, so nothing is written and nothing else is disturbed.
    #[test]
    fn an_unmapped_subagent_message_is_dropped_without_side_effects() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({"type": "assistant", "parent_tool_use_id": "call_unknown",
                "message": {"content": [{"type": "text", "text": "orphan"}]}}),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        assert!(
            event_rx.try_recv().is_err(),
            "an unmapped subagent message produces no event"
        );
    }

    /// Wire shapes read off CLI 2.1.330, with three Task subagents running at
    /// once: each one keeps its own item through the interleaved bookends, and
    /// the ids the CLI sends never collide. A session that collapses them into
    /// one card shows only whichever arrived last.
    #[test]
    fn concurrent_subagents_keep_separate_items_through_interleaved_bookends() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();

        let started = [
            ("ad33119e", "call_a", "List one file"),
            ("a33a76b8", "call_b", "Print number 7"),
            ("aa05027a", "call_c", "Echo word ok"),
        ];
        for (task_id, tool_use_id, description) in started {
            handle_message(
                &json!({
                    "type": "system",
                    "subtype": "task_started",
                    "task_id": task_id,
                    "tool_use_id": tool_use_id,
                    "description": description,
                    "subagent_type": "general-purpose",
                    "is_backgrounded": false,
                    "task_type": "local_agent",
                    "prompt": description
                }),
                "s",
                &events,
                &commands,
                &turn,
                true,
                &mut state,
            );
        }

        // Progress and settle arrive interleaved across the three, not grouped.
        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_progress",
                "task_id": "ad33119e",
                "run_id": "r-a",
                "description": "Running List files in working directory",
                "last_tool_name": "Bash"
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_updated",
                "task_id": "a33a76b8",
                "run_id": "r-b",
                "patch": {"status": "completed", "end_time": 1_795_506_827_906u64}
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );

        let mut items: HashMap<String, BackgroundWorkItem> = HashMap::new();
        while let Ok(event) = event_rx.try_recv() {
            if let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) = event {
                items.insert(item.key.provider_id.clone(), item);
            }
        }

        assert_eq!(items.len(), 3, "every subagent keeps its own card");
        assert_eq!(
            items["ad33119e"].status,
            BackgroundWorkStatus::Running,
            "a progress event must not settle the task it describes"
        );
        assert_eq!(
            items["a33a76b8"].status,
            BackgroundWorkStatus::Completed,
            "the settled task keeps its own status"
        );
        assert_eq!(items["aa05027a"].status, BackgroundWorkStatus::Running);
        assert_eq!(
            items["ad33119e"].detail.as_deref(),
            Some("Running List files in working directory"),
            "progress goes to the detail row of the task that reported it"
        );
        // The settle event itself carries no tool_use_id — the CLI only sends
        // it on `task_started`/`task_notification` — so the driver must not be
        // expected to repeat it. The registry keeps the original because
        // `merge_option` only overwrites with a present value.
        assert!(
            items["a33a76b8"].origin_activity_id.is_none(),
            "a task_updated bookend carries no tool_use_id to repeat"
        );
    }

    /// Wire shapes read off CLI 2.1.226: progress events reuse `description`
    /// for the agent's current activity line, and the settle notification's
    /// `summary` is the whole final report. Neither may retitle the task —
    /// the activity line is the detail row, the report is the output pane.
    #[test]
    fn task_progress_and_final_report_never_retitle_the_task() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "agent-42",
                "tool_use_id": "toolu-agent",
                "task_type": "local_agent",
                "subagent_type": "Explore",
                "description": "Map right panel + UI stack",
                "prompt": "Dig through src/app/right_panel.rs and report the stack."
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(started)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_started should surface a background item");
        };
        assert_eq!(started.title, "Map right panel + UI stack");
        assert_eq!(
            started.command.as_deref(),
            Some("Dig through src/app/right_panel.rs and report the stack."),
            "the launch prompt should ride into the detail surface"
        );

        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_progress",
                "task_id": "agent-42",
                "tool_use_id": "toolu-agent",
                "description": "Running Terminal/Browser entity patterns",
                "subagent_type": "Explore",
                "last_tool_name": "Bash",
                "usage": {"total_tokens": 900, "tool_uses": 12, "duration_ms": 61_000}
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(progress)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_progress should update the background item");
        };
        assert!(
            progress.title.is_empty(),
            "the activity line must not replace the stored title, got {:?}",
            progress.title
        );
        assert_eq!(
            progress.detail.as_deref(),
            Some("Running Terminal/Browser entity patterns")
        );

        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "agent-42",
                "tool_use_id": "toolu-agent",
                "status": "completed",
                "output_file": "",
                "summary": "I have a complete map.\n\n# Waku Right Panel\nDetails…"
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(settled)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_notification should settle the background item");
        };
        assert_eq!(settled.status, BackgroundWorkStatus::Completed);
        assert!(
            settled.title.is_empty(),
            "the report must not become the title"
        );
        assert!(
            settled.detail.is_none(),
            "the report must not become the detail row"
        );
        assert_eq!(
            settled.output.as_deref(),
            Some("I have a complete map.\n\n# Waku Right Panel\nDetails…"),
            "the final report belongs in the output pane"
        );
    }

    #[test]
    fn subagent_messages_stream_into_the_tasks_output_pane() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "agent-42",
                "tool_use_id": "toolu-agent",
                "task_type": "local_agent",
                "description": "Map right panel + UI stack"
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let _ = event_rx.try_recv().unwrap();

        // The subagent's own turn: narrative text, a tool call, its result.
        handle_message(
            &json!({"type": "assistant", "parent_tool_use_id": "toolu-agent", "message": {
                "usage": {"input_tokens": 999_999, "output_tokens": 1},
                "content": [
                    {"type": "text", "text": "Scanning the panel."},
                    {"type": "tool_use", "id": "toolu-sub-1", "name": "Bash",
                     "input": {"command": "rg overlay src"}}
                ]
            }}),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        handle_message(
            &json!({"type": "user", "parent_tool_use_id": "toolu-agent", "message": {
                "content": [{"type": "tool_result", "tool_use_id": "toolu-sub-1", "content": "hits"}]
            }}),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let mut seen = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            seen.push(event);
        }
        assert_eq!(
            seen.len(),
            1,
            "subagent content must not reach the transcript or usage meter"
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta { key, delta }) = &seen[0]
        else {
            panic!("the subagent message should stream into its task output");
        };
        assert_eq!(key.provider_id, "agent-42");
        assert_eq!(key.kind, BackgroundWorkKind::Subagent);
        assert_eq!(delta, "Scanning the panel.\n\n› Bash · rg overlay src\n");

        // The settle notification's summary would duplicate the streamed
        // transcript; the pane keeps what it already has.
        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "agent-42",
                "status": "completed",
                "summary": "Scanning the panel."
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(settled)) =
            event_rx.try_recv().unwrap()
        else {
            panic!("task_notification should settle the background item");
        };
        assert!(settled.output.is_none());
    }

    #[test]
    fn a_parent_send_message_is_marked_in_the_target_subagents_pane() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "agent-42",
                "tool_use_id": "toolu-agent",
                "task_type": "local_agent",
                "description": "Map the repo"
            }),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let _ = event_rx.try_recv().unwrap();

        // The parent relays a line into the running subagent.
        handle_message(
            &json!({"type": "assistant", "parent_tool_use_id": null, "message": {
                "content": [{"type": "tool_use", "id": "toolu-send", "name": "SendMessage",
                 "input": {"to": "agent-42", "message": "Also check the docs folder.", "summary": "s"}}]
            }}),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let relayed = std::iter::from_fn(|| event_rx.try_recv().ok())
            .find_map(|event| match event {
                DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta { key, delta }) => {
                    Some((key, delta))
                }
                _ => None,
            })
            .expect("the relay must land in the target subagent's pane");
        assert_eq!(relayed.0.provider_id, "agent-42");
        assert_eq!(
            relayed.1,
            format!(
                "{}\n  Also check the docs folder.\n\n",
                tr!("background.relay_heading")
            ),
            "the relay is labelled and indented so it cannot read as the subagent's own words"
        );
    }

    #[test]
    fn a_send_message_to_a_non_subagent_target_is_not_mirrored() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        // No task_started established `peer-7`, and a bare name is not a task.
        for target in ["peer-7", "/", ""] {
            handle_message(
                &json!({"type": "assistant", "parent_tool_use_id": null, "message": {
                    "content": [{"type": "tool_use", "id": "toolu-send", "name": "SendMessage",
                     "input": {"to": target, "message": "hello", "summary": "s"}}]
                }}),
                "s",
                &events,
                &commands,
                &turn,
                true,
                &mut state,
            );
        }
        let mirrored = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter(|event| {
                matches!(
                    event,
                    DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta { .. })
                )
            })
            .count();
        assert_eq!(mirrored, 0, "only a known subagent target is mirrored");
    }

    #[test]
    fn subagent_partials_do_not_rearm_the_main_delta_fallback() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let wire = [
            json!({"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}),
            // A concurrent subagent's partial stream interleaves mid-message.
            json!({"type":"stream_event","parent_tool_use_id":"toolu-agent","event":{"type":"message_start","message":{"role":"assistant"}}}),
            json!({"type":"stream_event","parent_tool_use_id":"toolu-agent","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"sub text"}}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}),
        ];
        for message in wire {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }
        let deltas = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                DriverEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            deltas,
            ["Hello"],
            "subagent partials leaked or re-armed the fallback"
        );
    }

    #[test]
    fn the_agent_tool_row_is_titled_by_its_description() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({"type": "assistant", "message": {"content": [{
                "type": "tool_use",
                "id": "toolu-agent",
                "name": "Agent",
                "input": {"description": "Map right panel + UI stack", "prompt": "Dig in."}
            }]}}),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        let DriverEvent::RichActivity(item) = event_rx.try_recv().unwrap() else {
            panic!("the tool call should surface an activity");
        };
        assert_eq!(item.title, "Map right panel + UI stack");
    }

    #[test]
    fn access_modes_map_to_claude_permission_modes() {
        assert_eq!(permission_mode(RuntimeMode::Ask), "default");
        assert_eq!(permission_mode(RuntimeMode::AutoAcceptEdits), "acceptEdits");
        assert_eq!(
            permission_mode(RuntimeMode::FullAccess),
            "bypassPermissions"
        );
    }

    #[test]
    fn streams_text_and_tools_and_ignores_its_own_replayed_prompt() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        // Payloads copied from a live streaming-input session.
        let wire = [
            json!({"type":"system","subtype":"init","session_id":"s","tools":[]}),
            // Waku's own prompt, echoed by --replay-user-messages.
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"go"}]},"isReplay":true}),
            json!({"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"pondering"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"I'll run that."}}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"text","text":"I'll run that."},
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo hi"}}
            ]}}),
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"hi","is_error":false}
            ]}}),
            json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}),
        ];
        for message in wire {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }

        let mut seen = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            seen.push(event);
        }
        assert!(matches!(&seen[0], DriverEvent::ReasoningDelta(t) if t == "pondering"));
        assert!(matches!(&seen[1], DriverEvent::TextDelta(t) if t == "I'll run that."));
        // The completed assistant block must not repeat the streamed text.
        assert!(matches!(&seen[2], DriverEvent::RichActivity(item)
                if item.kind == ActivityKind::Command && !item.complete));
        assert!(matches!(&seen[3], DriverEvent::RichActivity(item)
                if item.complete && item.output.as_deref() == Some("hi")));
        assert_eq!(seen.len(), 4, "replayed prompt or control noise leaked");
    }

    #[test]
    fn supervised_mode_asks_the_user_and_auto_modes_answer_themselves() {
        let (events, event_rx, commands, command_rx, turn, mut state) = harness();
        // Shape observed from a live `--permission-prompt-tool stdio` session.
        let request = json!({
            "type": "control_request",
            "request_id": "fa01120e",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "display_name": "Bash",
                "input": {"command": "echo hi", "description": "Write probe file"},
                "description": "Write probe file",
                "blocked_path": "/tmp/probe.txt",
                "tool_use_id": "toolu_1"
            }
        });

        handle_message(&request, "s", &events, &commands, &turn, false, &mut state);
        let DriverEvent::Permission {
            request_id,
            detail,
            options,
            ..
        } = event_rx.try_recv().unwrap()
        else {
            panic!("Supervised mode must surface the request to the user");
        };
        assert_eq!(request_id, "fa01120e");
        assert_eq!(detail, "Write probe file");
        assert_eq!(
            options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            ["allow", "deny"]
        );
        assert!(command_rx.try_recv().is_err());

        handle_message(&request, "s", &events, &commands, &turn, true, &mut state);
        let Ok(CommandMessage::Respond { option_id, .. }) = command_rx.try_recv() else {
            panic!("auto modes must answer without the user");
        };
        assert_eq!(option_id, "allow");
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn ask_user_question_is_never_treated_as_an_auto_approvable_permission() {
        let (events, event_rx, commands, command_rx, turn, mut state) = harness();
        let request = json!({
            "type": "control_request",
            "request_id": "ask-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "input": {
                    "questions": [{
                        "header": "Environment",
                        "question": "Where should this deploy?",
                        "options": [{
                            "label": "Preview",
                            "description": "Create a preview deployment"
                        }],
                        "multiSelect": false
                    }]
                }
            }
        });

        // Even Full Access cannot invent an answer to a content question.
        handle_message(&request, "s", &events, &commands, &turn, true, &mut state);
        let DriverEvent::UserInputRequested {
            request_id,
            questions,
        } = event_rx.try_recv().unwrap()
        else {
            panic!("AskUserQuestion must reach the structured question UI");
        };
        assert_eq!(request_id, "ask-1");
        assert_eq!(questions[0].id, "Where should this deploy?");
        assert_eq!(questions[0].options[0].label, "Preview");
        assert!(command_rx.try_recv().is_err());
        assert!(state.pending_user_inputs.lock().contains_key("ask-1"));
    }

    /// The whole captured stream, fed through the real entry point line by
    /// line: this is the only shape the desktop ever sees. A capture that has
    /// the subagent's text on the wire but produces no output for its card
    /// means the pane is empty while the subagent works — the reported
    /// symptom — no matter how correct the per-message tests look.
    #[test]
    fn a_real_capture_feeds_the_subagent_card_through_the_entry_point() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let capture = include_str!("../../tests/fixtures/claude_subagent_capture.jsonl");

        let mut output = String::new();
        let mut statuses = Vec::new();
        for line in capture.lines().filter(|line| !line.trim().is_empty()) {
            let value: Value = serde_json::from_str(line).expect("capture line");
            handle_message(&value, "s", &events, &commands, &turn, true, &mut state);
            while let Ok(event) = event_rx.try_recv() {
                if let DriverEvent::BackgroundWork(event) = event {
                    match event {
                        BackgroundWorkEvent::OutputDelta { delta, .. } => output.push_str(&delta),
                        BackgroundWorkEvent::Upsert(item) => {
                            statuses.push((item.title.clone(), item.status))
                        }
                        _ => {}
                    }
                }
            }
        }

        assert!(
            output.contains("1 — first") && output.contains("5 — fifth"),
            "the subagent's words must reach its card from the real capture: {output:?}"
        );
        assert!(
            statuses.iter().any(|(title, _)| title.contains("Count")),
            "the card exists under the name the capture gave it: {statuses:?}"
        );
        assert!(
            statuses
                .iter()
                .any(|(_, status)| *status == BackgroundWorkStatus::Completed),
            "the settle notification marks it completed: {statuses:?}"
        );
    }

    #[test]
    fn the_result_message_settles_the_turn_exactly_once() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let result = json!({"type":"result","is_error":false,"stop_reason":"end_turn"});

        handle_message(&result, "s", &events, &commands, &turn, true, &mut state);
        // The checkpoint read is best-effort; the turn must settle regardless.
        let settled = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter(|event| matches!(event, DriverEvent::TurnFinished { .. }))
            .count();
        assert_eq!(settled, 1);
        assert!(!*turn.lock());

        handle_message(&result, "s", &events, &commands, &turn, true, &mut state);
        assert!(
            !std::iter::from_fn(|| event_rx.try_recv().ok())
                .any(|event| matches!(event, DriverEvent::TurnFinished { .. })),
            "a second result must not settle an already-finished turn"
        );
    }

    #[test]
    fn a_reply_with_detached_work_parks_the_turn_and_the_wake_continues_it() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        // Shapes captured from a live 2.1.258 stream: the reply ends while a
        // backgrounded command still runs, and its settle wakes the model
        // without any prompt from the host.
        let reply = [
            json!({"type":"system","subtype":"background_tasks_changed","tasks":[
                {"task_id":"bkbum265v","task_type":"local_bash","description":"Sleep then print"}
            ]}),
            json!({"type":"system","subtype":"task_started","task_id":"bkbum265v",
                "tool_use_id":"toolu_1","description":"Sleep then print",
                "is_backgrounded":true,"task_type":"local_bash"}),
            json!({"type":"result","is_error":false,"stop_reason":"end_turn","num_turns":2}),
        ];
        for message in reply {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }
        let seen: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(
            seen.iter()
                .any(|event| matches!(event, DriverEvent::TurnParked)),
            "a reply with live detached work parks the turn"
        );
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, DriverEvent::TurnFinished { .. })),
            "a parked turn is not finished"
        );
        assert!(*turn.lock(), "the turn stays open for the wake");

        let prelude = [
            json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}),
            json!({"type":"system","subtype":"task_notification","task_id":"bkbum265v",
                "tool_use_id":"toolu_1","status":"completed","output_file":"","summary":""}),
            json!({"type":"system","subtype":"init","session_id":"s","tools":[]}),
            json!({"type":"system","subtype":"status","status":"requesting"}),
        ];
        for message in prelude {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }
        // `init` and `status` also open a resume handshake that never runs
        // the model; only model output reports the wake.
        assert!(
            !std::iter::from_fn(|| event_rx.try_recv().ok())
                .any(|event| matches!(event, DriverEvent::TurnStarted)),
            "a wake must not be reported before the model produces output"
        );

        let wake = [
            json!({"type":"stream_event","parent_tool_use_id":null,
                "event":{"type":"message_start","message":{"role":"assistant"}}}),
            json!({"type":"stream_event","parent_tool_use_id":null,
                "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"It finished."}}}),
            json!({"type":"assistant","parent_tool_use_id":null,
                "message":{"content":[{"type":"text","text":"It finished."}]}}),
            json!({"type":"result","is_error":false,"stop_reason":"end_turn","num_turns":1}),
        ];
        for message in wake {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }
        let seen: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(
            matches!(seen.first(), Some(DriverEvent::TurnStarted)),
            "the wake continues the held turn"
        );
        assert!(matches!(&seen[1], DriverEvent::TextDelta(text) if text == "It finished."));
        assert!(matches!(
            seen.last(),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, DriverEvent::TurnStarted))
                .count(),
            1,
            "one wake is reported once"
        );
        assert!(!*turn.lock());
        assert!(!state.parked);
    }

    #[test]
    fn subagent_output_between_turns_does_not_open_a_turn() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        handle_message(
            &json!({"type":"result","is_error":false,"stop_reason":"end_turn"}),
            "s",
            &events,
            &commands,
            &turn,
            true,
            &mut state,
        );
        while event_rx.try_recv().is_ok() {}

        // A detached subagent keeps streaming after the main turn settled.
        let wire = [
            json!({"type":"stream_event","parent_tool_use_id":"toolu_agent",
                "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"still going"}}}),
            json!({"type":"assistant","parent_tool_use_id":"toolu_agent",
                "message":{"content":[{"type":"text","text":"still going"}]}}),
        ];
        for message in wire {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }
        assert!(!*turn.lock());
        assert!(
            !std::iter::from_fn(|| event_rx.try_recv().ok())
                .any(|event| matches!(event, DriverEvent::TurnStarted)),
            "subagent output is not the main model waking"
        );
    }

    #[test]
    fn ambient_housekeeping_tasks_stay_out_of_the_registry() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        let wire = [
            json!({"type":"system","subtype":"task_started","task_id":"w-watch",
                "task_type":"local_bash","description":"Artifact live updates",
                "is_backgrounded":true,"ambient":true}),
            json!({"type":"system","subtype":"background_tasks_changed","tasks":[
                {"task_id":"w-watch","task_type":"local_bash","ambient":true},
                {"task_id":"b1","task_type":"local_bash","description":"build"}
            ]}),
            json!({"type":"system","subtype":"task_notification","task_id":"w-watch",
                "status":"completed","output_file":"","summary":"","ambient":true}),
        ];
        for message in wire {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }
        let seen: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(seen.len(), 1, "only the level signal carries user work");
        let DriverEvent::BackgroundWork(BackgroundWorkEvent::ReconcileLive { items }) = &seen[0]
        else {
            panic!("the level signal should reconcile live work");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key.provider_id, "b1");
        assert_eq!(
            state.live_tasks,
            HashSet::from(["b1".to_owned()]),
            "only user work can park a turn"
        );
    }

    #[test]
    fn usage_flows_from_main_thread_messages_and_the_settled_result() {
        let (events, event_rx, commands, _command_rx, turn, mut state) = harness();
        // Shapes captured from a live 2.1.223 stream.
        let wire = [
            // Main-thread call: the last iteration is the live context.
            json!({"type":"assistant","parent_tool_use_id":null,"message":{
                "model":"claude-fable-5",
                "usage":{
                    "input_tokens":900,"cache_read_input_tokens":70,
                    "cache_creation_input_tokens":20,"output_tokens":50,
                    "iterations":[{"input_tokens":2,"cache_read_input_tokens":100,
                        "cache_creation_input_tokens":10,"output_tokens":8}]
                },
                "content":[]}}),
            // A subagent runs its own context; it must not touch this meter.
            json!({"type":"assistant","parent_tool_use_id":"toolu_9","message":{
                "model":"claude-haiku-4-5-20251001",
                "usage":{"input_tokens":999999,"output_tokens":1},
                "content":[]}}),
            json!({"type":"result","is_error":false,"modelUsage":{
                "claude-haiku-4-5-20251001":{"contextWindow":200000,"canonicalModel":"claude-haiku-4-5"},
                "claude-fable-5":{"contextWindow":1000000,"canonicalModel":"claude-fable-5"}
            }}),
        ];
        for message in wire {
            handle_message(&message, "s", &events, &commands, &turn, true, &mut state);
        }

        let usage: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                DriverEvent::UsageUpdated {
                    context_tokens,
                    context_window,
                } => Some((context_tokens, context_window)),
                _ => None,
            })
            .collect();
        // 2+100+10+8 from the last iteration, then the main model's window —
        // not the subagent's tokens, not the smaller subagent window.
        assert_eq!(usage, [(Some(120), None), (None, Some(1_000_000))]);
    }
}
