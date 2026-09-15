//! Daemon ownership of a workset process and its single connection.
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tokio::sync::{mpsc, oneshot, watch};

use super::{ipc, transport};

#[derive(senax_encoder::Encode, senax_encoder::Decode)]
pub(super) struct Startup {
    pub version: u32,
    pub layout: rho_fs_view::WorksetLayout,
    pub claude: rho_claude::accounts::ClaudePaths,
    pub responses_base_url: String,
}

pub struct Process {
    #[cfg(test)]
    pub(crate) pid: u32,
    admission: Arc<tokio::sync::RwLock<()>>,
    commands: mpsc::UnboundedSender<super::workset::Message>,
    clients: super::workset::Clients,
    pending: super::workset::Pending,
    pub(super) sender: transport::Sender,
    pub(super) agents: Arc<Mutex<HashMap<rho_core::AgentId, mpsc::Sender<bytes::Bytes>>>>,
    pub(super) next: Arc<AtomicU64>,
    pub(crate) closed: watch::Receiver<bool>,
    pub(crate) mode: rho_fs_view::WorksetMode,
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Process {
    #[cfg(test)]
    pub(crate) fn fail_agent_service(&self, agent: rho_core::AgentId) {
        self.agents.lock().expect("poison").remove(&agent);
    }

    #[cfg(test)]
    pub(crate) fn fail_stop_send(&self) {
        self.sender.close();
    }

    #[cfg(test)]
    pub(crate) fn overload_agent_route(
        &self,
        agent: rho_core::AgentId,
    ) -> (mpsc::Sender<bytes::Bytes>, mpsc::Receiver<bytes::Bytes>) {
        let (send, receive) = mpsc::channel(1);
        let previous = self
            .agents
            .lock()
            .expect("poison")
            .insert(agent, send)
            .unwrap();
        (previous, receive)
    }

    #[cfg(test)]
    pub(crate) fn fail_shutdown_reply(&self, agent: rho_core::AgentId) {
        self.agents.lock().expect("poison")[&agent]
            .try_send(
                ipc::encode(&ipc::Message::Stopped {
                    error: Some("test cleanup failed".into()),
                })
                .unwrap(),
            )
            .unwrap();
    }

    pub async fn action(
        &self,
        action: super::workset::Action,
    ) -> anyhow::Result<super::workset::Reply> {
        let admission = self.admission.clone().read_owned().await;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.request(
            id,
            super::workset::Message::Action { id, action },
            Some(admission),
        )
        .await
    }

    async fn request(
        &self,
        id: u64,
        message: super::workset::Message,
        admission: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> anyhow::Result<super::workset::Reply> {
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
            super::workset::Reply::Error(error) => anyhow::bail!(error),
            reply => Ok(reply),
        }
    }

    pub async fn attach(
        &self,
        attach: super::workset::Attach,
    ) -> anyhow::Result<super::workset::Client> {
        let admission = self.admission.clone().read_owned().await;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let port = match &attach {
            super::workset::Attach::Terminal { .. } => transport::Port::Terminal(id),
            super::workset::Attach::Shell { .. } => transport::Port::Shell(id),
        };
        let (send, incoming) = mpsc::channel(32);
        self.clients.lock().expect("poison").insert(port, send);
        let client = super::workset::Client {
            port,
            sender: self.sender.clone(),
            incoming,
            commands: self.commands.clone(),
        };
        self.request(
            id,
            super::workset::Message::Attach { id, port, attach },
            Some(admission),
        )
        .await?;
        Ok(client)
    }

    pub async fn shutdown(&self) {
        self.stop();
        let _ = self.closed.clone().wait_for(|closed| *closed).await;
    }

    // The caller holds exclusive workset admission.
    pub(crate) async fn no_sessions(&self) -> anyhow::Result<bool> {
        use super::workset::{Action, Message, Reply};
        for action in [Action::TerminalList, Action::ShellList] {
            let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match self
                .request(id, Message::Action { id, action }, None)
                .await?
            {
                Reply::Terminals(entries) if entries.is_empty() => {}
                Reply::Shells(entries) if entries.is_empty() => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    pub fn stop(&self) {
        if let Some(stop) = self.stop.lock().expect("poison").take() {
            let _ = stop.send(());
        }
    }

    pub(crate) async fn start(
        pool: &Arc<crate::pool::AgentPool>,
        view: &crate::View,
        claude: rho_claude::accounts::ClaudePaths,
        admission: Arc<tokio::sync::RwLock<()>>,
    ) -> anyhow::Result<Arc<Self>> {
        let workset = pool.worksets().open_workset(view.workset_id()).await?;
        let root = tempfile::Builder::new().prefix("rho-workset-").tempdir()?;
        let layout = rho_fs_view::WorksetLayout::new(
            &workset,
            view.mode().clone(),
            camino::Utf8PathBuf::from_path_buf(root.path().to_owned())
                .map_err(|_| anyhow::anyhow!("non-UTF8 workset mount root"))?,
        )?;
        let startup = Startup {
            version: ipc::VERSION,
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
        let disconnect = server.try_clone()?;
        server.set_nonblocking(true)?;
        let mut socket = tokio::net::UnixStream::from_std(server)?;
        command.stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            client,
        )));
        command
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        command.kill_on_drop(true);
        let parent = rustix::process::getpid();
        unsafe {
            command.pre_exec(move || {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ))?;
                if rustix::process::getppid() != Some(parent) {
                    return Err(std::io::Error::other("daemon exited during workset launch"));
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .context("start rho-agent-worker companion")?;
        drop(command);
        #[cfg(test)]
        let pid = child.id().expect("spawned child");
        let (stop, stopped) = oneshot::channel();
        let (closed, closed_rx) = watch::channel(false);
        let agents: Arc<Mutex<HashMap<rho_core::AgentId, mpsc::Sender<bytes::Bytes>>>> =
            Arc::default();
        let clients: super::workset::Clients = Arc::default();
        let pending: super::workset::Pending = Arc::default();
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
                let bytes = senax_encoder::encode(&startup)
                    .map_err(|_| anyhow::anyhow!("encode workset startup"))?;
                socket.write_u32(bytes.len().try_into()?).await?;
                socket.write_all(&bytes).await?;
                let (sender, mut receiver, mut writer) = transport::connect(socket);
                let _ = connected.send(sender.clone());
                let (policy_incoming, policy_messages) = mpsc::channel(super::policy::MAX_REQUESTS);
                let policy = super::policy::serve(inference, sender.clone(), policy_messages);
                tokio::pin!(policy);
                let result = tokio::select! {
                    result = &mut policy => result,
                    result = &mut writer => result.context("workset writer failed")?.map_err(anyhow::Error::from),
                    result = async {
                        while let Some(message) = command_rx.recv().await {
                            if let super::workset::Message::Detach(port) = &message {
                                client_routes.lock().expect("poison").remove(port);
                            }
                            sender.send(transport::Port::Workset, super::workset::encode(&message)?).await?;
                        }
                        Ok::<(), anyhow::Error>(())
                    } => result,
                    result = async {
                        loop {
                            let packet = receiver.next().await?;
                            match packet.port {
                                transport::Port::Agent(id) => {
                                    let routes = routes.lock().expect("poison");
                                    if routes.get(&id).is_some_and(|route| route.try_send(packet.bytes).is_err()) {
                                        anyhow::bail!("agent route closed or overloaded");
                                    }
                                }
                                transport::Port::Workset => {
                                    match super::workset::decode(&packet.bytes)? {
                                        super::workset::Message::Reply { id, body } => {
                                            if let Some((reply, _admission)) = replies.lock().expect("poison").remove(&id) { let _ = reply.send(body); }
                                        }
                                        super::workset::Message::Policy(message) => {
                                            policy_incoming.try_send(message).map_err(|_| anyhow::anyhow!("workset policy route closed or overloaded"))?;
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
                                        let _ = routing_commands.send(super::workset::Message::Detach(port));
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
                _ = child.wait() => false,
                _ = &mut service => true,
            };
            let _ = disconnect.shutdown(std::net::Shutdown::Both);
            if !service_done {
                let _ = service.await;
            }
            if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            // Cancelled callers cannot release admission before an enqueued
            // operation replies or the execution process has actually exited.
            pending_close.lock().expect("poison").clear();
            // Mountpoint cleanup remains in the daemon's host-root frame.
            drop(root);
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
            #[cfg(test)]
            pid,
            admission,
            commands,
            clients,
            pending,
            sender,
            agents,
            next: Arc::new(AtomicU64::new(1)),
            closed: closed_rx,
            mode: view.workset_mode(),
            stop: Mutex::new(Some(stop)),
        }))
    }
}
