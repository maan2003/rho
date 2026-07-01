//! Daemon connection: a dedicated thread running a tokio reactor, bridged to
//! the GUI through channels. Inbound server messages become [`ConnEvent`]s on
//! a futures channel the workspace awaits (no polling); outbound commands are
//! fire-and-forget.

use std::path::PathBuf;

use anyhow::Context as _;
use futures::channel::mpsc as futures_mpsc;
use rho_ui_proto::client::Client;
use rho_ui_proto::remote::AgentRemoteFrame;
use rho_ui_proto::{AgentId, ClientMessage, ServerMessage, UiTopic, read_frame, write_frame};
use tokio::sync::mpsc as tokio_mpsc;

pub enum ConnEvent {
    Ready { topics: Vec<UiTopic> },
    TopicCreated(UiTopic),
    AgentAnnounced(AgentId),
    Frame { agent_id: AgentId, frame: AgentRemoteFrame },
    TurnCancelled(AgentId),
    ServerError(String),
    Disconnected(String),
}

pub struct Connection {
    commands: tokio_mpsc::UnboundedSender<ClientMessage>,
}

impl Connection {
    pub fn send(&self, message: ClientMessage) {
        let _ = self.commands.send(message);
    }
}

pub fn spawn(socket_path: PathBuf) -> (Connection, futures_mpsc::UnboundedReceiver<ConnEvent>) {
    let (event_tx, event_rx) = futures_mpsc::unbounded();
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = event_tx.unbounded_send(ConnEvent::Disconnected(format!(
                    "failed to start async runtime: {error:#}"
                )));
                return;
            }
        };
        if let Err(error) = runtime.block_on(run(socket_path, &event_tx, command_rx)) {
            let _ = event_tx.unbounded_send(ConnEvent::Disconnected(format!("{error:#}")));
        }
    });
    (
        Connection {
            commands: command_tx,
        },
        event_rx,
    )
}

async fn run(
    socket_path: PathBuf,
    events: &futures_mpsc::UnboundedSender<ConnEvent>,
    mut commands: tokio_mpsc::UnboundedReceiver<ClientMessage>,
) -> anyhow::Result<()> {
    let client = Client::connect(&socket_path)
        .await
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    let mut stream = client.into_stream();
    write_frame(&mut stream, &ClientMessage::Subscribe).await?;
    let message: ServerMessage = read_frame(&mut stream).await?;
    let ServerMessage::Ready { topics } = message else {
        anyhow::bail!("rho daemon did not send ready message");
    };
    if events.unbounded_send(ConnEvent::Ready { topics }).is_err() {
        return Ok(());
    }

    let (mut reader, mut writer) = stream.into_split();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = commands.recv().await {
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
            ServerMessage::Ready { topics } => Some(ConnEvent::Ready { topics }),
            ServerMessage::TopicCreated { topic } => Some(ConnEvent::TopicCreated(topic)),
            ServerMessage::AgentCreated { agent_id, .. }
            | ServerMessage::AgentLoaded { agent_id } => {
                Some(ConnEvent::AgentAnnounced(agent_id))
            }
            ServerMessage::Agent { agent_id, frame } => {
                Some(ConnEvent::Frame { agent_id, frame })
            }
            ServerMessage::TurnCancelled { agent_id } => Some(ConnEvent::TurnCancelled(agent_id)),
            ServerMessage::Error { message } => Some(ConnEvent::ServerError(message)),
            ServerMessage::Pong => None,
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
