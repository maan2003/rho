use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use futures::Stream;
use rho_agent::db::{AgentId, TagId, TagKind};
use rho_core::ContentPart;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};

use crate::remote::{AgentRemoteFrame, UiAgentState, UiBlock};
use crate::{
    AgentRole, ClientMessage, IoCounters, MessageDelivery, ProtocolLogDirection, ServerMessage,
    StartMode, UiAgentSummary, UiProject, UiTag, append_protocol_log_record,
    protocol_frame_bytes, read_frame_counted, write_frame_counted,
};

/// Raw async client for the rho UI Unix-socket protocol.
pub struct Client {
    stream: UnixStream,
    counters: IoCounters,
    logger: Option<ProtocolLogger>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self::from_stream(stream))
    }

    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            counters: IoCounters::default(),
            logger: ProtocolLogger::from_env(),
        }
    }

    pub async fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        write_frame_counted(&mut self.stream, message, Some(&self.counters)).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ClientToServer, message);
        }
        Ok(())
    }

    pub async fn recv(&mut self) -> anyhow::Result<ServerMessage> {
        let message = read_frame_counted(&mut self.stream, Some(&self.counters)).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ServerToClient, &message);
        }
        Ok(message)
    }

    pub fn io_counters(&self) -> IoCounters {
        self.counters.clone()
    }

    fn logger(&self) -> Option<ProtocolLogger> {
        self.logger.clone()
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// Typed client handle for controlling and observing a rho agent over the UI
/// protocol.
#[derive(Clone)]
pub struct AgentClient {
    commands: mpsc::UnboundedSender<ClientMessage>,
    state: watch::Receiver<HashMap<AgentId, UiAgentState>>,
    tags: watch::Receiver<Vec<UiTag>>,
    agents: watch::Receiver<Vec<UiAgentSummary>>,
    projects: watch::Receiver<Vec<UiProject>>,
    known_agent_ids: watch::Receiver<Vec<AgentId>>,
    frames: broadcast::Sender<(AgentId, AgentRemoteFrame)>,
    counters: IoCounters,
    machine_seed: u64,
    agent_counter: u64,
}

impl AgentClient {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::connect_client(Client::connect(path).await?).await
    }

    pub async fn connect_client(client: Client) -> anyhow::Result<Self> {
        let client_counters = client.io_counters();
        let logger = client.logger();
        let mut stream = client.into_stream();
        write_frame_counted(
            &mut stream,
            &ClientMessage::Subscribe,
            Some(&client_counters),
        )
        .await?;
        if let Some(logger) = &logger {
            logger.log(
                ProtocolLogDirection::ClientToServer,
                &ClientMessage::Subscribe,
            );
        }
        let ServerMessage::Ready {
            tags,
            agents,
            projects,
            machine_seed,
            agent_counter,
            workspace_counter,
        } = read_frame_counted(&mut stream, Some(&client_counters)).await?
        else {
            anyhow::bail!("rho daemon did not send ready message");
        };
        let agent_ids = summary_agent_ids(&agents);
        if let Some(logger) = &logger {
            logger.log(
                ProtocolLogDirection::ServerToClient,
                &ServerMessage::Ready {
                    tags: tags.clone(),
                    agents: agents.clone(),
                    projects: projects.clone(),
                    machine_seed,
                    agent_counter,
                    workspace_counter,
                },
            );
        }

        let (reader, writer) = stream.into_split();
        let (state_tx, state_rx) = watch::channel(HashMap::new());
        let (tags_tx, tags_rx) = watch::channel(tags.clone());
        let (agents_tx, agents_rx) = watch::channel(agents);
        let (projects_tx, projects_rx) = watch::channel(projects);
        let (known_agent_ids_tx, known_agent_ids_rx) = watch::channel(agent_ids);
        let (frame_tx, _) = broadcast::channel::<(AgentId, AgentRemoteFrame)>(256);
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<ClientMessage>();

        let reader_counters = client_counters.clone();
        let reader_logger = logger.clone();
        let reader_frame_tx = frame_tx.clone();
        tokio::spawn(async move {
            let mut reader = reader;
            let mut state = state_tx.borrow().clone();
            let mut known_agent_ids = known_agent_ids_tx.borrow().clone();
            loop {
                let message = match read_frame_counted::<_, ServerMessage>(
                    &mut reader,
                    Some(&reader_counters),
                )
                .await
                {
                    Ok(message) => message,
                    Err(_) => break,
                };
                if let Some(logger) = &reader_logger {
                    logger.log(ProtocolLogDirection::ServerToClient, &message);
                }
                match message {
                    ServerMessage::Agent { agent_id, frame } => {
                        let _ = reader_frame_tx.send((agent_id, frame.clone()));
                        let agent_state = state.entry(agent_id).or_insert_with(empty_agent_state);
                        frame.apply_diff(agent_state);
                        if state_tx.send(state.clone()).is_err() {
                            break;
                        }
                    }
                    ServerMessage::Ready {
                        tags,
                        agents,
                        projects,
                        machine_seed: _,
                        agent_counter: _,
                        workspace_counter: _,
                    } => {
                        known_agent_ids = summary_agent_ids(&agents);
                        if tags_tx.send(tags).is_err() {
                            break;
                        }
                        if agents_tx.send(agents).is_err() {
                            break;
                        }
                        if projects_tx.send(projects).is_err() {
                            break;
                        }
                        if known_agent_ids_tx.send(known_agent_ids.clone()).is_err() {
                            break;
                        }
                    }
                    ServerMessage::TagCreated { tag } => {
                        let mut tags = tags_tx.borrow().clone();
                        // Tags stay in the daemon's creation order; a new
                        // tag is the newest, so it belongs at the end.
                        tags.push(tag);
                        if tags_tx.send(tags).is_err() {
                            break;
                        }
                    }
                    ServerMessage::Error { message } => {
                        eprintln!("rho daemon error: {message}")
                    }
                    ServerMessage::AgentCreated { agent_id, .. }
                    | ServerMessage::AgentLoaded { agent_id } => {
                        if !known_agent_ids.contains(&agent_id) {
                            known_agent_ids.push(agent_id);
                            known_agent_ids.sort();
                            if known_agent_ids_tx.send(known_agent_ids.clone()).is_err() {
                                break;
                            }
                        }
                    }
                    ServerMessage::AgentAttention {
                        agent_id,
                        attention,
                    } => {
                        let mut agents = agents_tx.borrow().clone();
                        for agent in &mut agents {
                            if agent.agent_id == agent_id {
                                agent.attention = attention;
                            }
                        }
                        if agents_tx.send(agents).is_err() {
                            break;
                        }
                    }
                    ServerMessage::Pong
                    | ServerMessage::TurnCancelled { .. }
                    | ServerMessage::LandLeaseQueued { .. }
                    | ServerMessage::LandLeaseGranted { .. }
                    | ServerMessage::LandStatus { .. }
                    | ServerMessage::McpAgentToolResult(_)
                    | ServerMessage::PlatformStatus { .. }
                    | ServerMessage::IrohApproved { .. }
                    | ServerMessage::IrohRevoked { .. }
                    | ServerMessage::PrCommandResult { .. } => {}
                    // Zed channel handshake replies only appear on dedicated
                    // channel streams, never in a UI session.
                    ServerMessage::ChannelOpened { .. }
                    | ServerMessage::ChannelClosed { .. }
                    | ServerMessage::AgentStreamOpened { .. } => {}
                }
            }
        });

        let writer_counters = client_counters.clone();
        let writer_logger = logger.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(message) = command_rx.recv().await {
                if write_frame_counted(&mut writer, &message, Some(&writer_counters))
                    .await
                    .is_err()
                {
                    break;
                }
                if let Some(logger) = &writer_logger {
                    logger.log(ProtocolLogDirection::ClientToServer, &message);
                }
            }
        });

        Ok(Self {
            commands: command_tx,
            state: state_rx,
            tags: tags_rx,
            agents: agents_rx,
            projects: projects_rx,
            known_agent_ids: known_agent_ids_rx,
            frames: frame_tx,
            counters: client_counters,
            machine_seed,
            agent_counter,
        })
    }

    pub fn io_counters(&self) -> IoCounters {
        self.counters.clone()
    }

    /// The daemon database's machine seed, for encoding agent IDs.
    pub fn machine_seed(&self) -> u64 {
        self.machine_seed
    }

    pub fn agent_counter(&self) -> u64 {
        self.agent_counter
    }

    pub fn blocks(&self) -> Vec<UiBlock> {
        self.state().map(|state| state.blocks).unwrap_or_default()
    }

    pub fn state(&self) -> Option<UiAgentState> {
        self.states()
            .into_iter()
            .min_by(|(left, _), (right, _)| left.cmp(right))
            .map(|(_, state)| state)
    }

    pub fn state_for_agent(&self, agent_id: AgentId) -> Option<UiAgentState> {
        self.state.borrow().get(&agent_id).cloned()
    }

    pub fn states(&self) -> HashMap<AgentId, UiAgentState> {
        self.state.borrow().clone()
    }

    pub fn loaded_agent_ids(&self) -> Vec<AgentId> {
        let mut agent_ids = self.state.borrow().keys().cloned().collect::<Vec<_>>();
        agent_ids.sort();
        agent_ids
    }

    pub fn known_agent_ids(&self) -> Vec<AgentId> {
        self.known_agent_ids.borrow().clone()
    }

    pub fn tags(&self) -> Vec<UiTag> {
        self.tags.borrow().clone()
    }

    pub fn agents(&self) -> Vec<UiAgentSummary> {
        self.agents.borrow().clone()
    }

    pub fn projects(&self) -> Vec<UiProject> {
        self.projects.borrow().clone()
    }

    pub fn new_agent_with_user_message(&self, tags: Vec<TagId>, repo: Utf8PathBuf, text: String) {
        let _ = self.commands.send(ClientMessage::NewAgent {
            tags,
            role: AgentRole::default(),
            start: default_start(repo),
            content: Some(vec![ContentPart::Text { text }]),
        });
    }

    pub fn new_agent(&self, tags: Vec<TagId>, repo: Utf8PathBuf) {
        let _ = self.commands.send(ClientMessage::NewAgent {
            tags,
            role: AgentRole::default(),
            start: default_start(repo),
            content: None,
        });
    }

    pub fn new_tag(&self, name: String, kind: TagKind, parent: Option<TagId>) {
        let _ = self.commands.send(ClientMessage::NewTag { name, kind, parent });
    }

    pub fn set_project(&self, path: Utf8PathBuf, name: Option<String>, description: String) {
        let _ = self.commands.send(ClientMessage::ProjectSet {
            path,
            name,
            description,
        });
    }

    pub fn remove_project(&self, path: Utf8PathBuf) {
        let _ = self.commands.send(ClientMessage::ProjectRemove { path });
    }

    pub fn tag_agent(&self, agent_id: AgentId, target: crate::TagTarget) {
        let _ = self
            .commands
            .send(ClientMessage::TagAgent { agent_id, target });
    }

    pub fn untag_agent(&self, agent_id: AgentId, tag_id: TagId) {
        let _ = self
            .commands
            .send(ClientMessage::UntagAgent { agent_id, tag_id });
    }

    pub fn set_agent_status(&self, agent_id: AgentId, status: crate::Status) {
        let _ = self
            .commands
            .send(ClientMessage::SetAgentStatus { agent_id, status });
    }

    pub fn rename_agent(&self, agent_id: AgentId, name: String) {
        let _ = self
            .commands
            .send(ClientMessage::RenameAgent { agent_id, name });
    }

    pub fn change_prompt_cache_key(&self, agent_id: AgentId) {
        let _ = self
            .commands
            .send(ClientMessage::ChangePromptCacheKey { agent_id });
    }

    pub fn rename_tag(&self, tag_id: TagId, name: String) {
        let _ = self
            .commands
            .send(ClientMessage::RenameTag { tag_id, name });
    }

    pub fn set_tag_status(&self, tag_id: TagId, status: crate::Status) {
        let _ = self
            .commands
            .send(ClientMessage::SetTagStatus { tag_id, status });
    }

    pub fn load_agent(&self, agent_id: AgentId) {
        let _ = self.commands.send(ClientMessage::LoadAgent { agent_id });
    }

    pub fn send_user_message(&self, agent_id: AgentId, text: String, delivery: MessageDelivery) {
        let _ = self.commands.send(ClientMessage::SendUserMessage {
            agent_id,
            content: vec![ContentPart::Text { text }],
            delivery,
        });
    }

    pub fn compact_agent(&self, agent_id: AgentId, delivery: MessageDelivery) {
        let _ = self
            .commands
            .send(ClientMessage::CompactAgent { agent_id, delivery });
    }

    pub fn cancel(&self, agent_id: AgentId) {
        let _ = self.commands.send(ClientMessage::CancelTurn { agent_id });
    }

    pub fn rewind(&self, agent_id: AgentId, turns: u32) {
        let _ = self
            .commands
            .send(ClientMessage::RewindAgent { agent_id, turns });
    }

    pub fn continue_turn(&self, agent_id: AgentId) {
        let _ = self.commands.send(ClientMessage::ContinueTurn { agent_id });
    }

    pub fn subscribe(&self) -> impl Stream<Item = HashMap<AgentId, UiAgentState>> + use<> {
        let mut state = self.state.clone();
        async_stream::stream! {
            while state.changed().await.is_ok() {
                let current = state.borrow().clone();
                yield current;
            }
        }
    }

    pub fn subscribe_frames(&self) -> impl Stream<Item = (AgentId, AgentRemoteFrame)> + use<> {
        let mut frames = self.frames.subscribe();
        async_stream::stream! {
            loop {
                match frames.recv().await {
                    Ok(frame) => yield frame,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn empty_agent_state() -> UiAgentState {
    UiAgentState {
        blocks: Vec::new(),
        status: crate::remote::UiAgentStatus::Idle,
        context_used: None,
    }
}

fn summary_agent_ids(agents: &[UiAgentSummary]) -> Vec<AgentId> {
    let mut agent_ids = agents
        .iter()
        .map(|agent| agent.agent_id)
        .collect::<Vec<_>>();
    agent_ids.sort();
    agent_ids.dedup();
    agent_ids
}

#[derive(Clone)]
struct ProtocolLogger {
    file: Arc<Mutex<std::fs::File>>,
}

impl ProtocolLogger {
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("RHO_UI_PROTO_LOG")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    fn log<T>(&self, direction: ProtocolLogDirection, message: &T)
    where
        T: senax_encoder::Packer,
    {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let Ok(frame) = protocol_frame_bytes(message) else {
            return;
        };
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let _ = append_protocol_log_record(&mut *file, now_ms, direction, &frame);
    }
}

/// Without an explicit start point, new agents begin on the parents of the
/// user's working copy — a sibling of it.
fn default_start(repo: Utf8PathBuf) -> StartMode {
    StartMode::NewOn {
        repo,
        revset: "@-".to_owned(),
    }
}
