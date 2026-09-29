//! Worker-owned runtimes and their local control dispatch. No database or pool.
use std::sync::Arc;

use anyhow::Context as _;
use tokio::net::UnixStream;

use super::host_client::HostClient;
use crate::ipc::protocol::{self, Control, Message};
use crate::ipc::{transport, workset};
use crate::worker::claude::{ClaudeAgent, ClaudeLoop};
use crate::worker::native::{Agent, AgentHandle};

/// How long one agent may take to finish the request in flight when drained.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(50);

enum Controller {
    Rho(AgentHandle),
    Claude(ClaudeAgent),
}

impl Controller {
    async fn apply(&self, control: Control) -> anyhow::Result<()> {
        match control {
            Control::Retire => match self {
                Self::Rho(agent) => agent.retire().await?,
                Self::Claude(agent) => agent.retire().await?,
            },
            // Bounded here, inside the workset, so the agent host always hears
            // back before its own stop deadline (`DRAIN_TIMEOUT` there).
            Control::Drain => match self {
                Self::Rho(agent) => {
                    if tokio::time::timeout(DRAIN_TIMEOUT, agent.drain())
                        .await
                        .is_err()
                    {
                        anyhow::bail!("request still in flight after {DRAIN_TIMEOUT:?}");
                    }
                }
                // Claude Code keeps its own conversation; there is no tail
                // of ours to flush.
                Self::Claude(_) => {}
            },
            Control::User { id, content } => match self {
                Self::Rho(agent) => agent.send_user_content_accepted(id, content).await?,
                Self::Claude(agent) => agent.send_user_content_accepted(id, content).await?,
            },
            Control::Mail {
                sender,
                label,
                body,
                ..
            } => match self {
                Self::Rho(agent) => agent.send_agent_message_accepted(sender, body).await?,
                Self::Claude(agent) => {
                    agent
                        .send_agent_message_accepted(
                            sender,
                            format!("Message Type: MESSAGE\nSender: {label}\nPayload:\n{body}"),
                        )
                        .await?
                }
            },
            Control::NoticeCarried => match self {
                Self::Rho(agent) => agent.notice_carried(),
                Self::Claude(agent) => agent.notice_carried(),
            },
            Control::TellTail => match self {
                Self::Rho(agent) => agent.tell_tail(),
                Self::Claude(agent) => agent.tell_tail(),
            },
            Control::Compact => match self {
                Self::Rho(agent) => agent.compact(),
                Self::Claude(agent) => agent.compact(),
            },
            Control::Cancel => match self {
                Self::Rho(agent) => agent.cancel(),
                Self::Claude(agent) => agent.cancel(),
            },
            Control::Retry => match self {
                Self::Rho(agent) => agent.retry(),
                Self::Claude(_) => {}
            },
            Control::Effort(effort) => match self {
                Self::Claude(agent) => agent.set_effort(effort).await?,
                Self::Rho(_) => anyhow::bail!("cannot apply Claude effort to Rho agent"),
            },
            Control::Role(role) => match self {
                Self::Rho(agent) => agent.change_role(role).await?,
                Self::Claude(agent) => agent.change_role(role).await?,
            },
            Control::CacheKey => match self {
                Self::Rho(agent) => agent.change_prompt_cache_key(),
                Self::Claude(_) => {
                    anyhow::bail!("prompt cache keys are only available for Rho agents")
                }
            },
            Control::Rewind(turns) => match self {
                Self::Rho(agent) => agent.rewind(turns).await?,
                Self::Claude(agent) => agent.rewind(turns).await?,
            },
        }
        Ok(())
    }

    async fn run(&self, host: &HostClient) -> anyhow::Result<()> {
        let mut controls = host.controls();
        while let Some((id, body)) = controls.recv().await {
            let retiring = matches!(body, Control::Retire);
            let draining = matches!(body, Control::Drain);
            let error = self
                .apply(body)
                .await
                .err()
                .map(|error| format!("{error:#}"));
            let retired = retiring && error.is_none();
            host.send(Message::Controlled { id, error }).await?;
            // A drained agent takes no more input, whether or not its request
            // ended in time: the agent host is on its way down.
            if retired || draining {
                std::future::pending::<()>().await;
            }
        }
        Ok(())
    }
}

async fn drive_native(
    mut runtime: Agent,
    handle: AgentHandle,
    host: Arc<HostClient>,
) -> anyhow::Result<()> {
    let status = handle.status();
    let control = Controller::Rho(handle);
    host.send(Message::Ready { status }).await?;
    let result = tokio::select! {
        biased;
        _ = host.closed() => Ok(()),
        result = runtime.run() => result,
        result = control.run(&host) => result,
    };
    let cleanup = runtime.shutdown().await;
    result.and(cleanup)
}

async fn drive_claude(
    mut runtime: ClaudeLoop,
    handle: ClaudeAgent,
    host: Arc<HostClient>,
) -> anyhow::Result<()> {
    let status = handle.status();
    let control = Controller::Claude(handle);
    host.send(Message::Ready { status }).await?;
    let result = tokio::select! {
        biased;
        _ = host.closed() => Ok(()),
        result = runtime.run() => result,
        result = control.run(&host) => result,
    };
    let cleanup = runtime.shutdown().await;
    result.and(cleanup)
}

pub(crate) async fn run(
    socket: UnixStream,
    requests: UnixStream,
    startup: protocol::Startup,
    factory: crate::inference::WorkerFactory,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    use transport::Port;
    let (sender, mut receiver, mut writer) = transport::connect(socket);
    let (requests, mut answers, mut requests_writer) = transport::connect(requests);
    let agents: Arc<
        std::sync::Mutex<
            HashMap<
                rho_agent_types::AgentId,
                tokio::sync::mpsc::UnboundedSender<transport::Packet>,
            >,
        >,
    > = Arc::default();
    let execution = Arc::new(super::workset::Execution {
        terminals: Arc::default(),
        shells: Arc::default(),
        clients: Arc::default(),
    });
    let next = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let policy_sender: crate::inference::PolicySender = Arc::new({
        let sender = requests.clone();
        move |bytes| {
            let sender = sender.clone();
            Box::pin(async move {
                sender
                    .send(
                        Port::Workset,
                        workset::encode(&workset::Message::Policy(bytes))?,
                    )
                    .await?;
                Ok(())
            })
        }
    });
    let provider = factory(&startup.responses_base_url, policy_sender)?;
    let inference = provider.inference;
    let policy = provider.policy;
    let devshell_path = startup.layout.cache.join("rho-devshell");
    let devshell_dir = devshell_path.as_std_path();
    let mut devshells = rho_devshell::Resolver::new(
        Some(rho_devshell::Client::new(devshell_dir)),
        devshell_dir.to_owned(),
        rho_fs_view::devshell_builder(),
        std::env::vars_os().collect(),
    );
    match rho_watch::Watcher::global() {
        Ok(watcher) => devshells = devshells.with_watcher(watcher),
        Err(error) => eprintln!("rho: not watching dev shell inputs: {error}"),
    }
    rho_devshell::install(devshells);
    let mut tasks = tokio::task::JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = 'connection: loop {
        let packet = loop {
            let packet = tokio::select! {
                _ = term.recv() => break 'connection Ok(()),
                result = &mut writer => break 'connection match result {
                    Ok(result) => result.map_err(anyhow::Error::from),
                    Err(error) => Err(error.into()),
                },
                result = &mut requests_writer => break 'connection match result {
                    Ok(result) => result.map_err(anyhow::Error::from),
                    Err(error) => Err(error.into()),
                },
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result { break 'connection Err(error.into()); }
                    continue;
                }
                packet = receiver.next() => packet,
                packet = answers.next() => packet,
            };
            match packet {
                Ok(packet) => break packet,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break 'connection Ok(());
                }
                Err(error) => break 'connection Err(error.into()),
            }
        };
        let agent = match packet.port {
            Port::Agent(agent) => agent,
            Port::Workset => {
                let message = match workset::decode::<workset::Message>(&packet.bytes) {
                    Ok(message) => message,
                    Err(error) => break Err(error),
                };
                use workset::{Message as W, Reply};
                match message {
                    W::Policy(message) => {
                        if let Err(error) = policy.receive(&message) {
                            break Err(error);
                        }
                    }
                    W::Action { id, action } => {
                        let execution = execution.clone();
                        let sender = sender.clone();
                        tasks.spawn(async move {
                            let body = execution
                                .action(action)
                                .await
                                .unwrap_or_else(|error| Reply::Error(format!("{error:#}")));
                            let _ = sender
                                .send(
                                    Port::Workset,
                                    workset::encode(&W::Reply { id, body })
                                        .expect("encode workset reply"),
                                )
                                .await;
                        });
                    }
                    W::Attach { id, port, attach } => {
                        let (incoming, messages) = tokio::sync::mpsc::channel(32);
                        execution
                            .clients
                            .lock()
                            .expect("poison")
                            .insert(port, incoming);
                        let execution = execution.clone();
                        let sender = sender.clone();
                        tasks.spawn(async move {
                            if let Err(error) = execution
                                .attach(port, attach, sender.clone(), messages)
                                .await
                            {
                                let _ = sender
                                    .send(
                                        Port::Workset,
                                        workset::encode(&W::Reply {
                                            id,
                                            body: Reply::Error(format!("{error:#}")),
                                        })
                                        .expect("encode attach error"),
                                    )
                                    .await;
                            }
                            execution.clients.lock().expect("poison").remove(&port);
                            // Empty logical payload is GUI-port EOF, ordered after its final frame.
                            let _ = sender.send(port, bytes::Bytes::new()).await;
                        });
                    }
                    W::Detach(port) => {
                        execution.clients.lock().expect("poison").remove(&port);
                    }
                    _ => break Err(anyhow::anyhow!("unexpected workset control")),
                }
                continue;
            }
            port @ (Port::Terminal(_) | Port::Shell(_)) => {
                let mut clients = execution.clients.lock().expect("poison");
                if clients
                    .get(&port)
                    .is_some_and(|client| client.try_send(packet.bytes).is_err())
                {
                    clients.remove(&port);
                }
                continue;
            }
        };
        {
            let agents = agents.lock().expect("poison");
            if let Some(incoming) = agents.get(&agent) {
                // A retiring agent may have dropped its receiver. Its late replies
                // are no different from replies after the route is unregistered.
                let _ = incoming.send(packet);
                continue;
            }
        }
        let message = match protocol::decode(&packet.bytes) {
            Ok(message) => message,
            Err(error) => break Err(error.into()),
        };
        let Message::Bootstrap(bootstrap) = message else {
            // The old agent has drained. Late service replies have no consumer.
            continue;
        };
        let (incoming, messages) = tokio::sync::mpsc::unbounded_channel();
        agents.lock().expect("poison").insert(agent, incoming);
        let agents = agents.clone();
        let sender = sender.clone();
        let host = HostClient::connect(
            sender.clone(),
            requests.clone(),
            packet.port,
            messages,
            next.clone(),
        );
        let claude = startup.claude.clone();
        let inference = inference.clone();
        tasks.spawn(async move {
            let result = async {
                let cwd = bootstrap.cwd;
                anyhow::ensure!(
                    cwd.is_absolute() && cwd.is_dir(),
                    "working directory does not exist: {cwd}"
                );
                let head = host.head().await?;
                match head.config.runtime {
                    crate::log::AgentRuntime::Rho { .. } => {
                        let (handle, runtime) =
                            Agent::load(agent, host.clone(), inference, cwd).await?;
                        drive_native(runtime, handle, host.clone()).await
                    }
                    crate::log::AgentRuntime::Claude { .. } => {
                        let (handle, runtime) =
                            ClaudeLoop::load(agent, host.clone(), inference, claude, cwd).await?;
                        drive_claude(runtime, handle, host.clone()).await
                    }
                }
            }
            .await;
            host.shutdown().await;
            agents.lock().expect("poison").remove(&agent);
            let _ = sender
                .send(
                    packet.port,
                    protocol::encode(&Message::Stopped {
                        error: result.err().map(|error| format!("{error:#}")),
                    })
                    .expect("encode stopped"),
                )
                .await;
        });
    };
    policy.disconnect();
    agents.lock().expect("poison").clear();
    execution.clients.lock().expect("poison").clear();
    while tasks.join_next().await.is_some() {}
    tokio::join!(execution.terminals.shutdown(), execution.shells.shutdown());
    sender.close();
    requests.close();
    writer.abort();
    requests_writer.abort();
    result
}

/// Consume the inherited channel before Python or any child can inherit fd 0.
/// Must run before the runtime starts threads.
pub(crate) fn control_socket() -> anyhow::Result<std::os::unix::net::UnixStream> {
    let channel = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdin(), 3)
        .context("duplicate agent control channel")?;
    let null = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(null)?;
    let socket = std::os::unix::net::UnixStream::from(channel);
    Ok(socket)
}

/// Claims the inherited [`protocol::REQUESTS_FD`] before any child can
/// inherit it. Must run before the runtime starts threads.
pub(crate) fn requests_socket() -> anyhow::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::FromRawFd as _;
    // SAFETY: the agent host passes the connection at this fd, and nothing
    // else in this process claims it.
    let channel = unsafe { std::os::fd::OwnedFd::from_raw_fd(protocol::REQUESTS_FD) };
    rustix::io::fcntl_setfd(&channel, rustix::io::FdFlags::CLOEXEC)
        .context("inherit agent request channel")?;
    Ok(std::os::unix::net::UnixStream::from(channel))
}
