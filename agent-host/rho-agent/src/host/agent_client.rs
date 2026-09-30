//! Host-owned proxy and process lifetime. No agent loop or notebook runs
//! here.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent_types::{AgentId, AgentRole};
use tokio::sync::{oneshot, watch};

use super::services::Services;
use crate::AgentStatus;
use crate::db::AgentReadTxnExt as _;
use crate::ipc::protocol::{self, Bootstrap, Control, Message};

#[derive(Clone)]
pub struct AgentClient(Arc<Inner>);

struct Inner {
    services: Arc<Services>,
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

impl AgentClient {
    pub(crate) async fn start(
        pool: &Arc<crate::host::pool::AgentPool>,
        agent: AgentId,
    ) -> anyhow::Result<Self> {
        let place = pool.db().read().get_agent(agent).config.place;
        let (_, cwd) = pool.open_workset(&place).await?;
        anyhow::ensure!(
            cwd.is_dir(),
            "working directory does not exist: {}",
            place.cwd
        );
        let process = pool.process(&place).await?;
        Self::connect(pool, agent, process, Some(place.cwd)).await
    }

    /// Takes over an agent already running in `process`, handed over by the
    /// agent host this one re-executed. It is ready already; the successor
    /// learns its status once it tells its tail.
    pub(crate) async fn adopt(
        pool: &Arc<crate::host::pool::AgentPool>,
        agent: AgentId,
        process: Arc<crate::host::Process>,
    ) -> anyhow::Result<Self> {
        Self::connect(pool, agent, process, None).await
    }

    /// `cwd` bootstraps a new agent in the worker.
    async fn connect(
        pool: &Arc<crate::host::pool::AgentPool>,
        agent: AgentId,
        process: Arc<crate::host::Process>,
        cwd: Option<camino::Utf8PathBuf>,
    ) -> anyhow::Result<Self> {
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
        let port = crate::ipc::transport::Port::Agent(agent);
        let (stop, stopping) = oneshot::channel();
        let (closed, closed_rx) = watch::channel(false);
        let is_stopping = Arc::new(AtomicBool::new(false));
        let handle = Self(Arc::new(Inner {
            services: services.clone(),
            stop: Mutex::new(Some(stop)),
            stopping: is_stopping.clone(),
            closed: closed_rx.clone(),
            process_closed: process.closed.clone(),
        }));
        if cwd.is_none() {
            services.ready.send_replace(true);
        }
        let ready = services.ready.subscribe();
        tokio::spawn(async move {
            let mut service = tokio::spawn({
                let services = services.clone();
                let sender = process.sender.clone();
                let requests = process.requests.clone();
                async move {
                    if let Some(cwd) = cwd {
                        sender
                            .send(
                                port,
                                protocol::encode(&Message::Bootstrap(Bootstrap { cwd }))?,
                            )
                            .await?;
                    }
                    services.serve(sender, requests, port, receiver).await
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
                        .send(port, protocol::encode(&Message::Stop)?)
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

    pub fn status(&self) -> AgentStatus {
        self.0.services.status.borrow().clone()
    }
    pub fn head(&self) -> crate::log::AgentHead {
        self.0.services.db.read().get_agent(self.0.services.agent)
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
    pub fn send_user_message(&self, text: String) {
        self.send_user_content(vec![rho_agent_types::ContentPart::Text { text }]);
    }
    pub fn send_user_content(&self, content: Vec<rho_agent_types::ContentPart>) {
        self.send(Control::User {
            id: crate::entry::MessageId::new(),
            content,
        });
    }
    /// Waits until the message is logged. A message whose `id` the agent
    /// already logged is accepted again without a second row, so a client
    /// that never heard the first answer can safely send it again.
    pub async fn send_user_content_accepted(
        &self,
        id: crate::entry::MessageId,
        content: Vec<rho_agent_types::ContentPart>,
    ) -> anyhow::Result<()> {
        self.request(Control::User { id, content }).await
    }
    pub fn send_agent_message(&self, sender: AgentId, label: String, body: String) {
        self.send(Control::Mail {
            sender,
            label,
            body,
        });
    }
    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        label: String,
        body: String,
    ) -> anyhow::Result<()> {
        self.request(Control::Mail {
            sender,
            label,
            body,
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
                crate::log::AgentRuntime::Rho { .. }
            ),
            "prompt cache keys are only available for Rho agents"
        );
        self.0.services.control(Control::CacheKey)?;
        Ok(())
    }
}
