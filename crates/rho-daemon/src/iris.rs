//! Daemon-global Iris coordinator and its typed fleet-control tools.

use std::sync::{Arc, Weak};

use futures::future::BoxFuture;
use rho_agent::db::{
    AgentDisposition, AgentId, AgentReadTxnExt as _, AgentRole, AgentWriteTxnExt as _,
    EngineerIntelligence,
};
use rho_agent::iris_tools::IrisToolHost;
use rho_agent::pool::{AgentAssistantItemCompleted, AgentTurnCompleted, RunningAgent};
use rho_agent::{InputSourceId, MessageDelivery};
use rho_core::{MessagePhase, ToolCall, ToolOutput, ToolOutputStatus};
use rho_ui_proto::StartMode;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::AgentRegistry;

const IRIS_LABEL: &str = rho_agent::iris_tools::LABEL;
pub(crate) struct IrisBackend {
    agent_id: AgentId,
    agent: RunningAgent,
    source_id: InputSourceId,
    completed_items: broadcast::Receiver<AgentAssistantItemCompleted>,
    completed_turns: broadcast::Receiver<AgentTurnCompleted>,
    streamed_final: String,
}

pub(crate) enum IrisBackendEvent {
    Item { phase: MessagePhase, text: String },
    Completed { remaining_final: String },
}

impl IrisBackend {
    pub(crate) fn submit(&self, text: String, transcript_delta: &str, transcript_tail: bool) {
        let transcript = if transcript_delta.trim().is_empty() {
            String::new()
        } else {
            format!(
                "<transcript_delta>{}</transcript_delta>\n",
                escape_xml(transcript_delta)
            )
        };
        let source = if transcript_tail {
            "<source>transcript_tail_flush</source>\n"
        } else {
            ""
        };
        self.agent.send_user_message_with_source(
            format!(
                "<iris_voice_request>{source}<transcript>{}</transcript>\n{transcript}</iris_voice_request>",
                escape_xml(&text)
            ),
            MessageDelivery::Immediate,
            Some(self.source_id),
        );
    }

    pub(crate) async fn next_event(&mut self) -> anyhow::Result<IrisBackendEvent> {
        loop {
            tokio::select! {
                item = self.completed_items.recv() => {
                    let item = item?;
                    if item.agent_id != self.agent_id || item.text.is_empty() {
                        continue;
                    }
                    if item.phase == MessagePhase::FinalAnswer {
                        if !self.streamed_final.is_empty() {
                            self.streamed_final.push('\n');
                        }
                        self.streamed_final.push_str(&item.text);
                    }
                    return Ok(IrisBackendEvent::Item {
                        phase: item.phase,
                        text: item.text,
                    });
                }
                completed = self.completed_turns.recv() => {
                    let completed = completed?;
                    if completed.agent_id != self.agent_id {
                        continue;
                    }
                    let remaining_final = completed
                        .final_answer
                        .strip_prefix(&self.streamed_final)
                        .unwrap_or(&completed.final_answer)
                        .to_owned();
                    self.streamed_final.clear();
                    return Ok(IrisBackendEvent::Completed { remaining_final });
                }
            }
        }
    }
}

impl AgentRegistry {
    pub(crate) fn install_iris_tool_host(self: &Arc<Self>) {
        self.pool.set_iris_tool_host(Arc::new(IrisTools {
            registry: Arc::downgrade(self),
        }));
    }

    pub(crate) async fn iris_startup_context(&self) -> String {
        let kinds = self.agent_state_kinds().await;
        let mut lines = self
            .ui_agents(&kinds)
            .into_iter()
            .filter(|agent| !agent.hidden && !agent.labels.iter().any(|label| label == IRIS_LABEL))
            .map(|agent| {
                format!(
                    "{} | {} | {:?}",
                    self.display_agent_id(agent.agent_id),
                    agent.display_name.unwrap_or_else(|| "unnamed".to_owned()),
                    agent.attention,
                )
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return "No visible agents are currently registered.".to_owned();
        }
        lines.insert(0, "Visible agents: handle | name | attention".to_owned());
        let mut context = lines.join("\n");
        let mut end = context.len().min(16 * 1024);
        while !context.is_char_boundary(end) {
            end -= 1;
        }
        context.truncate(end);
        context
    }

    pub(crate) async fn iris_backend(self: &Arc<Self>) -> anyhow::Result<IrisBackend> {
        self.install_iris_tool_host();
        let completed_items = self.pool.subscribe_completed_assistant_items();
        let completed_turns = self.pool.subscribe_completed_turns();
        let agent_id = self.ensure_iris().await?;
        let (_, agent, _) = self.pool.load(agent_id).await?;
        Ok(IrisBackend {
            agent_id,
            agent,
            source_id: InputSourceId::fresh_internal(),
            completed_items,
            completed_turns,
            streamed_final: String::new(),
        })
    }

    async fn ensure_iris(self: &Arc<Self>) -> anyhow::Result<AgentId> {
        let mut active = self.iris_agent.lock().await;
        if let Some(agent_id) = *active {
            return Ok(agent_id);
        }

        let existing = self
            .db
            .read()
            .list_agents()
            .into_iter()
            .find(|(_, record)| {
                record.role == AgentRole::Iris
                    || record.labels.iter().any(|label| label == IRIS_LABEL)
            })
            .map(|(agent_id, _)| agent_id);
        let agent_id = if let Some(agent_id) = existing {
            if self.db.read().get_agent(agent_id).role != AgentRole::Iris {
                let mut write = self.db.write().await;
                write.set_agent_role(agent_id, AgentRole::Iris);
                write.commit();
            }
            self.pool.load(agent_id).await?;
            agent_id
        } else {
            let source = {
                let read = self.db.read();
                read.list_agents()
                    .into_iter()
                    .find(|(_, record)| !record.labels.iter().any(|label| label == IRIS_LABEL))
                    .map(|(_, record)| record.primary_workdir().clone())
            };

            let workspace = match source {
                Some(source) => self.pool.open_workspace(&source).await?,
                None => {
                    let project = self
                        .projects()
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("Iris needs a registered project or existing agent to establish its coordinator view"))?;
                    self.pool.repo(&project.path).await?.user_checkout().await?
                }
            };
            let (agent_id, _) = self
                .pool
                .create(
                    AgentRole::Iris,
                    Some("Iris".to_owned()),
                    vec![rho_agent::StartWorkdir::Existing(workspace)],
                )
                .await?;
            let mut write = self.db.write().await;
            write.agent_label(rho_core::UnixMs::now(), agent_id, IRIS_LABEL, true);
            write.set_agent_disposition(agent_id, AgentDisposition::Hidden);
            write.commit();
            agent_id
        };
        *active = Some(agent_id);
        Ok(agent_id)
    }
}

struct IrisTools {
    registry: Weak<AgentRegistry>,
}

impl IrisToolHost for IrisTools {
    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let registry = self.registry.clone();
        Box::pin(async move {
            let Some(registry) = registry.upgrade() else {
                return tool_error("Iris control plane is no longer available");
            };
            match call_iris_tool(&registry, call).await {
                Ok(output) => tool_ok(output),
                Err(error) => tool_error(error.to_string()),
            }
        })
    }
}

async fn call_iris_tool(registry: &Arc<AgentRegistry>, call: ToolCall) -> anyhow::Result<String> {
    match call.name.as_str() {
        "iris_list_agents" => {
            let kinds = registry.agent_state_kinds().await;
            let mut lines = Vec::new();
            for agent in registry
                .ui_agents(&kinds)
                .into_iter()
                .filter(|agent| !agent.labels.iter().any(|label| label == IRIS_LABEL))
            {
                lines.push(format!(
                    "{} | {} | {:?} | {}",
                    registry.display_agent_id(agent.agent_id),
                    agent.display_name.unwrap_or_else(|| "unnamed".to_owned()),
                    agent.attention,
                    agent.last_user_message_text
                ));
            }
            Ok(if lines.is_empty() {
                "No agents.".to_owned()
            } else {
                lines.join("\n")
            })
        }
        "iris_read_desk" => read_desk(&registry.desk),
        "iris_edit_desk" => {
            let args: EditDeskArgs = parse(&call)?;
            let edits = args
                .edits
                .into_iter()
                .map(|edit| (edit.old_str, edit.new_str))
                .collect::<Vec<_>>();
            apply_iris_desk_edits(
                &registry.desk,
                &registry.events,
                active_iris_id(registry).await?,
                &edits,
            )
            .await?;
            Ok("Desk updated.".to_owned())
        }
        "iris_start_agent" => {
            let args: StartAgentArgs = parse(&call)?;
            anyhow::ensure!(!args.prompt.trim().is_empty(), "prompt must not be empty");
            let projects = registry.projects();
            let project = match args.project.as_deref() {
                Some(needle) => projects
                    .iter()
                    .find(|project| project.name == needle || project.path.as_str() == needle)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unknown project {needle}"))?,
                None if projects.len() == 1 => projects[0].clone(),
                None => anyhow::bail!(
                    "project is required when zero or multiple projects are registered"
                ),
            };
            let role = parse_role(args.role.as_deref().unwrap_or("eng"))?;
            let (agent_id, agent) = registry
                .create(
                    role,
                    StartMode::NewOn {
                        repo: project.path,
                        revset: "trunk()".to_owned(),
                    },
                )
                .await?;
            registry
                .pool
                .set_response_subscription(active_iris_id(registry).await?, agent_id, true)
                .await?;
            agent
                .send_user_content_accepted(
                    vec![rho_core::ContentPart::Text {
                        text: args.prompt.clone(),
                    }],
                    MessageDelivery::NextRequest,
                    None,
                )
                .await?;
            {
                let mut write = registry.db.write().await;
                if let Some(name) = args.task_name {
                    write.set_agent_display_name(rho_core::UnixMs::now(), agent_id, name);
                }
                write.commit();
            }
            refresh_clients(registry).await;
            Ok(format!("Started {}.", registry.display_agent_id(agent_id)))
        }
        "iris_send_agent" => {
            let args: SendAgentArgs = parse(&call)?;
            anyhow::ensure!(!args.message.trim().is_empty(), "message must not be empty");
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            anyhow::ensure!(
                !is_iris(registry, agent_id),
                "cannot message Iris through a fleet tool"
            );
            registry
                .pool
                .set_response_subscription(active_iris_id(registry).await?, agent_id, true)
                .await?;
            let (_, agent, _) = registry.pool.load(agent_id).await?;
            let delivery = match args.delivery.as_deref() {
                None | Some("immediate") => MessageDelivery::Immediate,
                Some("next_turn") => MessageDelivery::NextTurn,
                Some(other) => anyhow::bail!("unknown delivery {other}"),
            };
            agent
                .send_user_content_accepted(
                    vec![rho_core::ContentPart::Text {
                        text: args.message.clone(),
                    }],
                    delivery,
                    Some(InputSourceId::fresh_internal()),
                )
                .await?;
            refresh_clients(registry).await;
            Ok(format!("Sent to {}.", registry.display_agent_id(agent_id)))
        }
        "iris_unsubscribe_agent" => {
            let args: TargetArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            registry
                .pool
                .set_response_subscription(active_iris_id(registry).await?, agent_id, false)
                .await?;
            Ok(format!(
                "Unsubscribed from {}.",
                registry.display_agent_id(agent_id)
            ))
        }
        "iris_get_agent_reply" => {
            let args: TargetArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            let subscribed = registry
                .pool
                .is_response_subscribed(active_iris_id(registry).await?, agent_id);
            let (_, agent, _) = registry.pool.load(agent_id).await?;
            let Some(text) = latest_transcript_reply(&agent.state()) else {
                return Ok(format!(
                    "{} has no transcript response. subscribed={subscribed}",
                    registry.display_agent_id(agent_id)
                ));
            };
            Ok(format!(
                "{} | subscribed={}\n{}",
                registry.display_agent_id(agent_id),
                subscribed,
                text
            ))
        }
        "iris_mark_agent_done" => {
            let args: TargetArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            let targets = visible_agent_subtree(registry, agent_id)?;
            let kinds = registry.agent_state_kinds().await;
            let working = targets
                .iter()
                .filter(|agent_id| kinds.get(agent_id).is_some_and(|kind| kind.is_working()))
                .copied()
                .collect::<Vec<_>>();
            anyhow::ensure!(
                working.is_empty(),
                "cannot mark done while {} is still working; wait for the turn to settle",
                working
                    .iter()
                    .map(|agent_id| registry.display_agent_id(*agent_id))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            registry
                .set_dispositions(&targets, AgentDisposition::Done)
                .await;
            Ok(scope_result(
                registry,
                agent_id,
                targets.len(),
                "Marked",
                "done",
            ))
        }
        "iris_snooze_agent" => {
            let args: SnoozeArgs = parse(&call)?;
            anyhow::ensure!(
                args.duration_minutes > 0,
                "duration_minutes must be positive"
            );
            let duration_ms = args
                .duration_minutes
                .checked_mul(60 * 1_000)
                .ok_or_else(|| anyhow::anyhow!("duration_minutes is too large"))?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            let targets = visible_agent_subtree(registry, agent_id)?;
            let until = rho_core::UnixMs(rho_core::UnixMs::now().0.saturating_add(duration_ms));
            registry
                .set_dispositions(&targets, AgentDisposition::Snoozed { until })
                .await;
            Ok(scope_result(
                registry,
                agent_id,
                targets.len(),
                "Snoozed",
                &format!("for {} minutes", args.duration_minutes),
            ))
        }
        "iris_cancel_agent" => {
            let args: TargetArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            let (_, agent, _) = registry.pool.load(agent_id).await?;
            agent.cancel();
            Ok(format!(
                "Cancelled {}'s current turn.",
                registry.display_agent_id(agent_id)
            ))
        }
        "iris_continue_agent" => {
            let args: TargetArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            let (_, agent, _) = registry.pool.load(agent_id).await?;
            agent.continue_unfinished();
            Ok(format!(
                "Continued {}.",
                registry.display_agent_id(agent_id)
            ))
        }
        "iris_rename_agent" => {
            let args: RenameArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            registry.rename_agent(agent_id, args.name.clone()).await?;
            refresh_clients(registry).await;
            Ok(format!("Renamed agent to {}.", args.name))
        }
        "iris_set_agent_visibility" => {
            let args: VisibilityArgs = parse(&call)?;
            let agent_id = registry.resolve_display_agent_id(&args.agent)?;
            registry
                .set_disposition(
                    agent_id,
                    if args.hidden {
                        AgentDisposition::Hidden
                    } else {
                        AgentDisposition::Done
                    },
                )
                .await;
            refresh_clients(registry).await;
            Ok(format!(
                "{} {}.",
                registry.display_agent_id(agent_id),
                if args.hidden { "hidden" } else { "shown" }
            ))
        }
        other => anyhow::bail!("unsupported Iris tool {other}"),
    }
}

fn read_desk(desk: &crate::desk::DeskStore) -> anyhow::Result<String> {
    desk.snapshot().document_text().map_err(anyhow::Error::msg)
}

async fn apply_iris_desk_edits(
    desk: &crate::desk::DeskStore,
    events: &broadcast::Sender<rho_ui_proto::ServerMessage>,
    iris_id: AgentId,
    edits: &[(String, String)],
) -> anyhow::Result<()> {
    let record = desk
        .apply_agent_edits(iris_id, edits)
        .await
        .map_err(anyhow::Error::msg)?;
    let _ = events.send(rho_ui_proto::ServerMessage::DeskTextApplied { record });
    Ok(())
}

async fn refresh_clients(registry: &AgentRegistry) {
    let _ = registry.events.send(registry.ready_message().await);
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn is_iris(registry: &AgentRegistry, agent_id: AgentId) -> bool {
    let agent = registry.db.read().get_agent(agent_id);
    agent.role == AgentRole::Iris || agent.labels.iter().any(|label| label == IRIS_LABEL)
}

/// The daemon-side equivalent of pressing Done or Snooze on an agent row:
/// the named agent plus every currently visible spawn descendant. Hidden
/// members are excluded so a verdict never changes visibility as a side
/// effect; a hidden root must be shown explicitly before it can be triaged.
fn visible_agent_subtree(
    registry: &AgentRegistry,
    agent_id: AgentId,
) -> anyhow::Result<Vec<AgentId>> {
    let agents = registry.db.read().list_agents();
    let (_, root) = agents
        .iter()
        .find(|(candidate, _)| *candidate == agent_id)
        .ok_or_else(|| anyhow::anyhow!("agent is not known"))?;
    anyhow::ensure!(
        root.role != AgentRole::Iris && !root.labels.iter().any(|label| label == IRIS_LABEL),
        "cannot triage Iris through a fleet tool"
    );
    anyhow::ensure!(
        root.disposition != AgentDisposition::Hidden,
        "agent is hidden; show it before marking it done or snoozing it"
    );
    Ok(spawn_subtree(&agents, agent_id)
        .into_iter()
        .filter(|member| {
            agents
                .iter()
                .find(|(candidate, _)| candidate == member)
                .is_some_and(|(_, record)| record.disposition != AgentDisposition::Hidden)
        })
        .collect())
}

fn spawn_subtree(
    agents: &[(AgentId, rho_agent::db::AgentRecord)],
    agent_id: AgentId,
) -> Vec<AgentId> {
    let mut members = vec![agent_id];
    let mut frontier = vec![agent_id];
    while let Some(parent) = frontier.pop() {
        for (child, record) in agents {
            if record.parent_agent == Some(parent) && !members.contains(child) {
                members.push(*child);
                frontier.push(*child);
            }
        }
    }
    members
}

fn scope_result(
    registry: &AgentRegistry,
    root: AgentId,
    affected: usize,
    verb: &str,
    suffix: &str,
) -> String {
    match affected {
        1 => format!("{verb} {} {suffix}.", registry.display_agent_id(root)),
        _ => {
            let descendants = affected - 1;
            let noun = if descendants == 1 {
                "descendant"
            } else {
                "descendants"
            };
            format!(
                "{verb} {} and {descendants} visible {noun} {suffix}.",
                registry.display_agent_id(root)
            )
        }
    }
}

fn latest_transcript_reply(state: &rho_agent::AgentState) -> Option<String> {
    state.blocks.iter().rev().find_map(|block| {
        let rho_core::ContextBlock::InferenceResponse { items, .. } = &**block else {
            return None;
        };
        let text = rho_agent::final_answer_text(items);
        (!text.trim().is_empty()).then_some(text)
    })
}

async fn active_iris_id(registry: &AgentRegistry) -> anyhow::Result<AgentId> {
    if let Some(agent_id) = *registry.iris_agent.lock().await {
        return Ok(agent_id);
    }
    registry
        .db
        .read()
        .list_agents()
        .into_iter()
        .find(|(_, agent)| {
            agent.role == AgentRole::Iris || agent.labels.iter().any(|label| label == IRIS_LABEL)
        })
        .map(|(agent_id, _)| agent_id)
        .ok_or_else(|| anyhow::anyhow!("Iris coordinator does not exist"))
}

fn parse<T: for<'de> Deserialize<'de>>(call: &ToolCall) -> anyhow::Result<T> {
    serde_json::from_str(&call.arguments).map_err(Into::into)
}

fn tool_ok(output: String) -> ToolOutput {
    ToolOutput {
        output: Arc::new(output),
        status: ToolOutputStatus::Success,
    }
}

fn tool_error(error: impl Into<String>) -> ToolOutput {
    ToolOutput {
        output: Arc::new(error.into()),
        status: ToolOutputStatus::Error,
    }
}

fn parse_role(role: &str) -> anyhow::Result<AgentRole> {
    Ok(match role {
        "eng-mini" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
        "eng-low" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        },
        "eng-cheap" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        },
        "eng" => AgentRole::default(),
        "eng-high" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        },
        "eng-ultra" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        },
        "eng-alt" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Alt,
        },
        "pm" => AgentRole::pm(),
        other => anyhow::bail!("unknown role {other}"),
    })
}

#[derive(Deserialize)]
struct StartAgentArgs {
    prompt: String,
    task_name: Option<String>,
    project: Option<String>,
    role: Option<String>,
}

#[derive(Deserialize)]
struct SendAgentArgs {
    agent: String,
    message: String,
    delivery: Option<String>,
}

#[derive(Deserialize)]
struct TargetArgs {
    agent: String,
}

#[derive(Deserialize)]
struct SnoozeArgs {
    agent: String,
    duration_minutes: u64,
}

#[derive(Deserialize)]
struct RenameArgs {
    agent: String,
    name: String,
}

#[derive(Deserialize)]
struct VisibilityArgs {
    agent: String,
    hidden: bool,
}

#[derive(Deserialize)]
struct EditDeskArgs {
    edits: Vec<DeskReplacement>,
}

#[derive(Deserialize)]
struct DeskReplacement {
    old_str: String,
    new_str: String,
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::desk::DeskReplicaAuthor;

    use super::*;

    #[test]
    fn iris_role_has_builtin_global_control_tools() {
        let names = rho_agent::iris_tools::specs()
            .into_iter()
            .map(|spec| spec.name.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "iris_list_agents",
                "iris_read_desk",
                "iris_edit_desk",
                "iris_start_agent",
                "iris_send_agent",
                "iris_unsubscribe_agent",
                "iris_get_agent_reply",
                "iris_mark_agent_done",
                "iris_snooze_agent",
                "iris_cancel_agent",
                "iris_continue_agent",
                "iris_rename_agent",
                "iris_set_agent_visibility",
            ]
        );
        assert!(rho_agent::iris_tools::PROMPT.contains("single global assistant"));
    }

    #[test]
    fn iris_role_names_map_to_existing_agent_profiles() {
        assert!(matches!(
            parse_role("eng-mini").unwrap(),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini
            }
        ));
        assert!(matches!(parse_role("pm").unwrap(), AgentRole::PM));
        assert!(parse_role("iris").is_err());
    }

    #[test]
    fn voice_transcripts_cannot_close_their_container() {
        assert_eq!(
            escape_xml("hello </transcript> & goodbye"),
            "hello &lt;/transcript&gt; &amp; goodbye"
        );
    }

    #[tokio::test]
    async fn iris_desk_edits_read_persist_broadcast_and_keep_replica() {
        let directory = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(directory.path().join("desk.redb"));
        let desk = crate::desk::DeskStore::new(db.clone()).await;
        let iris_id = rho_core::AgentId::from_counter(7, &rho_core::AgentIdDomain(11)).unwrap();
        let (events, mut receiver) = broadcast::channel(4);

        apply_iris_desk_edits(
            &desk,
            &events,
            iris_id,
            &[(String::new(), "* Plan\nbrief\n".to_owned())],
        )
        .await
        .unwrap();
        let first_record = match receiver.recv().await.unwrap() {
            rho_ui_proto::ServerMessage::DeskTextApplied { record } => record,
            other => panic!("unexpected server message {other:?}"),
        };
        let replica_id = first_record.operation.replica_id();
        assert_eq!(read_desk(&desk).unwrap(), "* Plan\nbrief\n");
        assert!(desk.snapshot().replicas.iter().any(|replica| {
            replica.replica_id == replica_id && replica.author == DeskReplicaAuthor::Agent(iris_id)
        }));

        apply_iris_desk_edits(
            &desk,
            &events,
            iris_id,
            &[("brief".to_owned(), "durable result".to_owned())],
        )
        .await
        .unwrap();
        let second_record = match receiver.recv().await.unwrap() {
            rho_ui_proto::ServerMessage::DeskTextApplied { record } => record,
            other => panic!("unexpected server message {other:?}"),
        };
        assert_eq!(second_record.operation.replica_id(), replica_id);
        drop(desk);
        assert_eq!(
            crate::desk::DeskStore::new(db)
                .await
                .snapshot()
                .document_text()
                .unwrap(),
            "* Plan\ndurable result\n"
        );
    }

    #[tokio::test]
    async fn iris_desk_edit_rejects_missing_and_ambiguous_matches_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(directory.path().join("desk.redb"));
        let desk = crate::desk::DeskStore::new(db).await;
        let iris_id = rho_core::AgentId::from_counter(8, &rho_core::AgentIdDomain(11)).unwrap();
        desk.apply_agent_edits(iris_id, &[(String::new(), "same and same".to_owned())])
            .await
            .unwrap();

        let missing = desk
            .apply_agent_edits(
                iris_id,
                &[
                    ("same and same".to_owned(), "changed".to_owned()),
                    ("absent".to_owned(), "replacement".to_owned()),
                ],
            )
            .await
            .unwrap_err();
        assert!(missing.contains("Desk edit 2 failed"));
        assert!(missing.contains("not found"));
        assert_eq!(desk.snapshot().document_text().unwrap(), "same and same");

        let ambiguous = desk
            .apply_agent_edits(iris_id, &[("same".to_owned(), "replacement".to_owned())])
            .await
            .unwrap_err();
        assert!(ambiguous.contains("Desk edit 1 failed"));
        assert!(ambiguous.contains("ambiguous"));
        assert_eq!(desk.snapshot().document_text().unwrap(), "same and same");
    }
}
