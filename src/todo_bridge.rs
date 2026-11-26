//! A tiny MCP server that gives a Claude session a `todo_write` tool.
//!
//! Some provider endpoints do not expose Claude Code's built-in TodoWrite.
//! Waku injects this server into every Claude session instead, so the model
//! has a familiar tool for recording its task list whatever endpoint it uses.
//!
//! The server deliberately keeps no state of its own. The tool call itself is
//! recorded in the provider's transcript as a `tool_use` block, which the
//! Claude driver already turns into a plan activity — that activity is what
//! the UI reads as the session's task list. An extra copy here would only be a
//! second source of truth that could drift from the transcript.

use std::io::{BufRead, BufReader, Write};

use anyhow::Context as _;
use serde_json::{Value, json};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// The tool name Claude shows the model. The driver recognizes `todo_write`
/// after stripping the MCP prefix, so this maps straight onto a plan activity.
const TODO_TOOL_NAME: &str = "todo_write";

fn tool_definitions() -> Vec<Value> {
    vec![json!({
        "name": TODO_TOOL_NAME,
        "description": "Record the task list for the current work. Call this whenever you plan multi-step work: pass the full list, updating it as items change. Each item needs its content and status.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete task list, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "What the task is."},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Where the task stands."
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Short present-continuous form shown while the task is in progress."
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }
    })]
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "waku_todo", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "Use todo_write to keep the task list current as you work."
    })
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

fn write_message(output: &mut impl Write, message: &Value) -> anyhow::Result<()> {
    let line = serde_json::to_string(message).context("could not encode an MCP response")?;
    writeln!(output, "{line}")?;
    output.flush()?;
    Ok(())
}

/// A `todo_write` call is confirmed without keeping a copy: the tool_use block
/// in the provider transcript is the task list's single source of truth.
fn call_tool(params: &Value) -> anyhow::Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tools/call requires a tool name"))?;
    if name != TODO_TOOL_NAME {
        return Err(anyhow::anyhow!("unknown tool {name}"));
    }
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let count = arguments
        .get("todos")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!("Recorded {count} task{}.", if count == 1 { "" } else { "s" })
        }],
        "isError": false
    }))
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

pub fn serve_stdio() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(
        BufReader::new(stdin.lock()),
        std::io::BufWriter::new(stdout.lock()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_todo_write_call_is_confirmed_with_a_count() {
        let result = call_tool(&json!({
            "name": "todo_write",
            "arguments": {"todos": [
                {"content": "one", "status": "pending"},
                {"content": "two", "status": "completed"}
            ]}
        }))
        .unwrap();
        assert_eq!(result["content"][0]["text"], "Recorded 2 tasks.");
        assert_eq!(result["isError"], false);
    }

    #[test]
    fn an_unknown_tool_is_refused() {
        assert!(call_tool(&json!({"name": "spawn_agent"})).is_err());
        assert!(call_tool(&json!({})).is_err());
    }

    #[test]
    fn the_server_advertises_only_todo_write() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "todo_write");
    }

    #[test]
    fn a_full_mcp_handshake_and_call() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"todo_write\",\"arguments\":{\"todos\":[{\"content\":\"x\",\"status\":\"pending\"}]}}}\n",
        );
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        let init: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "waku_todo");
        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "todo_write");
        let call: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(call["result"]["content"][0]["text"], "Recorded 1 task.");
    }
}
