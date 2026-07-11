use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
use rho_agent::multi_agent_tools;
use rho_ui_proto::{
    AgentId, AgentIdDomain, ClientMessage, McpAgentToolRequest, McpSpawnWorkdir, ServerMessage,
};
use serde::Deserialize;
use serde_json::{Value, json};

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
        let response = handle_request(&mut daemon, self_agent_id, message).await;
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
    daemon: &mut rho_ui_proto::client::Client,
    self_agent_id: AgentId,
    message: RpcRequest,
) -> anyhow::Result<Value> {
    match message.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "rho-agent-tools", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => Ok(json!({
            "tools": multi_agent_tools::agent_tool_specs()
                .into_iter()
                .map(|tool| json!({
                    "name": tool.name.as_str(),
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                }))
                .collect::<Vec<_>>(),
        })),
        "tools/call" => {
            let params: ToolCallParams =
                serde_json::from_value(message.params.unwrap_or(Value::Null))?;
            let request = tool_request(&params.name, params.arguments)?;
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            daemon
                .send(&ClientMessage::McpAgentTool {
                    request_id,
                    self_agent_id,
                    request,
                })
                .await?;
            loop {
                match daemon.recv().await? {
                    ServerMessage::McpAgentToolResult(response)
                        if response.request_id == request_id =>
                    {
                        return Ok(json!({
                            "content": [{"type": "text", "text": response.output}],
                            "isError": response.is_error,
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => anyhow::bail!("unsupported MCP method: {}", message.method),
    }
}

fn resolve_agent_id(text: &str, machine_seed: u64, agent_counter: u64) -> anyhow::Result<AgentId> {
    let (_, raw) = text
        .trim()
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("invalid role-prefixed agent id"))?;
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
        multi_agent_tools::SPAWN_ENGINEER_TOOL_NAME => {
            let args: SpawnArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::SpawnEngineer {
                task_name: args.task_name,
                prompt: args.prompt,
                workdirs: args
                    .workdirs
                    .into_iter()
                    .map(|entry| McpSpawnWorkdir {
                        repo: entry.repo,
                        revset: entry.revset,
                    })
                    .collect(),
            })
        }
        multi_agent_tools::MESSAGE_ENGINEER_TOOL_NAME => {
            let args: SendArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::MessageEngineer {
                engineer_id: args.engineer_id,
                message: args.message,
            })
        }
        multi_agent_tools::INTERRUPT_ENGINEER_TOOL_NAME => {
            let args: InterruptArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::InterruptEngineer {
                engineer_id: args.engineer_id,
            })
        }
        multi_agent_tools::ASK_ADVISOR_TOOL_NAME => Ok(McpAgentToolRequest::AskAdvisor {
            message: serde_json::from_value::<AdvisorArgs>(arguments)?.message,
        }),
        multi_agent_tools::FOLLOWUP_ADVISOR_TOOL_NAME => {
            let args: AdvisorArgs = serde_json::from_value(arguments)?;
            Ok(McpAgentToolRequest::FollowupAdvisor {
                advisor_id: args.advisor_id.context("advisor_id required")?,
                message: args.message,
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
}

#[derive(Deserialize)]
struct SendArgs {
    engineer_id: String,
    message: String,
}

#[derive(Deserialize)]
struct InterruptArgs {
    engineer_id: String,
}

#[derive(Deserialize)]
struct AdvisorArgs {
    advisor_id: Option<String>,
    message: String,
}

#[derive(Deserialize)]
struct WaitArgs {
    timeout_seconds: Option<u64>,
}
