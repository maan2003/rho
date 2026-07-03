use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use futures::StreamExt as _;
use rho_agent::Agent;
use rho_agent::db::{AgentId, AgentReadTxnExt as _, AgentWriteTxnExt as _, Status, TopicId};
use rho_core::text_content;
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_inference::config::InferenceConfig;
use rho_ui_proto::remote::AgentRemoteEncoder;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    ClientMessage, ServerMessage, UiAgentSummary, UiTopic, UiWorkdir, read_frame_counted,
    write_frame_counted,
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
    // The daemon's own cwd must never matter: agents each carry their own
    // working directory. Park the process somewhere empty and read-only so
    // any code still depending on process cwd fails loudly.
    let _ = std::env::set_current_dir("/var/empty").or_else(|_| std::env::set_current_dir("/"));

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
    /// The daemon-created topic agents are born into; announced in `Ready`
    /// so clients never guess it from topic ordering.
    default_topic_id: TopicId,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    agents: Mutex<HashMap<AgentId, Agent>>,
}

impl AgentRegistry {
    async fn new(db: RhoDb, auth: InferenceAuth, inference_config: InferenceConfig) -> Self {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        let machine_seed = db.read().machine_seed();
        // Topics are ad-hoc tab groups; every agent starts in the default
        // one (the oldest topic) until it is moved somewhere more specific.
        let oldest_topic = db
            .read()
            .list_topics()
            .into_iter()
            .min_by_key(|(_, topic)| topic.created_at);
        let default_topic_id = match oldest_topic {
            Some((topic_id, _)) => topic_id,
            None => {
                let mut write = db.write().await;
                let topic_id = write.create_topic(
                    rho_core::UnixMs::now(),
                    "default".to_owned(),
                    Status::Normal,
                );
                write.commit();
                topic_id
            }
        };
        Self {
            db,
            auth,
            inference_config,
            default_topic_id,
            machine_seed,
            agents: Mutex::new(HashMap::new()),
        }
    }

    fn topics(&self) -> Vec<UiTopic> {
        let read = self.db.read();
        // Key order over ids is meaningless (scrambled characters); creation
        // order comes from the timestamps.
        let mut records = read.list_topics();
        records.sort_by_key(|(_, topic)| topic.created_at);
        records
            .into_iter()
            .map(|(topic_id, topic)| {
                let mut agents = read
                    .list_topic_agents(topic_id)
                    .into_iter()
                    .map(|agent_id| (agent_id, read.get_agent(agent_id)))
                    .collect::<Vec<_>>();
                agents.sort_by_key(|(_, agent)| agent.created_at);
                UiTopic {
                    topic_id,
                    name: topic.name,
                    status: topic.status,
                    agents: agents
                        .into_iter()
                        .map(|(agent_id, agent)| UiAgentSummary {
                            agent_id,
                            display_name: agent.display_name,
                            working_directory: agent.working_directory,
                            status: agent.status,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    fn workdirs(&self) -> Vec<UiWorkdir> {
        let mut workdirs = self
            .db
            .read()
            .list_workdirs()
            .into_iter()
            .map(|(path, record)| UiWorkdir {
                path,
                name: record.name,
            })
            .collect::<Vec<_>>();
        workdirs.sort_by(|left, right| left.name.cmp(&right.name));
        workdirs
    }

    fn ready_message(&self) -> ServerMessage {
        ServerMessage::Ready {
            topics: self.topics(),
            workdirs: self.workdirs(),
            default_topic_id: self.default_topic_id,
            machine_seed: self.machine_seed,
        }
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

    async fn create_topic(&self, name: String) -> UiTopic {
        let mut write = self.db.write().await;
        let topic_id = write.create_topic(rho_core::UnixMs::now(), name, Status::Normal);
        write.commit();
        UiTopic {
            topic_id,
            name: self.db.read().get_topic(topic_id).name,
            status: Status::Normal,
            agents: Vec::new(),
        }
    }

    async fn create(
        &self,
        topic_id: TopicId,
        working_directory: PathBuf,
    ) -> anyhow::Result<(TopicId, AgentId, Agent)> {
        self.db.read().get_topic(topic_id);
        let working_directory = validate_working_directory(working_directory)?;
        let (agent_id, agent) = Agent::create(
            self.db.clone(),
            self.auth.clone(),
            self.inference_config.clone(),
            topic_id,
            None,
            working_directory,
        )
        .await;
        self.agents.lock().await.insert(agent_id, agent.clone());
        Ok((topic_id, agent_id, agent))
    }

    async fn move_agent(
        &self,
        agent_id: AgentId,
        target: rho_ui_proto::TopicTarget,
    ) -> anyhow::Result<()> {
        let read = self.db.read();
        if !read.list_agents().into_iter().any(|(id, _)| id == agent_id) {
            anyhow::bail!("unknown agent id: {agent_id:?}");
        }
        let topics = read.list_topics();
        drop(read);
        let mut write = self.db.write().await;
        let topic_id = match target {
            rho_ui_proto::TopicTarget::Existing(topic_id) => {
                if !topics.iter().any(|(id, _)| *id == topic_id) {
                    anyhow::bail!("unknown topic id: {topic_id:?}");
                }
                topic_id
            }
            rho_ui_proto::TopicTarget::Named(name) => topics
                .iter()
                .find(|(_, topic)| topic.name == name)
                .map(|(topic_id, _)| *topic_id)
                .unwrap_or_else(|| {
                    write.create_topic(rho_core::UnixMs::now(), name, Status::Normal)
                }),
        };
        write.move_agent_to_topic(agent_id, topic_id);
        write.commit();
        Ok(())
    }

    async fn set_agent_status(&self, agent_id: AgentId, status: Status) -> anyhow::Result<()> {
        if !self
            .db
            .read()
            .list_agents()
            .into_iter()
            .any(|(id, _)| id == agent_id)
        {
            anyhow::bail!("unknown agent id: {agent_id:?}");
        }
        let mut write = self.db.write().await;
        write.set_agent_status(rho_core::UnixMs::now(), agent_id, status);
        write.commit();
        Ok(())
    }

    async fn rename_agent(&self, agent_id: AgentId, name: String) -> anyhow::Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("agent name cannot be empty");
        }
        if !self
            .db
            .read()
            .list_agents()
            .into_iter()
            .any(|(id, _)| id == agent_id)
        {
            anyhow::bail!("unknown agent id: {agent_id:?}");
        }
        let mut write = self.db.write().await;
        write.set_agent_display_name(rho_core::UnixMs::now(), agent_id, name);
        write.commit();
        Ok(())
    }

    async fn rename_topic(&self, topic_id: TopicId, name: String) -> anyhow::Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("topic name cannot be empty");
        }
        if !self
            .db
            .read()
            .list_topics()
            .into_iter()
            .any(|(id, _)| id == topic_id)
        {
            anyhow::bail!("unknown topic id: {topic_id:?}");
        }
        let mut write = self.db.write().await;
        write.set_topic_name(rho_core::UnixMs::now(), topic_id, name);
        write.commit();
        Ok(())
    }

    async fn set_topic_status(&self, topic_id: TopicId, status: Status) -> anyhow::Result<()> {
        if !self
            .db
            .read()
            .list_topics()
            .into_iter()
            .any(|(id, _)| id == topic_id)
        {
            anyhow::bail!("unknown topic id: {topic_id:?}");
        }
        // New agents land in the default topic; archiving it would hide them
        // as they are created.
        if topic_id == self.default_topic_id && status == Status::Archived {
            anyhow::bail!("cannot archive the default topic");
        }
        let mut write = self.db.write().await;
        write.set_topic_status(rho_core::UnixMs::now(), topic_id, status);
        write.commit();
        Ok(())
    }

    async fn set_workdir(&self, path: PathBuf, name: Option<String>) -> anyhow::Result<()> {
        let path = validate_working_directory(path)?;
        let name = match name {
            Some(name) => name,
            None => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    anyhow::anyhow!("workdir path has no basename: {}", path.display())
                })?,
        };
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("workdir path is not valid UTF-8"))?
            .to_owned();
        let mut write = self.db.write().await;
        write.upsert_workdir(rho_core::UnixMs::now(), &path, name);
        write.commit();
        Ok(())
    }

    async fn remove_workdir(&self, path: PathBuf) -> anyhow::Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("workdir path is not valid UTF-8"))?;
        let mut write = self.db.write().await;
        write.remove_workdir(path);
        write.commit();
        Ok(())
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
            anyhow::bail!("unknown agent id: {agent_id:?}");
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

    let _ = outgoing_tx.send(agents.ready_message());

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
            ClientMessage::NewTopic { name } => {
                let topic = agents.create_topic(name).await;
                let _ = outgoing_tx.send(ServerMessage::TopicCreated { topic });
                let _ = outgoing_tx.send(agents.ready_message());
            }
            ClientMessage::NewAgent {
                topic_id,
                working_directory,
                content,
            } => {
                let (topic_id, agent_id, agent) =
                    match agents.create(topic_id, working_directory).await {
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
                let _ = outgoing_tx.send(agents.ready_message());
                if let Some(content) = content {
                    agent.send_user_message(text_content(&content));
                }
            }
            ClientMessage::WorkdirSet { path, name } => {
                match agents.set_workdir(path, name).await {
                    Ok(()) => {
                        let _ = outgoing_tx.send(agents.ready_message());
                    }
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientMessage::WorkdirRemove { path } => match agents.remove_workdir(path).await {
                Ok(()) => {
                    let _ = outgoing_tx.send(agents.ready_message());
                }
                Err(error) => {
                    let _ = outgoing_tx.send(ServerMessage::Error {
                        message: error.to_string(),
                    });
                }
            },
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
                            message: format!("agent is not loaded: {agent_id:?}"),
                        });
                        continue;
                    }
                };
                agent.send_user_message(text_content(&content));
            }
            ClientMessage::MoveAgent { agent_id, topic } => {
                match agents.move_agent(agent_id, topic).await {
                    Ok(()) => {
                        let _ = outgoing_tx.send(agents.ready_message());
                    }
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientMessage::SetAgentStatus { agent_id, status } => {
                match agents.set_agent_status(agent_id, status).await {
                    Ok(()) => {
                        let _ = outgoing_tx.send(agents.ready_message());
                    }
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientMessage::RenameAgent { agent_id, name } => {
                match agents.rename_agent(agent_id, name).await {
                    Ok(()) => {
                        let _ = outgoing_tx.send(agents.ready_message());
                    }
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientMessage::RenameTopic { topic_id, name } => {
                match agents.rename_topic(topic_id, name).await {
                    Ok(()) => {
                        let _ = outgoing_tx.send(agents.ready_message());
                    }
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientMessage::SetTopicStatus { topic_id, status } => {
                match agents.set_topic_status(topic_id, status).await {
                    Ok(()) => {
                        let _ = outgoing_tx.send(agents.ready_message());
                    }
                    Err(error) => {
                        let _ = outgoing_tx.send(ServerMessage::Error {
                            message: error.to_string(),
                        });
                    }
                }
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

/// Working directories must be absolute (the daemon's cwd is meaningless by
/// design) and must exist when an agent is created or a workdir registered.
/// A leading `~` expands to the daemon's home: clients may run on another
/// machine, so path interpretation belongs here.
fn validate_working_directory(path: PathBuf) -> anyhow::Result<PathBuf> {
    let path = expand_home(&path).unwrap_or(path);
    if !path.is_absolute() {
        anyhow::bail!("working directory must be absolute: {}", path.display());
    }
    if !path.is_dir() {
        anyhow::bail!("working directory does not exist: {}", path.display());
    }
    Ok(path)
}

fn expand_home(path: &std::path::Path) -> Option<PathBuf> {
    let rest = path.strip_prefix("~").ok()?;
    Some(dirs::home_dir()?.join(rest))
}
