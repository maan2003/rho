//! Daemon-owned proxy and process lifetime. No agent loop or notebook runs
//! here.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use tokio::sync::{oneshot, watch};

use super::ipc::{self, Bootstrap, Control, Message};
use super::services::Services;
use crate::db::{AgentId, AgentReadTxnExt as _, AgentRole};
use crate::lazy::Lazy;
use crate::{AgentStatus, MessageDelivery, View};

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
        self.send_user_content(
            vec![rho_agent_host_proto::ContentPart::Text { text }],
            delivery,
        );
    }
    pub fn send_user_content(
        &self,
        content: Vec<rho_agent_host_proto::ContentPart>,
        delivery: MessageDelivery,
    ) {
        self.send(Control::User { content, delivery });
    }
    pub async fn send_user_content_accepted(
        &self,
        content: Vec<rho_agent_host_proto::ContentPart>,
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
