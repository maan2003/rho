//! Worker-owned runtimes and their local control dispatch. No database or pool.
use std::sync::Arc;

use anyhow::Context as _;
use tokio::net::UnixStream;

use super::ipc::{self, Control, Host, Message};
use crate::agent::{Agent, AgentHandle};
use crate::claude::{ClaudeAgent, ClaudeLoop};

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
            Control::User { content, delivery } => match self {
                Self::Rho(agent) => agent.send_user_content_accepted(content, delivery).await?,
                Self::Claude(agent) => agent.send_user_content_accepted(content).await?,
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
                        .send_agent_message_accepted(format!(
                            "Message Type: MESSAGE\nSender: {label}\nPayload:\n{body}"
                        ))
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

    async fn run(&self, host: &Host) -> anyhow::Result<()> {
        let mut controls = host.controls();
        while let Some((id, body)) = controls.recv().await {
            let retiring = matches!(body, Control::Retire);
            let error = self
                .apply(body)
                .await
                .err()
                .map(|error| format!("{error:#}"));
            let retired = retiring && error.is_none();
            host.send(Message::Controlled { id, error }).await?;
            if retired {
                std::future::pending::<()>().await;
            }
        }
        Ok(())
    }
}

async fn drive_native(
    mut runtime: Agent,
    handle: AgentHandle,
    host: Arc<Host>,
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
    host: Arc<Host>,
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

pub(super) async fn run(
    socket: UnixStream,
    startup: super::process::Startup,
    base: Arc<crate::View>,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    use super::transport::Port;
    let (sender, mut receiver, mut writer) = super::transport::connect(socket);
    let agents: Arc<
        std::sync::Mutex<
            HashMap<
                rho_agent_types::AgentId,
                tokio::sync::mpsc::UnboundedSender<super::transport::Packet>,
            >,
        >,
    > = Arc::default();
    let execution = Arc::new(super::workset::Execution {
        base: base.clone(),
        terminals: Arc::default(),
        shells: Arc::default(),
        clients: Arc::default(),
    });
    let next = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let policy = super::policy::Host::new(sender.clone(), next.clone());
    let devshell = super::devshell::Host::new(sender.clone(), next.clone());
    rho_devshell::install(rho_devshell::Resolver::new(
        Some(Arc::new(super::devshell::Cache(devshell.clone()))),
        base.devshell_cache().as_std_path().to_owned(),
        rho_fs_view::devshell_builder(),
        base.command_environment(),
    ));
    let inference = rho_inference::Inference::from_host(
        policy.clone(),
        rho_inference::InferenceConfig::with_responses_base_url(
            startup.responses_base_url.clone(),
        )?,
    );

    let mut tasks = tokio::task::JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = 'connection: loop {
        let receive = receiver.next();
        tokio::pin!(receive);
        let packet = loop {
            tokio::select! {
                _ = term.recv() => break 'connection Ok(()),
                result = &mut writer => break 'connection match result {
                    Ok(result) => result.map_err(anyhow::Error::from),
                    Err(error) => Err(error.into()),
                },
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result { break 'connection Err(error.into()); }
                    continue;
                }
                packet = &mut receive => match packet {
                    Ok(packet) => break packet,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break 'connection Ok(()),
                    Err(error) => break 'connection Err(error.into()),
                },
            }
        };
        let agent = match packet.port {
            Port::Agent(agent) => agent,
            Port::Workset => {
                let message = match super::workset::decode::<super::workset::Message>(&packet.bytes)
                {
                    Ok(message) => message,
                    Err(error) => break Err(error),
                };
                use super::workset::{Message as W, Reply};
                match message {
                    W::Policy(message) => {
                        if let Err(error) = policy.receive(message) {
                            break Err(error);
                        }
                    }
                    W::Devshell(message) => {
                        if let Err(error) = devshell.receive(message) {
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
                                    super::workset::encode(&W::Reply { id, body })
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
                                        super::workset::encode(&W::Reply {
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
        let message = match ipc::decode(&packet.bytes) {
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
        let host = Host::connect(sender.clone(), packet.port, messages, next.clone());
        let base = base.clone();
        let claude = startup.claude.clone();
        let inference = inference.clone();
        tasks.spawn(async move {
            let result = async {
                let namespace = base.for_cwd(&bootstrap.cwd)?;
                let view = Arc::new(crate::lazy::Lazy::new(move || {
                    let namespace = namespace.clone();
                    async move { Ok(namespace) }
                }));
                let head = host.head().await?;
                match head.config.runtime {
                    crate::db::AgentRuntime::Rho { .. } => {
                        let (handle, runtime) =
                            Agent::load(agent, host.clone(), inference, view).await?;
                        drive_native(runtime, handle, host.clone()).await
                    }
                    crate::db::AgentRuntime::Claude { .. } => {
                        let (handle, runtime) =
                            ClaudeLoop::load(agent, host.clone(), inference, claude, view).await?;
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
                    ipc::encode(&Message::Stopped {
                        error: result.err().map(|error| format!("{error:#}")),
                    })
                    .expect("encode stopped"),
                )
                .await;
        });
    };
    policy.disconnect();
    devshell.disconnect();
    agents.lock().expect("poison").clear();
    execution.clients.lock().expect("poison").clear();
    while tasks.join_next().await.is_some() {}
    tokio::join!(execution.terminals.shutdown(), execution.shells.shutdown());
    sender.close();
    writer.abort();
    result
}

/// Consume the inherited channel before Python or any child can inherit fd 0.
/// Must run before the runtime starts threads.
pub(super) fn control_socket() -> anyhow::Result<std::os::unix::net::UnixStream> {
    let channel = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdin(), 3)
        .context("duplicate agent control channel")?;
    let null = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(null)?;
    let socket = std::os::unix::net::UnixStream::from(channel);
    Ok(socket)
}
