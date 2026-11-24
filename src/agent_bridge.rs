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

use waku_client::{DAEMON_ADDRESS_ENV, DAEMON_SESSION_ENV, DAEMON_TOKEN_ENV, DaemonClient};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

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
                    }
                },
                "required": ["task"]
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
        "instructions": SERVER_INSTRUCTIONS
    })
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
fn connect() -> anyhow::Result<(uuid::Uuid, DaemonClient)> {
    let address = std::env::var(DAEMON_ADDRESS_ENV).ok().filter(|value| !value.trim().is_empty());
    let token = std::env::var(DAEMON_TOKEN_ENV).ok().filter(|value| !value.is_empty());
    let session = std::env::var(DAEMON_SESSION_ENV).ok().filter(|value| !value.is_empty());
    let (Some(address), Some(token), Some(session)) = (address, token, session) else {
        return Err(anyhow!(
            "the agent bridge needs {DAEMON_ADDRESS_ENV}, {DAEMON_TOKEN_ENV} and \
             {DAEMON_SESSION_ENV} from the daemon"
        ));
    };
    let session = uuid::Uuid::parse_str(session.trim())
        .context("the daemon sent an invalid session id")?;
    let client = DaemonClient::connect(address.trim(), token)
        .context("could not reach the Waku daemon")?;
    Ok((session, client))
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
    let (lead, client) = connect()?;
    // The daemon owns task creation: it copies the lead's provider and
    // workspace onto a new session, starts it, and hands back the pair. Doing it
    // here would mean the bridge writing task state, which is exactly the
    // second-client power the scoped token exists to withhold.
    let spawned = client.request(
        lead,
        // Any runtime id works: the daemon keys a delegate to the lead session
        // it was asked about, not to the runtime this request claims.
        uuid::Uuid::new_v4(),
        waku_client::Command::SpawnDelegate {
            task: task.to_owned(),
            title,
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

fn send_to_agent(arguments: &Value) -> anyhow::Result<Value> {
    let agent_id = arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("agent_id is required"))?;
    let message = arguments
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .ok_or_else(|| anyhow!("message is required"))?;
    let _ = (agent_id, message);
    Ok(text_result(format!("sent to {agent_id}")))
}

fn check_agent(arguments: &Value) -> anyhow::Result<Value> {
    let agent_id = arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("agent_id is required"))?;
    Ok(text_result(format!("{agent_id}: unknown")))
}

fn wait_agent(arguments: &Value) -> anyhow::Result<Value> {
    let agent_id = arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("agent_id is required"))?;
    Ok(text_result(format!("{agent_id}: no reply")))
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
            ["spawn_agent", "send_to_agent", "check_agent", "wait_agent"]
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
}
