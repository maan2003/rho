//! Built-in multi-agent tools: `spawn_engineer`, `message_agent`,
//! `interrupt_engineer`, `wait_agent`.
//!
//! These are ordinary fast tools (codex-v2 style): asynchrony lives in the
//! per-agent message queue, not in tool execution. `spawn_engineer` returns the
//! child id immediately; results come back as mail. `wait` is special: the
//! agent loop arms and resolves it itself — when deliverable input arrives
//! or the deadline passes — so only its spec and argument parsing live here.
//!
//! The tools are injected into the core agent as a [`MultiAgentTools`]
//! handle holding a `Weak<AgentPool>`; the agent loop itself knows nothing
//! about the pool.

use std::sync::Arc;

use camino::Utf8PathBuf;
use rho_core::{ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType};
use serde::Deserialize;
use serde_json::json;

use crate::MessageDelivery;
use crate::db::{AgentId, AgentReadTxnExt as _, AgentRole};
use crate::pool::{AgentPool, SpawnCheckout, SpawnWorkdir};

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

    pub(crate) fn self_id(&self) -> AgentId {
        self.self_id
    }

    pub(crate) fn parent(&self) -> Option<AgentId> {
        self.parent
    }

    pub(crate) fn spawned_by(&self) -> crate::db::AgentSpawnedBy {
        self.pool()
            .expect("multi-agent tools require a live agent pool")
            .db()
            .read()
            .get_agent(self.self_id)
            .spawned_by
    }

    pub(crate) fn role(&self) -> AgentRole {
        self.pool()
            .expect("multi-agent tools require a live agent pool")
            .db()
            .read()
            .get_agent(self.self_id)
            .role
    }

    pub(crate) fn display_id(&self, agent_id: AgentId) -> String {
        let pool = self
            .pool()
            .expect("multi-agent tools require a live agent pool");
        pool.agent_handle(agent_id)
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

pub const SPAWN_ENGINEER_TOOL_NAME: &str = "spawn_engineer";
pub const MESSAGE_AGENT_TOOL_NAME: &str = "message_agent";
pub const INTERRUPT_ENGINEER_TOOL_NAME: &str = "interrupt_engineer";
pub const ASK_ADVISOR_TOOL_NAME: &str = "ask_advisor";
pub const WAIT_TOOL_NAME: &str = "wait_agent";

const DEFAULT_WAIT_SECONDS: u64 = 300;
const MAX_WAIT_SECONDS: u64 = 3600;
const AGENT_ID_EXAMPLE: &str = "eng-h6u7";

pub fn is_agent_tool(name: &str) -> bool {
    matches!(
        name,
        SPAWN_ENGINEER_TOOL_NAME
            | MESSAGE_AGENT_TOOL_NAME
            | INTERRUPT_ENGINEER_TOOL_NAME
            | ASK_ADVISOR_TOOL_NAME
            | WAIT_TOOL_NAME
    )
}

pub fn agent_tool_specs(role: AgentRole) -> Vec<ToolSpec> {
    match role {
        AgentRole::PM | AgentRole::Engineer { .. } => vec![
            spawn_engineer_spec(),
            message_agent_spec(),
            interrupt_engineer_spec(),
            advisor_spec(),
            wait_spec(),
        ],
        AgentRole::Advisor { .. } => vec![message_agent_spec(), wait_spec()],
    }
}

fn spawn_engineer_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(SPAWN_ENGINEER_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Start a sub-agent with its own working set of workdirs (defaulting to a \
                      fork of yours) and return its agent id immediately. Use this for a concrete, bounded subtask, including side \
                      investigations or experiments when the user asks for them or they de-risk \
                      the main task. The subtask should run independently alongside useful local \
                      work; otherwise continue locally. The prompt must be self-contained and \
                      task-focused: the child already receives repo guidance, skills, tools, and \
                      workspace instructions, so do not restate generic process rules. The \
                      child's turn results arrive later as agent mail; use `wait_agent` when you are \
                      blocked on those results."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task_name", "prompt"],
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short user-visible kebab-case label for the sub-task."
                },
                "prompt": {
                    "type": "string",
                    "description": "Complete, self-contained task for the sub-agent."
                },
                "workdirs": {
                    "type": "array",
                    "description": "The child's working set, primary workdir first. Omit for \
                                    the default: your whole working set, with the child's own \
                                    jj workspace forked from your current change in each \
                                    workdir (safe for concurrent edits). List entries \
                                    explicitly to share your checkouts, start from another \
                                    revision, or work in other repositories.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["repo"],
                        "properties": {
                            "repo": {
                                "type": "string",
                                "description": "Absolute path of the repository or directory \
                                                (or anywhere inside it)."
                            },
                            "revset": {
                                "type": "string",
                                "description": "With checkout=own: jj revset the child's \
                                                change starts from. Defaults to your current \
                                                change in that repo, or trunk() for \
                                                repositories outside your working set."
                            }
                        }
                    }
                }
            }
        }),
        format: None,
    }
}

fn message_agent_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(MESSAGE_AGENT_TOOL_NAME).expect("valid tool name"),
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
                    "description": format!("Role-prefixed agent handle, for example {AGENT_ID_EXAMPLE} or adv-h6u7.")
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

fn interrupt_engineer_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(INTERRUPT_ENGINEER_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Interrupt another agent's current turn by id. The agent remains available \
                      for follow-up messages. Returns plain text after the interrupt request is \
                      accepted."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["engineer_id"],
            "properties": {
                "engineer_id": {
                    "type": "string",
                    "description": format!("example: {AGENT_ID_EXAMPLE}")
                }
            }
        }),
        format: None,
    }
}

fn advisor_spec() -> ToolSpec {
    let properties = serde_json::Map::from_iter([(
        "message".to_owned(),
        json!({"type": "string", "description": "Question or follow-up for the Advisor."}),
    )]);
    let required = vec!["message"];
    ToolSpec {
        name: ToolName::try_from(ASK_ADVISOR_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Start a fresh independent Advisor consultation. The answer arrives later as mail. Use message_agent with its handle for follow-up or context exchange.".to_owned(),
        input_schema: json!({
            "type": "object", "additionalProperties": false,
            "required": required, "properties": properties,
        }),
        format: None,
    }
}

fn wait_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(WAIT_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Wait for a mailbox update from any live agent, including queued messages \
                      and final responses. The wait also ends early when new user input is \
                      steered into the active turn. Does not return the content; queued input \
                      enters context after this tool returns."
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
        SPAWN_ENGINEER_TOOL_NAME => spawn_engineer(&tools, &call).await,
        MESSAGE_AGENT_TOOL_NAME => message_agent(&tools, &call).await,
        INTERRUPT_ENGINEER_TOOL_NAME => interrupt_engineer(&tools, &call).await,
        ASK_ADVISOR_TOOL_NAME => ask_advisor(&tools, &call).await,
        WAIT_TOOL_NAME => wait_agent(&tools, &call).await,
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

async fn wait_agent(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let timeout = std::time::Duration::from_secs(parse_wait_timeout(&call.arguments)?);
    let pool = tools.pool()?;
    let (_, agent, _) = pool.load(tools.self_id).await?;
    Ok(if agent.wait_for_input(timeout).await {
        "Wait completed."
    } else {
        "Wait timed out."
    }
    .to_owned())
}

#[derive(Deserialize)]
struct AdvisorArgs {
    message: String,
}

async fn ask_advisor(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: AdvisorArgs = serde_json::from_str(&call.arguments)?;
    anyhow::ensure!(!args.message.trim().is_empty(), "message must not be empty");
    let pool = tools.pool()?;
    let workdirs = pool
        .db()
        .read()
        .get_agent(tools.self_id)
        .workdirs
        .into_iter()
        .map(|info| SpawnWorkdir {
            repo: info.repo().to_owned(),
            checkout: SpawnCheckout::Shared,
        })
        .collect();
    let advisor = pool
        .spawn_child(
            tools.self_id,
            "advisor".to_owned(),
            args.message,
            workdirs,
            AgentRole::Advisor {
                intelligence: crate::db::AdvisorIntelligence::Medium,
            },
        )
        .await?;
    Ok(format!(
        "Advisor adv-{} is considering the question. Its answer will arrive as mail.",
        pool.agent_id_prefix(advisor)
    ))
}

#[derive(Deserialize)]
struct SpawnArgs {
    task_name: String,
    prompt: String,
    #[serde(default)]
    workdirs: Vec<SpawnWorkdirArgs>,
}

/// One `workdirs` entry as the spawn tools accept it; shared with the MCP
/// agent-tool surface so both parse identically.
#[derive(Deserialize)]
pub struct SpawnWorkdirArgs {
    pub repo: String,
    pub checkout: Option<String>,
    pub revset: Option<String>,
}

/// Parses tool-surface workdir entries into pool spawn entries.
pub fn parse_spawn_workdirs(entries: Vec<SpawnWorkdirArgs>) -> anyhow::Result<Vec<SpawnWorkdir>> {
    entries
        .into_iter()
        .map(|entry| {
            anyhow::ensure!(
                entry.checkout.as_deref().is_none_or(|value| value == "own"),
                "shared Engineer checkouts are not supported"
            );
            Ok(SpawnWorkdir {
                repo: Utf8PathBuf::from(entry.repo),
                checkout: SpawnCheckout::Own {
                    revset: entry.revset,
                },
            })
        })
        .collect()
}

pub fn parse_spawn_role(role: &str) -> anyhow::Result<AgentRole> {
    anyhow::ensure!(role == "eng", "only Engineer spawning is supported");
    Ok(AgentRole::default())
}

async fn spawn_engineer(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: SpawnArgs = serde_json::from_str(&call.arguments)?;
    if args.prompt.trim().is_empty() {
        anyhow::bail!("prompt must not be empty");
    }
    let workdirs = parse_spawn_workdirs(args.workdirs)?;
    let task_name = args.task_name.clone();
    let config = AgentRole::default();
    let pool = tools.pool()?;
    let child_id = pool
        .spawn_child(tools.self_id, args.task_name, args.prompt, workdirs, config)
        .await?;
    let child_record = pool.db().read().get_agent(child_id);
    let workspace_note = match child_record.primary_workdir().workspace_name() {
        Some(workspace) => format!(
            " Its jj workspace is `{workspace}`; inspect its working-copy commit with `jj diff -r \
             '{workspace}@' --stat`."
        ),
        None => " It is running in the shared user checkout workspace; there is no separate \
                 `<workspace>@` handle."
            .to_owned(),
    };
    let child_id = format!("eng-{}", pool.agent_id_prefix(child_id));
    Ok(format!(
        "Spawned agent {} for task \"{}\". It is working now; its results will arrive as mail \
         from that Engineer.{} Use message_agent to follow up.",
        child_id, task_name, workspace_note,
    ))
}

#[derive(Deserialize)]
struct SendArgs {
    agent_id: String,
    message: String,
}

async fn message_agent(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: SendArgs = serde_json::from_str(&call.arguments)?;
    if args.message.trim().is_empty() {
        anyhow::bail!("message must not be empty");
    }
    let pool = tools.pool()?;
    let handle = args.agent_id.trim();
    let (_, raw_agent_id) = handle
        .split_once('-')
        .filter(|(prefix, _)| matches!(*prefix, "eng" | "pm" | "adv"))
        .ok_or_else(|| anyhow::anyhow!("agent_id must use an eng-, pm-, or adv- handle"))?;
    let recipient = match pool.resolve_agent_id(raw_agent_id)? {
        prefix_id::PrefixResolution::Unique(agent_id) => agent_id,
        prefix_id::PrefixResolution::Ambiguous { .. } => {
            anyhow::bail!("ambiguous agent id {handle}")
        }
        prefix_id::PrefixResolution::NotFound => {
            anyhow::bail!("no agent with id {handle}")
        }
    };
    if !pool.agent_exists(recipient) {
        anyhow::bail!("no agent with id {handle}");
    }
    anyhow::ensure!(
        pool.db().read().get_agent(recipient).role.handle_prefix()
            == handle.split('-').next().unwrap(),
        "agent handle role prefix does not match target"
    );
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
    Ok(format!("Message sent to {}.", pool.agent_handle(recipient)))
}

#[derive(Deserialize)]
struct InterruptArgs {
    engineer_id: String,
}

async fn interrupt_engineer(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: InterruptArgs = serde_json::from_str(&call.arguments)?;
    let pool = tools.pool()?;
    let raw_agent_id = args
        .engineer_id
        .trim()
        .strip_prefix("eng-")
        .ok_or_else(|| anyhow::anyhow!("engineer_id must start with eng-"))?;
    let target = match pool.resolve_agent_id(raw_agent_id)? {
        prefix_id::PrefixResolution::Unique(agent_id)
        | prefix_id::PrefixResolution::Ambiguous {
            first: agent_id, ..
        } => agent_id,
        prefix_id::PrefixResolution::NotFound => {
            anyhow::bail!("no agent with id {}", args.engineer_id)
        }
    };
    if !pool.agent_exists(target) {
        anyhow::bail!("no agent with id {}", args.engineer_id);
    }
    anyhow::ensure!(
        matches!(
            pool.db().read().get_agent(target).role,
            AgentRole::Engineer { .. }
        ),
        "target is not an Engineer"
    );
    if target == tools.self_id {
        anyhow::bail!("cannot interrupt yourself");
    }
    let (_, agent, _) = pool.load(target).await?;
    agent.cancel();
    Ok(format!(
        "Engineer eng-{} interrupted. It remains available for follow-up messages.",
        pool.agent_id_prefix(target)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spawn_role() {
        assert_eq!(parse_spawn_role("eng").unwrap(), AgentRole::default());
        assert!(parse_spawn_role("terra").is_err());
    }
}
