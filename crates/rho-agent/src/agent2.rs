//! The `rho-agent2` loop as a pool runtime.
//!
//! Same records, workdirs, roles and tools as a Rho agent; a different loop
//! underneath. Opt in with `RHO_AGENT2=1` in the daemon's environment. What
//! it does not do yet: Iris, presentation (titles, activity), usage
//! accounting, rewind, role and profile changes.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::{Stream, StreamExt as _};
use rho_agent2::{AgentActivity, AgentHandle, AgentSnapshot, Delivery, Preview, Store, Tool};
use rho_agent2_tools::FutureTool;
use rho_core::{
    AgentId, ContentPart, ContextBlock, InferenceResponseItem, MessageDelivery, MessageSender,
    ToolCall, ToolOutput, ToolSpec,
};
use rho_db::RhoDb;
use rho_inference::Inference;
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_web_search::WebSearchTools;
use rho_workspaces::View;

use crate::db::{
    AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentRole, AgentRuntime, AgentUsageModel,
    AgentWriteTxnExt as _, InferenceModel, SessionBinding, UnixMillis,
};
use crate::lazy::Lazy;
use crate::multi_agent_tools::{self, MultiAgentTools};
use crate::{
    AgentState, AgentStateKind, FailedInferenceResponse, InputQueues, QueuedItem, QueuedItemKind,
    StartWorkdir, ToolPreview, materialize_workdirs, pool, system_prompt,
};

/// Whether new GPT agents get the `rho-agent2` loop.
pub fn enabled() -> bool {
    std::env::var_os("RHO_AGENT2").is_some_and(|value| !value.is_empty() && value != "0")
}

const INSTRUCTIONS_APPENDIX: &str = "\n\n## How tool results arrive\n\n\
Every tool call is answered with its finished result, however long it takes; you never poll. \
A command that is still running when something else needs your attention is answered with what \
it has printed so far and a session ID, and everything it prints later arrives on that same \
call by itself. If you have nothing to do until something happens, call `wait` with the number \
of seconds you can afford to be left alone: anything ending, a user message or mail wakes you \
sooner, so a long interval costs nothing and a short one costs a request.";

#[derive(Clone)]
pub struct Agent2Runtime {
    handle: AgentHandle,
    model: InferenceModel,
    total_usage: crate::db::AgentUsageBucket,
}

impl Agent2Runtime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create(
        db: RhoDb,
        inference: Inference,
        mode: SessionBinding,
        role: AgentRole,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
        parent: Option<AgentId>,
        pool: std::sync::Weak<pool::AgentPool>,
    ) -> anyhow::Result<(AgentId, Self)> {
        anyhow::ensure!(role != AgentRole::Iris, "rho-agent2 does not run Iris yet");
        let config = mode
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("cannot create a Rho runtime for a Claude mode"))?;
        let model = mode.deep_model().expect("deep config implies a deep model");
        // Two records, two transactions: the agent2 store opens its own write
        // on the same database, so this one must not be held across it.
        let agent_id = {
            let mut write = db.write().await;
            let agent_id = write.alloc_agent_id();
            write.commit();
            agent_id
        };
        let entries = materialize_workdirs(start).await?;
        let view = View::new(entries.clone())?;
        let (tools, instructions) = surface(
            &db, &view, role, agent_id, &inference, config, parent, &pool,
        )?;
        let handle = AgentHandle::create(
            Store::from_db(db.clone()),
            inference,
            config,
            model,
            tools,
            instructions,
        )
        .await?;
        let mut write = db.write().await;
        write.create_agent(
            UnixMillis::now(),
            agent_id,
            display_name,
            entries
                .iter()
                .map(|workspace| workspace.info().clone())
                .collect(),
            mode,
            AgentRuntime::Rho2 { core: handle.id() },
            parent,
        );
        write.set_agent_role(agent_id, role);
        write.commit();
        Ok((
            agent_id,
            Self {
                handle,
                model,
                total_usage: Default::default(),
            },
        ))
    }

    pub(crate) async fn load(
        db: RhoDb,
        inference: Inference,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<View>>>,
        pool: std::sync::Weak<pool::AgentPool>,
    ) -> anyhow::Result<Self> {
        let record = db.read().get_agent(agent_id);
        let AgentRuntime::Rho2 { core } = record.runtime else {
            anyhow::bail!("agent {agent_id:?} does not use the rho-agent2 runtime");
        };
        let config = record
            .binding
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("Rho2 runtime stored with a Claude mode"))?;
        let model = record
            .binding
            .deep_model()
            .expect("deep config implies a deep model");
        let view = Arc::clone(view.get().await?);
        let (tools, instructions) = surface(
            &db,
            &view,
            record.role,
            agent_id,
            &inference,
            config,
            record.parent_agent,
            &pool,
        )?;
        let total_usage = db.read().agent_usage_total(agent_id);
        let handle = AgentHandle::load(Store::from_db(db), inference, tools, core, instructions)?;
        Ok(Self {
            handle,
            model,
            total_usage,
        })
    }

    pub fn state(&self) -> AgentState {
        self.project(self.handle.snapshot())
    }

    pub fn subscribe(&self) -> impl Stream<Item = AgentState> + use<> {
        let this = self.clone();
        self.handle
            .subscribe()
            .map(move |snapshot| this.project(snapshot))
    }

    pub fn send_user_content(&self, content: Vec<ContentPart>, delivery: MessageDelivery) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.send_user_content(content, lane(delivery)).await;
        });
    }

    pub async fn send_user_content_accepted(
        &self,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.handle.send_user_content(content, lane(delivery)).await,
            "agent loop has stopped"
        );
        Ok(())
    }

    pub fn send_agent_message(&self, sender: AgentId, body: String) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.send_mail(sender, body).await;
        });
    }

    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        body: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.handle.send_mail(sender, body).await,
            "agent loop has stopped"
        );
        Ok(())
    }

    pub fn compact(&self) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.compact().await;
        });
    }

    pub fn cancel(&self) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.cancel().await;
        });
    }

    /// Retry a failed request, or resume after a restart.
    pub fn retry(&self) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.retry().await;
        });
    }

    /// The `AgentState` shape the daemon and UI already understand.
    fn project(&self, snapshot: AgentSnapshot) -> AgentState {
        let mut queued_inputs = InputQueues::default();
        let mut previews = std::collections::BTreeMap::new();
        for preview in &snapshot.previews {
            match preview {
                Preview::User { items } => {
                    for item in items {
                        queued_inputs.push(QueuedItem {
                            kind: QueuedItemKind::UserMessage {
                                sender: MessageSender::User,
                                content: Arc::new(vec![ContentPart::Text {
                                    text: item.text.clone(),
                                }]),
                                source_id: None,
                            },
                            delivery: MessageDelivery::NextRequest,
                        });
                    }
                }
                Preview::Mail { sender, items } => {
                    for item in items {
                        queued_inputs.push(QueuedItem {
                            kind: QueuedItemKind::UserMessage {
                                sender: MessageSender::Agent { id: *sender },
                                content: Arc::new(vec![ContentPart::Text {
                                    text: item.text.clone(),
                                }]),
                                source_id: None,
                            },
                            delivery: MessageDelivery::NextRequest,
                        });
                    }
                }
                Preview::Tool { call_id, .. } => {
                    if let Some(call) = find_call(&snapshot.history, call_id) {
                        previews.insert(
                            call_id.clone(),
                            ToolPreview {
                                call,
                                started_at: rho_core::UnixMs::now(),
                                metadata: None,
                            },
                        );
                    }
                }
            }
        }
        let kind = match (snapshot.streaming, snapshot.activity) {
            (Some(pending_response), _) => AgentStateKind::ApiStreaming {
                pending_response,
                previous_attempt: None,
            },
            (None, AgentActivity::Stopped) => match snapshot.last_error {
                Some(error) => AgentStateKind::Error(FailedInferenceResponse {
                    partial_response: Default::default(),
                    attempt_count: std::num::NonZeroU64::MIN,
                    error: Arc::new(error.to_string()),
                }),
                None => AgentStateKind::Idle,
            },
            (None, AgentActivity::Live) if !previews.is_empty() => AgentStateKind::ToolCalling {
                previews,
                results: Vec::new(),
                waiting: None,
            },
            (None, AgentActivity::Live) => AgentStateKind::Idle,
        };
        AgentState {
            blocks: snapshot.history,
            queued_inputs,
            kind,
            context_used: snapshot.context_used,
            total_usage: self.total_usage.clone(),
            usage_provider: match self.model {
                InferenceModel::Gpt56Terra => AgentUsageModel::TERRA,
                InferenceModel::Gpt56Luna => AgentUsageModel::LUNA,
                InferenceModel::Gemini37FlashLow => AgentUsageModel::GEMINI,
                _ => AgentUsageModel::GPT,
            },
        }
    }
}

fn lane(delivery: MessageDelivery) -> Delivery {
    match delivery {
        MessageDelivery::Immediate => Delivery::Interrupt,
        MessageDelivery::NextRequest | MessageDelivery::NextTurn => Delivery::NextRequest,
    }
}

fn find_call(history: &[Arc<ContextBlock>], id: &rho_core::ToolCallId) -> Option<ToolCall> {
    history.iter().rev().find_map(|block| match &**block {
        ContextBlock::InferenceResponse { items, .. } => items.iter().find_map(|item| match item {
            InferenceResponseItem::ToolCall {
                id: call_id,
                name,
                tool_type,
                arguments,
                ..
            } if call_id == id => Some(ToolCall {
                id: call_id.clone(),
                name: name.clone(),
                tool_type: *tool_type,
                arguments: arguments.clone(),
            }),
            _ => None,
        }),
        _ => None,
    })
}

/// The tools and instructions of one agent: what a Rho agent gets, as
/// `rho-agent2` sources.
#[allow(clippy::too_many_arguments)]
fn surface(
    db: &RhoDb,
    view: &Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: &Inference,
    config: rho_inference::config::InferenceProfile,
    parent: Option<AgentId>,
    pool: &std::sync::Weak<pool::AgentPool>,
) -> anyhow::Result<(Vec<Arc<dyn Tool>>, String)> {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let multi_agent = pool
        .upgrade()
        .map(|_| MultiAgentTools::new(pool.clone(), agent_id, parent));
    let mut others: Vec<Arc<dyn FutureTool>> = vec![Arc::new(ImageTool(
        crate::image_tool::ImageTools::new(Arc::clone(view)),
    ))];
    if let Some(multi_agent) = &multi_agent {
        others.extend(
            multi_agent_tools::agent_tool_specs(role)
                .into_iter()
                .map(|spec| {
                    Arc::new(AgentTool {
                        tools: multi_agent.clone(),
                        spec,
                    }) as Arc<dyn FutureTool>
                }),
        );
    }
    others.push(Arc::new(WebSearchTools::new(
        inference.clone(),
        agent_id.encoded().to_owned(),
    )));
    let code_mode = cfg!(feature = "code-mode") && config.code_mode && !role.is_pm();
    let tools = rho_agent2_tools::tools(shell, others, code_mode)
        .map_err(|error| anyhow::anyhow!("code mode failed to start: {error}"))?;
    let projects = db
        .read()
        .list_projects()
        .into_iter()
        .map(|(path, project)| (path, project.description))
        .collect::<Vec<_>>();
    let mut instructions = system_prompt::prompt(
        view.as_ref(),
        multi_agent.as_ref(),
        code_mode,
        role,
        &projects,
    )
    .to_string();
    instructions.push_str(INSTRUCTIONS_APPENDIX);
    Ok((tools, instructions))
}

struct ImageTool(crate::image_tool::ImageTools);

impl FutureTool for ImageTool {
    fn spec(&self) -> ToolSpec {
        crate::image_tool::ImageTools::spec()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.0.clone();
        Box::pin(async move { tools.call(call).await })
    }
}

/// One of the collaboration tools, answered by the pool.
struct AgentTool {
    tools: MultiAgentTools,
    spec: ToolSpec,
}

impl FutureTool for AgentTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.tools.clone();
        Box::pin(async move { multi_agent_tools::call_agent_tool(tools, call).await })
    }
}

#[cfg(test)]
mod tests {
    use rho_workspaces::Repo;

    use super::*;
    use crate::db::AgentRoleSessionProfile as _;

    #[tokio::test]
    async fn creates_and_reloads_an_agent_on_the_agent2_loop() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
        }
        rho_inference::ensure_crypto_provider();
        let inference = Inference::new(db.clone()).await.unwrap();
        let repo = Arc::new(
            Repo::open_plain_with_path_overrides(temp.path(), Default::default()).unwrap(),
        );
        let checkout = repo.user_checkout().await.unwrap();
        let role = AgentRole::default();
        let (agent_id, runtime) = Agent2Runtime::create(
            db.clone(),
            inference.clone(),
            role.session_profile().unwrap(),
            role,
            Some("two".to_owned()),
            vec![StartWorkdir::Existing(checkout.clone())],
            None,
            std::sync::Weak::new(),
        )
        .await
        .unwrap();
        let record = db.read().get_agent(agent_id);
        assert!(matches!(record.runtime, AgentRuntime::Rho2 { .. }));
        assert_eq!(record.display_name.as_deref(), Some("two"));
        let state = runtime.state();
        assert_eq!(state.kind, AgentStateKind::Idle);
        assert!(state.blocks.is_empty());
        drop(runtime);

        let reloaded = Agent2Runtime::load(
            db,
            inference,
            agent_id,
            Arc::new(Lazy::ready(View::new(vec![checkout]).unwrap())),
            std::sync::Weak::new(),
        )
        .await
        .unwrap();
        assert_eq!(reloaded.state().kind, AgentStateKind::Idle);
    }

    #[test]
    fn immediate_delivery_interrupts_and_the_rest_ride_along() {
        assert_eq!(lane(MessageDelivery::Immediate), Delivery::Interrupt);
        assert_eq!(lane(MessageDelivery::NextRequest), Delivery::NextRequest);
        assert_eq!(lane(MessageDelivery::NextTurn), Delivery::NextRequest);
    }
}
