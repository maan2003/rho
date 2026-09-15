use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot};

use super::transport::{Port, Sender};

#[derive(Encode, Decode)]
pub enum Action {
    TerminalList,
    ShellList,
    ShellStart {
        agent: rho_core::AgentId,
        cwd: camino::Utf8PathBuf,
        program: std::path::PathBuf,
        pager: std::path::PathBuf,
    },
    ShellClose {
        agent: rho_core::AgentId,
    },
}

#[derive(Encode, Decode)]
pub enum Attach {
    Terminal {
        agent: rho_core::AgentId,
        terminal: u64,
        create: bool,
        cols: u16,
        rows: u16,
        cwd: camino::Utf8PathBuf,
        shell: String,
    },
    Shell {
        agent: rho_core::AgentId,
    },
}

#[derive(Encode, Decode)]
pub enum Reply {
    Done,
    Terminals(Vec<rho_ui_proto::term::TerminalInfo>),
    Shells(Vec<rho_ui_proto::shell::ShellInfo>),
    Error(String),
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
            while let Some((frame, _)) =
                rho_rpc::read_frame_optional::<_, I>(&mut reader, rho_ui_proto::MAX_FRAME_LEN)
                    .await?
            {
                self.sender.send(self.port, encode(&frame)?).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let output = async {
            while let Some(bytes) = self.incoming.recv().await {
                let frame: O = decode(&bytes)?;
                rho_rpc::write_frame(&mut writer, &frame, rho_ui_proto::MAX_FRAME_LEN).await?;
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
            Action::TerminalList => Reply::Terminals(
                self.terminals
                    .list()
                    .await
                    .into_iter()
                    .map(|entry| rho_ui_proto::term::TerminalInfo {
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
                    .map(|entry| rho_ui_proto::shell::ShellInfo {
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
                        use rho_ui_proto::term::TermClientFrame as F;

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
    use rho_ui_proto::shell::{ShellClientFrame as C, ShellServerFrame as S};

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
