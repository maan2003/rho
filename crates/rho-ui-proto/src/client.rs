use std::path::Path;
use std::sync::Arc;

use futures::Stream;
use rho_core::{ContentPart, ContextBlock};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use crate::{ClientMessage, ServerMessage, read_frame, write_frame};

/// Raw async client for the rho UI Unix-socket protocol.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self { stream })
    }

    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    pub async fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        write_frame(&mut self.stream, message).await
    }

    pub async fn recv(&mut self) -> anyhow::Result<ServerMessage> {
        read_frame(&mut self.stream).await
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// Typed client handle for controlling and observing a rho agent over the UI
/// protocol.
#[derive(Clone)]
pub struct AgentClient {
    commands: mpsc::UnboundedSender<ClientMessage>,
    state: watch::Receiver<rho_agent::AgentState>,
}

impl AgentClient {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::connect_client(Client::connect(path).await?).await
    }

    pub async fn connect_client(client: Client) -> anyhow::Result<Self> {
        let mut stream = client.into_stream();
        write_frame(&mut stream, &ClientMessage::Subscribe).await?;
        let ServerMessage::Agent(frame) = read_frame(&mut stream).await? else {
            anyhow::bail!("rho daemon did not send initial agent state");
        };
        let crate::remote::AgentRemoteFrame::Snapshot(initial_state) = frame else {
            anyhow::bail!("rho daemon sent diff before snapshot");
        };

        let (reader, writer) = stream.into_split();
        let (state_tx, state_rx) = watch::channel(initial_state);
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<ClientMessage>();

        tokio::spawn(async move {
            let mut reader = reader;
            let mut state = state_tx.borrow().clone();
            loop {
                let message = match read_frame::<_, ServerMessage>(&mut reader).await {
                    Ok(message) => message,
                    Err(_) => break,
                };
                match message {
                    ServerMessage::Agent(frame) => {
                        frame.apply_diff(&mut state);
                        if state_tx.send(state.clone()).is_err() {
                            break;
                        }
                    }
                    ServerMessage::Error { message } => eprintln!("rho daemon error: {message}"),
                    ServerMessage::Pong | ServerMessage::TurnCancelled => {}
                }
            }
        });

        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(message) = command_rx.recv().await {
                if write_frame(&mut writer, &message).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            commands: command_tx,
            state: state_rx,
        })
    }

    pub fn blocks(&self) -> Vec<Arc<ContextBlock>> {
        self.state.borrow().blocks.clone()
    }

    pub fn state(&self) -> rho_agent::AgentState {
        self.state.borrow().clone()
    }

    pub fn send_user_message(&self, text: String) {
        let _ = self.commands.send(ClientMessage::SendUserMessage {
            content: vec![ContentPart::Text { text }],
        });
    }

    pub fn cancel(&self) {
        let _ = self.commands.send(ClientMessage::CancelTurn);
    }

    pub fn subscribe(&self) -> impl Stream<Item = rho_agent::AgentState> + use<> {
        let mut state = self.state.clone();
        async_stream::stream! {
            while state.changed().await.is_ok() {
                let current = state.borrow().clone();
                yield current;
            }
        }
    }
}
