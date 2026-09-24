//! Claude Code SDK's Python MCP wire protocol. Execution and scheduling are
//! caller-owned.
use std::time::Duration;

use base64::Engine as _;
use rho_agent_types::ToolOutputStatus;
use rho_inference::types::{ExecId, ToolName, ToolOutput, ToolSpec, ToolType};
use serde_json::{Value, json};

/// The server's name in Claude Code, which is where the model's tool name
/// `mcp__py__exec` comes from: Claude Code offers no other naming.
pub const SERVER_NAME: &str = "py";
pub const TOOL_NAME: &str = "exec";

/// How long Claude Code lets one exec call stay open. Above the longest
/// check-in a cell can ask for (an hour) with room for the patience around
/// it, so the CLI never fails a call the boundary is holding on purpose.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

const EXEC_DESCRIPTION: &str = "Run Python in Rho's persistent notebook. Returns once the cell \
has something worth reporting (it returned, produced output, a check-in fired, or a message \
arrived), with the output so far; cells keep running afterwards and later output is attached to \
the next exec result.";

/// The tool as a Rho spec, for renderings of the surface.
pub fn exec_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(TOOL_NAME).expect("exec is a valid tool name"),
        tool_type: ToolType::Function,
        description: EXEC_DESCRIPTION.into(),
        input_schema: exec_input_schema(),
        format: None,
    }
}

fn exec_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "description": "Python source to run as one cell." }
        },
        "required": ["source"],
    })
}

/// The tool as MCP lists it. `anthropic/alwaysLoad` keeps it out of the
/// deferred set should tool search ever be on after all.
fn tool_listing() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": EXEC_DESCRIPTION,
        "inputSchema": exec_input_schema(),
        "_meta": { "anthropic/alwaysLoad": true },
    })
}

/// What one JSON-RPC message from the CLI asks for.
#[derive(Debug, PartialEq)]
pub enum Rpc {
    /// Answered on the spot.
    Reply(Value),
    /// A cell to run; the reply waits on the boundary.
    Exec {
        id: Value,
        exec_id: ExecId,
        source: String,
    },
    /// A notification: nothing to say back beyond acknowledging the control
    /// request.
    Ignore,
}

/// Sorts one JSON-RPC message: the MCP handshake and listing are answered
/// here, a call of `exec` is handed back to run.
pub fn handle_rpc(message: &Value) -> Rpc {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = message.get("id").cloned() else {
        return Rpc::Ignore;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => Rpc::Reply(reply(
            id,
            json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "rho-python", "version": env!("CARGO_PKG_VERSION") },
            }),
        )),
        "ping" => Rpc::Reply(reply(id, json!({}))),
        "tools/list" => Rpc::Reply(reply(id, json!({ "tools": [tool_listing()] }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name != TOOL_NAME {
                return Rpc::Reply(error_reply(id, -32602, format!("unknown tool {name}")));
            }
            let Some(exec_id) = params
                .pointer("/_meta/claudecode~1toolUseId")
                .and_then(Value::as_str)
                .and_then(|id| ExecId::try_from(id).ok())
            else {
                return Rpc::Reply(error_reply(
                    id,
                    -32602,
                    "exec requires Claude Code's provider tool-use identity".into(),
                ));
            };
            match params
                .get("arguments")
                .and_then(|arguments| arguments.get("source"))
                .and_then(Value::as_str)
            {
                Some(source) => Rpc::Exec {
                    id,
                    exec_id,
                    source: source.to_owned(),
                },
                None => Rpc::Reply(reply(
                    id,
                    tool_result(
                        vec![text_item("exec takes {\"source\": \"<python>\"}")],
                        true,
                    ),
                )),
            }
        }
        _ => Rpc::Reply(error_reply(
            id,
            -32601,
            format!("method {method} is not supported"),
        )),
    }
}

pub fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub fn tool_result(content: Vec<Value>, is_error: bool) -> Value {
    json!({ "content": content, "isError": is_error })
}

pub fn text_item(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// Encode one boundary contribution in the SDK's MCP content vocabulary.
pub fn exec_result(own: Option<ToolOutput>, updates: Vec<(ExecId, ToolOutput)>) -> Value {
    let mut content = Vec::new();
    let mut is_error = false;
    if let Some(own) = &own {
        is_error = own.status == ToolOutputStatus::Error;
        content.extend(output_items(own, None));
    }
    for (id, update) in &updates {
        content.extend(output_items(
            update,
            Some(&format!("Later output from exec {}:\n", id.as_str())),
        ));
    }
    tool_result(content, is_error)
}

fn output_items(output: &ToolOutput, prefix: Option<&str>) -> Vec<Value> {
    // A cell that answered with nothing because older cells speak in the
    // same reply gets no item of its own; the older cells' items follow.
    let mut items = Vec::new();
    if prefix.is_some() || !output.output.is_empty() {
        items.push(text_item(&format!(
            "{}{}",
            prefix.unwrap_or_default(),
            output.output
        )));
    }
    items.extend(output.images.iter().map(|image| {
        json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&image.data),
            "mimeType": image.media_type,
        })
    }));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lists_exec_as_always_loaded() {
        let Rpc::Reply(reply) =
            handle_rpc(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        else {
            panic!("tools/list is answered on the spot");
        };
        assert_eq!(reply["id"], 1);
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "exec");
        assert_eq!(tools[0]["_meta"]["anthropic/alwaysLoad"], true);
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["source"]));
    }

    #[test]
    fn hands_exec_calls_back_and_refuses_the_rest() {
        assert_eq!(
            handle_rpc(&json!({
                "jsonrpc": "2.0", "id": "c1", "method": "tools/call",
                "params": {"name": "exec", "arguments": {"source": "print(1)"}, "_meta": {"claudecode/toolUseId": "toolu_one"}},
            })),
            Rpc::Exec {
                id: json!("c1"),
                exec_id: ExecId::try_from("toolu_one").unwrap(),
                source: "print(1)".into()
            }
        );
        let Rpc::Reply(reply) = handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "exec", "arguments": {}, "_meta": {"claudecode/toolUseId": "toolu_two"}},
        })) else {
            panic!("a call without source is answered on the spot");
        };
        assert_eq!(reply["result"]["isError"], true);
        let Rpc::Reply(reply) = handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "other", "arguments": {}},
        })) else {
            panic!("an unknown tool is refused on the spot");
        };
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(
            handle_rpc(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            Rpc::Ignore
        );
        let Rpc::Reply(reply) = handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"},
        })) else {
            panic!("initialize is answered on the spot");
        };
        assert_eq!(reply["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn sdk_trace_uses_provider_identity_not_rpc_id() {
        let trace: Value = serde_json::from_str(include_str!("mcp_trace.json")).unwrap();
        let mut provider_id = None;
        let mut response_ended = false;
        let mut saw_exec = false;
        for message in trace["events"].as_array().unwrap() {
            let event = &message["event"];
            if event["type"] == "content_block_start"
                && event["content_block"]["type"] == "tool_use"
            {
                provider_id = event["content_block"]["id"].as_str();
            }
            if event["type"] == "message_stop" {
                response_ended = true;
            }
            if message["request"]["message"]["method"] == "tools/call" {
                let Rpc::Exec {
                    id,
                    exec_id,
                    source,
                } = handle_rpc(&message["request"]["message"])
                else {
                    panic!("the SDK forwarded an exec");
                };
                assert_eq!(exec_id.as_str(), provider_id.unwrap());
                assert_eq!(id, 2); // independent JSON-RPC counter, not the provider id
                assert_eq!(source, "print(42)");
                assert!(response_ended);
                saw_exec = true;
            }
        }
        assert!(saw_exec);
    }

    #[test]
    fn missing_identity_never_invents_or_matches_an_exec() {
        for meta in [
            Value::Null,
            json!({"claudecode/toolUseId": ""}),
            json!({"claudecode/toolUseId": 7}),
        ] {
            let Rpc::Reply(value) = handle_rpc(&json!({
                "id": 2, "method": "tools/call",
                "params": {"name": "exec", "arguments": {"source": "pass"}, "_meta": meta}
            })) else {
                panic!("invalid identity must be refused");
            };
            assert_eq!(value["error"]["code"], -32602);
        }
    }
}
