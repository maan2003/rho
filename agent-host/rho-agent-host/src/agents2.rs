//! Agent2 chat service. Its per-agent host log is primary; sessions receive
//! only its chat projection. Notebook reports never cross this protocol.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use camino::Utf8PathBuf;
use rho_agent2::agent::{Agent, AgentHandle, Config, Inbound, Trace};
use rho_agent2::chat as chat2;
use rho_agent2::log::{self, Block, Entry, Notice};
use rho_agents2_client::protocol as wire;
use rho_inference2::Model;
use rho_inference2::openai::{Effort as ModelEffort, OpenAi};
use rho_rpc::protocol::{Answer, write_frame};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::io::AsyncReadExt as _;

use crate::Services;

#[derive(Serialize, Deserialize)]
struct Stored {
    workdir: String,
    model: String,
    effort: String,
}
struct Record {
    info: wire::AgentInfo,
    handle: AgentHandle,
}

pub(crate) struct Agents2 {
    root: Utf8PathBuf,
    records: Mutex<HashMap<wire::AgentId, Record>>,
    changes: broadcast::Sender<wire::ServerFrame>,
    make_model: Arc<dyn Fn(&wire::AgentInfo) -> Arc<Model> + Send + Sync>,
}

impl Agents2 {
    fn open(
        root: Utf8PathBuf,
        make_model: impl Fn(&wire::AgentInfo) -> Arc<Model> + Send + Sync + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        let manager = Arc::new(Self {
            root,
            records: Mutex::new(HashMap::new()),
            changes: broadcast::channel(1024).0,
            make_model: Arc::new(make_model),
        });
        for entry in std::fs::read_dir(&manager.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = wire::AgentId::new(entry.file_name().to_string_lossy().into_owned())
                .map_err(anyhow::Error::msg)?;
            let metadata = entry.path().join("config.json");
            let stored: Stored = match std::fs::read(&metadata)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
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
                workdir: stored.workdir.into(),
                model: stored.model,
                effort,
                archived,
                status: None,
                chat: Vec::new(),
            };
            manager.start(info, log)?;
        }
        Ok(manager)
    }

    pub(crate) fn live(root: Utf8PathBuf, base_url: String) -> anyhow::Result<Arc<Self>> {
        Self::open(root, move |info| {
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
    }

    fn log_path(&self, id: &wire::AgentId) -> PathBuf {
        self.root.join(id.as_str()).join("log").into_std_path_buf()
    }

    fn start(self: &Arc<Self>, mut info: wire::AgentInfo, log: log::Log) -> anyhow::Result<()> {
        let id = info.id.clone();
        let shell = rho_tool_shell::ShellTools::in_directory(
            Duration::from_secs(20),
            info.workdir.clone(),
            rho_fs_view::PathOverrides::default(),
        );
        let (agent, handle) = Agent::new(Config {
            id: log::AgentId::new(id.as_str()).map_err(anyhow::Error::msg)?,
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
                            tracing::warn!(%observer_id, "agent2 chat observer lagged; reread on host restart");
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
                tracing::error!(%id, %error, "agent2 runtime exited");
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
                    from: log::Party::Agent(
                        log::AgentId::new(sender.as_str()).expect("nonempty agent id"),
                    ),
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
        agents.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        agents
    }
    fn subscribe(&self) -> broadcast::Receiver<wire::ServerFrame> {
        self.changes.subscribe()
    }

    fn create(self: &Arc<Self>, call: wire::CreateAgent) -> anyhow::Result<wire::AgentId> {
        let workdir = std::fs::canonicalize(&call.workdir)
            .with_context(|| format!("workdir {}", call.workdir))?;
        anyhow::ensure!(workdir.is_dir(), "agent2 workdir must be a directory");
        let workdir = Utf8PathBuf::try_from(workdir).context("workdir is not UTF-8")?;
        anyhow::ensure!(!call.model.trim().is_empty(), "model is empty");
        let id =
            wire::AgentId::new(uuid::Uuid::new_v4().to_string()).map_err(anyhow::Error::msg)?;
        let dir = self.root.join(id.as_str());
        std::fs::create_dir(&dir)?;
        let stored = Stored {
            workdir: workdir.to_string(),
            model: call.model.clone(),
            effort: call.effort.to_string(),
        };
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&stored)?)?;
        let info = wire::AgentInfo {
            id: id.clone(),
            workdir,
            model: call.model,
            effort: call.effort,
            archived: false,
            status: None,
            chat: Vec::new(),
        };
        self.start(info, log::Log::open(&self.log_path(&id))?)?;
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
            .ok_or_else(|| anyhow!("agent2 {} not found", call.agent_id))?;
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
            .ok_or_else(|| anyhow!("agent2 {} not found", call.agent_id))?;
        handle.archive();
        Ok(())
    }
}

fn convert_party(party: log::Party) -> Option<wire::Party> {
    match party {
        log::Party::Human => Some(wire::Party::Human),
        log::Party::Agent(id) => Some(wire::Party::Agent(wire::AgentId::new(id.as_str()).ok()?)),
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
                respond(&mut writer, call, |call| async { manager.create(call) }).await
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
    async fn chat_archive_revival_and_host_restart_use_the_persistent_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().join("state")).unwrap();
        let workdir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let script = Arc::new(Scripted::new());
        script
            .then("human.status('ready')\nhuman.send('hello')\nawait human.reply()")
            .then("human.send('fresh')\nawait human.reply()");
        let make_model = {
            let script = script.clone();
            move |_: &wire::AgentInfo| Arc::new(Model::Scripted(script.clone()))
        };
        let manager = Agents2::open(root.clone(), make_model).unwrap();
        let id = manager
            .create(wire::CreateAgent {
                workdir,
                model: "scripted".into(),
                effort: wire::Effort::High,
                initial_message: Some("start".into()),
            })
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
        let reloaded = Agents2::open(root, move |_: &wire::AgentInfo| {
            Arc::new(Model::Scripted(restart_script.clone()))
        })
        .unwrap();
        assert_eq!(reloaded.snapshot()[0].chat, before);
        until(|| reloaded.snapshot()[0].chat.iter().any(|event| matches!(&event.kind,
            wire::ChatKind::Message { text, from: wire::Party::Agent(_), .. } if text == "restarted"))).await;
        assert_eq!(reloaded.snapshot()[0].id, id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_mail_routes_into_recipient_chat_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().join("state")).unwrap();
        let workdir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let script = Arc::new(Scripted::new());
        let manager = Agents2::open(root, {
            let script = script.clone();
            move |_: &wire::AgentInfo| Arc::new(Model::Scripted(script.clone()))
        })
        .unwrap();
        let recipient = manager
            .create(wire::CreateAgent {
                workdir: workdir.clone(),
                model: "scripted".into(),
                effort: wire::Effort::Low,
                initial_message: None,
            })
            .unwrap();
        let sender = manager
            .create(wire::CreateAgent {
                workdir,
                model: "scripted".into(),
                effort: wire::Effort::High,
                initial_message: None,
            })
            .unwrap();
        script
            .then(&format!(
                "agents.send('{}', 'ping')\nawait human.reply()",
                recipient.as_str()
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
