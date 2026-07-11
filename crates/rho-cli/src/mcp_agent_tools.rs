use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent::multi_agent_tools;
use rho_code_mode::{CodeModeSession, NestedTool, ToolDispatcher};
use rho_core::{ToolCall, ToolCallId, ToolOutput, ToolOutputStatus};
use rho_tool_shell::ShellTools;
use rho_ui_proto::{
    AgentId, AgentIdDomain, ClientMessage, McpAgentToolRequest, McpSpawnWorkdir, ServerMessage,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{McpAgentToolsArgs, connect_or_start_daemon};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn run(args: McpAgentToolsArgs) -> anyhow::Result<()> {
    let agent_id = args
        .agent_id
        .or_else(|| std::env::var("RHO_MCP_AGENT_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("missing --agent-id or RHO_MCP_AGENT_ID"))?;
    let socket_path = match args.socket_path {
        Some(path) => path,
        None => rho_daemon::default_socket_path()?,
    };
    let mut daemon = connect_or_start_daemon(&socket_path, &args.auth).await?;
    daemon.send(&ClientMessage::Subscribe).await?;
    let ready = loop {
        if let ServerMessage::Ready {
            machine_seed,
            agent_counter,
            ..
        } = daemon.recv().await?
        {
            break (machine_seed, agent_counter);
        }
    };
    let self_agent_id = resolve_agent_id(&agent_id, ready.0, ready.1)?;
    let agent_tools = std::env::var("RHO_MCP_AGENT_TOOLS").as_deref() != Ok("0");
    let shell_tools = ShellTools::in_directory(
        Duration::from_secs(rho_tool_shell::DEFAULT_TIMEOUT_SECS),
        Utf8PathBuf::try_from(std::env::current_dir()?)?,
        rho_workspaces::PathOverrides::default(),
    );
    let mut nested = shell_tools
        .specs()
        .iter()
        .map(NestedTool::from_spec)
        .collect::<Vec<_>>();
    if agent_tools {
        nested.extend(
            multi_agent_tools::agent_tool_specs()
                .into_iter()
                .filter(|spec| spec.name.as_str() != multi_agent_tools::WAIT_TOOL_NAME)
                .map(|spec| NestedTool::from_spec(&spec)),
        );
    }
    let exec_spec = rho_code_mode::exec_tool_spec(&nested);
    let daemon = Arc::new(Mutex::new(daemon));
    let session = CodeModeSession::new(
        nested,
        Arc::new(McpDispatcher {
            daemon: Arc::clone(&daemon),
            self_agent_id,
            shell_tools,
            agent_tools,
        }),
    )
    .map_err(anyhow::Error::msg)?;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: RpcRequest = serde_json::from_str(&line)
            .with_context(|| format!("parse MCP JSON-RPC request: {line}"))?;
        let Some(id) = message.id.clone() else {
            continue;
        };
        let response = handle_request(&session, &exec_spec, message).await;
        let response = match response {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": error.to_string(),
                },
            }),
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        std::io::Write::write_all(&mut stdout, &bytes)?;
        std::io::Write::flush(&mut stdout)?;
    }
    Ok(())
}

async fn handle_request(
    session: &CodeModeSession,
    exec_spec: &rho_core::ToolSpec,
    message: RpcRequest,
) -> anyhow::Result<Value> {
    match message.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "rho-agent-tools", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => {
            let tools = [exec_spec.clone(), rho_code_mode::wait_tool_spec()]
                .into_iter()
                .map(|tool| json!({
                    "name": tool.name.as_str(),
                    "description": tool.description,
                    "inputSchema": if tool.name.as_str() == rho_code_mode::EXEC_TOOL_NAME {
                        json!({
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["code"],
                            "properties": {"code": {"type": "string", "description": "JavaScript source to execute."}}
                        })
                    } else { tool.input_schema },
                }))
                .collect::<Vec<_>>();
            Ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let params: ToolCallParams =
                serde_json::from_value(message.params.unwrap_or(Value::Null))?;
            let output = match params.name.as_str() {
                rho_code_mode::EXEC_TOOL_NAME => {
                    let code = params
                        .arguments
                        .get("code")
                        .and_then(Value::as_str)
                        .context("exec requires a string code field")?;
                    session.execute(next_call_id(), code).await
                }
                rho_code_mode::WAIT_TOOL_NAME => {
                    session
                        .wait(serde_json::from_value(params.arguments)?)
                        .await
                }
                _ => anyhow::bail!("unsupported tool: {}", params.name),
            };
            Ok(json!({
                "content": [{"type": "text", "text": output.output}],
                "isError": output.status != ToolOutputStatus::Success,
            }))
        }
        _ => anyhow::bail!("unsupported MCP method: {}", message.method),
    }
}

fn next_call_id() -> ToolCallId {
    ToolCallId::try_from(format!(
        "mcp-code-mode-{}",
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("generated tool call id is valid")
}

struct McpDispatcher {
    daemon: Arc<Mutex<rho_ui_proto::client::Client>>,
    self_agent_id: AgentId,
    shell_tools: ShellTools,
    agent_tools: bool,
}

impl ToolDispatcher for McpDispatcher {
    fn call_tool(&self, call: ToolCall) -> futures::future::BoxFuture<'static, ToolOutput> {
        let daemon = Arc::clone(&self.daemon);
        let shell_tools = self.shell_tools.clone();
        let self_agent_id = self.self_agent_id;
        let agent_tools = self.agent_tools;
        Box::pin(async move {
            if shell_tools.supports(call.name.as_str()) {
                return shell_tools.call(call).await;
            }
            if !agent_tools {
                return tool_error("tool is not available to this agent");
            }
            let arguments = match serde_json::from_str(&call.arguments) {
                Ok(arguments) => arguments,
                Err(error) => return tool_error(error.to_string()),
            };
            let request = match tool_request(call.name.as_str(), arguments) {
                Ok(request) => request,
                Err(error) => return tool_error(error.to_string()),
            };
            match call_daemon(&daemon, self_agent_id, request).await {
                Ok(output) => output,
                Err(error) => tool_error(error.to_string()),
            }
        })
    }
}

async fn call_daemon(
    daemon: &Mutex<rho_ui_proto::client::Client>,
    self_agent_id: AgentId,
    request: McpAgentToolRequest,
) -> anyhow::Result<ToolOutput> {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut daemon = daemon.lock().await;
    daemon
        .send(&ClientMessage::McpAgentTool {
            request_id,
            self_agent_id,
            request,
        })
        .await?;
    loop {
        if let ServerMessage::McpAgentToolResult(response) = daemon.recv().await?
            && response.request_id == request_id
        {
            return Ok(ToolOutput {
                output: Arc::new(response.output),
                status: if response.is_error {
                    ToolOutputStatus::Error
                } else {
                    ToolOutputStatus::Success
                },
            });
        }
    }
}

fn tool_error(error: impl Into<String>) -> ToolOutput {
    ToolOutput {
        output: Arc::new(error.into()),
        status: ToolOutputStatus::Error,
    }
}

fn resolve_agent_id(text: &str, machine_seed: u64, agent_counter: u64) -> anyhow::Result<AgentId> {
    let raw = text
        .trim()
        .strip_prefix("ag-")
        .ok_or_else(|| anyhow::anyhow!("agent id must start with ag-"))?;
    let domain = AgentIdDomain(machine_seed);
    match AgentId::from_prefix(raw, agent_counter + 1, &domain)? {
        prefix_id::PrefixResolution::Unique(agent_id)
        | prefix_id::PrefixResolution::Ambiguous {
            first: agent_id, ..
        } => Ok(agent_id),
        prefix_id::PrefixResolution::NotFound => anyhow::bail!("no agent with id {text}"),
    }
}

fn tool_request(name: &str, arguments: Value) -> anyhow::Result<McpAgentToolRequest> {
    match name {
        multi_agent_tools::SPAWN_AGENT_TOOL_NAME => {
            let args: SpawnArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::SpawnAgent {
                task_name: args.task_name,
                prompt: args.prompt,
                workdirs: args
                    .workdirs
                    .into_iter()
                    .map(|entry| McpSpawnWorkdir {
                        repo: entry.repo,
                        checkout: entry.checkout,
                        revset: entry.revset,
                    })
                    .collect(),
                role: args.role,
            })
        }
        multi_agent_tools::SEND_MESSAGE_TOOL_NAME => {
            let args: SendArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::SendMessage {
                agent_id: args.agent_id,
                message: args.message,
            })
        }
        multi_agent_tools::INTERRUPT_AGENT_TOOL_NAME => {
            let args: InterruptArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::InterruptAgent {
                agent_id: args.agent_id,
            })
        }
        multi_agent_tools::WAIT_TOOL_NAME => {
            let args: WaitArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::Wait {
                timeout_seconds: args.timeout_seconds,
            })
        }
        _ => anyhow::bail!("unsupported tool: {name}"),
    }
}

#[derive(Deserialize)]
struct RpcRequest {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Deserialize)]
struct SpawnArgs {
    task_name: String,
    prompt: String,
    #[serde(default)]
    workdirs: Vec<multi_agent_tools::SpawnWorkdirArgs>,
    role: String,
}

#[derive(Deserialize)]
struct SendArgs {
    agent_id: String,
    message: String,
}

#[derive(Deserialize)]
struct InterruptArgs {
    agent_id: String,
}

#[derive(Deserialize)]
struct WaitArgs {
    timeout_seconds: Option<u64>,
}
