//! Worker-owned runtimes and their local control dispatch. No database or pool.
use std::sync::Arc;

use anyhow::Context as _;
use tokio::net::UnixStream;

use super::ipc::{self, Control, Host, Message};
use crate::agent::{Agent, AgentHandle};
use crate::claude::{ClaudeAgent, ClaudeLoop};

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

/// The agent2 loop uses the same workset namespace and policy channel as the
/// legacy loops, but its own append-only log and chat projection.
async fn drive_chat(
    agent: rho_agent_types::AgentId,
    bootstrap: ipc::ChatBootstrap,
    base: Arc<crate::View>,
    policy: Arc<super::policy::Host>,
    sender: super::transport::Sender,
    base_url: String,
    mut incoming: tokio::sync::mpsc::UnboundedReceiver<super::transport::Packet>,
) -> anyhow::Result<()> {
    use rho_agent2::agent::{Agent as ChatAgent, Config, Inbound, Trace};
    use rho_agent2::log::{Block, Log};
    use rho_inference::InferenceHost as _;
    use rho_inference2::Model;
    use rho_inference2::openai::{AuthResolver, Effort, OpenAi};

    let port = super::transport::Port::Agent(agent);
    let setup = (|| {
        let cwd = base.for_cwd(&bootstrap.cwd)?;
        let path = base
            .state_dir()
            .join("agents")
            .join(agent.encoded())
            .join("log");
        std::fs::create_dir_all(path.parent().expect("agent log parent"))?;
        let resolve_auth: AuthResolver = Arc::new(move |_name| {
            let policy = policy.clone();
            Box::pin(async move {
                let (_, auth) = policy.select_resolved().await?;
                Ok(rho_inference::ResolvedOAuth {
                    bearer_token: auth.bearer_token,
                    account_id: auth.account_id,
                })
            })
        });
        let model = Arc::new(Model::OpenAiWithAuth {
            model: OpenAi {
                base_url,
                model: bootstrap.model,
                effort: bootstrap
                    .effort
                    .parse::<Effort>()
                    .map_err(anyhow::Error::msg)?,
                auth: "default".into(),
            },
            resolve_auth,
        });
        ChatAgent::new(Config {
            id: agent,
            log: Log::open(path.as_std_path())?,
            model,
            shell: rho_tool_shell::ShellTools::new(std::time::Duration::from_secs(20), cwd),
            instructions: rho_agent2::prompt::INSTRUCTIONS.into(),
        })
    })();
    let (runtime, handle) = match setup {
        Ok(pair) => {
            sender
                .send(port, ipc::encode(&Message::ChatStarted { error: None })?)
                .await?;
            pair
        }
        Err(error) => {
            sender
                .send(
                    port,
                    ipc::encode(&Message::ChatStarted {
                        error: Some(format!("{error:#}")),
                    })?,
                )
                .await?;
            return Err(error);
        }
    };
    let mut chat = handle.chat();
    let mut trace = handle.trace();
    let mut running = Box::pin(runtime.run());
    let result = loop {
        tokio::select! {
            result = &mut running => break result,
            packet = incoming.recv() => {
                let packet = packet.context("agent port closed")?;
                match ipc::decode(&packet.bytes)? {
                    Message::ChatSend { from, text } => handle.send(Inbound { from, body: vec![Block::Text(text)] })?,
                    Message::ChatArchive => handle.archive(),
                    Message::Stop => {
                        drop(handle);
                        // The notebook can still be running an admitted cell.
                        break tokio::time::timeout(std::time::Duration::from_secs(5), &mut running).await
                            .context("agent2 notebook did not stop")?;
                    }
                    _ => anyhow::bail!("unexpected agent2 control"),
                }
            }
            event = chat.recv() => match event {
                Ok(event) => sender.send(port, ipc::encode(&Message::Chat { event })?).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
            },
            event = trace.recv() => match event {
                Ok(Trace::ArchiveState { archived }) => sender.send(port, ipc::encode(&Message::ChatArchived { archived })?).await?,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
            },
        }
    };
    // `run` may finish immediately after publishing its final event. Deliver
    // those buffered events before Stopped makes the host release this route.
    while let Ok(event) = chat.try_recv() {
        sender
            .send(port, ipc::encode(&Message::Chat { event })?)
            .await?;
    }
    while let Ok(event) = trace.try_recv() {
        if let Trace::ArchiveState { archived } = event {
            sender
                .send(port, ipc::encode(&Message::ChatArchived { archived })?)
                .await?;
        }
    }
    result
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
    let devshell_dir = base.devshell_cache().as_std_path();
    let mut devshells = rho_devshell::Resolver::new(
        Some(rho_devshell::Client::new(devshell_dir)),
        devshell_dir.to_owned(),
        rho_fs_view::devshell_builder(),
        base.command_environment(),
    );
    match rho_watch::Watcher::global() {
        Ok(watcher) => devshells = devshells.with_watcher(watcher),
        Err(error) => eprintln!("rho: not watching dev shell inputs: {error}"),
    }
    rho_devshell::install(devshells);
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
                                .attach(port, attach, sender.clone(), messages, |id, body| {
                                    super::workset::encode(&W::Reply { id, body })
                                })
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
        if !matches!(message, Message::Bootstrap(_) | Message::ChatBootstrap(_)) {
            // The old agent has drained. Late service replies have no consumer.
            continue;
        }
        let (incoming, messages) = tokio::sync::mpsc::unbounded_channel();
        agents.lock().expect("poison").insert(agent, incoming);
        let agents = agents.clone();
        let sender = sender.clone();
        if let Message::ChatBootstrap(chat) = message {
            let agents = agents.clone();
            let base = base.clone();
            let policy = policy.clone();
            let url = startup.responses_base_url.clone();
            tasks.spawn(async move {
                let result =
                    drive_chat(agent, chat, base, policy, sender.clone(), url, messages).await;
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
            continue;
        }
        let Message::Bootstrap(bootstrap) = message else {
            unreachable!("checked bootstrap")
        };
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
