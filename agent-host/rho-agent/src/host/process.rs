//! Agent host ownership of a workset process and its two connections: one
//! for the requests the worker makes, see [`protocol::REQUESTS_FD`], and one
//! for everything else.
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
    /// The connections, open across exec.
    pub(crate) fd: i32,
    pub(crate) requests_fd: i32,
    /// The worker's protocol version.
    pub(crate) version: u32,
    pub(crate) next: u64,
    pub(crate) agents: Vec<rho_agent_types::AgentId>,
    /// GUI attachments; their clients did not survive the exec.
    pub(crate) ports: Vec<transport::Port>,
    /// Frames for the worker not yet written, first to write.
    pub(crate) unwritten: Vec<transport::Packet>,
    pub(crate) unwritten_requests: Vec<transport::Packet>,
}

/// A handoff step, taken by the router between frames.
enum Handoff {
    /// Stop reading the worker's requests.
    StopRequests(oneshot::Sender<anyhow::Result<()>>),
    /// Stop reading everything else too.
    Stop(oneshot::Sender<anyhow::Result<()>>),
    /// Read on.
    Resume,
}

pub struct Process {
    pub(crate) pid: u32,
    version: u32,
    /// The connections, kept for a handoff.
    socket: std::os::fd::OwnedFd,
    requests_socket: std::os::fd::OwnedFd,
    handoff: mpsc::UnboundedSender<Handoff>,
    /// Cloned into every request in flight; see [`AgentRoute`].
    inflight: Arc<()>,
    admission: Arc<tokio::sync::RwLock<()>>,
    commands: mpsc::UnboundedSender<workset::Message>,
    clients: workset_client::Clients,
    pending: workset_client::Pending,
    pub(crate) sender: transport::Sender,
    /// For answers to the worker's requests.
    pub(crate) requests: transport::Sender,
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

    /// Starts a handoff: stops reading the worker's requests between frames;
    /// what is unread stays in the socket. Answers still arrive.
    pub(crate) async fn stop_requests(&self) -> anyhow::Result<()> {
        self.step(Handoff::StopRequests).await
    }

    /// Whether every request the host started for the worker has its reply
    /// queued.
    pub(crate) fn idle(&self) -> bool {
        Arc::strong_count(&self.inflight) == 1
    }

    /// Stops reading the other connection too.
    pub(crate) async fn stop_reading(&self) -> anyhow::Result<()> {
        self.step(Handoff::Stop).await
    }

    async fn step(
        &self,
        step: fn(oneshot::Sender<anyhow::Result<()>>) -> Handoff,
    ) -> anyhow::Result<()> {
        let (stopped, done) = oneshot::channel();
        anyhow::ensure!(
            self.handoff.send(step(stopped)).is_ok(),
            "workset process closed"
        );
        done.await.context("workset process closed")?
    }

    /// Undoes a handoff and carries on.
    pub(crate) fn resume(&self) {
        for socket in [&self.socket, &self.requests_socket] {
            let _ = rustix::io::fcntl_setfd(socket, rustix::io::FdFlags::CLOEXEC);
        }
        let _ = self.sender.resume();
        let _ = self.requests.resume();
        let _ = self.handoff.send(Handoff::Resume);
    }

    /// What a successor needs to take this process over, once it is idle
    /// and not reading. Holds the writers between frames and clears
    /// `CLOEXEC` on the connections.
    pub(crate) async fn hand(&self, workset: String) -> anyhow::Result<Handed> {
        use std::os::fd::AsRawFd as _;
        // Whatever is sent after this stays queued; no frame is cut short.
        self.sender.hold().await?;
        self.requests.hold().await?;
        let unwritten = self.sender.queued().await?;
        let unwritten_requests = self.requests.queued().await?;
        for socket in [&self.socket, &self.requests_socket] {
            rustix::io::fcntl_setfd(socket, rustix::io::FdFlags::empty())?;
        }
        Ok(Handed {
            workset,
            pid: self.pid,
            fd: self.socket.as_raw_fd(),
            requests_fd: self.requests_socket.as_raw_fd(),
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
            unwritten,
            unwritten_requests,
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
        let (requests, requests_client) = std::os::unix::net::UnixStream::pair()?;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().pre_exec(move || {
                if requests_client.as_raw_fd() == protocol::REQUESTS_FD {
                    rustix::io::fcntl_setfd(&requests_client, rustix::io::FdFlags::empty())?;
                } else {
                    // dup2 leaves the copy open across exec.
                    let mut target = std::mem::ManuallyDrop::new(
                        std::os::fd::OwnedFd::from_raw_fd(protocol::REQUESTS_FD),
                    );
                    rustix::io::dup2(&requests_client, &mut target)?;
                }
                Ok(())
            });
        }
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
        Self::connect(
            pool.inference(),
            server,
            requests,
            worker,
            Some(startup),
            1,
            admission,
        )
        .await
    }

    /// Takes over a workset process from the agent host this one re-executed.
    /// It reads nothing until [`Process::resume`], once its agents are
    /// adopted.
    pub(crate) async fn adopt(
        pool: &Arc<crate::host::pool::AgentPool>,
        handed: &Handed,
        admission: Arc<tokio::sync::RwLock<()>>,
    ) -> anyhow::Result<Arc<Self>> {
        use std::os::fd::FromRawFd as _;
        // SAFETY: the predecessor left these fds open for us and nothing else
        // claims them.
        let (socket, requests) = unsafe {
            (
                std::os::unix::net::UnixStream::from_raw_fd(handed.fd),
                std::os::unix::net::UnixStream::from_raw_fd(handed.requests_fd),
            )
        };
        rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::CLOEXEC)?;
        rustix::io::fcntl_setfd(&requests, rustix::io::FdFlags::CLOEXEC)?;
        let worker = Worker::open(handed.pid)?;
        if handed.version != protocol::VERSION {
            // Closing the connections ends it; it is still ours to reap.
            drop((socket, requests));
            tokio::spawn(async move { worker.wait().await });
            anyhow::bail!(
                "worker speaks protocol {}, not {}",
                handed.version,
                protocol::VERSION
            );
        }
        let process = Self::connect(
            pool.inference(),
            socket,
            requests,
            worker,
            None,
            handed.next,
            admission,
        )
        .await?;
        for (sender, unwritten) in [
            (&process.sender, &handed.unwritten),
            (&process.requests, &handed.unwritten_requests),
        ] {
            for packet in unwritten {
                sender.send(packet.port, packet.bytes.clone()).await?;
            }
        }
        for port in &handed.ports {
            let _ = process.commands.send(workset::Message::Detach(*port));
        }
        Ok(process)
    }

    async fn connect(
        inference: &crate::inference::Accounts,
        server: std::os::unix::net::UnixStream,
        requests: std::os::unix::net::UnixStream,
        worker: Worker,
        startup: Option<Startup>,
        next: u64,
        admission: Arc<tokio::sync::RwLock<()>>,
    ) -> anyhow::Result<Arc<Self>> {
        let pid = worker.pid;
        let version = startup
            .as_ref()
            .map_or(protocol::VERSION, |startup| startup.version);
        let handoff_socket = std::os::fd::OwnedFd::from(server.try_clone()?);
        let requests_socket = std::os::fd::OwnedFd::from(requests.try_clone()?);
        let disconnect = [server.try_clone()?, requests.try_clone()?];
        server.set_nonblocking(true)?;
        requests.set_nonblocking(true)?;
        let mut socket = tokio::net::UnixStream::from_std(server)?;
        let requests = tokio::net::UnixStream::from_std(requests)?;
        let adopted = startup.is_none();
        let (handoff, mut steps) = mpsc::unbounded_channel();
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
        let inference = inference.clone();
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
                let open = if adopted {
                    transport::connect_stopped
                } else {
                    transport::connect
                };
                let (sender, mut receiver, mut writer) = open(socket);
                let (request_sender, mut request_receiver, mut request_writer) = open(requests);
                let _ = connected.send((sender.clone(), request_sender.clone()));
                let (policy_incoming, policy_messages) = mpsc::channel(32);
                let policy_sender: crate::inference::PolicySender = Arc::new({
                    let sender = request_sender.clone();
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
                    result = &mut request_writer => result.context("workset writer failed")?.map_err(anyhow::Error::from),
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
                        // Only a request carries a live in-flight token.
                        let route = |packet: transport::Packet, token: Arc<()>| -> anyhow::Result<()> {
                            match packet.port {
                                transport::Port::Agent(id) => {
                                    let routes = routes.lock().expect("poison");
                                    if let Some(route) = routes.get(&id) {
                                        // Retirement can close this receiver before unregistering.
                                        let _ = route.send((packet, token));
                                    }
                                }
                                transport::Port::Workset => {
                                    match workset::decode(&packet.bytes)? {
                                        workset::Message::Reply { id, body } => {
                                            if let Some((reply, _admission)) = replies.lock().expect("poison").remove(&id) { let _ = reply.send(body); }
                                        }
                                        workset::Message::Policy(message) => {
                                            policy_incoming.try_send((message, token)).map_err(|_| anyhow::anyhow!("workset policy route closed or overloaded"))?;
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
                            Ok(())
                        };
                        let request = || token.upgrade().unwrap_or_default();
                        loop {
                            tokio::select! {
                                biased;
                                Some(step) = steps.recv() => {
                                    let (stopped, all) = match step {
                                        Handoff::StopRequests(stopped) => (stopped, false),
                                        Handoff::Stop(stopped) => (stopped, true),
                                        Handoff::Resume => {
                                            request_receiver.resume();
                                            receiver.resume();
                                            continue;
                                        }
                                    };
                                    // What was read before the stop is handled as usual.
                                    let result = async {
                                        if all {
                                            for packet in receiver.stop().await? {
                                                route(packet, Arc::default())?;
                                            }
                                        } else {
                                            for packet in request_receiver.stop().await? {
                                                route(packet, request())?;
                                            }
                                        }
                                        Ok(())
                                    }.await;
                                    let failed = result.as_ref().err().map(|error: &anyhow::Error| anyhow::anyhow!("{error:#}"));
                                    let _ = stopped.send(result);
                                    if let Some(error) = failed { return Err(error); }
                                }
                                packet = request_receiver.next() => route(packet?, request())?,
                                packet = receiver.next() => route(packet?, Arc::default())?,
                            }
                        }
                        #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
                    } => result,
                };
                routes.lock().expect("poison").clear();
                client_routes.lock().expect("poison").clear();
                sender.close();
                request_sender.close();
                writer.abort();
                request_writer.abort();
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
            for socket in &disconnect {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
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
        let (sender, requests) = connection.await.context("workset startup failed")?;
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
            socket: handoff_socket,
            requests_socket,
            handoff,
            inflight,
            admission,
            commands,
            clients,
            pending,
            sender,
            requests,
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

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use rho_agent_types::{AgentId, AgentIdDomain};

    use super::*;
    use crate::ipc::protocol::{Message, Request};

    fn frame(message: &Message<'_>) -> Bytes {
        protocol::encode(message).unwrap()
    }

    fn worker_end(
        socket: std::os::unix::net::UnixStream,
    ) -> (transport::Sender, transport::Receiver) {
        socket.set_nonblocking(true).unwrap();
        let (sender, receiver, _writing) =
            transport::connect(tokio::net::UnixStream::from_std(socket).unwrap());
        (sender, receiver)
    }

    /// Answers keep arriving while requests wait unread, so a request that
    /// waits on another worker can finish; the successor then reads what
    /// was left in the sockets.
    #[tokio::test]
    async fn a_handoff_leaves_new_requests_unread_for_the_successor() {
        let (host_end, worker) = std::os::unix::net::UnixStream::pair().unwrap();
        let (host_requests, worker_requests) = std::os::unix::net::UnixStream::pair().unwrap();
        let (worker, _worker_receiver) = worker_end(worker);
        let (worker_requests, _worker_answers) = worker_end(worker_requests);
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let inference = crate::inference::testing::accounts();
        let agent = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        let port = transport::Port::Agent(agent);
        let request = |id| {
            frame(&Message::Request {
                id,
                body: Request::Head,
            })
        };
        let answer = |id| frame(&Message::Controlled { id, error: None });

        let old = Process::connect(
            &inference,
            host_end.try_clone().unwrap(),
            host_requests.try_clone().unwrap(),
            Worker::open(child.id()).unwrap(),
            None,
            1,
            Default::default(),
        )
        .await
        .unwrap();
        let (route, mut routed) = mpsc::unbounded_channel();
        old.agents.lock().unwrap().insert(agent, route);
        old.resume();
        worker_requests.send(port, request(1)).await.unwrap();
        let (first, started) = routed.recv().await.unwrap();
        assert_eq!(first.bytes, request(1));

        old.stop_requests().await.unwrap();
        worker_requests.send(port, request(2)).await.unwrap();
        worker.send(port, answer(8)).await.unwrap();
        worker_requests.send(port, request(3)).await.unwrap();
        let (arrived, _) = routed.recv().await.unwrap();
        assert_eq!(arrived.bytes, answer(8), "answers still arrive");
        assert!(!old.idle(), "request 1 is still being handled");
        drop(started);
        assert!(old.idle());
        old.stop_reading().await.unwrap();
        worker.send(port, answer(9)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(routed.try_recv().is_err(), "nothing more is read");
        let handed = old.hand("workset".into()).await.unwrap();

        let new = Process::connect(
            &inference,
            host_end,
            host_requests,
            Worker::open(child.id()).unwrap(),
            None,
            handed.next,
            Default::default(),
        )
        .await
        .unwrap();
        let (route, mut routed) = mpsc::unbounded_channel();
        new.agents.lock().unwrap().insert(agent, route);
        new.resume();
        let mut requests = Vec::new();
        let mut answers = Vec::new();
        for _ in 0..3 {
            let (packet, _) = routed.recv().await.unwrap();
            match protocol::decode(&packet.bytes).unwrap() {
                Message::Request { id, .. } => requests.push(id),
                Message::Controlled { id, .. } => answers.push(id),
                _ => unreachable!(),
            }
        }
        assert_eq!((requests, answers), (vec![2, 3], vec![9]));
        let _ = child.kill();
        let _ = child.wait();
    }
}
