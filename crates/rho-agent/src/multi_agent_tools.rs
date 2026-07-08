//! Built-in multi-agent tools: `spawn_agent`, `send_message`,
//! `interrupt_agent`, `wait`.
//!
//! These are ordinary fast tools (codex-v2 style): asynchrony lives in the
//! per-agent message queue, not in tool execution. `spawn_agent` returns the
//! child id immediately; results come back as mail. `wait` is special: the
//! agent loop arms and resolves it itself — when deliverable input arrives
//! or the deadline passes — so only its spec and argument parsing live here.
//!
//! The tools are injected into the core agent as a [`MultiAgentTools`]
//! handle holding a `Weak<AgentPool>`; the agent loop itself knows nothing
//! about the pool.

use std::sync::Arc;

use rho_core::{ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType};
use serde::Deserialize;
use serde_json::json;

use crate::MessageDelivery;
use crate::db::AgentId;
use crate::pool::{AgentPool, SpawnWorkspace};

/// A pooled agent's handle to the multi-agent world: its identity plus the
/// pool for spawning, mail routing, and id resolution. `Agent::create` and
/// `load` build it themselves once the agent id is known (create allocates
/// it, load is given it; the parent edge comes from the record on load).
/// Holds only a `Weak` — the pool owns the agents, not vice versa.
#[derive(Clone)]
pub struct MultiAgentTools {
    pool: std::sync::Weak<AgentPool>,
    self_id: AgentId,
    parent: Option<AgentId>,
}

impl MultiAgentTools {
    pub(crate) fn new(
        pool: std::sync::Weak<AgentPool>,
        self_id: AgentId,
        parent: Option<AgentId>,
    ) -> Self {
        Self {
            pool,
            self_id,
            parent,
        }
    }

    fn pool(&self) -> anyhow::Result<Arc<AgentPool>> {
        self.pool
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("agent pool is shutting down"))
    }

    /// Mail the parent agent, if any, as a fire-and-forget task (the parent
    /// may need loading). Failure means the parent is gone; nothing useful
    /// to do about it here.
    pub(crate) fn mail_parent(&self, body: String, delivery: MessageDelivery) {
        let Some(parent) = self.parent else { return };
        let Ok(pool) = self.pool() else { return };
        let from = self.self_id;
        tokio::spawn(async move {
            let _ = pool.deliver_mail(from, parent, body, delivery).await;
        });
    }
}

pub const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";
pub const SEND_MESSAGE_TOOL_NAME: &str = "send_message";
pub const INTERRUPT_AGENT_TOOL_NAME: &str = "interrupt_agent";
pub const WAIT_TOOL_NAME: &str = "wait";

const DEFAULT_WAIT_SECONDS: u64 = 300;
const MAX_WAIT_SECONDS: u64 = 3600;
const AGENT_ID_EXAMPLE: &str = "ag-h6u7";

pub fn is_agent_tool(name: &str) -> bool {
    matches!(
        name,
        SPAWN_AGENT_TOOL_NAME | SEND_MESSAGE_TOOL_NAME | INTERRUPT_AGENT_TOOL_NAME | WAIT_TOOL_NAME
    )
}

pub fn agent_tool_specs() -> Vec<ToolSpec> {
    vec![
        spawn_agent_spec(),
        send_message_spec(),
        interrupt_agent_spec(),
        wait_spec(),
    ]
}

fn spawn_agent_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(SPAWN_AGENT_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Start a sub-agent working in this repository and return its agent id \
                      immediately. The prompt must be self-contained: the child starts with \
                      fresh context. The child's turn results arrive later as agent mail; use \
                      `wait` to block for them. The child is a regular agent the user can also \
                      see and steer."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task_name", "prompt", "workspace"],
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short user-visible label for the sub-task (a few words)."
                },
                "prompt": {
                    "type": "string",
                    "description": "Complete, self-contained task for the sub-agent."
                },
                "workspace": {
                    "type": "string",
                    "enum": ["join", "fork", "new"],
                    "description": "join: share this agent's working copy (read-mostly tasks). \
                                    fork: own jj workspace forked from this agent's current \
                                    change (editing tasks). new: own jj workspace on a fresh \
                                    change from trunk (or `revset`)."
                },
                "revset": {
                    "type": "string",
                    "description": "With workspace=new: jj revset for the parent revision \
                                    (default trunk())."
                }
            }
        }),
        format: None,
    }
}

fn send_message_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(SEND_MESSAGE_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Send an async message to another agent by id. Wakes an idle recipient; a \
                      busy recipient sees it at its next inference step. Returns immediately \
                      after queueing."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id", "message"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": format!("example: {AGENT_ID_EXAMPLE}")
                },
                "message": {
                    "type": "string",
                    "description": "Message body."
                }
            }
        }),
        format: None,
    }
}

fn interrupt_agent_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(INTERRUPT_AGENT_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Interrupt another agent's current turn by id. The agent remains available \
                      for follow-up messages. Returns plain text after the interrupt request is \
                      accepted."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": format!("example: {AGENT_ID_EXAMPLE}")
                }
            }
        }),
        format: None,
    }
}

fn wait_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(WAIT_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Block until a message is waiting in your queue (sub-agent mail or new \
                      user input) or the timeout passes. Queued messages enter your context \
                      right after this tool returns. Call this when you are blocked on \
                      sub-agent results and have nothing else useful to do."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "timeout_seconds": {
                    "type": "integer",
                    "description": format!("Give up after this many seconds (default: {DEFAULT_WAIT_SECONDS}, min: 1, max: {MAX_WAIT_SECONDS}).")
                }
            }
        }),
        format: None,
    }
}

pub(crate) async fn call_agent_tool(tools: MultiAgentTools, call: ToolCall) -> ToolOutput {
    let result = match call.name.as_str() {
        SPAWN_AGENT_TOOL_NAME => spawn_agent(&tools, &call).await,
        SEND_MESSAGE_TOOL_NAME => send_message(&tools, &call).await,
        INTERRUPT_AGENT_TOOL_NAME => interrupt_agent(&tools, &call).await,
        // `wait` is intercepted and resolved by the agent loop; it never
        // reaches tool dispatch.
        _ => Err(anyhow::anyhow!(
            "unsupported tool call: {}",
            call.name.as_str()
        )),
    };
    match result {
        Ok(output) => ToolOutput {
            output: Arc::new(output),
            status: ToolOutputStatus::Success,
        },
        Err(error) => ToolOutput {
            output: Arc::new(error.to_string()),
            status: ToolOutputStatus::Error,
        },
    }
}

#[derive(Deserialize)]
struct SpawnArgs {
    task_name: String,
    prompt: String,
    workspace: WorkspaceChoice,
    revset: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceChoice {
    Join,
    Fork,
    New,
}

async fn spawn_agent(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: SpawnArgs = serde_json::from_str(&call.arguments)?;
    if args.prompt.trim().is_empty() {
        anyhow::bail!("prompt must not be empty");
    }
    let workspace = match args.workspace {
        WorkspaceChoice::Join => SpawnWorkspace::Join,
        WorkspaceChoice::Fork => SpawnWorkspace::Fork,
        WorkspaceChoice::New => SpawnWorkspace::New {
            revset: args.revset,
        },
    };
    let task_name = args.task_name.clone();
    let pool = tools.pool()?;
    let child_id = pool
        .spawn_child(tools.self_id, args.task_name, args.prompt, workspace)
        .await?;
    let child_id = format!("ag-{}", pool.agent_id_prefix(child_id));
    Ok(format!(
        "Spawned agent {} for task \"{}\". It is working now; its results will arrive as mail \
         from that agent. Use send_message to follow up and wait to block for its results.",
        child_id, task_name,
    ))
}

#[derive(Deserialize)]
struct SendArgs {
    agent_id: String,
    message: String,
}

async fn send_message(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: SendArgs = serde_json::from_str(&call.arguments)?;
    if args.message.trim().is_empty() {
        anyhow::bail!("message must not be empty");
    }
    let pool = tools.pool()?;
    let raw_agent_id = args
        .agent_id
        .trim()
        .strip_prefix("ag-")
        .ok_or_else(|| anyhow::anyhow!("agent_id must start with ag-"))?;
    let recipient = match pool.resolve_agent_id(raw_agent_id)? {
        prefix_id::PrefixResolution::Unique(agent_id)
        | prefix_id::PrefixResolution::Ambiguous {
            first: agent_id, ..
        } => agent_id,
        prefix_id::PrefixResolution::NotFound => {
            anyhow::bail!("no agent with id {}", args.agent_id)
        }
    };
    if !pool.agent_exists(recipient) {
        anyhow::bail!("no agent with id {}", args.agent_id);
    }
    if recipient == tools.self_id {
        anyhow::bail!("cannot send a message to yourself");
    }
    pool.deliver_mail(
        tools.self_id,
        recipient,
        args.message,
        MessageDelivery::NextRequest,
    )
    .await?;
    Ok(format!(
        "Message sent to agent {}.",
        format!("ag-{}", pool.agent_id_prefix(recipient))
    ))
}

#[derive(Deserialize)]
struct InterruptArgs {
    agent_id: String,
}

async fn interrupt_agent(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: InterruptArgs = serde_json::from_str(&call.arguments)?;
    let pool = tools.pool()?;
    let raw_agent_id = args
        .agent_id
        .trim()
        .strip_prefix("ag-")
        .ok_or_else(|| anyhow::anyhow!("agent_id must start with ag-"))?;
    let target = match pool.resolve_agent_id(raw_agent_id)? {
        prefix_id::PrefixResolution::Unique(agent_id)
        | prefix_id::PrefixResolution::Ambiguous {
            first: agent_id, ..
        } => agent_id,
        prefix_id::PrefixResolution::NotFound => {
            anyhow::bail!("no agent with id {}", args.agent_id)
        }
    };
    if !pool.agent_exists(target) {
        anyhow::bail!("no agent with id {}", args.agent_id);
    }
    if target == tools.self_id {
        anyhow::bail!("cannot interrupt yourself");
    }
    let (_, agent, _) = pool.load(target).await?;
    agent.cancel();
    Ok(format!(
        "Agent {} interrupted. It remains available for follow-up messages.",
        format!("ag-{}", pool.agent_id_prefix(target))
    ))
}

/// Parse a `wait` call's timeout for the loop that arms it.
pub(crate) fn parse_wait_timeout(arguments: &str) -> anyhow::Result<u64> {
    #[derive(Deserialize)]
    struct WaitArgs {
        timeout_seconds: Option<u64>,
    }
    let args: WaitArgs = if arguments.trim().is_empty() {
        WaitArgs {
            timeout_seconds: None,
        }
    } else {
        serde_json::from_str(arguments)?
    };
    Ok(args
        .timeout_seconds
        .unwrap_or(DEFAULT_WAIT_SECONDS)
        .clamp(1, MAX_WAIT_SECONDS))
}
