//! A daemon's control and agents streams held at once, with agent commands
//! on request streams beside them, for a harness that reads the journal and
//! drives agents in one loop.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rho_agent_host_proto::agents::{ClientFrame, Reply, Request, ServerFrame};
use rho_agent_host_proto::client::Client;
use rho_agent_host_proto::control::ServerFrame as ControlFrame;
use rho_agent_host_proto::transcript::Seq;
use rho_agent_host_proto::{AgentCommand, Open, agents, host, read_frame, write_frame};
use tokio::io::WriteHalf;
use tokio::sync::mpsc;

/// A frame from the agents stream or a reply, in the order it was read.
pub enum Incoming {
    Agents(ServerFrame),
    /// The answer to a command sent with [`Streams::send`].
    Reply(Reply),
}

pub struct Streams {
    socket: PathBuf,
    /// Held so the control stream stays open.
    _control: WriteHalf<rho_rpc::Stream>,
    agents: WriteHalf<rho_rpc::Stream>,
    incoming_tx: mpsc::UnboundedSender<Result<Incoming>>,
    incoming: mpsc::UnboundedReceiver<Result<Incoming>>,
    /// How far the journal ran when the agents stream opened.
    pub journal_head: Seq,
}

impl Streams {
    /// Opens `control` as the control stream, waits for `Ready`, and opens
    /// an agents stream beside it. Nothing is followed yet.
    pub async fn open(mut control: Client, socket: &Path) -> Result<Self> {
        control.send(&Open::Host(host::Open::Control)).await?;
        let ControlFrame::Ready { .. } = control.recv().await? else {
            bail!("the control stream did not open with Ready");
        };
        let mut agents = Client::connect(socket)
            .await
            .context("open an agents stream")?
            .into_stream();
        write_frame(&mut agents, &Open::Agents(agents::Open::Session)).await?;
        let ServerFrame::JournalHead { journal_head, .. } = read_frame(&mut agents).await? else {
            bail!("the agents stream did not open with its journal head");
        };
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let (mut control_read, control) = tokio::io::split(control.into_stream());
        let (mut agents_read, agents) = tokio::io::split(agents);
        // Nothing the control stream pushes matters to a harness; it is
        // read so the host is never held up, and its end is the host's.
        let tx = incoming_tx.clone();
        tokio::spawn(async move {
            let error = loop {
                if let Err(error) = read_frame::<_, ControlFrame>(&mut control_read).await {
                    break error;
                }
            };
            let _ = tx.send(Err(error));
        });
        let tx = incoming_tx.clone();
        tokio::spawn(async move {
            loop {
                let read = read_frame(&mut agents_read).await.map(Incoming::Agents);
                let failed = read.is_err();
                if tx.send(read).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            socket: socket.to_owned(),
            _control: control,
            agents,
            incoming_tx,
            incoming,
            journal_head,
        })
    }

    /// Sends `command` on a request stream of its own; its reply arrives
    /// as [`Incoming::Reply`].
    pub fn send(&self, command: AgentCommand) {
        let socket = self.socket.clone();
        let tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            // A refusal is a reply like any other here: the harness
            // decides what it means.
            let reply = async {
                let mut client = Client::connect(&socket).await?;
                client
                    .send(&Open::Agents(agents::Open::Request(Request::Command(
                        command,
                    ))))
                    .await?;
                client.recv().await
            }
            .await
            .map(Incoming::Reply);
            let _ = tx.send(reply);
        });
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
