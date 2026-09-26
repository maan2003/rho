//! Host-owned proxy and process lifetime. No agent loop or notebook runs
//! here.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent_types::{AgentId, AgentRole, MessageDelivery};
use tokio::sync::{oneshot, watch};

use super::ipc::{self, Bootstrap, Control, Message};
use super::services::Services;
use crate::db::AgentReadTxnExt as _;
use crate::lazy::Lazy;
use crate::{AgentStatus, View};

#[derive(Clone)]
pub struct Remote(Arc<Inner>);

struct Inner {
    services: Arc<Services>,
    view: Arc<Lazy<Arc<View>>>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    stopping: Arc<AtomicBool>,
    closed: watch::Receiver<bool>,
    process_closed: watch::Receiver<bool>,
}

impl Inner {
    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(stop) = self.stop.lock().expect("poison").take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Remote {
    pub(crate) async fn start(
        pool: &Arc<crate::pool::AgentPool>,
        claude: rho_claude::accounts::ClaudePaths,
        agent: AgentId,
        view: Arc<Lazy<Arc<View>>>,
    ) -> anyhow::Result<Self> {
        let _ = claude; // Process configuration is workset-wide.
        let description = view.get().await?;
        let process = pool.process(description).await?;
        let services = Arc::new(Services::new(
            pool.db().clone(),
            pool.inference().clone(),
            agent,
            Arc::downgrade(pool),
            process.next.clone(),
        ));
        let (incoming, receiver) = tokio::sync::mpsc::unbounded_channel();
        anyhow::ensure!(
            process
                .agents
                .lock()
                .expect("poison")
                .insert(agent, incoming)
                .is_none(),
            "agent port already registered"
        );
        let port = super::transport::Port::Agent(agent);
        let (stop, stopping) = oneshot::channel();
        let (closed, closed_rx) = watch::channel(false);
        let is_stopping = Arc::new(AtomicBool::new(false));
        let handle = Self(Arc::new(Inner {
            services: services.clone(),
            view: view.clone(),
            stop: Mutex::new(Some(stop)),
            stopping: is_stopping.clone(),
            closed: closed_rx.clone(),
            process_closed: process.closed.clone(),
        }));
        let ready = services.ready.subscribe();
        let cwd = description.cwd().to_owned();
        tokio::spawn(async move {
            let mut service = tokio::spawn({
                let services = services.clone();
                let sender = process.sender.clone();
                async move {
                    sender
                        .send(port, ipc::encode(&Message::Bootstrap(Bootstrap { cwd }))?)
                        .await?;
                    services.serve(sender, port, receiver).await
                }
            });
            let (expected, done, error) = tokio::select! {
                _ = stopping => (true, false, String::new()),
                result = &mut service => (false, true, format!("agent runtime ended: {result:?}")),
            };
            is_stopping.store(true, Ordering::Release);
            if done && !services.stopped.load(Ordering::Acquire) {
                process.stop();
                let _ = process.closed.clone().wait_for(|closed| *closed).await;
            }
            if !done {
                let mut joined = false;
                let drain = async {
                    process
                        .sender
                        .send(port, ipc::encode(&Message::Stop)?)
                        .await?;
                    let result = (&mut service).await;
                    joined = true;
                    result.context("agent service task failed")?
                };
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(5), drain).await,
                    Ok(Ok(()))
                ) {
                    // A stuck interpreter is a workset-wide failure domain.
                    process.stop();
                    let _ = process.closed.clone().wait_for(|closed| *closed).await;
                    if !joined {
                        let _ = service.await;
                    }
                }
            }
            process.agents.lock().expect("poison").remove(&agent);
            if !expected {
                services.worker_failed(error.clone()).await;
                services.publish_failure(error).await;
            }
            closed.send_replace(true);
        });
        let mut ready = ready;
        let mut closed = closed_rx;
        let result = tokio::select! {
            result = ready.wait_for(|ready| *ready) => result.map(|_| ()).context("agent readiness closed"),
            _ = closed.wait_for(|closed| *closed) => Err(anyhow::anyhow!("agent exited during startup")),
            _ = tokio::time::sleep(Duration::from_secs(30)) => Err(anyhow::anyhow!("agent startup timed out")),
        };
        if let Err(error) = result {
            handle.shutdown().await;
            return Err(error);
        }
        Ok(handle)
    }

    /// Background tasks must never retain Inner: external handles protect a
    /// settled worker from eviction, just as last-handle lifetime did locally.
    pub(crate) fn pool_only(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }

    pub(crate) fn stopping(&self) -> bool {
        self.0.stopping.load(Ordering::Acquire) || *self.0.process_closed.borrow()
    }

    pub(crate) async fn shutdown(&self) {
        self.0.stop();
        let _ = self.0.closed.clone().wait_for(|closed| *closed).await;
    }

    fn send(&self, control: Control) {
        if let Err(error) = self.0.services.control(control) {
            eprintln!("rho-agent: {error:#}");
        }
    }

    async fn request(&self, control: Control) -> anyhow::Result<()> {
        self.0
            .services
            .control(control)?
            .await
            .context("agent worker connection closed")?
            .map_err(anyhow::Error::msg)
    }

    pub async fn view(&self) -> anyhow::Result<Arc<View>> {
        Ok(self.0.view.get().await?.clone())
    }
    pub fn status(&self) -> AgentStatus {
        self.0.services.status.borrow().clone()
    }
    pub fn head(&self) -> crate::db::AgentHead {
        self.0.services.db.read().get_agent(self.0.services.agent)
    }
    /// Lets the request in flight end and freezes the agent with its log
    /// flushed; see `Agent::drain`. Errs if the request outlived the
    /// workset's deadline.
    pub async fn drain(&self) -> anyhow::Result<()> {
        self.request(Control::Drain).await
    }

    pub(crate) async fn retire(&self) -> anyhow::Result<()> {
        self.request(Control::Retire).await
    }

    pub fn settled(&self) -> bool {
        self.status().settled()
    }
    pub fn notice_carried(&self) {
        self.send(Control::NoticeCarried);
    }
    pub fn tell_tail(&self) {
        self.send(Control::TellTail);
    }
    pub fn compact(&self) {
        self.send(Control::Compact);
    }
    pub fn cancel(&self) {
        self.send(Control::Cancel);
    }
    pub fn retry(&self) {
        self.send(Control::Retry);
    }
    pub fn send_user_message(&self, text: String, delivery: MessageDelivery) {
        self.send_user_content(vec![rho_agent_types::ContentPart::Text { text }], delivery);
    }
    pub fn send_user_content(
        &self,
        content: Vec<rho_agent_types::ContentPart>,
        delivery: MessageDelivery,
    ) {
        self.send(Control::User { content, delivery });
    }
    pub async fn send_user_content_accepted(
        &self,
        content: Vec<rho_agent_types::ContentPart>,
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        self.request(Control::User { content, delivery }).await
    }
    pub fn send_agent_message(
        &self,
        sender: AgentId,
        label: String,
        body: String,
        delivery: MessageDelivery,
    ) {
        self.send(Control::Mail {
            sender,
            label,
            body,
            delivery,
        });
    }
    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        label: String,
        body: String,
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        self.request(Control::Mail {
            sender,
            label,
            body,
            delivery,
        })
        .await
    }
    pub async fn set_claude_effort(&self, effort: rho_claude::Effort) -> anyhow::Result<()> {
        self.request(Control::Effort(effort)).await
    }
    pub async fn change_role(&self, role: AgentRole) -> anyhow::Result<()> {
        self.request(Control::Role(role)).await
    }
    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        self.request(Control::Rewind(turns)).await
    }
    pub fn change_prompt_cache_key(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                self.head().config.runtime,
                crate::db::AgentRuntime::Rho { .. }
            ),
            "prompt cache keys are only available for Rho agents"
        );
        self.0.services.control(Control::CacheKey)?;
        Ok(())
    }
}

/// What the host can observe from the agent2 loop in this workset.
pub enum ChatWorkerEvent {
    Chat(rho_agent2::chat::ChatEvent),
    Archived(bool),
    Stopped(Option<String>),
}

/// An agent2 loop on the existing workset process and agent port.
#[derive(Clone)]
pub struct ChatRemote(Arc<ChatInner>);

struct ChatInner {
    next: AtomicU64,
    rewinds: Arc<Mutex<std::collections::HashMap<u64, oneshot::Sender<anyhow::Result<()>>>>>,
    commands: tokio::sync::mpsc::UnboundedSender<ipc::Message<'static>>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    closed: watch::Receiver<bool>,
}

impl ChatInner {
    fn stop(&self) {
        if let Some(stop) = self.stop.lock().expect("poison").take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for ChatInner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ChatRemote {
    pub async fn start(
        pool: &Arc<crate::pool::AgentPool>,
        place: &rho_agent_types::Place,
        id: AgentId,
        model: String,
        effort: String,
        role: AgentRole,
        parent: Option<AgentId>,
        user_owned: bool,
        on_event: Arc<dyn Fn(ChatWorkerEvent) + Send + Sync>,
    ) -> anyhow::Result<Self> {
        // Mode changes hold the write side through process replacement.
        let _admission = pool.chat_admission(&place.workset).await;
        let view = pool.materialize_view(place).await?;
        let process = pool.process(&view).await?;
        let (route, mut incoming) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut agents = process.agents.lock().expect("poison");
            anyhow::ensure!(!agents.contains_key(&id), "agent port already registered");
            agents.insert(id, route);
        }
        let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop, mut stopping) = oneshot::channel();
        let (closed, closed_rx) = watch::channel(false);
        let (ready, started) = oneshot::channel();
        let rewinds: Arc<Mutex<std::collections::HashMap<_, _>>> = Arc::default();
        let pending_rewinds = rewinds.clone();
        let handle = Self(Arc::new(ChatInner {
            next: AtomicU64::new(1),
            rewinds,
            commands,
            stop: Mutex::new(Some(stop)),
            closed: closed_rx,
        }));
        let cwd = place.cwd.clone();
        let pool = Arc::downgrade(pool);
        tokio::spawn(async move {
            let port = super::transport::Port::Agent(id);
            let result = async {
                process.sender.send(port, ipc::encode(&ipc::Message::ChatBootstrap(
                    ipc::ChatBootstrap { cwd, model, effort, role, parent, user_owned },
                ))?).await?;
                let mut ready = Some(ready);
                let mut stopping_requested = false;
                let mut stop_deadline = tokio::time::Instant::now();
                loop {
                    let packet = tokio::select! {
                        biased;
                        _ = &mut stopping, if !stopping_requested => {
                            stopping_requested = true;
                            stop_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                            process.sender.send(port, ipc::encode(&ipc::Message::Stop)?).await?;
                            continue;
                        }
                        _ = tokio::time::sleep_until(stop_deadline), if stopping_requested => {
                            anyhow::bail!("agent2 worker did not stop");
                        }
                        command = command_rx.recv(), if !stopping_requested => {
                            let Some(command) = command else { anyhow::bail!("agent2 commands closed"); };
                            process.sender.send(port, ipc::encode(&command)?).await?;
                            continue;
                        }
                        packet = incoming.recv() => packet.context("agent port closed")?,
                    };
                    match ipc::decode(&packet.bytes)? {
                        ipc::Message::ChatStarted { error } => {
                            let answer = error.map_or(Ok(()), |error| Err(anyhow::anyhow!(error)));
                            if let Some(ready) = ready.take() { let _ = ready.send(answer); }
                        }
                        ipc::Message::Chat { event } => on_event(ChatWorkerEvent::Chat(event)),
                        ipc::Message::ChatArchived { archived } => on_event(ChatWorkerEvent::Archived(archived)),
                        ipc::Message::ChatTool { request, call } => {
                            let pool = pool.clone();
                            let sender = process.sender.clone();
                            tokio::spawn(async move {
                                let result = match super::services::agent2_tool(pool, id, call).await {
                                    Ok(reply) => ipc::Agent2ToolResult::Ok(reply),
                                    Err(error) => ipc::Agent2ToolResult::Error(error),
                                };
                                let _ = sender.send(port, ipc::encode(&ipc::Message::ChatToolReply { request, result })
                                    .expect("encode agent2 tool reply")).await;
                            });
                        }
                        ipc::Message::ChatRewound { request, error } => {
                            if let Some(reply) = pending_rewinds.lock().expect("poison").remove(&request) {
                                let _ = reply.send(error.map_or(Ok(()), |reason| Err(anyhow::anyhow!(reason))));
                            }
                        },
                        ipc::Message::Stopped { error } => {
                            on_event(ChatWorkerEvent::Stopped(error));
                            return Ok::<(), anyhow::Error>(());
                        }
                        _ => anyhow::bail!("unexpected agent2 worker message"),
                    }
                }
            }.await;
            process.agents.lock().expect("poison").remove(&id);
            pending_rewinds.lock().expect("poison").clear();
            if let Err(error) = result {
                on_event(ChatWorkerEvent::Stopped(Some(format!("{error:#}"))));
                process.stop();
                let _ = process.closed.clone().wait_for(|closed| *closed).await;
            }
            closed.send_replace(true);
        });
        let result = tokio::time::timeout(Duration::from_secs(30), started)
            .await
            .context("agent2 startup timed out")
            .and_then(|result| result.context("agent2 workset closed before startup"))
            .and_then(|result| result);
        if let Err(error) = result {
            handle.shutdown().await;
            return Err(error);
        }
        Ok(handle)
    }

    pub fn send(&self, from: rho_agent2::log::Party, text: String) -> anyhow::Result<()> {
        anyhow::ensure!(!*self.0.closed.borrow(), "agent2 worker closed");
        self.0
            .commands
            .send(ipc::Message::ChatSend { from, text })
            .map_err(|_| anyhow::anyhow!("agent2 worker closed"))
    }

    pub fn archive(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!*self.0.closed.borrow(), "agent2 worker closed");
        self.0
            .commands
            .send(ipc::Message::ChatArchive)
            .map_err(|_| anyhow::anyhow!("agent2 worker closed"))
    }

    pub fn cancel(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!*self.0.closed.borrow(), "agent2 worker closed");
        self.0
            .commands
            .send(ipc::Message::ChatCancel)
            .map_err(|_| anyhow::anyhow!("agent2 worker closed"))
    }

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        anyhow::ensure!(!*self.0.closed.borrow(), "agent2 worker closed");
        let request = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (reply, result) = oneshot::channel();
        self.0
            .rewinds
            .lock()
            .expect("poison")
            .insert(request, reply);
        if self
            .0
            .commands
            .send(ipc::Message::ChatRewind { request, turns })
            .is_err()
        {
            self.0.rewinds.lock().expect("poison").remove(&request);
            anyhow::bail!("agent2 worker closed");
        }
        result.await.context("agent2 worker closed")?
    }

    pub async fn shutdown(&self) {
        self.0.stop();
        let _ = self.0.closed.clone().wait_for(|closed| *closed).await;
    }
}
