//! Rho's Python notebook, served to Claude Code as an in-process MCP server.
//!
//! Claude Code routes every JSON-RPC message for an SDK-hosted server
//! through its control channel, and the loop hands those here. A
//! `tools/call` of `exec` starts a cell and holds the reply until
//! [`boundary`] — the one decision that opens a native agent's next request
//! — says the model should look: the cell returned, its output stands on its
//! own, a check-in came due, or a user message is waiting. Everything older
//! cells have said since the model last looked rides along with that reply.
//! With no call open, the same decision says when an idle model is woken
//! with a message instead. Nothing here decides anything of its own.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use rho_agent_tools::{PythonExec, PythonTool, SourceWaker, Tool, ToolSession};
use rho_core::{
    ToolCall, ToolCallId, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType, UnixMs,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::agent::boundary::{Boundary, ModelAsked, ModelTurn, SourceKind, boundary};
use crate::agent::{Phase, Standing, ToolCallAnswer};

/// The server's name in Claude Code, which is where the model's tool name
/// `mcp__py__exec` comes from: Claude Code offers no other naming.
pub(crate) const SERVER_NAME: &str = "py";
pub(crate) const TOOL_NAME: &str = "exec";

/// How long Claude Code lets one exec call stay open. Above the longest
/// check-in a cell can ask for (an hour) with room for the patience around
/// it, so the CLI never fails a call the boundary is holding on purpose.
pub(crate) const EXEC_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

const EXEC_DESCRIPTION: &str = "Run Python in Rho's persistent notebook. Returns once the cell \
has something worth reporting (it returned, produced output, a check-in fired, or a message \
arrived), with the output so far; cells keep running afterwards and later output is attached to \
the next exec result.";

/// The tool as a Rho spec, for renderings of the surface.
pub(crate) fn exec_spec() -> ToolSpec {
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
pub(crate) enum Rpc {
    /// Answered on the spot.
    Reply(Value),
    /// A cell to run; the reply waits on the boundary.
    Exec { id: Value, source: String },
    /// A notification: nothing to say back beyond acknowledging the control
    /// request.
    Ignore,
}

/// Sorts one JSON-RPC message: the MCP handshake and listing are answered
/// here, a call of `exec` is handed back to run.
pub(crate) fn handle_rpc(message: &Value) -> Rpc {
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
            match params
                .get("arguments")
                .and_then(|arguments| arguments.get("source"))
                .and_then(Value::as_str)
            {
                Some(source) => Rpc::Exec {
                    id,
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

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_result(content: Vec<Value>, is_error: bool) -> Value {
    json!({ "content": content, "isError": is_error })
}

fn text_item(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// One exec call the CLI is waiting on.
#[derive(Debug)]
pub(crate) struct PendingExec {
    /// The control request carrying the call, which the reply answers.
    pub request_id: String,
    /// The JSON-RPC id of the `tools/call`.
    pub rpc_id: Value,
    cell: u64,
}

/// The core's bookkeeping for one cell, as the native runtime keeps it for a
/// call: how much of its story the model has.
struct Cell {
    session: Box<dyn ToolSession>,
    answer: ToolCallAnswer,
    answered_sources: HashSet<u64>,
}

/// One notebook, its cells, and the exec call (if any) the CLI is waiting on.
pub(crate) struct PythonHost {
    tool: PythonTool,
    /// The host functions the notebook exposes, for the prompt.
    host_specs: Vec<ToolSpec>,
    /// Woken by any cell with something new; the loop asks the boundary.
    notify: Arc<Notify>,
    cells: BTreeMap<u64, Cell>,
    next_cell: u64,
    pending: Option<PendingExec>,
    /// The newest exec, whose check-in sets the pace even after its session
    /// is reaped, as in the native runtime.
    latest: Option<(u64, Arc<PythonExec>)>,
    /// What the model's latest turn settled: an open call, or prose.
    turn: Option<ModelTurn>,
    /// Whether the user stopped the agent since anything was asked of it:
    /// a cancelled cell's last words must not wake the model the user just
    /// silenced, as natively (`DECISION-stopped-agents-wait-for-fresh-input`).
    standing: Standing,
}

/// Everything waiting for the model at one boundary.
pub(crate) struct Drained {
    /// The open call's own answer, when there was one.
    pub own: Option<ToolOutput>,
    /// What older cells have said since the model last looked.
    pub updates: Vec<ToolOutput>,
}

impl Drained {
    pub(crate) fn is_empty(&self) -> bool {
        self.own.is_none() && self.updates.is_empty()
    }

    /// The MCP `tools/call` result carrying all of it.
    pub(crate) fn into_mcp_result(self) -> Value {
        let mut content = Vec::new();
        let mut is_error = false;
        if let Some(own) = &self.own {
            is_error = own.status == ToolOutputStatus::Error;
            content.extend(output_items(own, None));
        }
        for update in &self.updates {
            content.extend(output_items(
                update,
                Some("Later output from an earlier cell:\n"),
            ));
        }
        tool_result(content, is_error)
    }
}

fn output_items(output: &ToolOutput, prefix: Option<&str>) -> Vec<Value> {
    let mut items = vec![text_item(&format!(
        "{}{}",
        prefix.unwrap_or_default(),
        output.output
    ))];
    items.extend(output.images.iter().map(|image| {
        json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&image.data),
            "mimeType": image.media_type,
        })
    }));
    items
}

impl PythonHost {
    pub(crate) fn new(tool: PythonTool, host_specs: Vec<ToolSpec>) -> Self {
        Self {
            tool,
            host_specs,
            notify: Arc::new(Notify::new()),
            cells: BTreeMap::new(),
            next_cell: 1,
            pending: None,
            latest: None,
            turn: None,
            standing: Standing::Nothing,
        }
    }

    pub(crate) fn host_specs(&self) -> &[ToolSpec] {
        &self.host_specs
    }

    pub(crate) fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Starts a cell for a `tools/call`. The reply waits on the boundary,
    /// except for a second call while one is open, which is refused on the
    /// spot: the notebook takes one exec per model response, as natively.
    pub(crate) fn exec(
        &mut self,
        request_id: String,
        rpc_id: Value,
        source: String,
        now: UnixMs,
    ) -> Option<Value> {
        if self.pending.is_some() {
            return Some(reply(
                rpc_id,
                tool_result(
                    vec![text_item(
                        "Python mode permits one exec call at a time; this call was not run. Wait \
                         for the open call to return, then put the work in one cell.",
                    )],
                    true,
                ),
            ));
        }
        let cell = self.next_cell;
        self.next_cell += 1;
        let call = ToolCall {
            id: ToolCallId::try_from(format!("py-{cell}").as_str())
                .expect("cell ids are valid identifiers"),
            name: ToolName::try_from(TOOL_NAME).expect("exec is a valid tool name"),
            tool_type: ToolType::Custom,
            arguments: source,
        };
        let session = self
            .tool
            .run(call, SourceWaker::new(Arc::clone(&self.notify)));
        if let Some(exec) = session.python_exec() {
            self.latest = Some((cell, exec));
        }
        self.cells.insert(
            cell,
            Cell {
                session,
                answer: ToolCallAnswer::Owed,
                answered_sources: HashSet::new(),
            },
        );
        self.pending = Some(PendingExec {
            request_id,
            rpc_id,
            cell,
        });
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: ModelAsked::Calls,
        });
        None
    }

    /// The user spoke: a stop, if there was one, is lifted.
    pub(crate) fn user_spoke(&mut self) {
        self.standing = Standing::Nothing;
    }

    /// The model ended a turn with prose: nothing is owed, and only what the
    /// cells go on to say is a reason to wake it.
    pub(crate) fn turn_ended(&mut self, now: UnixMs) {
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: ModelAsked::Nothing,
        });
    }

    /// Every source, as the boundary wants them. `user_oldest_at` is the
    /// longest-waiting message in the CLI's queue: company for an open call,
    /// which returns so the model can read it.
    fn sources(&self, user_oldest_at: Option<UnixMs>) -> Vec<SourceKind> {
        let mut sources = vec![
            SourceKind::User {
                interrupt: false,
                oldest_at: user_oldest_at,
            },
            SourceKind::Mail {
                oldest_at: None,
                newest_at: None,
            },
        ];
        for (id, cell) in &self.cells {
            let latest = self.latest.as_ref().is_some_and(|(latest, _)| latest == id);
            sources.extend(cell.session.sources().into_iter().map(|(source, facts)| {
                let answer = if cell.answered_sources.contains(&source) {
                    ToolCallAnswer::Sent
                } else {
                    ToolCallAnswer::Owed
                };
                match facts {
                    rho_agent_tools::SourceFacts::Tool(haste) => SourceKind::Tool { answer, haste },
                    rho_agent_tools::SourceFacts::PythonExec(facts) => SourceKind::PythonExec {
                        answer,
                        facts,
                        latest,
                    },
                    rho_agent_tools::SourceFacts::PythonOperation(facts) => {
                        SourceKind::PythonOperation { answer, facts }
                    }
                }
            }));
        }
        if let Some((id, exec)) = &self.latest
            && !self.cells.contains_key(id)
        {
            sources.push(SourceKind::PythonExec {
                answer: ToolCallAnswer::Sent,
                facts: exec.facts(),
                latest: true,
            });
        }
        sources
    }

    /// Should the model look now?
    pub(crate) fn decide(&self, user_oldest_at: Option<UnixMs>, now: UnixMs) -> Boundary {
        boundary(
            &self.sources(user_oldest_at),
            self.turn.as_ref(),
            &Phase::Idle {
                owed: Vec::new(),
                standing: self.standing.clone(),
            },
            now,
        )
    }

    /// Answers the open call with everything waiting.
    pub(crate) fn answer_pending(&mut self) -> Option<(PendingExec, Drained)> {
        let pending = self.pending.take()?;
        let drained = self.drain(Some(pending.cell));
        Some((pending, drained))
    }

    /// Everything waiting, for a model with no call open.
    pub(crate) fn drain_idle(&mut self) -> Drained {
        self.drain(None)
    }

    /// Every cell's contribution, the open call's cell first, as the native
    /// drain does it: a first contribution answers the call and every later
    /// one is an update. Snapshots each cell's sources before draining, so
    /// work registered later still owes its first word.
    fn drain(&mut self, own: Option<u64>) -> Drained {
        let mut drained = Drained {
            own: None,
            updates: Vec::new(),
        };
        let mut order = self.cells.keys().copied().collect::<Vec<_>>();
        order.sort_by_key(|id| Some(*id) != own);
        for id in order {
            let cell = self.cells.get_mut(&id).expect("listed above");
            cell.answered_sources = cell
                .session
                .sources()
                .into_iter()
                .map(|(source, _)| source)
                .collect();
            match cell.answer {
                ToolCallAnswer::Owed => {
                    cell.answer = ToolCallAnswer::Sent;
                    let output = cell.session.first_output();
                    if Some(id) == own {
                        drained.own = Some(output);
                    } else {
                        drained.updates.push(output);
                    }
                }
                ToolCallAnswer::Sent => {
                    if let Some(output) = cell.session.more_output() {
                        drained.updates.push(output);
                    }
                }
            }
        }
        // Asked after the drain, so whatever a cell said last has been taken.
        self.cells.retain(|_, cell| !cell.session.done());
        drained
    }

    /// Stops every cell and forgets the open call, which the caller answers
    /// or lets the CLI abandon. Until the user speaks again, nothing the
    /// cells say on their way out wakes the model.
    pub(crate) fn cancel(&mut self, now: UnixMs) -> Option<PendingExec> {
        for cell in self.cells.values_mut() {
            cell.session.cancel();
        }
        self.standing = Standing::Cancelled { at: now };
        self.pending.take()
    }

    /// Forgets the open call without touching the cells: the CLI that was
    /// waiting on it is gone, the notebook is not.
    pub(crate) fn take_pending(&mut self) -> Option<PendingExec> {
        self.pending.take()
    }
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
                "params": {"name": "exec", "arguments": {"source": "print(1)"}},
            })),
            Rpc::Exec {
                id: json!("c1"),
                source: "print(1)".into()
            }
        );
        let Rpc::Reply(reply) = handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "exec", "arguments": {}},
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
    fn drained_output_becomes_mcp_content() {
        let output = |text: &str, status| ToolOutput {
            output: Arc::new(text.to_owned()),
            full_output: None,
            images: Arc::new(vec![rho_core::ImageContent {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
                detail: Default::default(),
            }]),
            status,
        };
        let result = Drained {
            own: Some(output("ran", ToolOutputStatus::Error)),
            updates: vec![output("later", ToolOutputStatus::Success)],
        }
        .into_mcp_result();
        assert_eq!(result["isError"], true);
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["text"], "ran");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["data"], "AQID");
        assert_eq!(
            content[2]["text"],
            "Later output from an earlier cell:\nlater"
        );
        let result = Drained {
            own: None,
            updates: vec![output("later", ToolOutputStatus::Error)],
        }
        .into_mcp_result();
        assert_eq!(
            result["isError"], false,
            "an update's failure is not the call's"
        );
    }
}
