//! Built-in multi-agent tools: `spawn_engineer`, `message_agent`,
//! `interrupt_engineer`.
//!
//! These are ordinary fast tools (codex-v2 style): asynchrony lives in the
//! per-agent message queue, not in tool execution. `spawn_engineer` returns the
//! child id immediately; results come back as mail, and the loop's own `wait`
//! tool is how an agent waits for them.
//!
//! The tools are injected into the core agent as a [`MultiAgentTools`]
//! handle holding a `Weak<AgentPool>`; the agent loop itself knows nothing
//! about the pool.

use std::sync::Arc;

use rho_core::{ToolCall, ToolOutput, ToolOutputStatus};
use serde::Deserialize;

use crate::MessageDelivery;
use crate::db::{AgentId, AgentReadTxnExt as _, AgentRole};
use crate::pool::AgentPool;

/// Startup presentation identities for a worker's prompts. Pool capabilities
/// never cross into the worker; its snapshot keeps the original handles even
/// if later role changes or allocations change their preferred presentation.
#[derive(Clone, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Team {
    pub agent: String,
    pub parent: Option<String>,
    pub spawned_by: crate::db::AgentSpawnedBy,
}

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

    pub(crate) fn team(&self) -> anyhow::Result<Team> {
        let pool = self.pool()?;
        Ok(Team {
            agent: pool.agent_handle(self.self_id),
            parent: self.parent.map(|parent| pool.agent_handle(parent)),
            spawned_by: pool.db().read().get_agent(self.self_id).config.spawned_by,
        })
    }

    fn pool(&self) -> anyhow::Result<Arc<AgentPool>> {
        self.pool
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("agent pool is shutting down"))
    }
}

pub const SPAWN_ENGINEER_TOOL_NAME: &str = "spawn_engineer";
pub const MESSAGE_AGENT_TOOL_NAME: &str = "message_agent";
pub const INTERRUPT_ENGINEER_TOOL_NAME: &str = "interrupt_engineer";
pub const ASK_ADVISOR_TOOL_NAME: &str = "ask_advisor";

pub fn is_agent_tool(name: &str) -> bool {
    matches!(
        name,
        SPAWN_ENGINEER_TOOL_NAME
            | MESSAGE_AGENT_TOOL_NAME
            | INTERRUPT_ENGINEER_TOOL_NAME
            | ASK_ADVISOR_TOOL_NAME
    )
}

pub fn agent_functions(role: AgentRole) -> &'static [&'static str] {
    match role {
        AgentRole::Engineer { .. } => &[
            SPAWN_ENGINEER_TOOL_NAME,
            MESSAGE_AGENT_TOOL_NAME,
            INTERRUPT_ENGINEER_TOOL_NAME,
            ASK_ADVISOR_TOOL_NAME,
        ],
        AgentRole::Advisor { .. } => &[MESSAGE_AGENT_TOOL_NAME],
    }
}

pub(crate) async fn call_agent_tool(tools: MultiAgentTools, call: ToolCall) -> ToolOutput {
    let result = match call.name.as_str() {
        SPAWN_ENGINEER_TOOL_NAME => spawn_engineer(&tools, &call).await,
        MESSAGE_AGENT_TOOL_NAME => message_agent(&tools, &call).await,
        INTERRUPT_ENGINEER_TOOL_NAME => interrupt_engineer(&tools, &call).await,
        ASK_ADVISOR_TOOL_NAME => ask_advisor(&tools, &call).await,
        _ => Err(anyhow::anyhow!(
            "unsupported tool call: {}",
            call.name.as_str()
        )),
    };
    match result {
        Ok(output) => ToolOutput {
            full_output: None,
            images: std::sync::Arc::new(Vec::new()),
            output: Arc::new(output),
            status: ToolOutputStatus::Success,
        },
        Err(error) => ToolOutput {
            full_output: None,
            images: std::sync::Arc::new(Vec::new()),
            output: Arc::new(error.to_string()),
            status: ToolOutputStatus::Error,
        },
    }
}

#[derive(Deserialize)]
struct AdvisorArgs {
    message: String,
}

async fn ask_advisor(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: AdvisorArgs = serde_json::from_str(&call.arguments)?;
    anyhow::ensure!(!args.message.trim().is_empty(), "message must not be empty");
    let pool = tools.pool()?;
    let parent = pool.db().read().get_agent(tools.self_id).config;
    let advisor_intelligence = default_advisor_intelligence(parent.role);
    // The advisor reads what the asker sees: same directory, same workset.
    let advisor = pool
        .spawn_child(
            tools.self_id,
            "advisor".to_owned(),
            args.message,
            AgentRole::Advisor {
                intelligence: advisor_intelligence,
            },
            None,
        )
        .await?;
    Ok(format!(
        "Advisor adv-{} is considering the question. Its answer will arrive as mail.",
        pool.agent_id_prefix(advisor)
    ))
}

fn default_advisor_intelligence(role: AgentRole) -> crate::db::AdvisorIntelligence {
    match role {
        AgentRole::Engineer {
            intelligence:
                crate::db::EngineerIntelligence::High | crate::db::EngineerIntelligence::HighNotes,
        } => crate::db::AdvisorIntelligence::High,
        _ => crate::db::AdvisorIntelligence::Medium,
    }
}

#[derive(Deserialize)]
struct SpawnArgs {
    task_name: String,
    prompt: String,
    workdir: Option<String>,
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
    let task_name = args.task_name.clone();
    let config = AgentRole::default();
    let pool = tools.pool()?;
    let child_id = pool
        .spawn_child(
            tools.self_id,
            args.task_name,
            args.prompt,
            config,
            args.workdir.map(Into::into),
        )
        .await?;
    let child_record = pool.db().read().get_agent(child_id);
    let workspace_note = format!(" It works in {}.", child_record.place().cwd);
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
        pool.db()
            .read()
            .get_agent(recipient)
            .config
            .role
            .handle_prefix()
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
    agent_id: String,
}

async fn interrupt_engineer(tools: &MultiAgentTools, call: &ToolCall) -> anyhow::Result<String> {
    let args: InterruptArgs = serde_json::from_str(&call.arguments)?;
    let pool = tools.pool()?;
    let raw_agent_id = args
        .agent_id
        .trim()
        .strip_prefix("eng-")
        .ok_or_else(|| anyhow::anyhow!("agent_id must start with eng-"))?;
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
    anyhow::ensure!(
        pool.db().read().get_agent(target).config.role.is_engineer(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spawn_role() {
        assert_eq!(parse_spawn_role("eng").unwrap(), AgentRole::default());
        assert!(parse_spawn_role("terra").is_err());
    }

    #[test]
    fn high_engineers_get_high_advisors() {
        assert_eq!(
            default_advisor_intelligence(AgentRole::Engineer {
                intelligence: crate::db::EngineerIntelligence::High,
            }),
            crate::db::AdvisorIntelligence::High
        );
        assert_eq!(
            default_advisor_intelligence(AgentRole::default()),
            crate::db::AdvisorIntelligence::Medium
        );
    }
}
