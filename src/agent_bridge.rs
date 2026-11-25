//! MCP server that lets a running agent drive other Waku sessions.
//!
//! This is the transport half of group agents: the conversation that owns this
//! server is the group's lead, and the tools here create, message, and read the
//! sessions it delegates to. Every one of those sessions is an ordinary Waku
//! task, so a delegate runs in its own provider process with its own transcript.
//!
//! The channel is the one Waku already ships. A provider launches its MCP
//! servers itself, so this process is a child of that provider, and it reaches
//! the daemon the same way any client does — through the address and token the
//! daemon handed to its own children. Nothing new crosses a trust boundary: the
//! bridge holds exactly the credential the desktop held when it started.
//!
//! Deliberately synchronous per call. `spawn_agent` returns as soon as the
//! session exists so the lead can start several at once; `wait_agent` is the
//! only tool that blocks, and the lead chooses to call it.

use std::io::{BufRead, BufReader, Write};

use anyhow::{Context as _, anyhow};
use serde_json::{Value, json};

use waku_client::{
    DAEMON_ADDRESS_ENV, DAEMON_RUNTIME_ENV, DAEMON_SESSION_ENV, DAEMON_TOKEN_ENV, DaemonClient,
};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// How long `wait_agent` blocks before handing the delegate back with whatever
/// it has. A model turn can be long, but an unbounded wait would hold the lead's
/// tool call open forever if the delegate never settles.
const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
/// Bounded by the turn it watches, not by a tick: the delegate's state is read
/// from the daemon each time rather than cached here.
const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

const SERVER_INSTRUCTIONS: &str = "This server runs Waku sessions for a lead agent. \
Use `spawn_agent` to start a delegate on a self-contained task, then `check_agent` \
or `wait_agent` to read what it produced. `spawn_agent` returns immediately — start \
every delegate you need first, then collect their results, rather than waiting after \
each one. Use `send_to_agent` to add instructions to a delegate that is still \
running.";

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "spawn_agent",
            "description": "Start a new Waku session, running one delegate on the given task. Returns the new agent id at once; the delegate keeps working in the background. Give each delegate a self-contained task: it does not see your conversation.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The complete instruction for the delegate, written as if it had no other context."
                    },
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 200,
                        "description": "Short label shown in the session list."
                    },
                    "persona": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Id of an agent identity to start this delegate as, from the roster you were given. An identity fixes how it works, what it must not do, and which model it runs on. Omit it to start a plain delegate that inherits your own settings."
                    }
                },
                "required": ["task"]
            }
        }),
        json!({
            "name": "spawn_team",
            "description": "Start one delegate per member of a saved team, each running as its own identity. Use this when a job needs several roles at once; the members share the team's brief and each applies its own role to the same task.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "team": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Id of a team from the roster you were given."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The job every member works on, written as if it had no other context."
                    },
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 200,
                        "description": "Short label prefixed onto each member's session title."
                    }
                },
                "required": ["team", "task"]
            }
        }),
        json!({
            "name": "send_to_agent",
            "description": "Send another instruction to a running delegate. It joins the delegate's current turn as a steer, so it can redirect work already in progress.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Id returned by spawn_agent."
                    },
                    "message": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The instruction to pass on."
                    }
                },
                "required": ["agent_id", "message"]
            }
        }),
        json!({
            "name": "check_agent",
            "description": "Read a delegate's current status and the text it has produced so far, without waiting.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Id returned by spawn_agent."
                    }
                },
                "required": ["agent_id"]
            }
        }),
        json!({
            "name": "wait_agent",
            "description": "Block until a delegate finishes its current turn, then return its reply. Use this only after every delegate has been spawned.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Id returned by spawn_agent."
                    }
                },
                "required": ["agent_id"]
            }
        }),
    ]
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "waku_agents", "version": env!("CARGO_PKG_VERSION")},
        "instructions": server_instructions()
    })
}

/// What the lead is told when this server comes up.
///
/// The roster is read here rather than baked into `spawn_agent`'s description
/// because it changes as the user edits identities: a description is the
/// static part of the handshake, and an identity edited mid-session would
/// otherwise go unnoticed until the next launch. A failure to read it leaves
/// the static instructions alone: a lead that cannot see the roster can still
/// start an anonymous delegate, and failing the handshake would cost it every
/// tool.
fn server_instructions() -> String {
    let Ok((lead, runtime, client)) = connect() else {
        return SERVER_INSTRUCTIONS.to_owned();
    };
    let Ok(waku_client::ResponsePayload::AgentsCatalog { catalog }) = client.request(
        lead,
        runtime,
        waku_client::Command::LoadAgents { project: None },
    ) else {
        return SERVER_INSTRUCTIONS.to_owned();
    };
    let roster = catalog.roster();
    if roster.is_empty() {
        return SERVER_INSTRUCTIONS.to_owned();
    }
    format!("{SERVER_INSTRUCTIONS}\n\n{roster}")
}

pub fn serve_stdio() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(BufReader::new(stdin.lock()), std::io::BufWriter::new(stdout.lock()))
}

fn serve<R: BufRead, W: Write>(mut input: R, mut output: W) -> anyhow::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        if line.len() > MAX_REQUEST_BYTES {
            write_message(
                &mut output,
                &json_rpc_error(Value::Null, -32600, "MCP request is too large"),
            )?;
            continue;
        }
        let message: Value = match serde_json::from_str(line.trim()) {
            Ok(message) => message,
            Err(error) => {
                write_message(
                    &mut output,
                    &json_rpc_error(Value::Null, -32700, &format!("invalid JSON: {error}")),
                )?;
                continue;
            }
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            if let Some(id) = message.get("id") {
                write_message(
                    &mut output,
                    &json_rpc_error(id.clone(), -32600, "JSON-RPC method is required"),
                )?;
            }
            continue;
        };
        // A notification carries no id and expects no reply.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => json_rpc_result(id, initialize_result()),
            "tools/list" => json_rpc_result(id, json!({"tools": tool_definitions()})),
            "ping" => json_rpc_result(id, json!({})),
            "tools/call" => call_tool(&params)
                .map(|result| json_rpc_result(id.clone(), result))
                .unwrap_or_else(|error| json_rpc_error(id, -32603, &format!("{error:#}"))),
            other => json_rpc_error(id, -32601, &format!("unknown method {other}")),
        };
        write_message(&mut output, &response)?;
    }
    Ok(())
}

fn call_tool(params: &Value) -> anyhow::Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("tools/call requires a tool name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "spawn_agent" => spawn_agent(&arguments),
        "spawn_team" => spawn_team(&arguments),
        "send_to_agent" => send_to_agent(&arguments),
        "check_agent" => check_agent(&arguments),
        "wait_agent" => wait_agent(&arguments),
        other => Err(anyhow!("unknown tool {other}")),
    }
}

/// Opens the daemon connection the tools share.
///
/// The address and token are the ones the daemon handed its own children, so a
/// bridge started by a provider inherits them rather than being told anything
/// new. A bridge that cannot find them cannot act, which is the honest answer:
/// it is not part of a Waku session.
/// What one tool call needs to reach the daemon: the client, and the lead
/// session and runtime its token is scoped to.
type Bridge = (uuid::Uuid, uuid::Uuid, DaemonClient);

fn connect() -> anyhow::Result<Bridge> {
    let address = std::env::var(DAEMON_ADDRESS_ENV).ok().filter(|value| !value.trim().is_empty());
    let token = std::env::var(DAEMON_TOKEN_ENV).ok().filter(|value| !value.is_empty());
    let session = std::env::var(DAEMON_SESSION_ENV).ok().filter(|value| !value.is_empty());
    let runtime = std::env::var(DAEMON_RUNTIME_ENV).ok().filter(|value| !value.is_empty());
    let (Some(address), Some(token), Some(session), Some(runtime)) =
        (address, token, session, runtime)
    else {
        return Err(anyhow!(
            "the agent bridge needs {DAEMON_ADDRESS_ENV}, {DAEMON_TOKEN_ENV}, \
             {DAEMON_SESSION_ENV} and {DAEMON_RUNTIME_ENV} from the daemon"
        ));
    };
    let session = uuid::Uuid::parse_str(session.trim())
        .context("the daemon sent an invalid session id")?;
    let runtime = uuid::Uuid::parse_str(runtime.trim())
        .context("the daemon sent an invalid runtime id")?;
    let client = DaemonClient::connect(address.trim(), token)
        .context("could not reach the Waku daemon")?;
    Ok((session, runtime, client))
}

fn text_result(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": false})
}

fn spawn_agent(arguments: &Value) -> anyhow::Result<Value> {
    let task = arguments
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| anyhow!("task is required"))?;
    let title = arguments
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    // The identity the lead chooses, by catalog id. It rides as an id rather
    // than as instructions so the daemon stays the authority on which
    // identities exist: a bridge cannot invent one.
    let persona = arguments
        .get("persona")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|persona| !persona.is_empty())
        .map(str::to_owned);
    let (lead, runtime, client) = connect()?;
    // The daemon owns task creation: it copies the lead's provider and
    // workspace onto a new session, starts it, and hands back the pair. Doing it
    // here would mean the bridge writing task state, which is exactly the
    // second-client power the scoped token exists to withhold.
    let spawned = client.request(
        lead,
        runtime,
        waku_client::Command::SpawnDelegate {
            task: task.to_owned(),
            title,
            persona,
        },
    )?;
    let waku_client::ResponsePayload::DelegateSpawned {
        session_id,
        runtime_id,
    } = spawned
    else {
        return Err(anyhow!("the daemon did not report a delegate"));
    };
    Ok(text_result(format!(
        "spawned agent {session_id} for task: {task}"
    )))
    .map(|mut result| {
        result["delegate"] = json!({
            "agent_id": session_id.to_string(),
            "runtime_id": runtime_id.to_string()
        });
        result
    })
}

/// Starts one delegate per member of a saved team.
///
/// A member that could not start is reported as such, because the caller has to
/// know which of the roles it asked for are actually running: a report that
/// says "the team started" while two of five failed would make the lead wait
/// for answers that are never coming.
fn spawn_team(arguments: &Value) -> anyhow::Result<Value> {
    let team = arguments
        .get("team")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|team| !team.is_empty())
        .ok_or_else(|| anyhow!("team is required"))?;
    let task = arguments
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| anyhow!("task is required"))?;
    let title = arguments
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let (lead, runtime, client) = connect()?;
    let spawned = client.request(
        lead,
        runtime,
        waku_client::Command::SpawnTeam {
            team: team.to_owned(),
            task: task.to_owned(),
            title,
        },
    )?;
    let waku_client::ResponsePayload::DelegateTeamSpawned { delegates } = spawned else {
        return Err(anyhow!("the daemon did not report the team's delegates"));
    };
    let mut started = Vec::new();
    let mut failed = Vec::new();
    for delegate in delegates {
        if let Some(error) = delegate.error {
            failed.push(format!("{}: {error}", delegate.persona));
        } else {
            started.push(format!("{} ({})", delegate.persona, delegate.session_id));
        }
    }
    let mut text = if started.is_empty() {
        format!("no member of team {team} could start")
    } else {
        format!("started {} of team {team}: {}", started.len(), started.join(", "))
    };
    if !failed.is_empty() {
        text.push_str(&format!("\nfailed: {}", failed.join("; ")));
    }
    Ok(text_result(text))
}

fn send_to_agent(arguments: &Value) -> anyhow::Result<Value> {
    let agent_id = agent_id(arguments)?;
    let message = arguments
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .ok_or_else(|| anyhow!("message is required"))?;
    let (lead, runtime, client) = connect()?;
    // Which of the two shapes this takes is the daemon's call: only it knows
    // whether the delegate's turn is still open, and a steer into a finished
    // turn is refused by every provider.
    let prompted = client.request(
        lead,
        runtime,
        waku_client::Command::PromptDelegate {
            session_id: agent_id,
            prompt: message.to_owned(),
        },
    )?;
    let waku_client::ResponsePayload::DelegatePrompted { steered, .. } = prompted else {
        return Err(anyhow!("the daemon did not report how the message landed"));
    };
    Ok(text_result(if steered {
        format!("agent {agent_id} was working, so the message steered its running turn")
    } else {
        format!("agent {agent_id} was idle, so the message started a new turn")
    }))
}

fn check_agent(arguments: &Value) -> anyhow::Result<Value> {
    let agent_id = agent_id(arguments)?;
    let (lead, runtime, client) = connect()?;
    let report = read_delegate(&client, lead, runtime, agent_id)?;
    Ok(text_result(describe_delegate(&report)))
}

/// Blocks until the delegate's turn settles, then returns what it wrote.
///
/// Polled rather than pushed: the bridge is a synchronous MCP server answering
/// one tool call at a time, so there is nowhere to put a subscription that
/// outlives this call. A poll every few hundred milliseconds is cheap next to a
/// model turn, and it keeps the state it reads authoritative — the daemon's own
/// record, not a copy this process made.
fn wait_agent(arguments: &Value) -> anyhow::Result<Value> {
    let agent_id = agent_id(arguments)?;
    let (lead, runtime, client) = connect()?;
    let deadline = std::time::Instant::now() + WAIT_TIMEOUT;
    loop {
        let report = read_delegate(&client, lead, runtime, agent_id)?;
        if !report.status.is_busy() || std::time::Instant::now() >= deadline {
            return Ok(text_result(describe_delegate(&report)));
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

fn read_delegate(
    client: &DaemonClient,
    lead: uuid::Uuid,
    runtime: uuid::Uuid,
    agent_id: uuid::Uuid,
) -> anyhow::Result<DelegateReport> {
    let report = client.request(
        lead,
        runtime,
        waku_client::Command::ReadDelegate {
            session_id: agent_id,
        },
    )?;
    match report {
        waku_client::ResponsePayload::DelegateReport {
            session_id,
            title,
            status,
            reply,
            failure,
        } => Ok(DelegateReport {
            session_id,
            title,
            status,
            reply,
            failure,
        }),
        _ => Err(anyhow!("the daemon did not report on that agent")),
    }
}

/// One delegate as the bridge reads it.
///
/// The wire variant is matched once, on the way in, so formatting has a single
/// input shape rather than a `let … else` at every call site.
struct DelegateReport {
    session_id: uuid::Uuid,
    title: String,
    status: waku_client::model::SessionStatus,
    reply: String,
    failure: Option<String>,
}

/// How a report reads to the lead.
///
/// State first, because it decides whether waiting longer is worth it; then the
/// reason it failed, which is what the lead has to act on; then the delegate's
/// own words, which are what it asked for.
fn describe_delegate(report: &DelegateReport) -> String {
    let label = match report.status {
        waku_client::model::SessionStatus::Idle => "finished",
        waku_client::model::SessionStatus::Failed => "failed",
        _ => "still working",
    };
    let title = report.title.trim();
    let mut text = if title.is_empty() {
        format!("agent {} ({label})", report.session_id)
    } else {
        format!("agent {} \"{title}\" ({label})", report.session_id)
    };
    if let Some(failure) = report
        .failure
        .as_deref()
        .map(str::trim)
        .filter(|failure| !failure.is_empty())
    {
        text.push_str(&format!("\nreason: {failure}"));
    }
    text.push('\n');
    let reply = report.reply.trim();
    if reply.is_empty() {
        text.push_str(if report.status.is_busy() {
            "it has not written anything yet"
        } else {
            "it finished without writing anything"
        });
    } else {
        text.push_str(reply);
    }
    text
}

/// The agent id a tool call names.
///
/// Parsed here so every tool refuses a made-up id the same way, instead of
/// sending the daemon a string it would have to interpret.
fn agent_id(arguments: &Value) -> anyhow::Result<uuid::Uuid> {
    let raw = arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("agent_id is required"))?;
    uuid::Uuid::parse_str(raw.trim())
        .with_context(|| format!("agent_id {raw:?} is not one spawn_agent returned"))
}

fn write_message(output: &mut impl Write, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *output, message)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lead discovers the tool surface before it can call anything, so the
    /// handshake has to advertise exactly these tools.
    #[test]
    fn the_handshake_advertises_the_agent_tools() {
        let result = initialize_result();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], "waku_agents");
        let names = tool_definitions()
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["spawn_agent", "spawn_team", "send_to_agent", "check_agent", "wait_agent"]
        );
    }

    /// `spawn_agent` is the one call that must not block: the lead starts
    /// several delegates before collecting any result, so a blocking spawn
    /// would serialize the whole group.
    #[test]
    fn spawn_requires_a_task_and_never_waits_for_one() {
        let missing = call_tool(&json!({"name": "spawn_agent", "arguments": {}}));
        assert!(missing.is_err(), "a spawn without a task is refused");
    }

    /// The bridge is useless without the daemon's own credential, and it has to
    /// say so rather than pretend a delegate exists.
    #[test]
    fn a_bridge_without_daemon_credentials_refuses_to_act() {
        // The test process never carries a daemon token.
        let spawned = call_tool(&json!({
            "name": "spawn_agent",
            "arguments": {"task": "do something"}
        }));
        assert!(
            spawned.is_err(),
            "without the daemon address and token there is nothing to drive"
        );
    }

    #[test]
    fn an_unknown_method_is_reported_not_ignored() {
        let mut output = Vec::new();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/nope"}"#,
            "\n"
        );
        serve(BufReader::new(input.as_bytes()), &mut output).unwrap();
        let message: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(message["id"], 7);
        assert_eq!(message["error"]["code"], -32601);
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let mut output = Vec::new();
        serve(
            BufReader::new(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.as_bytes()),
            &mut output,
        )
        .unwrap();
        assert!(output.is_empty(), "notifications are not answered");
    }

    /// A report leads with the state, then the reason it failed, then the
    /// delegate's own words.
    ///
    /// That order is the one a lead can act on: the state decides whether to
    /// wait longer, the reason says whether waiting could help at all, and the
    /// words are what it came to read.
    #[test]
    fn a_report_reads_state_then_reason_then_the_delegates_words() {
        let id = uuid::Uuid::new_v4();
        let working = describe_delegate(&DelegateReport {
            session_id: id,
            title: "probe".into(),
            status: waku_client::model::SessionStatus::Working,
            reply: "half an answer".into(),
            failure: None,
        });
        assert!(working.contains("still working"), "{working}");
        assert!(
            working.contains("probe"),
            "the title belongs in it: {working}"
        );
        assert!(working.ends_with("half an answer"), "{working}");

        let failed = describe_delegate(&DelegateReport {
            session_id: id,
            title: String::new(),
            status: waku_client::model::SessionStatus::Failed,
            reply: String::new(),
            failure: Some("context window exhausted".into()),
        });
        assert!(failed.contains("failed"), "{failed}");
        assert!(failed.contains("context window exhausted"), "{failed}");
        assert!(
            failed.contains("finished without writing anything"),
            "a settled delegate must not claim it is still writing: {failed}"
        );
        assert!(
            !failed.contains("\"\""),
            "an absent title must not render as empty quotes: {failed}"
        );
    }

    /// The reading tools refuse an id that `spawn_agent` could not have
    /// returned, instead of asking the daemon about a string it would have to
    /// interpret.
    #[test]
    fn the_reading_tools_refuse_an_id_that_was_never_handed_out() {
        for tool in ["check_agent", "wait_agent"] {
            let refused = call_tool(&json!({
                "name": tool,
                "arguments": {"agent_id": "not-a-uuid"}
            }));
            let error = refused.expect_err("a made-up id cannot name a delegate");
            assert!(
                format!("{error}").contains("not one spawn_agent returned"),
                "{tool} has to explain the bad id: {error}"
            );
        }
        let refused = call_tool(&json!({
            "name": "send_to_agent",
            "arguments": {"agent_id": "not-a-uuid", "message": "carry on"}
        }));
        assert!(refused.is_err(), "send_to_agent must refuse the same id");
    }
}
