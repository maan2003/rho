//! Daemon connection: an IO task on the shared tokio runtime ([`gpui_tokio`]),
//! bridged to the GUI through channels. Inbound server messages become
//! [`ConnEvent`]s on a futures channel the workspace awaits (no polling);
//! outbound commands are fire-and-forget.

use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Context as _;
use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::{App, Task};
use gpui_tokio::Tokio;
use rho_ui_proto::client::Client;
use rho_ui_proto::remote::AgentRemoteFrame;
use rho_ui_proto::{
    AgentId, ClientMessage, ServerMessage, UiTopic, UiWorkdir, VoiceRole, VoiceState,
    VoiceUiAction, read_frame, write_frame,
};

use crate::workspace::AttachTarget;

trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncStream for T {}

struct IrohStream {
    inner: tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    _endpoint: iroh::Endpoint,
}

impl tokio::io::AsyncRead for IrohStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for IrohStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub enum ConnEvent {
    Ready {
        topics: Vec<UiTopic>,
        workdirs: Vec<UiWorkdir>,
        default_topic_id: rho_ui_proto::TopicId,
        machine_seed: u64,
        agent_counter: u64,
        workspace_counter: u64,
    },
    TopicCreated(UiTopic),
    /// The daemon created an agent this connection asked for.
    AgentCreated(AgentId),
    AgentLoaded(AgentId),
    Frame {
        agent_id: AgentId,
        frame: AgentRemoteFrame,
    },
    TurnCancelled,
    AgentAttention {
        agent_id: AgentId,
        attention: rho_ui_proto::UiAttention,
    },
    ServerError(String),
    Enrollment(String),
    Disconnected(String),
    /// Assistant audio (wire-format PCM16) for immediate playback.
    VoiceAudio(Vec<u8>),
    /// The user barged in; drop buffered playback now.
    VoiceFlushPlayback,
    VoiceState(VoiceState),
    VoiceTranscript {
        role: VoiceRole,
        text: String,
    },
    VoiceUiAction(VoiceUiAction),
}

pub struct Connection {
    commands: futures_mpsc::UnboundedSender<ClientMessage>,
    /// Dropping this aborts the IO task, tearing the connection down with the
    /// workspace.
    _io_task: Task<Result<(), gpui_tokio::JoinError>>,
}

impl Connection {
    pub fn send(&self, message: ClientMessage) {
        let _ = self.commands.unbounded_send(message);
    }

    /// A handle other threads (the microphone capture thread) can send
    /// through without touching the gpui entity.
    pub fn sender(&self) -> futures_mpsc::UnboundedSender<ClientMessage> {
        self.commands.clone()
    }
}

pub fn spawn(
    target: AttachTarget,
    cx: &App,
) -> (Connection, futures_mpsc::UnboundedReceiver<ConnEvent>) {
    let (event_tx, event_rx) = futures_mpsc::unbounded();
    let (command_tx, command_rx) = futures_mpsc::unbounded();
    let io_task = Tokio::spawn(cx, async move {
        if let Err(error) = run(target, &event_tx, command_rx).await {
            let _ = event_tx.unbounded_send(ConnEvent::Disconnected(format!("{error:#}")));
        }
    });
    (
        Connection {
            commands: command_tx,
            _io_task: io_task,
        },
        event_rx,
    )
}

async fn run(
    target: AttachTarget,
    events: &futures_mpsc::UnboundedSender<ConnEvent>,
    mut commands: futures_mpsc::UnboundedReceiver<ClientMessage>,
) -> anyhow::Result<()> {
    let mut stream = match target {
        AttachTarget::Unix(socket_path) => {
            let client = Client::connect(&socket_path)
                .await
                .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
            Box::new(client.into_stream()) as Box<dyn AsyncStream>
        }
        AttachTarget::Iroh {
            endpoint_id,
            secret_path,
        } => connect_iroh(endpoint_id, &secret_path, events).await?,
    };
    write_frame(&mut stream, &ClientMessage::Subscribe).await?;
    let message: ServerMessage = read_frame(&mut stream).await?;
    let ServerMessage::Ready {
        topics,
        workdirs,
        default_topic_id,
        machine_seed,
        agent_counter,
        workspace_counter,
    } = message
    else {
        anyhow::bail!("rho daemon did not send ready message");
    };
    if events
        .unbounded_send(ConnEvent::Ready {
            topics,
            workdirs,
            default_topic_id,
            machine_seed,
            agent_counter,
            workspace_counter,
        })
        .is_err()
    {
        return Ok(());
    }

    let (mut reader, mut writer) = tokio::io::split(stream);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = commands.next().await {
            if write_frame(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });

    loop {
        let message: ServerMessage = match read_frame(&mut reader).await {
            Ok(message) => message,
            Err(error) => {
                let _ = events.unbounded_send(ConnEvent::Disconnected(error.to_string()));
                break;
            }
        };
        let event = match message {
            ServerMessage::Ready {
                topics,
                workdirs,
                default_topic_id,
                machine_seed,
                agent_counter,
                workspace_counter,
            } => Some(ConnEvent::Ready {
                topics,
                workdirs,
                default_topic_id,
                machine_seed,
                agent_counter,
                workspace_counter,
            }),
            ServerMessage::TopicCreated { topic } => Some(ConnEvent::TopicCreated(topic)),
            ServerMessage::AgentCreated { agent_id, .. } => Some(ConnEvent::AgentCreated(agent_id)),
            ServerMessage::AgentLoaded { agent_id } => Some(ConnEvent::AgentLoaded(agent_id)),
            ServerMessage::Agent { agent_id, frame } => Some(ConnEvent::Frame { agent_id, frame }),
            ServerMessage::TurnCancelled { .. } => Some(ConnEvent::TurnCancelled),
            ServerMessage::AgentAttention {
                agent_id,
                attention,
            } => Some(ConnEvent::AgentAttention {
                agent_id,
                attention,
            }),
            ServerMessage::Error { message } => Some(ConnEvent::ServerError(message)),
            ServerMessage::VoiceAudio { pcm } => Some(ConnEvent::VoiceAudio(pcm)),
            ServerMessage::VoiceFlushPlayback => Some(ConnEvent::VoiceFlushPlayback),
            ServerMessage::VoiceState { state } => Some(ConnEvent::VoiceState(state)),
            ServerMessage::VoiceTranscript { role, text } => {
                Some(ConnEvent::VoiceTranscript { role, text })
            }
            ServerMessage::VoiceUiAction(action) => Some(ConnEvent::VoiceUiAction(action)),
            ServerMessage::Pong
            | ServerMessage::LandLeaseQueued { .. }
            | ServerMessage::LandLeaseGranted { .. }
            | ServerMessage::LandStatus { .. }
            | ServerMessage::McpAgentToolResult(_)
            | ServerMessage::PlatformStatus { .. }
            | ServerMessage::IrohApproved { .. } => None,
        };
        if let Some(event) = event
            && events.unbounded_send(event).is_err()
        {
            break;
        }
    }
    writer_task.abort();
    Ok(())
}

async fn connect_iroh(
    daemon_id: iroh::EndpointId,
    secret_path: &Path,
    events: &futures_mpsc::UnboundedSender<ConnEvent>,
) -> anyhow::Result<Box<dyn AsyncStream>> {
    use rho_iroh_auth::EnrollmentCodeExt as _;

    let (secret, needs_persist) = load_iroh_secret(secret_path)?;
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret.clone())
        .bind()
        .await
        .context("bind iroh client endpoint")?;
    let connection = endpoint
        .connect(daemon_id, rho_ui_proto::IROH_ALPN)
        .await
        .context("connect to daemon over iroh")?;
    let open_stream = connection.open_bi();
    tokio::pin!(open_stream);
    let (send, recv) = tokio::select! {
        stream = &mut open_stream => stream,
        () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
            let code = connection.enrollment_code(endpoint.id());
            let _ = events.unbounded_send(ConnEvent::Enrollment(code.to_string()));
            open_stream.await
        }
    }
    .context("open iroh UI stream")?;
    if needs_persist {
        persist_iroh_secret(secret_path, &secret)?;
    }
    Ok(Box::new(IrohStream {
        inner: tokio::io::join(recv, send),
        _endpoint: endpoint,
    }))
}

fn load_iroh_secret(path: &Path) -> anyhow::Result<(iroh::SecretKey, bool)> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                anyhow::anyhow!("iroh secret file {} is not 32 bytes", path.display())
            })?;
            Ok((iroh::SecretKey::from_bytes(&bytes), false))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((iroh::SecretKey::generate(), true))
        }
        Err(error) => Err(error).context("read rho-gui iroh secret"),
    }
}

fn persist_iroh_secret(path: &Path, secret: &iroh::SecretKey) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create rho-gui state directory")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .context("create rho-gui iroh secret")?;
    file.write_all(&secret.to_bytes())
        .context("write rho-gui iroh secret")
}
