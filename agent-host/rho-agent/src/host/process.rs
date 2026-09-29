//! Agent host ownership of a workset process and its single connection.
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tokio::sync::{mpsc, oneshot, watch};

use super::workset_client;
use crate::ipc::protocol::Startup;
use crate::ipc::{protocol, transport, workset};

/// Routes the worker's packets for one agent. Each carries a clone of the
/// process's in-flight token until its reply, if any, is handed to the
/// transport.
pub(crate) type AgentRoute = mpsc::UnboundedSender<(transport::Packet, Arc<()>)>;

/// A workset process handed from this agent host to its re-executed
/// successor: the same pid, so the worker is still its child.
#[derive(senax_encoder::Encode, senax_encoder::Decode)]
pub struct Handed {
    pub(crate) workset: String,
    pub(crate) pid: u32,
    /// The connection, open across exec.
    pub(crate) fd: i32,
    /// The worker's protocol version.
    pub(crate) version: u32,
    pub(crate) next: u64,
    pub(crate) agents: Vec<rho_agent_types::AgentId>,
    /// GUI attachments; their clients did not survive the exec.
    pub(crate) ports: Vec<transport::Port>,
}

pub struct Process {
    pub(crate) pid: u32,
    version: u32,
    /// The connection, kept for a handoff.
    socket: std::os::fd::OwnedFd,
    /// Counts `Paused` frames read.
    paused: watch::Receiver<u64>,
    /// Cloned into every packet in flight; see [`AgentRoute`].
    inflight: Arc<()>,
    admission: Arc<tokio::sync::RwLock<()>>,
    commands: mpsc::UnboundedSender<workset::Message>,
    clients: workset_client::Clients,
    pending: workset_client::Pending,
    pub(crate) sender: transport::Sender,
    pub(crate) agents: Arc<Mutex<HashMap<rho_agent_types::AgentId, AgentRoute>>>,
    pub(crate) next: Arc<AtomicU64>,
    pub(crate) closed: watch::Receiver<bool>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Process {
    #[cfg(test)]
    pub(crate) fn fail_agent_service(&self, agent: rho_agent_types::AgentId) {
        self.agents.lock().expect("poison").remove(&agent);
    }

    #[cfg(test)]
    pub(crate) fn fail_stop_send(&self) {
        self.sender.close();
    }

    #[cfg(test)]
    pub(crate) fn pause_agent_route(
        &self,
        agent: rho_agent_types::AgentId,
    ) -> (
        AgentRoute,
        mpsc::UnboundedReceiver<(transport::Packet, Arc<()>)>,
    ) {
        let (send, receive) = mpsc::unbounded_channel();
        let previous = self
            .agents
            .lock()
            .expect("poison")
            .insert(agent, send)
            .unwrap();
        (previous, receive)
    }

    #[cfg(test)]
    pub(crate) fn restore_agent_route(
        &self,
        agent: rho_agent_types::AgentId,
        route: AgentRoute,
        blocked: &mut mpsc::UnboundedReceiver<(transport::Packet, Arc<()>)>,
    ) {
        let mut agents = self.agents.lock().expect("poison");
        while let Ok(packet) = blocked.try_recv() {
            route.send(packet).unwrap();
        }
        agents.insert(agent, route);
    }

    #[cfg(test)]
    pub(crate) fn fail_shutdown_reply(&self, agent: rho_agent_types::AgentId) {
        self.agents.lock().expect("poison")[&agent]
            .send((
                transport::Packet::for_test(
                    transport::Port::Agent(agent),
                    protocol::encode(&protocol::Message::Stopped {
                        error: Some("test cleanup failed".into()),
                    })
                    .unwrap(),
                ),
                self.inflight.clone(),
            ))
            .unwrap();
    }

    pub async fn action(&self, action: workset::Action) -> anyhow::Result<workset::Reply> {
        let admission = self.admission.clone().read_owned().await;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.request(id, workset::Message::Action { id, action }, Some(admission))
            .await
    }

    async fn request(
        &self,
        id: u64,
        message: workset::Message,
        admission: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> anyhow::Result<workset::Reply> {
        anyhow::ensure!(!*self.closed.borrow(), "workset process closed");
        let (reply, response) = oneshot::channel();
        self.pending
            .lock()
            .expect("poison")
            .insert(id, (reply, admission));
        if self.commands.send(message).is_err() {
            self.pending.lock().expect("poison").remove(&id);
            anyhow::bail!("workset process closed");
        }
        match response.await.context("workset process closed")? {
            workset::Reply::Error(error) => anyhow::bail!(error),
            reply => Ok(reply),
        }
    }

    pub async fn attach(&self, attach: workset::Attach) -> anyhow::Result<workset_client::Client> {
        let admission = self.admission.clone().read_owned().await;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let port = match &attach {
            workset::Attach::Terminal { .. } => transport::Port::Terminal(id),
            workset::Attach::Shell { .. } => transport::Port::Shell(id),
        };
        let (send, incoming) = mpsc::channel(32);
        self.clients.lock().expect("poison").insert(port, send);
        let client = workset_client::Client {
            port,
            sender: self.sender.clone(),
            incoming,
            commands: self.commands.clone(),
        };
        self.request(
            id,
            workset::Message::Attach { id, port, attach },
            Some(admission),
        )
        .await?;
        Ok(client)
    }

    pub async fn shutdown(&self) {
        self.stop();
        let _ = self.closed.clone().wait_for(|closed| *closed).await;
    }

    pub fn stop(&self) {
        if let Some(stop) = self.stop.lock().expect("poison").take() {
            let _ = stop.send(());
        }
    }

    /// Readies the connection for a handoff: the worker writes nothing more,
    /// everything it wrote has been handled and answered, and every answer
    /// has been written. Undo with [`Process::resume`].
    pub(crate) async fn pause(&self) -> anyhow::Result<()> {
        let mut paused = self.paused.clone();
        let seen = *paused.borrow_and_update();
        anyhow::ensure!(
            self.commands.send(workset::Message::Pause).is_ok(),
            "workset process closed"
        );
        paused.wait_for(|count| *count > seen).await?;
        while Arc::strong_count(&self.inflight) > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Whatever is sent after this stays queued and dies with the exec,
        // as it would in a crash; no frame is cut short.
        self.sender.hold().await?;
        Ok(())
    }

    pub(crate) fn resume(&self) {
        let _ = rustix::io::fcntl_setfd(&self.socket, rustix::io::FdFlags::CLOEXEC);
        let _ = self.sender.resume();
        let _ = self.commands.send(workset::Message::Resume);
    }

    /// What a successor needs to take this process over. Only after
    /// [`Process::pause`]; clears `CLOEXEC` on the connection.
    pub(crate) fn hand(&self, workset: String) -> std::io::Result<Handed> {
        use std::os::fd::AsRawFd as _;
        let fd = self.socket.as_raw_fd();
        rustix::io::fcntl_setfd(&self.socket, rustix::io::FdFlags::empty())?;
        Ok(Handed {
            workset,
            pid: self.pid,
            fd,
            version: self.version,
            next: self.next.load(std::sync::atomic::Ordering::Relaxed),
            agents: self
                .agents
                .lock()
                .expect("poison")
                .keys()
                .copied()
                .collect(),
            ports: self
                .clients
                .lock()
                .expect("poison")
                .keys()
                .copied()
                .collect(),
        })
    }

    pub(crate) async fn start(
        pool: &Arc<crate::host::pool::AgentPool>,
        workset: &rho_fs_view::Workset,
        claude: rho_claude::accounts::ClaudePaths,
        admission: Arc<tokio::sync::RwLock<()>>,
    ) -> anyhow::Result<Arc<Self>> {
        let layout = rho_fs_view::WorksetLayout::new(workset)?;
        let startup = Startup {
            version: protocol::VERSION,
            layout,
            claude,
            responses_base_url: pool.inference().responses_base_url().to_owned(),
        };
        let sibling = std::env::current_exe()?.with_file_name("rho-agent-worker");
        let executable = if sibling.is_file() {
            sibling
        } else {
            "rho-agent-worker".into()
        };
        let mut command = tokio::process::Command::new(executable);
        pool.worksets().environment().apply(&mut command);
        command.envs(pool.worksets().store_environment());
        command.envs(pool.worksets().identity_environment().iter().cloned());
        let (server, client) = std::os::unix::net::UnixStream::pair()?;
        command.stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            client,
        )));
        command
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        // No parent-death signal: it would fire when a re-exec ends the
        // thread that spawned the worker. A worker ends with its connection.
        // Spawned without tokio, which would reap it behind the pidfd's back.
        let child = command
            .as_std_mut()
            .spawn()
            .context("start rho-agent-worker companion")?;
        drop(command);
        let worker = Worker::open(child.id())?;
        Self::connect(pool, server, worker, Some(startup), 1, admission).await
    }

    /// Takes over a workset process from the agent host this one re-executed.
    pub(crate) async fn adopt(
        pool: &Arc<crate::host::pool::AgentPool>,
        handed: &Handed,
        admission: Arc<tokio::sync::RwLock<()>>,
    ) -> anyhow::Result<Arc<Self>> {
        use std::os::fd::FromRawFd as _;
        // SAFETY: the predecessor left this fd open for us and nothing else
        // claims it.
        let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(handed.fd) };
        rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::CLOEXEC)?;
        let worker = Worker::open(handed.pid)?;
        if handed.version != protocol::VERSION {
            // Closing the connection ends it; it is still ours to reap.
            drop(socket);
            tokio::spawn(async move { worker.wait().await });
            anyhow::bail!(
                "worker speaks protocol {}, not {}",
                handed.version,
                protocol::VERSION
            );
        }
        let process = Self::connect(pool, socket, worker, None, handed.next, admission).await?;
        for port in &handed.ports {
            let _ = process.commands.send(workset::Message::Detach(*port));
        }
        Ok(process)
    }

    async fn connect(
        pool: &Arc<crate::host::pool::AgentPool>,
        server: std::os::unix::net::UnixStream,
        worker: Worker,
        startup: Option<Startup>,
        next: u64,
        admission: Arc<tokio::sync::RwLock<()>>,
    ) -> anyhow::Result<Arc<Self>> {
        let pid = worker.pid;
        let version = startup
            .as_ref()
            .map_or(protocol::VERSION, |startup| startup.version);
        let handoff = std::os::fd::OwnedFd::from(server.try_clone()?);
        let disconnect = server.try_clone()?;
        server.set_nonblocking(true)?;
        let mut socket = tokio::net::UnixStream::from_std(server)?;
        let (counted, paused) = watch::channel(0u64);
        let inflight = Arc::new(());
        let token = Arc::downgrade(&inflight);
        let (stop, stopped) = oneshot::channel();
        let (closed, closed_rx) = watch::channel(false);
        let agents: Arc<Mutex<HashMap<rho_agent_types::AgentId, AgentRoute>>> = Arc::default();
        let clients: workset_client::Clients = Arc::default();
        let pending: workset_client::Pending = Arc::default();
        let client_routes = clients.clone();
        let replies = pending.clone();
        let pending_close = pending.clone();
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let routing_commands = commands.clone();
        let routes = agents.clone();
        let inference = pool.inference().clone();
        // The supervisor owns the child before any cancellable bootstrap I/O.
        let (connected, connection) = oneshot::channel();
        tokio::spawn(async move {
            let mut service = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                if let Some(startup) = startup {
                    let bytes = senax_encoder::encode(&startup)
                        .map_err(|_| anyhow::anyhow!("encode workset startup"))?;
                    socket.write_u32(bytes.len().try_into()?).await?;
                    socket.write_all(&bytes).await?;
                }
                let (sender, mut receiver, mut writer) = transport::connect(socket);
                let _ = connected.send(sender.clone());
                let (policy_incoming, policy_messages) = mpsc::channel(32);
                let policy_sender: crate::inference::PolicySender = Arc::new({
                    let sender = sender.clone();
                    move |bytes| {
                        let sender = sender.clone();
                        Box::pin(async move {
                            sender
                                .send(
                                    transport::Port::Workset,
                                    workset::encode(&workset::Message::Policy(bytes))?,
                                )
                                .await?;
                            Ok(())
                        })
                    }
                });
                let policy = inference.serve_policy(policy_sender, policy_messages);
                tokio::pin!(policy);
                let result = tokio::select! {
                    result = &mut policy => result,
                    result = &mut writer => result.context("workset writer failed")?.map_err(anyhow::Error::from),
                    result = async {
                        while let Some(message) = command_rx.recv().await {
                            if let workset::Message::Detach(port) = &message {
                                client_routes.lock().expect("poison").remove(port);
                            }
                            sender.send(transport::Port::Workset, workset::encode(&message)?).await?;
                        }
                        Ok::<(), anyhow::Error>(())
                    } => result,
                    result = async {
                        loop {
                            let packet = receiver.next().await?;
                            match packet.port {
                                transport::Port::Agent(id) => {
                                    let routes = routes.lock().expect("poison");
                                    if let Some(route) = routes.get(&id) {
                                        // Retirement can close this receiver before unregistering.
                                        let _ = route.send((packet, token.upgrade().unwrap_or_default()));
                                    }
                                }
                                transport::Port::Workset => {
                                    match workset::decode(&packet.bytes)? {
                                        workset::Message::Reply { id, body } => {
                                            if let Some((reply, _admission)) = replies.lock().expect("poison").remove(&id) { let _ = reply.send(body); }
                                        }
                                        workset::Message::Policy(message) => {
                                            policy_incoming.try_send(message).map_err(|_| anyhow::anyhow!("workset policy route closed or overloaded"))?;
                                        }
                                        workset::Message::Paused => {
                                            counted.send_modify(|count| *count += 1);
                                        }
                                        _ => anyhow::bail!("unexpected workset reply"),
                                    }
                                }
                                port @ (transport::Port::Terminal(_) | transport::Port::Shell(_)) => {
                                    let mut clients = client_routes.lock().expect("poison");
                                    if packet.bytes.is_empty() {
                                        clients.remove(&port);
                                    } else if clients.get(&port).is_some_and(|client| client.try_send(packet.bytes).is_err()) {
                                        clients.remove(&port);
                                        // A slow GUI loses only its attachment; never drop incremental frames and keep it connected.
                                        let _ = routing_commands.send(workset::Message::Detach(port));
                                    }
                                }
                            }
                        }
                        #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
                    } => result,
                };
                routes.lock().expect("poison").clear();
                client_routes.lock().expect("poison").clear();
                sender.close();
                writer.abort();
                result
            });
            let service_done = tokio::select! {
                _ = stopped => false,
                _ = worker.wait() => false,
                result = &mut service => {
                    if !matches!(&result, Ok(Ok(()))) {
                        eprintln!("rho-agent: workset service ended: {result:?}");
                    }
                    true
                },
            };
            let _ = disconnect.shutdown(std::net::Shutdown::Both);
            if !service_done {
                let _ = service.await;
            }
            if tokio::time::timeout(std::time::Duration::from_secs(5), worker.wait())
                .await
                .is_err()
            {
                worker.kill();
                worker.wait().await;
            }
            // Cancelled callers cannot release admission before an enqueued
            // operation replies or the execution process has actually exited.
            pending_close.lock().expect("poison").clear();
            closed.send_replace(true);
        });
        let sender = connection.await.context("workset startup failed")?;
        // EOF wakes all agent service tasks, independently of their runtimes.
        let clearing = agents.clone();
        let mut ending = closed_rx.clone();
        tokio::spawn(async move {
            let _ = ending.wait_for(|closed| *closed).await;
            clearing.lock().expect("poison").clear();
        });
        Ok(Arc::new(Self {
            pid,
            version,
            socket: handoff,
            paused,
            inflight,
            admission,
            commands,
            clients,
            pending,
            sender,
            agents,
            next: Arc::new(AtomicU64::new(next)),
            closed: closed_rx,
            stop: Mutex::new(Some(stop)),
        }))
    }
}

/// A worker process, watched through a pidfd: tokio cannot take over a child
/// it did not spawn, and after a re-exec none of them are its own.
struct Worker {
    pid: u32,
    pidfd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

impl Worker {
    fn open(pid: u32) -> std::io::Result<Self> {
        let raw = rustix::process::Pid::from_raw(pid as i32)
            .ok_or_else(|| std::io::Error::other("invalid worker pid"))?;
        let pidfd = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::NONBLOCK)?;
        Ok(Self {
            pid,
            pidfd: tokio::io::unix::AsyncFd::with_interest(pidfd, tokio::io::Interest::READABLE)?,
        })
    }

    /// Waits for the worker to exit and reaps it.
    async fn wait(&self) {
        use rustix::process::{WaitId, WaitIdOptions};
        loop {
            let Ok(mut ready) = self.pidfd.readable().await else {
                return;
            };
            match rustix::process::waitid(
                WaitId::PidFd(std::os::fd::AsFd::as_fd(self.pidfd.get_ref())),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
            ) {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => ready.clear_ready(),
            }
        }
    }

    fn kill(&self) {
        let _ =
            rustix::process::pidfd_send_signal(self.pidfd.get_ref(), rustix::process::Signal::KILL);
    }
}
