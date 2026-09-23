//! Both of a daemon's streams held at once, for a harness that reads the
//! journal and drives agents in one loop.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use rho_agent_host_proto::agents::{ClientFrame, ServerFrame};
use rho_agent_host_proto::client::Client;
use rho_agent_host_proto::transcript::Seq;
use rho_agent_host_proto::{ClientMessage, ServerMessage, read_frame, write_frame};
use tokio::io::WriteHalf;
use tokio::sync::mpsc;

/// A frame from either stream, in the order it was read.
pub enum Incoming {
    Control(ServerMessage),
    Agents(ServerFrame),
}

pub struct Streams {
    control: WriteHalf<rho_rpc::Stream>,
    agents: WriteHalf<rho_rpc::Stream>,
    incoming: mpsc::UnboundedReceiver<Result<Incoming>>,
    /// How far the journal ran when the agents stream opened.
    pub journal_head: Seq,
}

impl Streams {
    /// Subscribes on `control`, waits for `Ready`, and opens an agents
    /// stream beside it. Nothing is followed yet.
    pub async fn open(mut control: Client, socket: &Path) -> Result<Self> {
        control.send(&ClientMessage::Subscribe).await?;
        loop {
            match control.recv().await? {
                ServerMessage::Ready { .. } => break,
                ServerMessage::Error { message } => bail!("daemon readiness error: {message}"),
                _ => {}
            }
        }
        let mut agents = Client::connect(socket)
            .await
            .context("open an agents stream")?
            .into_stream();
        write_frame(&mut agents, &ClientMessage::AgentsOpen).await?;
        let ServerFrame::JournalHead { journal_head, .. } = read_frame(&mut agents).await? else {
            bail!("the agents stream did not open with its journal head");
        };
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let (mut control_read, control) = tokio::io::split(control.into_stream());
        let (mut agents_read, agents) = tokio::io::split(agents);
        let tx = incoming_tx.clone();
        tokio::spawn(async move {
            loop {
                let read = read_frame(&mut control_read).await.map(Incoming::Control);
                let failed = read.is_err();
                if tx.send(read).is_err() || failed {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            loop {
                let read = read_frame(&mut agents_read).await.map(Incoming::Agents);
                let failed = read.is_err();
                if incoming_tx.send(read).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            control,
            agents,
            incoming,
            journal_head,
        })
    }

    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        write_frame(&mut self.control, message).await
    }

    pub async fn send_agents(&mut self, frame: &ClientFrame) -> Result<()> {
        write_frame(&mut self.agents, frame).await
    }

    pub async fn recv(&mut self) -> Result<Incoming> {
        self.incoming
            .recv()
            .await
            .context("the daemon's streams closed")?
    }
}
