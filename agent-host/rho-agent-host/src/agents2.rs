//! Agent2 chat service. Its per-agent host log is primary; sessions receive
//! only its chat projection. Notebook reports never cross this protocol.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use camino::Utf8PathBuf;
use rho_agent_types::{AgentIdDomain, AgentRole, Place, WorksetMode, WorkspaceInfo};
use rho_agent2::agent::{Agent, AgentHandle, Config, Inbound, Trace};
use rho_agent2::chat as chat2;
use rho_agent2::log::{self, Block, Entry, Notice};
use rho_agents2_client::protocol as wire;
use rho_fs_view::Worksets;
use rho_inference2::Model;
use rho_inference2::openai::{Effort as ModelEffort, OpenAi};
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
}
struct Record {
    info: wire::AgentInfo,
    handle: AgentHandle,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    machine_seed: u64,
    next_counter: u64,
}

pub(crate) struct Agents2 {
    root: Utf8PathBuf,
    worksets: Arc<Worksets>,
    identity: Mutex<Identity>,
    records: Mutex<HashMap<wire::AgentId, Record>>,
    changes: broadcast::Sender<wire::ServerFrame>,
    make_model: Arc<dyn Fn(&wire::AgentInfo) -> Arc<Model> + Send + Sync>,
}

impl Agents2 {
    async fn open(
        root: Utf8PathBuf,
        worksets: Arc<Worksets>,
        machine_seed: u64,
        initial_counter: u64,
        make_model: impl Fn(&wire::AgentInfo) -> Arc<Model> + Send + Sync + 'static,
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
            worksets,
            identity: Mutex::new(identity),
            records: Mutex::new(HashMap::new()),
            changes: broadcast::channel(1024).0,
            make_model: Arc::new(make_model),
        });
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
                    tracing::warn!(path = %metadata.display(), "skipping agent2 without valid metadata");
                    continue;
                }
            };
            let effort: wire::Effort = stored.effort.parse().map_err(anyhow::Error::msg)?;
            let log = log::Log::open(&manager.log_path(&id))?;
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
            let info = wire::AgentInfo {
                id,
                place: stored.place,
                role: stored.role,
                model: stored.model,
                effort,
                archived,
                status: None,
                chat: Vec::new(),
            };
            let directory = manager.shell_directory(&info.place).await?;
            manager.start(info, log, directory)?;
        }
        Ok(manager)
    }

    pub(crate) async fn live(
        root: Utf8PathBuf,
        worksets: Arc<Worksets>,
        base_url: String,
        machine_seed: u64,
        initial_counter: u64,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open(root, worksets, machine_seed, initial_counter, move |info| {
            Arc::new(Model::OpenAi(OpenAi {
                base_url: base_url.clone(),
                model: info.model.clone(),
                effort: match info.effort {
                    wire::Effort::Low => ModelEffort::Low,
                    wire::Effort::Medium => ModelEffort::Medium,
                    wire::Effort::High => ModelEffort::High,
                    wire::Effort::XHigh => ModelEffort::XHigh,
                },
                auth: "default".into(),
            }))
        })
        .await
    }

    async fn shell_directory(&self, place: &Place) -> anyhow::Result<Utf8PathBuf> {
        let workset = self.worksets.open_workset(&place.workset).await?;
        let directory = workset.host_path(&place.cwd)?;
        anyhow::ensure!(
            directory.is_dir(),
            "workset working directory does not exist: {directory}"
        );
        Ok(directory)
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
                let workset = self.worksets.create().await?;
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
                self.shell_directory(&place).await?;
                Ok(place)
            }
            wire::StartMode::Join(_) => anyhow::bail!(
                "agents no longer work in the user's own checkout: start on the repository's URL or path instead"
            ),
        }
    }

    fn log_path(&self, id: &wire::AgentId) -> PathBuf {
        self.root.join(id.encoded()).join("log").into_std_path_buf()
    }

    fn start(
        self: &Arc<Self>,
        mut info: wire::AgentInfo,
        log: log::Log,
        directory: Utf8PathBuf,
    ) -> anyhow::Result<()> {
        let id = info.id.clone();
        // TODO: Start the notebook inside a workset child process before using the
        // namespace view. The in-process runtime cannot enter `/src` safely.
        let shell = rho_tool_shell::ShellTools::in_directory(
            Duration::from_secs(20),
            directory,
            self.worksets.path_overrides().clone(),
        );
        let (agent, handle) = Agent::new(Config {
            id,
            log,
            model: (self.make_model)(&info),
            shell,
            instructions: rho_agent2::prompt::INSTRUCTIONS.into(),
        })?;
        info.chat = agent.chat().into_iter().filter_map(convert_chat).collect();
        info.status = info.chat.iter().rev().find_map(|event| match &event.kind {
            wire::ChatKind::Status(text) => Some(text.clone()),
            _ => None,
        });
        let mut chat = handle.chat();
        let mut trace = handle.trace();
        self.records
            .lock()
            .unwrap()
            .insert(id.clone(), Record { info, handle });
        let weak = Arc::downgrade(self);
        let observer_id = id.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    received = chat.recv() => match received {
                        Ok(event) => {
                            if let Some(manager) = weak.upgrade() && let Some(event) = convert_chat(event) {
                                manager.on_chat(&observer_id, event);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            tracing::warn!(agent_id = %observer_id.encoded(), "agent2 chat observer lagged; reread on host restart");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    received = trace.recv() => match received {
                        Ok(Trace::ArchiveState { archived }) => {
                            if let Some(manager) = weak.upgrade() { manager.on_archive(&observer_id, archived); }
                        }
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
        tokio::spawn(async move {
            if let Err(error) = agent.run().await {
                tracing::error!(agent_id = %id.encoded(), %error, "agent2 runtime exited");
            }
        });
        Ok(())
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
            if let wire::ChatKind::Status(text) = &event.kind {
                record.info.status = Some(text.clone());
            }
            record.info.chat.push(event.clone());
        }
        let _ = self.changes.send(wire::ServerFrame::Chat {
            agent_id: id.clone(),
            event: event.clone(),
        });
        // The sender's logged mail is delivered to the recipient's own log.
        if let wire::ChatKind::Message {
            from: wire::Party::Agent(sender),
            to: wire::Party::Agent(recipient),
            text,
            ..
        } = event.kind
            && sender == *id
            && recipient != *id
        {
            let recipient_handle = self
                .records
                .lock()
                .unwrap()
                .get(&recipient)
                .map(|record| record.handle.clone());
            if let Some(handle) = recipient_handle {
                let _ = handle.send(Inbound {
                    from: log::Party::Agent(sender),
                    body: vec![Block::Text(text)],
                });
            }
        }
    }

    fn on_archive(&self, id: &wire::AgentId, archived: bool) {
        if let Some(record) = self.records.lock().unwrap().get_mut(id) {
            record.info.archived = archived;
            let _ = self.changes.send(wire::ServerFrame::Archived {
                agent_id: id.clone(),
                archived,
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

    async fn create(self: &Arc<Self>, call: wire::CreateAgent) -> anyhow::Result<wire::AgentId> {
        anyhow::ensure!(!call.model.trim().is_empty(), "model is empty");
        let place = self.resolve_place(call.start, call.mode).await?;
        let directory = self.shell_directory(&place).await?;
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
        };
        std::fs::write(dir.join("config.senax"), senax_encoder::pack(&stored)?)?;
        let info = wire::AgentInfo {
            id: id.clone(),
            place,
            role: call.role,
            model: call.model,
            effort: call.effort,
            archived: false,
            status: None,
            chat: Vec::new(),
        };
        self.start(info, log::Log::open(&self.log_path(&id))?, directory)?;
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
            self.send(wire::SendMessage {
                agent_id: id.clone(),
                text,
            })?;
        }
        Ok(id)
    }

    fn send(&self, call: wire::SendMessage) -> anyhow::Result<()> {
        anyhow::ensure!(!call.text.trim().is_empty(), "message is empty");
        let handle = self
            .records
            .lock()
            .unwrap()
            .get(&call.agent_id)
            .map(|record| record.handle.clone())
            .ok_or_else(|| anyhow!("agent2 {} not found", call.agent_id.encoded()))?;
        handle.send(Inbound {
            from: log::Party::Human,
            body: vec![Block::Text(call.text)],
        })
    }
    fn archive(&self, call: wire::ArchiveAgent) -> anyhow::Result<()> {
        let handle = self
            .records
            .lock()
            .unwrap()
            .get(&call.agent_id)
            .map(|record| record.handle.clone())
            .ok_or_else(|| anyhow!("agent2 {} not found", call.agent_id.encoded()))?;
        handle.archive();
        Ok(())
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
    use rho_inference2::scripted::Scripted;

    use super::*;

    async fn test_worksets(dir: &std::path::Path) -> Arc<Worksets> {
        Worksets::open(
            dir.join("worksets-state"),
            rho_fs_view::UserEnvironment::new(std::env::vars_os().collect()),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap()
    }

    async fn test_place(worksets: &Arc<Worksets>) -> Place {
        let workset = worksets.create().await.unwrap();
        Place {
            workset: workset.id().to_owned(),
            cwd: "/src".into(),
            mode: WorksetMode::View,
            origin: None,
        }
    }

    fn new_agent(
        place: Place,
        effort: wire::Effort,
        initial_message: Option<&str>,
    ) -> wire::CreateAgent {
        wire::CreateAgent {
            start: wire::StartMode::Join(wire::JoinTarget::Workspace(WorkspaceInfo::Workset(
                place,
            ))),
            mode: WorksetMode::View,
            role: AgentRole::default(),
            model: "scripted".into(),
            effort,
            initial_message: initial_message.map(str::to_owned),
        }
    }

    async fn until(mut good: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(12), async {
            while !good() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("agent2 update arrived");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_on_checks_out_selected_revision_and_join_reuses_place() {
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
        std::fs::write(repo.join("selection"), "base").unwrap();
        git(&["add", "selection"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "--quiet",
            "-m",
            "base",
        ]);
        std::fs::write(repo.join("selection"), "newer").unwrap();
        git(&["add", "selection"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "--quiet",
            "-m",
            "newer",
        ]);

        let root = Utf8PathBuf::try_from(dir.path().join("state")).unwrap();
        let worksets = test_worksets(dir.path()).await;
        let model = Arc::new(Scripted::new());
        let manager = Agents2::open(root.clone(), worksets.clone(), 3, 0, {
            let model = model.clone();
            move |_| Arc::new(Model::Scripted(model.clone()))
        })
        .await
        .unwrap();
        let origin = Utf8PathBuf::try_from(repo).unwrap();
        let role = AgentRole::default();
        let first = manager
            .create(wire::CreateAgent {
                start: wire::StartMode::NewOn {
                    repo: origin.clone(),
                    revset: "HEAD~1".into(),
                },
                mode: WorksetMode::View,
                role,
                model: "scripted".into(),
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
        let workset = worksets.open_workset(&place.workset).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workset.host_path(&place.cwd).unwrap().join("selection"))
                .unwrap(),
            "base"
        );
        let second = manager
            .create(wire::CreateAgent {
                start: wire::StartMode::Join(wire::JoinTarget::Workspace(WorkspaceInfo::Workset(
                    place.clone(),
                ))),
                mode: WorksetMode::Exposed,
                role,
                model: "scripted".into(),
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
        assert_eq!(joined.place.workset, place.workset);
        assert_eq!(joined.place.cwd, place.cwd);
        assert_eq!(joined.place.mode, WorksetMode::Exposed);
        assert_eq!(joined.role, role);
        drop(manager);
        let reloaded = Agents2::open(root, worksets, 3, 0, move |_| {
            Arc::new(Model::Scripted(model.clone()))
        })
        .await
        .unwrap();
        assert_eq!(
            reloaded
                .snapshot()
                .into_iter()
                .find(|info| info.id == second)
                .unwrap()
                .place,
            joined.place
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chat_archive_revival_and_host_restart_use_the_persistent_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().join("state")).unwrap();
        let worksets = test_worksets(dir.path()).await;
        let place = test_place(&worksets).await;
        let script = Arc::new(Scripted::new());
        script
            .then("human.status('ready')\nhuman.send('hello')\nawait human.reply()")
            .then("human.send('fresh')\nawait human.reply()");
        let make_model = {
            let script = script.clone();
            move |_: &wire::AgentInfo| Arc::new(Model::Scripted(script.clone()))
        };
        let manager = Agents2::open(root.clone(), worksets.clone(), 0, 0, make_model)
            .await
            .unwrap();
        let id = manager
            .create(new_agent(place, wire::Effort::High, Some("start")))
            .await
            .unwrap();
        until(|| {
            manager.snapshot()[0].chat.iter().any(|event| {
                matches!(&event.kind,
            wire::ChatKind::Message { text, from: wire::Party::Agent(_), .. } if text == "hello")
            })
        })
        .await;
        assert_eq!(manager.snapshot()[0].status.as_deref(), Some("ready"));
        manager
            .archive(wire::ArchiveAgent {
                agent_id: id.clone(),
            })
            .unwrap();
        until(|| manager.snapshot()[0].archived).await;
        manager
            .send(wire::SendMessage {
                agent_id: id.clone(),
                text: "return".into(),
            })
            .unwrap();
        until(|| {
            manager.snapshot()[0].chat.iter().any(|event| {
                matches!(&event.kind,
            wire::ChatKind::Message { text, from: wire::Party::Agent(_), .. } if text == "fresh")
            })
        })
        .await;
        assert!(!manager.snapshot()[0].archived);
        let before = manager.snapshot()[0].chat.clone();
        drop(manager);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let restart_script = Arc::new(Scripted::new());
        restart_script.then("human.send('restarted')\nawait human.reply()");
        let reloaded = Agents2::open(root, worksets, 0, 0, move |_: &wire::AgentInfo| {
            Arc::new(Model::Scripted(restart_script.clone()))
        })
        .await
        .unwrap();
        assert_eq!(reloaded.snapshot()[0].chat, before);
        until(|| reloaded.snapshot()[0].chat.iter().any(|event| matches!(&event.kind,
            wire::ChatKind::Message { text, from: wire::Party::Agent(_), .. } if text == "restarted"))).await;
        assert_eq!(reloaded.snapshot()[0].id, id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ids_continue_after_restart_in_the_host_machine_domain() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().join("state")).unwrap();
        let worksets = test_worksets(dir.path()).await;
        let place = test_place(&worksets).await;
        let model = Arc::new(Scripted::new());
        let manager = Agents2::open(root.clone(), worksets.clone(), 77, 19, {
            let model = model.clone();
            move |_| Arc::new(Model::Scripted(model.clone()))
        })
        .await
        .unwrap();
        let new_agent = || new_agent(place.clone(), wire::Effort::Medium, None);
        let first = manager.create(new_agent()).await.unwrap();
        assert_eq!(first.to_counter(&AgentIdDomain(77)), 20);
        drop(manager);
        let restarted = Agents2::open(root.clone(), worksets.clone(), 77, 19, move |_| {
            Arc::new(Model::Scripted(model.clone()))
        })
        .await
        .unwrap();
        let second = restarted.create(new_agent()).await.unwrap();
        assert_eq!(second.to_counter(&AgentIdDomain(77)), 21);
        assert_ne!(second, first);
        assert!(
            Agents2::open(root, worksets, 78, 19, |_| Arc::new(Model::Scripted(
                Arc::new(Scripted::new())
            )))
            .await
            .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_mail_routes_into_recipient_chat_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().join("state")).unwrap();
        let worksets = test_worksets(dir.path()).await;
        let place = test_place(&worksets).await;
        let script = Arc::new(Scripted::new());
        let manager = Agents2::open(root, worksets, 0, 0, {
            let script = script.clone();
            move |_: &wire::AgentInfo| Arc::new(Model::Scripted(script.clone()))
        })
        .await
        .unwrap();
        let recipient = manager
            .create(new_agent(place.clone(), wire::Effort::Low, None))
            .await
            .unwrap();
        let sender = manager
            .create(new_agent(place, wire::Effort::High, None))
            .await
            .unwrap();
        script
            .then(&format!(
                "agents.send('{}', 'ping')\nawait human.reply()",
                recipient.encoded()
            ))
            .then("human.send('mail delivered')\nawait human.reply()");
        manager
            .send(wire::SendMessage {
                agent_id: sender,
                text: "send mail".into(),
            })
            .unwrap();
        until(|| {
            manager
                .snapshot()
                .iter()
                .find(|agent| agent.id == recipient)
                .is_some_and(|agent| {
                    agent.chat.iter().any(|event| {
                        matches!(&event.kind,
                wire::ChatKind::Message { text, from: wire::Party::Agent(_), .. } if text == "ping")
                    })
                })
        })
        .await;
    }
}
