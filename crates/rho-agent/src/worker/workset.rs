use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot};

use super::transport::{Port, Sender};

#[derive(Encode, Decode)]
pub enum Action {
    TerminalList,
    ShellList,
    ShellStart {
        agent: rho_agent_host_proto::AgentId,
        cwd: camino::Utf8PathBuf,
        program: std::path::PathBuf,
        pager: std::path::PathBuf,
    },
    ShellClose {
        agent: rho_agent_host_proto::AgentId,
    },
    Desktop {
        agent: rho_agent_host_proto::AgentId,
        session: String,
    },
    DesktopList,
}

#[derive(Encode, Decode)]
pub enum Attach {
    Terminal {
        agent: rho_agent_host_proto::AgentId,
        terminal: u64,
        create: bool,
        cols: u16,
        rows: u16,
        cwd: camino::Utf8PathBuf,
        shell: String,
    },
    Shell {
        agent: rho_agent_host_proto::AgentId,
    },
}

#[derive(Encode, Decode)]
pub enum Reply {
    Done,
    Terminals(Vec<rho_agent_host_proto::term::TerminalInfo>),
    Shells(Vec<rho_agent_host_proto::shell::ShellInfo>),
    Error(String),
    Desktop { socket: String },
    DesktopSessions(Vec<rho_agent_host_proto::DesktopSession>),
}

#[derive(Encode, Decode)]
pub(super) enum Message {
    Policy(super::policy::Message),
    Action { id: u64, action: Action },
    Attach { id: u64, port: Port, attach: Attach },
    Reply { id: u64, body: Reply },
    Detach(Port),
}

pub(super) fn encode<T: senax_encoder::Encoder>(value: &T) -> anyhow::Result<bytes::Bytes> {
    let mut bytes = bytes::BytesMut::new();
    senax_encoder::encode_to(value, &mut bytes)
        .map_err(|_| anyhow::anyhow!("encode workset message"))?;
    Ok(bytes.freeze())
}

pub(super) fn decode<T: senax_encoder::Decoder>(bytes: &[u8]) -> anyhow::Result<T> {
    let mut remaining = bytes;
    let value = senax_encoder::decode(&mut remaining)
        .map_err(|_| anyhow::anyhow!("invalid workset message"))?;
    anyhow::ensure!(remaining.is_empty(), "trailing workset message data");
    Ok(value)
}

/// One GUI attachment. It owns no PTY, shell, or transport connection.
pub struct Client {
    pub(super) port: Port,
    pub(super) sender: Sender,
    pub(super) incoming: mpsc::Receiver<bytes::Bytes>,
    pub(super) commands: mpsc::UnboundedSender<Message>,
}
impl Client {
    /// Relay one already-authenticated GUI stream without owning execution.
    pub async fn relay<R, W, I, O>(mut self, mut reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
        I: senax_encoder::Unpacker + senax_encoder::Encoder,
        O: senax_encoder::Decoder + senax_encoder::Packer,
    {
        let input = async {
            while let Some((frame, _)) = rho_rpc::read_frame_optional::<_, I>(
                &mut reader,
                rho_agent_host_proto::MAX_FRAME_LEN,
            )
            .await?
            {
                self.sender.send(self.port, encode(&frame)?).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let output = async {
            while let Some(bytes) = self.incoming.recv().await {
                let frame: O = decode(&bytes)?;
                rho_rpc::write_frame(&mut writer, &frame, rho_agent_host_proto::MAX_FRAME_LEN)
                    .await?;
            }
            tokio::io::AsyncWriteExt::shutdown(&mut writer).await?;
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! { result = input => result, result = output => result }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.commands.send(Message::Detach(self.port));
    }
}

pub(super) type Clients = Arc<Mutex<HashMap<Port, mpsc::Sender<bytes::Bytes>>>>;
pub(super) type Pending = Arc<
    Mutex<
        HashMap<
            u64,
            (
                oneshot::Sender<Reply>,
                Option<tokio::sync::OwnedRwLockReadGuard<()>>,
            ),
        >,
    >,
>;

/// Root-owned retained execution, independent of loaded agent handles.
pub(super) struct Execution {
    pub base: Arc<crate::View>,
    pub terminals: Arc<crate::terminal::TerminalRegistry>,
    pub shells: Arc<crate::shell::ShellRegistry>,
    pub clients: Clients,
}
impl Execution {
    pub async fn action(&self, action: Action) -> anyhow::Result<Reply> {
        Ok(match action {
            Action::Desktop { agent, session } => {
                anyhow::ensure!(
                    !session.is_empty()
                        && session
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
                    "invalid desktop session name"
                );
                let runtime =
                    std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?;
                let descriptor: serde_json::Value = serde_json::from_slice(
                    &tokio::fs::read(
                        std::path::PathBuf::from(runtime)
                            .join("rho-desktop/agents")
                            .join(agent.encoded())
                            .join(format!("{session}.json")),
                    )
                    .await
                    .context("No desktop is running for this agent; ask the agent to open an application")?,
                )?;
                Reply::Desktop {
                    socket: descriptor["socket"]
                        .as_str()
                        .context("desktop descriptor missing socket")?
                        .to_owned(),
                }
            }
            Action::DesktopList => Reply::DesktopSessions(desktop_sessions().await?),
            Action::TerminalList => Reply::Terminals(
                self.terminals
                    .list()
                    .await
                    .into_iter()
                    .map(|entry| rho_agent_host_proto::term::TerminalInfo {
                        agent: entry.agent_id.encoded(),
                        terminal_id: entry.terminal_id,
                        title: entry.title.unwrap_or_default(),
                        cols: entry.cols,
                        rows: entry.rows,
                        clients: entry.clients as u32,
                    })
                    .collect(),
            ),
            Action::ShellList => Reply::Shells(
                self.shells
                    .list()
                    .await
                    .into_iter()
                    .map(|entry| rho_agent_host_proto::shell::ShellInfo {
                        agent: entry.agent_id.encoded(),
                        clients: entry.clients as u32,
                    })
                    .collect(),
            ),
            Action::ShellStart {
                agent,
                cwd,
                program,
                pager,
            } => {
                self.shells
                    .start(
                        agent,
                        crate::shell::ShellSpawn {
                            view: self.base.for_cwd(&cwd)?,
                            program: program.into_os_string(),
                            args: Vec::new(),
                            pager_program: pager.into_os_string(),
                        },
                    )
                    .await?;
                Reply::Done
            }
            Action::ShellClose { agent } => {
                self.shells.close(agent).await?;
                Reply::Done
            }
        })
    }

    pub async fn attach(
        self: &Arc<Self>,
        port: Port,
        attach: Attach,
        sender: Sender,
        mut incoming: mpsc::Receiver<bytes::Bytes>,
    ) -> anyhow::Result<()> {
        match attach {
            Attach::Terminal {
                agent,
                terminal,
                create,
                cols,
                rows,
                cwd,
                shell,
            } => {
                let mut client = if create {
                    self.terminals
                        .create(
                            agent,
                            terminal,
                            cols,
                            rows,
                            crate::terminal::TerminalSpawn {
                                view: self.base.for_cwd(&cwd)?,
                                shell,
                            },
                        )
                        .await?
                } else {
                    self.terminals.attach(agent, terminal, cols, rows).await?
                };
                sender
                    .send(
                        Port::Workset,
                        encode(&Message::Reply {
                            id: match port {
                                Port::Terminal(id) => id,
                                _ => unreachable!(),
                            },
                            body: Reply::Done,
                        })?,
                    )
                    .await?;
                let output = async {
                    while let Some(frame) = client.frames.recv().await {
                        sender.send(port, encode(&frame)?).await?;
                    }
                    Ok::<(), anyhow::Error>(())
                };
                let input = async {
                    while let Some(bytes) = incoming.recv().await {
                        use rho_agent_host_proto::term::TermClientFrame as F;

                        use crate::terminal::ClientInput as I;
                        let input = match decode::<F>(&bytes)? {
                            F::Input(bytes) => I::Bytes(bytes),
                            F::Resize { cols, rows } => I::Resize { cols, rows },
                            F::Keystroke(key) => I::Keystroke(key),
                            F::Paste(text) => I::Paste(text),
                            F::Scroll {
                                lines,
                                col,
                                row,
                                ctrl,
                                alt,
                                shift,
                            } => I::Scroll {
                                lines,
                                col,
                                row,
                                ctrl,
                                alt,
                                shift,
                            },
                        };
                        if client.input.send(input).is_err() {
                            break;
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                };
                tokio::select! { result = output => result, result = input => result }
            }
            Attach::Shell { agent } => {
                let client = self.shells.attach(agent).await?;
                sender
                    .send(
                        Port::Workset,
                        encode(&Message::Reply {
                            id: match port {
                                Port::Shell(id) => id,
                                _ => unreachable!(),
                            },
                            body: Reply::Done,
                        })?,
                    )
                    .await?;
                serve_shell(client, incoming, sender, port).await
            }
        }
    }
}

async fn serve_shell(
    client: crate::shell::ShellClient,
    mut incoming: mpsc::Receiver<bytes::Bytes>,
    sender: Sender,
    port: Port,
) -> anyhow::Result<()> {
    use rho_agent_host_proto::shell::{ShellClientFrame as C, ShellServerFrame as S};

    use crate::shell::{ShellControl, ShellSubmitError};
    let crate::shell::ShellClient {
        mut frames,
        mut exit,
        submit,
        control,
    } = client;
    let (accepted, mut accepting) = mpsc::channel(crate::shell::SUBMIT_QUEUE);
    let output = async {
        loop {
            while let Ok((submission, execution)) = accepting.try_recv() {
                sender
                    .send(
                        port,
                        encode(&S::Accepted {
                            submission,
                            execution,
                        })?,
                    )
                    .await?;
            }
            let ended = { exit.borrow_and_update().clone() };
            if let Some(ended) = ended {
                sender
                    .send(
                        port,
                        encode(&S::Snapshot {
                            state: ended.state.clone(),
                        })?,
                    )
                    .await?;
                sender
                    .send(
                        port,
                        encode(&S::Exited {
                            status: ended.status,
                        })?,
                    )
                    .await?;
                return Ok::<(), anyhow::Error>(());
            }
            tokio::select! {
                biased;
                result = exit.changed() => { if result.is_err() { return Ok(()); } }
                message = accepting.recv() => {
                    let Some((submission, execution)) = message else { return Ok(()); };
                    sender.send(port, encode(&S::Accepted { submission, execution })?).await?;
                }
                frame = frames.recv() => {
                    let Some(frame) = frame else { return Ok(()); };
                    sender.send(port, encode(&frame)?).await?;
                }
            }
        }
    };
    let input = async {
        while let Some(bytes) = incoming.recv().await {
            match decode::<C>(&bytes)? {
                C::Submit {
                    submission,
                    command,
                } => {
                    let execution = submit.try_send(command).map_err(|error| {
                        anyhow::anyhow!(match error {
                            ShellSubmitError::Full => "shell command queue is full",
                            ShellSubmitError::Closed => "shell closed",
                            ShellSubmitError::Exhausted => "shell execution ids exhausted",
                            ShellSubmitError::TooLarge => "shell command exceeds the input limit",
                        })
                    })?;
                    accepted.send((submission, execution)).await?;
                }
                C::Interrupt => control
                    .send(ShellControl::Interrupt)
                    .await
                    .map_err(|_| anyhow::anyhow!("shell closed"))?,
                C::Eof => control
                    .send(ShellControl::Eof)
                    .await
                    .map_err(|_| anyhow::anyhow!("shell closed"))?,
                C::PagerAction {
                    execution,
                    pager,
                    page,
                    action,
                } => control
                    .pager_action(execution, pager, page, action)
                    .await
                    .map_err(|_| anyhow::anyhow!("shell closed"))?,
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! { result = output => result, result = input => result }
}

// Advertisements are ephemeral: starting a desktop atomically publishes one,
// orderly stop removes it, and the lifetime lock excludes leftovers after a
// crash.
async fn desktop_sessions() -> anyhow::Result<Vec<rho_agent_host_proto::DesktopSession>> {
    let mut sessions = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            return Ok(sessions);
        };
        let root = std::path::PathBuf::from(runtime).join("rho-desktop/agents");
        let mut agents = match tokio::fs::read_dir(root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
            Err(error) => return Err(error.into()),
        };
        while let Some(agent) = agents.next_entry().await? {
            if !agent.file_type().await?.is_dir() {
                continue;
            }
            let mut entries = match tokio::fs::read_dir(agent.path()).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while let Some(entry) = entries.next_entry().await? {
                if entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "json")
                {
                    continue;
                }
                let Ok(bytes) = tokio::fs::read(entry.path()).await else {
                    continue;
                };
                let Ok(advertisement) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                let (Some(owner), Some(name), Some(_socket)) = (
                    advertisement["agent"].as_str(),
                    advertisement["name"].as_str(),
                    advertisement["socket"].as_str(),
                ) else {
                    continue;
                };
                if agent.file_name() != owner
                    || entry.file_name() != format!("{name}.json").as_str()
                {
                    continue;
                }
                // The compositor holds this lock from before publishing until
                // shutdown. Checking it cannot block on a full socket backlog.
                use std::os::fd::AsRawFd;
                let Ok(lock) = tokio::fs::File::open(entry.path().with_extension("lock")).await
                else {
                    continue;
                };
                if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
                {
                    sessions.push(rho_agent_host_proto::DesktopSession {
                        agent: owner.to_owned(),
                        name: name.to_owned(),
                    });
                }
            }
        }
    }
    sessions.sort();
    sessions.dedup();
    Ok(sessions)
}
