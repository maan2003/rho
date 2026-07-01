use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use futures::StreamExt as _;
use rho_agent::Agent;
use rho_agent::db::{AgentId, AgentReadTxnExt as _, AgentWriteTxnExt as _, TopicId, TopicStatus};
use rho_core::text_content;
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_inference::config::InferenceConfig;
use rho_ui_proto::remote::AgentRemoteEncoder;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    ClientMessage, ServerMessage, UiTopic, UiTopicStatus, read_frame_counted, write_frame_counted,
};
use tokio::sync::{Mutex, Notify, mpsc};

pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    let base = dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .ok_or_else(|| anyhow::anyhow!("runtime directory not available"))?;
    Ok(base.join("rho").join("rho.sock"))
}

pub fn default_db_path() -> anyhow::Result<PathBuf> {
    let base = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("state directory not available"))?;
    Ok(base.join("rho").join("rho.redb"))
}

#[derive(Clone, Debug, clap::Args)]
pub struct DaemonArgs {
    #[arg(long = "auth", default_value = "default")]
    pub auth: String,
    #[arg(long = "socket-path")]
    pub socket_path: Option<PathBuf>,
    /// Exit once the last UI client disconnects.
    #[arg(long = "die-on-detached")]
    pub die_on_detached: bool,
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    let socket_path = args.socket_path.unwrap_or(default_socket_path()?);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("create socket directory")?;
    }
    let _ = std::fs::remove_file(&socket_path);
    let server = Server::bind(&socket_path).context("bind rho daemon socket")?;

    let db = RhoDb::open(default_db_path()?);
    let auth = InferenceAuth::named(&args.auth)?;
    let inference_config = InferenceConfig::deep();
    let agents = Arc::new(AgentRegistry::new(db, auth, inference_config).await);

    let active_connections = Arc::new(AtomicUsize::new(0));
    let connection_closed = Arc::new(Notify::new());
    let mut accepted_connection = false;

    loop {
        if args.die_on_detached
            && accepted_connection
            && active_connections.load(Ordering::Relaxed) == 0
        {
            return Ok(());
        }

        tokio::select! {
            connection = server.accept() => {
                let connection = connection?;
                accepted_connection = true;
                active_connections.fetch_add(1, Ordering::Relaxed);
                let agents = agents.clone();
                let active_connections = active_connections.clone();
                let connection_closed = connection_closed.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(agents, connection).await {
                        eprintln!("rho daemon connection error: {error:#}");
                    }
                    active_connections.fetch_sub(1, Ordering::Relaxed);
                    connection_closed.notify_one();
                });
            }
            () = connection_closed.notified(), if active_connections.load(Ordering::Relaxed) > 0 => {}
        }
    }
}

struct AgentRegistry {
    db: RhoDb,
    auth: InferenceAuth,
    inference_config: InferenceConfig,
    agents: Mutex<HashMap<AgentId, Agent>>,
}

impl AgentRegistry {
    async fn new(db: RhoDb, auth: InferenceAuth, inference_config: InferenceConfig) -> Self {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        if db.read().list_topics().is_empty() {
            let mut write = db.write().await;
            write.create_topic(rho_core::UnixMs::now(), None, TopicStatus::Normal);
            write.commit();
        }
        Self {
            db,
            auth,
            inference_config,
            agents: Mutex::new(HashMap::new()),
        }
    }

    fn topics(&self) -> Vec<UiTopic> {
        let read = self.db.read();
        let mut topics = read
            .list_topics()
            .into_iter()
            .map(|(topic_id, topic)| UiTopic {
                topic_id,
                display_name: topic.display_name,
                status: ui_topic_status(topic.status),
                agent_ids: read.list_topic_agents(topic_id).into_iter().collect(),
            })
            .collect::<Vec<_>>();
        topics.sort_by(|left, right| left.topic_id.cmp(&right.topic_id));
        topics
    }

    async fn loaded(&self) -> Vec<(AgentId, Agent)> {
        let mut agents = self
            .agents
            .lock()
            .await
            .iter()
            .map(|(agent_id, agent)| (*agent_id, agent.clone()))
            .collect::<Vec<_>>();
        agents.sort_by_key(|(agent_id, _)| *agent_id);
        agents
    }

    async fn get(&self, agent_id: AgentId) -> Option<Agent> {
        self.agents.lock().await.get(&agent_id).cloned()
    }

    async fn create_topic(&self, display_name: Option<String>) -> UiTopic {
        let mut write = self.db.write().await;
        let topic_id =
            write.create_topic(rho_core::UnixMs::now(), display_name, TopicStatus::Normal);
        write.commit();
        UiTopic {
            topic_id,
            display_name: self.db.read().get_topic(topic_id).display_name,
            status: UiTopicStatus::Normal,
            agent_ids: Vec::new(),
        }
    }

    async fn create(&self, topic_id: TopicId) -> anyhow::Result<(TopicId, AgentId, Agent)> {
        self.db.read().get_topic(topic_id);
        let (agent_id, agent) = Agent::create_in_topic_with_id(
            self.db.clone(),
            self.auth.clone(),
            self.inference_config.clone(),
            topic_id,
            None,
        )
        .await;
        self.agents.lock().await.insert(agent_id, agent.clone());
        Ok((topic_id, agent_id, agent))
    }

    async fn load(&self, agent_id: AgentId) -> anyhow::Result<(AgentId, Agent, bool)> {
        if let Some(agent) = self.agents.lock().await.get(&agent_id).cloned() {
            return Ok((agent_id, agent, false));
        }
        if !self
            .db
            .read()
            .list_agents()
            .into_iter()
            .any(|(id, _)| id == agent_id)
        {
            anyhow::bail!("unknown agent id: {agent_id}");
        }
        let agent = Agent::load(self.db.clone(), self.auth.clone(), agent_id);
        self.agents.lock().await.insert(agent_id, agent.clone());
        Ok((agent_id, agent, true))
    }
}

async fn serve_connection(
    agents: Arc<AgentRegistry>,
    connection: ServerConnection,
) -> anyhow::Result<()> {
    let counters = connection.io_counters();
    let stream = connection.into_stream();
    let (reader, writer) = stream.into_split();

    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let writer_counters = counters.clone();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = outgoing_rx.recv().await {
            if write_frame_counted(&mut writer, &message, Some(&writer_counters))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let _ = outgoing_tx.send(ServerMessage::Ready {
        topics: agents.topics(),
    });

    for (agent_id, agent) in agents.loaded().await {
        subscribe_agent(agent_id, agent, outgoing_tx.clone());
    }

    let mut reader = reader;
    loop {
        match read_frame_counted::<_, ClientMessage>(&mut reader, Some(&counters)).await? {
            ClientMessage::Ping => {
                let _ = outgoing_tx.send(ServerMessage::Pong);
            }
            ClientMessage::Subscribe => {}
            ClientMessage::NewTopic { display_name } => {
                let topic = agents.create_topic(display_name).await;
                let _ = outgoing_tx.send(ServerMessage::TopicCreated { topic });
                let _ = outgoing_tx.send(ServerMessage::Ready {
                    topics: agents.topics(),
                });
            }
            ClientMessage::NewAgent { topic_id, content } => {
                let (topic_id, agent_id, agent) = match agents.create(topic_id).await {
                    Ok(created) => created,
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                subscribe_agent(agent_id.clone(), agent.clone(), outgoing_tx.clone());
                let _ = outgoing_tx.send(ServerMessage::AgentCreated {
                    topic_id,
                    agent_id: agent_id.clone(),
                });
                let _ = outgoing_tx.send(ServerMessage::Ready {
                    topics: agents.topics(),
                });
                if let Some(content) = content {
                    agent.send_user_message(text_content(&content));
                }
            }
            ClientMessage::LoadAgent { agent_id } => match agents.load(agent_id).await {
                Ok((agent_id, agent, loaded_now)) => {
                    if loaded_now {
                        subscribe_agent(agent_id.clone(), agent, outgoing_tx.clone());
                    }
                    let _ = outgoing_tx.send(ServerMessage::AgentLoaded { agent_id });
                }
                Err(error) => {
                    let _ = outgoing_tx.send(ServerMessage::Error {
                        message: error.to_string(),
                    });
                }
            },
            ClientMessage::SendUserMessage { agent_id, content } => {
                let agent = match agents.get(agent_id).await {
                    Some(agent) => agent,
                    None => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: format!("agent is not loaded: {agent_id}"),
                        });
                        continue;
                    }
                };
                agent.send_user_message(text_content(&content));
            }
            ClientMessage::CancelTurn { agent_id } => {
                if let Some(agent) = agents.get(agent_id).await {
                    agent.cancel();
                    let _ = outgoing_tx.send(ServerMessage::TurnCancelled { agent_id });
                }
            }
        }
    }
}

fn subscribe_agent(
    agent_id: AgentId,
    agent: Agent,
    state_tx: mpsc::UnboundedSender<ServerMessage>,
) {
    tokio::spawn(async move {
        let changes = agent.subscribe();
        let mut encoder = AgentRemoteEncoder::new();
        let _ = state_tx.send(ServerMessage::Agent {
            agent_id: agent_id.clone(),
            frame: encoder.encode(agent.state()),
        });
        futures::pin_mut!(changes);
        while let Some(state) = changes.next().await {
            if state_tx
                .send(ServerMessage::Agent {
                    agent_id: agent_id.clone(),
                    frame: encoder.encode(state),
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn ui_topic_status(status: TopicStatus) -> UiTopicStatus {
    match status {
        TopicStatus::Normal => UiTopicStatus::Normal,
        TopicStatus::Pinned => UiTopicStatus::Pinned,
        TopicStatus::Archived => UiTopicStatus::Archived,
    }
}
