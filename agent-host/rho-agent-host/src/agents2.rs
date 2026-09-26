//! Chat service: host-owned metadata and append-only logs, worker-owned
//! notebooks. Sessions receive only the chat projection, never cells or tool
//! output.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, anyhow};
use camino::Utf8PathBuf;
use rho_agent::pool::{Agent2ToolHandler, AgentPool};
use rho_agent::{ChatRemote, ChatWorkerEvent};
use rho_agent_types::{AgentIdDomain, AgentRole, Place, WorksetMode, WorkspaceInfo};
use rho_agent2::chat as chat2;
use rho_agent2::human::{Agent2Call, Agent2Reply};
use rho_agent2::log::{self, Block, Entry, Notice};
use rho_agents2_client::protocol as wire;
use rho_fs_view::Workset;
use rho_rpc::protocol::{Answer, write_frame};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use tokio::sync::broadcast;

use crate::{Services, expand_home, visible_path};

#[derive(senax_encoder::Pack, senax_encoder::Unpack)]
struct Stored {
    place: Place,
    role: AgentRole,
    model: String,
    effort: String,
    #[senax(default)]
    parent: Option<wire::AgentId>,
    #[senax(default)]
    user_owned: bool,
}
struct Record {
    info: wire::AgentInfo,
    remote: Arc<ChatRemote>,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    machine_seed: u64,
    next_counter: u64,
}

pub(crate) struct Agents2 {
    root: Utf8PathBuf,
    pool: Arc<AgentPool>,
    identity: Mutex<Identity>,
    records: Mutex<HashMap<wire::AgentId, Record>>,
    changes: broadcast::Sender<wire::ServerFrame>,
}

impl Agents2 {
    async fn open(
        root: Utf8PathBuf,
        pool: Arc<AgentPool>,
        machine_seed: u64,
        initial_counter: u64,
    ) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        let identity_path = root.join("identity.json");
        let identity: Identity = if identity_path.exists() {
            serde_json::from_slice(&std::fs::read(&identity_path)?)?
        } else {
            Identity {
                machine_seed,
                next_counter: initial_counter.saturating_add(1),
            }
        };
        anyhow::ensure!(
            identity.machine_seed == machine_seed,
            "agent identity belongs to another host"
        );
        let manager = Arc::new(Self {
            root,
            pool,
            identity: Mutex::new(identity),
            records: Mutex::new(HashMap::new()),
            changes: broadcast::channel(1024).0,
        });
        let weak = Arc::downgrade(&manager);
        let handler: Agent2ToolHandler = Arc::new(move |source, call| {
            let weak = weak.clone();
            Box::pin(async move {
                let manager = weak.upgrade().context("chat manager is shutting down")?;
                manager.tool_call(source, call).await
            })
        });
        manager.pool.install_agent2_tool_handler(handler)?;
        for entry in std::fs::read_dir(&manager.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = match wire::AgentId::from_encoded(&entry.file_name().to_string_lossy()) {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(path = %entry.path().display(), %error, "skipping unrecognized agent directory");
                    continue;
                }
            };
            let metadata = entry.path().join("config.senax");
            let stored: Stored = match std::fs::read(&metadata)
                .ok()
                .and_then(|bytes| senax_encoder::unpack(&mut bytes.as_slice()).ok())
            {
                Some(stored) => stored,
                None => {
                    tracing::warn!(path = %metadata.display(), "skipping agent without valid metadata");
                    continue;
                }
            };
            let effort: wire::Effort = stored.effort.parse().map_err(anyhow::Error::msg)?;
            let workset = manager
                .pool
                .worksets()
                .open_workset(&stored.place.workset)
                .await?;
            let log_path = Self::log_path(&workset, &id)?;
            let info = Self::info_from_log(
                id,
                stored.place,
                stored.role,
                stored.parent,
                stored.user_owned,
                stored.model,
                effort,
                log::Log::open(log_path.as_std_path())?,
            );
            manager.start(info).await?;
        }
        Ok(manager)
    }

    pub(crate) async fn live(
        root: Utf8PathBuf,
        pool: Arc<AgentPool>,
        machine_seed: u64,
        initial_counter: u64,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open(root, pool, machine_seed, initial_counter).await
    }

    fn log_path(workset: &Workset, id: &wire::AgentId) -> anyhow::Result<Utf8PathBuf> {
        let path = workset
            .state_dir()?
            .join("agents")
            .join(id.encoded())
            .join("log");
        std::fs::create_dir_all(path.parent().expect("agent log has a directory"))?;
        Ok(path)
    }

    fn info_from_log(
        id: wire::AgentId,
        place: Place,
        role: AgentRole,
        parent: Option<wire::AgentId>,
        user_owned: bool,
        model: String,
        effort: wire::Effort,
        log: log::Log,
    ) -> wire::AgentInfo {
        let archived = log
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Entry::Notice {
                    notice: Notice::Archived,
                    ..
                } => Some(true),
                Entry::Notice {
                    notice: Notice::FreshNotebook,
                    ..
                } => Some(false),
                _ => None,
            })
            .unwrap_or(false);
        let chat: Vec<_> = chat2::chat(&id, log.entries())
            .into_iter()
            .filter_map(convert_chat)
            .collect();
        let status = wire::visible_chat(&chat)
            .into_iter()
            .rev()
            .find_map(|event| match &event.kind {
                wire::ChatKind::Status(text) => Some(text.clone()),
                _ => None,
            });
        wire::AgentInfo {
            id,
            place,
            role,
            parent,
            user_owned,
            model,
            effort,
            archived,
            status,
            running_since: None,
            chat,
        }
    }

    async fn start(self: &Arc<Self>, info: wire::AgentInfo) -> anyhow::Result<()> {
        let id = info.id;
        let weak = Arc::downgrade(self);
        // The worker can publish before its startup acknowledgement. Queue
        // those events until the record exists, then replay under this lock:
        // later events cannot overtake earlier sequence numbers.
        let pending: Arc<Mutex<Option<Vec<ChatWorkerEvent>>>> =
            Arc::new(Mutex::new(Some(Vec::new())));
        let observer_pending = pending.clone();
        let observer = Arc::new(move |event| {
            let mut queue = observer_pending.lock().unwrap();
            if let Some(queue) = queue.as_mut() {
                queue.push(event);
            } else {
                drop(queue);
                if let Some(manager) = weak.upgrade() {
                    manager.on_worker_event(id, event);
                }
            }
        });
        // The process enters the workset namespace before its notebook starts.
        let remote = ChatRemote::start(
            &self.pool,
            &info.place,
            id,
            info.model.clone(),
            info.effort.to_string(),
            info.role,
            info.parent,
            info.user_owned,
            observer,
        )
        .await?;
        self.records.lock().unwrap().insert(
            id,
            Record {
                info,
                remote: Arc::new(remote),
            },
        );
        let mut guard = pending.lock().unwrap();
        for event in guard.take().expect("pending until agent is registered") {
            self.on_worker_event(id, event);
        }
        Ok(())
    }

    fn on_worker_event(&self, id: wire::AgentId, event: ChatWorkerEvent) {
        match event {
            ChatWorkerEvent::Chat(event) => {
                if let Some(event) = convert_chat(event) {
                    self.on_chat(&id, event);
                }
            }
            ChatWorkerEvent::Archived(archived) => self.on_archive(&id, archived),
            ChatWorkerEvent::RunningSince(since) => self.on_running(&id, since),
            ChatWorkerEvent::Stopped(Some(error)) => {
                tracing::error!(agent_id = %id.encoded(), %error, "agent runtime stopped");
            }
            ChatWorkerEvent::Stopped(None) => {}
        }
    }

    fn on_chat(&self, id: &wire::AgentId, event: wire::ChatEvent) {
        {
            let mut records = self.records.lock().unwrap();
            let Some(record) = records.get_mut(id) else {
                return;
            };
            if record
                .info
                .chat
                .last()
                .is_some_and(|last| last.seq >= event.seq)
            {
                return;
            }
            record.info.chat.push(event.clone());
            if matches!(
                event.kind,
                wire::ChatKind::Status(_) | wire::ChatKind::Rewound { .. }
            ) {
                record.info.status = wire::visible_chat(&record.info.chat)
                    .into_iter()
                    .rev()
                    .find_map(|event| match &event.kind {
                        wire::ChatKind::Status(text) => Some(text.clone()),
                        _ => None,
                    });
            }
        }
        let _ = self.changes.send(wire::ServerFrame::Chat {
            agent_id: *id,
            event: event.clone(),
        });
        if let wire::ChatKind::Message {
            from: wire::Party::Agent(sender),
            to: wire::Party::Agent(recipient),
            text,
            ..
        } = event.kind
            && sender == *id
            && recipient != *id
        {
            let remote = self
                .records
                .lock()
                .unwrap()
                .get(&recipient)
                .map(|record| record.remote.clone());
            if let Some(remote) = remote {
                let _ = remote.send(log::Party::Agent(sender), text);
            }
        }
    }

    fn on_archive(&self, id: &wire::AgentId, archived: bool) {
        if let Some(record) = self.records.lock().unwrap().get_mut(id) {
            record.info.archived = archived;
            let _ = self.changes.send(wire::ServerFrame::Archived {
                agent_id: *id,
                archived,
            });
        }
    }

    fn on_running(&self, id: &wire::AgentId, since: Option<rho_agent_types::UnixMs>) {
        if let Some(record) = self.records.lock().unwrap().get_mut(id)
            && record.info.running_since != since
        {
            record.info.running_since = since;
            let _ = self.changes.send(wire::ServerFrame::RunningSince {
                agent_id: *id,
                since,
            });
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<wire::AgentInfo> {
        let mut agents: Vec<_> = self
            .records
            .lock()
            .unwrap()
            .values()
            .map(|record| record.info.clone())
            .collect();
        agents.sort_by_key(|agent| agent.id);
        agents
    }
    fn subscribe(&self) -> broadcast::Receiver<wire::ServerFrame> {
        self.changes.subscribe()
    }

    async fn resolve_place(
        &self,
        start: wire::StartMode,
        mode: WorksetMode,
    ) -> anyhow::Result<Place> {
        match start {
            wire::StartMode::NewOn { repo, revset } => {
                let origin = expand_home(&repo).unwrap_or(repo);
                let name = rho_fs_view::repo_name(origin.as_str())
                    .with_context(|| format!("no repository name in {origin}"))?;
                let workset = self.pool.worksets().create().await?;
                let checkout = workset.clone_repo(origin.as_str(), Some(&name)).await?;
                workset.checkout(&checkout, &revset).await?;
                Ok(Place {
                    workset: workset.id().to_owned(),
                    cwd: visible_path(&workset, &checkout)?,
                    mode,
                    origin: Some(origin),
                })
            }
            wire::StartMode::Join(wire::JoinTarget::Workspace(WorkspaceInfo::Workset(
                mut place,
            ))) => {
                place.mode = mode;
                let workset = self.pool.worksets().open_workset(&place.workset).await?;
                anyhow::ensure!(
                    workset.host_path(&place.cwd)?.is_dir(),
                    "workset working directory is missing"
                );
                Ok(place)
            }
            wire::StartMode::Join(_) => anyhow::bail!(
                "agents no longer work in the user's own checkout: start on the repository's URL or path instead"
            ),
        }
    }

    async fn create(self: &Arc<Self>, call: wire::CreateAgent) -> anyhow::Result<wire::AgentId> {
        self.create_with_parent(call, None, false).await
    }

    async fn create_with_parent(
        self: &Arc<Self>,
        call: wire::CreateAgent,
        parent: Option<wire::AgentId>,
        user_owned: bool,
    ) -> anyhow::Result<wire::AgentId> {
        anyhow::ensure!(!call.model.trim().is_empty(), "model is empty");
        let place = self.resolve_place(call.start, call.mode).await?;
        let id = {
            let mut identity = self.identity.lock().unwrap();
            let id = wire::AgentId::from_counter(
                identity.next_counter,
                &AgentIdDomain(identity.machine_seed),
            )
            .ok_or_else(|| anyhow!("agent ID counter exhausted"))?;
            identity.next_counter += 1;
            std::fs::write(
                self.root.join("identity.json"),
                serde_json::to_vec(&*identity)?,
            )?;
            id
        };
        let dir = self.root.join(id.encoded());
        std::fs::create_dir(&dir)?;
        let stored = Stored {
            place: place.clone(),
            role: call.role,
            model: call.model.clone(),
            effort: call.effort.to_string(),
            parent,
            user_owned,
        };
        std::fs::write(dir.join("config.senax"), senax_encoder::pack(&stored)?)?;
        let workset = self.pool.worksets().open_workset(&place.workset).await?;
        let log_path = Self::log_path(&workset, &id)?;
        let info = Self::info_from_log(
            id,
            place,
            call.role,
            parent,
            user_owned,
            call.model,
            call.effort,
            log::Log::open(log_path.as_std_path())?,
        );
        self.start(info).await?;
        let _ = self.changes.send(wire::ServerFrame::Created {
            agent: self
                .records
                .lock()
                .unwrap()
                .get(&id)
                .expect("just started")
                .info
                .clone(),
        });
        if let Some(text) = call.initial_message {
            self.send(wire::SendMessage { agent_id: id, text })?;
        }
        Ok(id)
    }

    async fn tool_call(
        self: &Arc<Self>,
        source: wire::AgentId,
        call: Agent2Call,
    ) -> anyhow::Result<Agent2Reply> {
        let origin = self
            .records
            .lock()
            .unwrap()
            .get(&source)
            .map(|record| record.info.clone())
            .ok_or_else(|| anyhow!("agent {} not found", source.encoded()))?;
        anyhow::ensure!(
            call.allowed(origin.role),
            "tool not available to this agent role"
        );
        let text = match call {
            Agent2Call::SpawnEngineer {
                task_name,
                prompt,
                workdir,
            } => {
                let role = match origin.role {
                    AgentRole::Engineer {
                        intelligence: rho_agent_types::EngineerIntelligence::Mini,
                    } => origin.role,
                    _ => AgentRole::default(),
                };
                self.spawn_child(&origin, task_name, prompt, workdir, role, false)
                    .await?
            }
            Agent2Call::SpawnUserOwnedEngineer {
                task_name,
                prompt,
                workdir,
            } => {
                anyhow::ensure!(
                    origin.parent.is_none() || origin.user_owned,
                    "only a user-managed agent can spawn a user-owned Engineer"
                );
                self.spawn_child(
                    &origin,
                    task_name,
                    prompt,
                    workdir,
                    AgentRole::default(),
                    true,
                )
                .await?
            }
            Agent2Call::SpawnAdvisor { message } => {
                use rho_agent_types::{AdvisorIntelligence, EngineerIntelligence};
                let intelligence = match origin.role {
                    AgentRole::Engineer {
                        intelligence: EngineerIntelligence::Mini,
                    } => AdvisorIntelligence::Low,
                    AgentRole::Engineer {
                        intelligence: EngineerIntelligence::High,
                    } => AdvisorIntelligence::Medium1,
                    _ => AdvisorIntelligence::Medium,
                };
                self.spawn_child(
                    &origin,
                    "advisor".into(),
                    message,
                    None,
                    AgentRole::Advisor { intelligence },
                    false,
                )
                .await?
            }
            Agent2Call::Message { agent_id, message } => {
                anyhow::ensure!(agent_id != source, "cannot send a message to yourself");
                anyhow::ensure!(!message.trim().is_empty(), "message is empty");
                let remote = {
                    let records = self.records.lock().unwrap();
                    anyhow::ensure!(
                        records.contains_key(&agent_id),
                        "agent {} not found",
                        agent_id.encoded()
                    );
                    records
                        .get(&source)
                        .expect("validated source")
                        .remote
                        .clone()
                };
                remote.send_to(log::Party::Agent(agent_id), message)?;
                format!("Message sent to {}.", agent_id.encoded())
            }
            Agent2Call::Cancel { agent_id } => {
                anyhow::ensure!(agent_id != source, "cannot interrupt yourself");
                let remote = {
                    let records = self.records.lock().unwrap();
                    let target = records
                        .get(&agent_id)
                        .ok_or_else(|| anyhow!("agent {} not found", agent_id.encoded()))?;
                    anyhow::ensure!(
                        target.info.parent == Some(source) && !target.info.user_owned,
                        "only the managing agent can interrupt this Engineer"
                    );
                    anyhow::ensure!(target.info.role.is_engineer(), "target is not an Engineer");
                    target.remote.clone()
                };
                remote.cancel()?;
                format!(
                    "Engineer {} interrupted; it remains available for follow-up.",
                    agent_id.encoded()
                )
            }
            Agent2Call::Team => {
                let records = self.records.lock().unwrap();
                let mut team = records
                    .values()
                    .filter(|record| record.info.parent == Some(source))
                    .map(|record| {
                        format!(
                            "{} · {} · {}",
                            record.info.id.encoded(),
                            if record.info.user_owned {
                                "user-owned"
                            } else {
                                "delegated"
                            },
                            record.info.status.as_deref().unwrap_or("idle")
                        )
                    })
                    .collect::<Vec<_>>();
                team.sort();
                let parent = origin
                    .parent
                    .map_or_else(|| "the human".to_owned(), |id| id.encoded());
                format!(
                    "You are {}. Parent: {}.\n{}",
                    source.encoded(),
                    parent,
                    if team.is_empty() {
                        "No agents in your team.".to_owned()
                    } else {
                        team.join("\n")
                    }
                )
            }
        };
        Ok(Agent2Reply { text })
    }

    async fn spawn_child(
        self: &Arc<Self>,
        origin: &wire::AgentInfo,
        task_name: String,
        prompt: String,
        workdir: Option<String>,
        role: AgentRole,
        user_owned: bool,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(!task_name.trim().is_empty(), "task name is empty");
        anyhow::ensure!(!prompt.trim().is_empty(), "prompt is empty");
        let mut place = origin.place.clone();
        if let Some(workdir) = workdir {
            let requested = camino::Utf8Path::new(&workdir);
            place.cwd = if requested.is_absolute() {
                requested.to_owned()
            } else {
                place.cwd.join(requested)
            };
            let workset = self.pool.worksets().open_workset(&place.workset).await?;
            anyhow::ensure!(
                workset.host_path(&place.cwd)?.is_dir(),
                "subagent working directory does not exist: {}",
                place.cwd
            );
        }
        let id = self
            .create_with_parent(
                wire::CreateAgent {
                    start: wire::StartMode::Join(wire::JoinTarget::Workspace(
                        WorkspaceInfo::Workset(place.clone()),
                    )),
                    mode: place.mode,
                    role,
                    model: origin.model.clone(),
                    effort: origin.effort,
                    initial_message: Some(prompt),
                },
                Some(origin.id),
                user_owned,
            )
            .await?;
        Ok(format!(
            "Spawned {} for task \"{}\" in {}. {}",
            id.encoded(),
            task_name,
            place.cwd,
            if user_owned {
                "It reports to the user."
            } else {
                "It reports to you by message."
            }
        ))
    }

    fn send(&self, call: wire::SendMessage) -> anyhow::Result<()> {
        anyhow::ensure!(!call.text.trim().is_empty(), "message is empty");
        let remote = self
            .records
            .lock()
            .unwrap()
            .get(&call.agent_id)
            .map(|record| record.remote.clone())
            .ok_or_else(|| anyhow!("agent {} not found", call.agent_id.encoded()))?;
        remote.send(log::Party::Human, call.text)
    }
    async fn rewind(&self, call: wire::RewindAgent) -> anyhow::Result<()> {
        let remote = self
            .records
            .lock()
            .unwrap()
            .get(&call.agent_id)
            .map(|record| record.remote.clone())
            .ok_or_else(|| anyhow!("agent {} not found", call.agent_id.encoded()))?;
        remote.rewind(call.turns).await
    }

    fn archive(&self, call: wire::ArchiveAgent) -> anyhow::Result<()> {
        let remote = self
            .records
            .lock()
            .unwrap()
            .get(&call.agent_id)
            .map(|record| record.remote.clone())
            .ok_or_else(|| anyhow!("agent {} not found", call.agent_id.encoded()))?;
        remote.archive()
    }
}

fn convert_party(party: log::Party) -> Option<wire::Party> {
    match party {
        log::Party::Human => Some(wire::Party::Human),
        log::Party::Agent(id) => Some(wire::Party::Agent(id)),
    }
}
fn convert_chat(event: chat2::ChatEvent) -> Option<wire::ChatEvent> {
    let kind = match event.kind {
        chat2::ChatKind::Message { id, from, to, body } => wire::ChatKind::Message {
            id: wire::MessageId(id.0),
            from: convert_party(from)?,
            to: convert_party(to)?,
            text: body
                .into_iter()
                .filter_map(|block| match block {
                    Block::Text(text) => Some(text),
                    Block::Quote { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        chat2::ChatKind::Status(text) => wire::ChatKind::Status(text),
        chat2::ChatKind::Rewound { to } => wire::ChatKind::Rewound { to },
    };
    Some(wire::ChatEvent {
        seq: event.seq,
        at: event.at,
        kind,
    })
}

pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    open: wire::Open,
    mut reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let manager = &services.agents2;
    match open {
        wire::Open::Session => {
            let mut changes = manager.subscribe();
            let snapshot = manager.snapshot();
            let mut seen: HashMap<_, _> = snapshot
                .iter()
                .map(|agent| {
                    (
                        agent.id.clone(),
                        (
                            agent.chat.last().map_or(0, |event| event.seq + 1),
                            agent.archived,
                        ),
                    )
                })
                .collect();
            write_frame(
                &mut writer,
                &wire::ServerFrame::Snapshot { agents: snapshot },
            )
            .await?;
            loop {
                let change = tokio::select! {
                    change = changes.recv() => change,
                    input = reader.read_u8() => match input {
                        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
                        Err(error) => break Err(error.into()),
                        Ok(_) => anyhow::bail!("unexpected agent2 session input"),
                    }
                };
                match change {
                    Ok(wire::ServerFrame::Created { agent }) => {
                        if seen.contains_key(&agent.id) {
                            continue;
                        }
                        seen.insert(
                            agent.id.clone(),
                            (
                                agent.chat.last().map_or(0, |event| event.seq + 1),
                                agent.archived,
                            ),
                        );
                        write_frame(&mut writer, &wire::ServerFrame::Created { agent }).await?;
                    }
                    Ok(wire::ServerFrame::Chat { agent_id, event }) => {
                        let current = seen.entry(agent_id.clone()).or_insert((0, false));
                        if event.seq < current.0 {
                            continue;
                        }
                        current.0 = event.seq + 1;
                        write_frame(&mut writer, &wire::ServerFrame::Chat { agent_id, event })
                            .await?;
                    }
                    Ok(wire::ServerFrame::Archived { agent_id, archived }) => {
                        let current = seen.entry(agent_id.clone()).or_insert((0, !archived));
                        if current.1 == archived {
                            continue;
                        }
                        current.1 = archived;
                        write_frame(
                            &mut writer,
                            &wire::ServerFrame::Archived { agent_id, archived },
                        )
                        .await?;
                    }
                    Ok(wire::ServerFrame::RunningSince { agent_id, since }) => {
                        if seen.contains_key(&agent_id) {
                            write_frame(
                                &mut writer,
                                &wire::ServerFrame::RunningSince { agent_id, since },
                            )
                            .await?;
                        }
                    }
                    Ok(wire::ServerFrame::Snapshot { .. }) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let agents = manager.snapshot();
                        seen = agents
                            .iter()
                            .map(|agent| {
                                (
                                    agent.id.clone(),
                                    (
                                        agent.chat.last().map_or(0, |event| event.seq + 1),
                                        agent.archived,
                                    ),
                                )
                            })
                            .collect();
                        write_frame(&mut writer, &wire::ServerFrame::Snapshot { agents }).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
        }
        wire::Open::Request(request) => match request {
            wire::Request::CreateAgent(call) => {
                respond(&mut writer, call, |call| manager.create(call)).await
            }
            wire::Request::SendMessage(call) => {
                respond(&mut writer, call, |call| async { manager.send(call) }).await
            }
            wire::Request::ArchiveAgent(call) => {
                respond(&mut writer, call, |call| async { manager.archive(call) }).await
            }
            wire::Request::RewindAgent(call) => {
                respond(&mut writer, call, |call| manager.rewind(call)).await
            }
            wire::Request::ListAgents(call) => {
                respond(&mut writer, call, |_| async { Ok(manager.snapshot()) }).await
            }
        },
    }
}
async fn respond<W, C, F, Fut>(writer: &mut W, call: C, run: F) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    C: rho_rpc::protocol::Call,
    F: FnOnce(C) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<C::Reply>>,
{
    let answer = match run(call).await {
        Ok(reply) => Answer::Done(reply),
        Err(error) => Answer::Failed {
            reason: format!("{error:#}"),
        },
    };
    write_frame(writer, &answer).await
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;
    use rho_inference::Inference;

    use super::*;

    async fn test_manager(root: &std::path::Path) -> Arc<Agents2> {
        let worker = std::env::current_exe()
            .unwrap()
            .ancestors()
            .map(|path| path.join("rho-agent-worker"))
            .chain(std::iter::once(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/rho-agent-worker"),
            ))
            .find(|path| path.is_file())
            .expect("build rho-agent-worker before workset test");
        let bin = worker.parent().unwrap();
        let mut environment = std::env::vars_os()
            .filter(|(name, _)| name != "PATH")
            .collect::<Vec<_>>();
        environment.push((
            "PATH".into(),
            std::env::join_paths(
                std::iter::once(bin.to_owned())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        ));
        let worksets = rho_fs_view::Worksets::open(
            root.join("worksets-state"),
            rho_fs_view::UserEnvironment::new(environment),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap();
        let db = RhoDb::open(root.join("agent.redb"));
        let inference = Inference::new_with_config(
            db.clone(),
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        let pool = AgentPool::new(
            db,
            inference,
            worksets,
            rho_claude::accounts::ClaudePaths::at(
                Utf8PathBuf::try_from(root.join("claude")).unwrap(),
            ),
        )
        .await;
        Agents2::open(
            Utf8PathBuf::try_from(root.join("agents-state")).unwrap(),
            pool,
            13,
            0,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn new_on_checkout_and_join_share_worker_and_persist_place() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("source");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        };
        git(&["init", "--quiet"]);
        std::fs::write(repo.join("selected"), "old").unwrap();
        git(&["add", "selected"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "--quiet",
            "-m",
            "old",
        ]);
        std::fs::write(repo.join("selected"), "new").unwrap();
        git(&["add", "selected"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "--quiet",
            "-m",
            "new",
        ]);
        let manager = test_manager(dir.path()).await;
        let role = AgentRole::default();
        let origin = Utf8PathBuf::try_from(repo).unwrap();
        let first = manager
            .create(wire::CreateAgent {
                start: wire::StartMode::NewOn {
                    repo: origin.clone(),
                    revset: "HEAD~1".into(),
                },
                mode: WorksetMode::View,
                role,
                model: "gpt-6-sol".into(),
                effort: wire::Effort::Medium,
                initial_message: None,
            })
            .await
            .unwrap();
        let place = manager
            .snapshot()
            .into_iter()
            .find(|info| info.id == first)
            .unwrap()
            .place;
        assert_eq!(place.cwd, Utf8PathBuf::from("/src/source"));
        assert_eq!(place.origin.as_ref(), Some(&origin));
        let workset = manager
            .pool
            .worksets()
            .open_workset(&place.workset)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workset.host_path(&place.cwd).unwrap().join("selected"))
                .unwrap(),
            "old"
        );
        let second = manager
            .create(wire::CreateAgent {
                start: wire::StartMode::Join(wire::JoinTarget::Workspace(WorkspaceInfo::Workset(
                    place.clone(),
                ))),
                mode: WorksetMode::View,
                role,
                model: "gpt-6-sol".into(),
                effort: wire::Effort::High,
                initial_message: None,
            })
            .await
            .unwrap();
        let joined = manager
            .snapshot()
            .into_iter()
            .find(|info| info.id == second)
            .unwrap();
        assert_eq!(joined.place, place);
        assert_eq!(joined.role, role);
        manager.on_running(&first, Some(rho_agent_types::UnixMs(1234)));
        assert_eq!(
            manager
                .snapshot()
                .into_iter()
                .find(|info| info.id == first)
                .unwrap()
                .running_since,
            Some(rho_agent_types::UnixMs(1234))
        );
        manager.on_running(&first, None);
        assert_eq!(
            manager
                .snapshot()
                .into_iter()
                .find(|info| info.id == first)
                .unwrap()
                .running_since,
            None
        );
        let child_reply = manager
            .tool_call(
                first,
                Agent2Call::SpawnEngineer {
                    task_name: "review".into(),
                    prompt: "review the change".into(),
                    workdir: None,
                },
            )
            .await
            .unwrap();
        let child = manager
            .snapshot()
            .into_iter()
            .find(|info| info.parent == Some(first) && !info.user_owned)
            .unwrap();
        assert_eq!(child.place, place);
        assert_eq!(child.role, AgentRole::default());
        assert!(child_reply.text.contains(&child.id.encoded()));
        assert!(
            manager
                .tool_call(
                    child.id,
                    Agent2Call::SpawnUserOwnedEngineer {
                        task_name: "escape".into(),
                        prompt: "try".into(),
                        workdir: None,
                    }
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .tool_call(first, Agent2Call::Cancel { agent_id: second })
                .await
                .is_err(),
            "a peer cannot interrupt an agent it did not manage"
        );
        assert!(
            manager
                .tool_call(
                    first,
                    Agent2Call::SpawnEngineer {
                        task_name: "bad path".into(),
                        prompt: "try".into(),
                        workdir: Some("../../outside".into()),
                    }
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .tool_call(first, Agent2Call::Team)
                .await
                .unwrap()
                .text
                .contains(&child.id.encoded())
        );
        manager
            .tool_call(first, Agent2Call::Cancel { agent_id: child.id })
            .await
            .unwrap();
        manager
            .tool_call(
                first,
                Agent2Call::Message {
                    agent_id: child.id,
                    message: "follow up".into(),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let agents = manager.snapshot();
                let source = agents.iter().find(|info| info.id == first).unwrap();
                let recipient = agents.iter().find(|info| info.id == child.id).unwrap();
                let sent = source
                    .chat
                    .iter()
                    .filter(|event| {
                        matches!(&event.kind,
                    wire::ChatKind::Message { from: wire::Party::Agent(sender),
                    to: wire::Party::Agent(target), text, .. }
                    if *sender == first && *target == child.id && text == "follow up")
                    })
                    .count();
                let received = recipient
                    .chat
                    .iter()
                    .filter(|event| {
                        matches!(&event.kind,
                    wire::ChatKind::Message { from: wire::Party::Agent(sender),
                    to: wire::Party::Agent(target), text, .. }
                    if *sender == first && *target == child.id && text == "follow up")
                    })
                    .count();
                if sent == 1 && received == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        manager
            .archive(wire::ArchiveAgent { agent_id: second })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager
                .snapshot()
                .into_iter()
                .find(|info| info.id == second)
                .unwrap()
                .archived
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let first_remote = manager
            .records
            .lock()
            .unwrap()
            .get(&first)
            .unwrap()
            .remote
            .clone();
        first_remote
            .send(log::Party::Agent(second), "mail inside workset".into())
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager
                .snapshot()
                .into_iter()
                .find(|info| info.id == first)
                .unwrap()
                .chat
                .iter()
                .any(|event| {
                    matches!(&event.kind, wire::ChatKind::Message {
                    from: wire::Party::Agent(sender),
                    to: wire::Party::Agent(recipient), text, ..
                } if *sender == second && *recipient == first && text == "mail inside workset")
                })
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        manager
            .send(wire::SendMessage {
                agent_id: first,
                text: "abandoned human prompt".into(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager.snapshot().into_iter().find(|info| info.id == first).unwrap().chat.iter().any(|event| {
                matches!(&event.kind, wire::ChatKind::Message { from: wire::Party::Human, text, .. } if text == "abandoned human prompt")
            }) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        manager
            .rewind(wire::RewindAgent {
                agent_id: first,
                turns: 1,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager
                .snapshot()
                .into_iter()
                .find(|info| info.id == first)
                .unwrap()
                .chat
                .iter()
                .any(|event| matches!(event.kind, wire::ChatKind::Rewound { .. }))
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let first_chat = manager
            .snapshot()
            .into_iter()
            .find(|info| info.id == first)
            .unwrap()
            .chat;
        assert!(first_chat.iter().any(|event| matches!(&event.kind, wire::ChatKind::Message { text, .. } if text == "abandoned human prompt")));
        assert!(!wire::visible_chat(&first_chat).iter().any(|event| matches!(&event.kind, wire::ChatKind::Message { text, .. } if text == "abandoned human prompt")));
        let remotes: Vec<_> = manager
            .records
            .lock()
            .unwrap()
            .values()
            .map(|record| record.remote.clone())
            .collect();
        for remote in remotes {
            remote.shutdown().await;
        }
        let pool = manager.pool.clone();
        drop(manager);
        let reloaded = Agents2::open(
            Utf8PathBuf::try_from(dir.path().join("agents-state")).unwrap(),
            pool,
            13,
            0,
        )
        .await
        .unwrap();
        let revived = reloaded
            .snapshot()
            .into_iter()
            .find(|info| info.id == second)
            .unwrap();
        assert_eq!(revived.place, place);
        assert!(revived.archived);
        let revived_child = reloaded
            .snapshot()
            .into_iter()
            .find(|info| info.id == child.id)
            .unwrap();
        assert_eq!(revived_child.parent, Some(first));
        assert!(!revived_child.user_owned);
        assert!(reloaded.snapshot().into_iter().find(|info| info.id == first).unwrap().chat.iter().any(|event| {
            matches!(&event.kind, wire::ChatKind::Message { text, .. } if text == "mail inside workset")
        }));
    }
}
