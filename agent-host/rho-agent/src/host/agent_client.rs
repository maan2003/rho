//! Host-owned proxy and process lifetime. No agent loop or notebook runs
//! here.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent_types::AgentId;
use tokio::sync::{oneshot, watch};

use super::services::Services;
use crate::AgentStatus;
use crate::ipc::protocol::{self, Control, Message};
use crate::store::StoreRead as _;

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
        let workset = pool
            .db()
            .read()
            .agent(agent)
            .context("agent was deleted or does not exist")?
            .workset;
        let process = pool.process(workset).await?;
        Self::connect(pool, agent, process, true).await
    }

    /// Takes over an agent already running in `process`, handed over by the
    /// agent host this one re-executed. It is ready already; the successor
    /// learns its status once it tells its tail.
    pub(crate) async fn adopt(
        pool: &Arc<crate::host::pool::AgentPool>,
        agent: AgentId,
        process: Arc<crate::host::Process>,
    ) -> anyhow::Result<Self> {
        Self::connect(pool, agent, process, false).await
    }

    /// `bootstrap` starts the agent's loop in the worker.
    async fn connect(
        pool: &Arc<crate::host::pool::AgentPool>,
        agent: AgentId,
        process: Arc<crate::host::Process>,
        bootstrap: bool,
    ) -> anyhow::Result<Self> {
        let mut services = Services::new(
            pool.db().clone(),
            agent,
            Arc::downgrade(pool),
            process.next.clone(),
        );
        services.stale = process.stale();
        let services = Arc::new(services);
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
        if !bootstrap {
            services.ready.send_replace(true);
        }
        let ready = services.ready.subscribe();
        tokio::spawn(async move {
            let mut service = tokio::spawn({
                let services = services.clone();
                let sender = process.sender.clone();
                let requests = process.requests.clone();
                async move {
                    if bootstrap {
                        sender
                            .send(port, protocol::encode(&Message::Bootstrap)?)
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
                services.worker_failed(error).await;
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

    /// Hand the loop a control and wait until it has applied it.
    pub(crate) async fn control(&self, control: Control) -> anyhow::Result<()> {
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
    pub fn statuses(&self) -> watch::Receiver<AgentStatus> {
        self.0.services.status.subscribe()
    }
    pub(crate) fn status_cell(&self) -> &watch::Sender<AgentStatus> {
        &self.0.services.status
    }
    pub(crate) async fn retire(&self) -> anyhow::Result<()> {
        self.control(Control::Retire).await
    }
    pub fn settled(&self) -> bool {
        self.status().settled()
    }
    /// Asks the worker to send its status again: an adopted agent's host
    /// has none yet.
    pub fn tell_tail(&self) {
        self.send(Control::TellTail);
    }
    /// The agent's head changed outside its loop.
    pub(crate) fn named(&self, head: crate::store::AgentHead) {
        self.0.services.named(head);
    }
}
