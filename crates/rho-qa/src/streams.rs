//! A daemon's agents stream, with agent commands on call streams beside
//! it, for a harness that reads the journal and
//! drives agents in one loop.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rho_agent_types::{AgentId, Seq};
use rho_agents_client::protocol as agents;
use rho_agents_client::protocol::{AgentCommand, ClientFrame, NewAgent, ServerFrame};
use rho_rpc::protocol::client::Client;
use rho_rpc::protocol::{Answer, read_frame, write_frame, write_open};
use tokio::io::WriteHalf;
use tokio::sync::mpsc;

/// A frame from the agents stream or an answer, in the order it was read.
pub enum Incoming {
    Agents(ServerFrame),
    /// The agent [`Streams::create`] asked for.
    Created(AgentId),
    /// A call the daemon would not make, and why.
    Refused(String),
}

pub struct Streams {
    socket: PathBuf,
    agents: WriteHalf<rho_rpc::Stream>,
    incoming_tx: mpsc::UnboundedSender<Result<Incoming>>,
    incoming: mpsc::UnboundedReceiver<Result<Incoming>>,
    /// How far the journal ran when the agents stream opened.
    pub journal_head: Seq,
}

impl Streams {
    /// Opens an agents stream on `agents`, a connection to the daemon at
    /// `socket`, and reads its journal head. Nothing is followed yet.
    pub async fn open(agents: Client, socket: &Path) -> Result<Self> {
        let mut agents = agents.into_stream();
        write_open(&mut agents, &agents::Open::Session).await?;
        let ServerFrame::JournalHead { journal_head, .. } = read_frame(&mut agents).await? else {
            bail!("the agents stream did not open with its journal head");
        };
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let (mut agents_read, agents) = tokio::io::split(agents);
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
            agents,
            incoming_tx,
            incoming,
            journal_head,
        })
    }

    /// Starts an agent on a call stream of its own; the agent arrives as
    /// [`Incoming::Created`].
    pub fn create(&self, new: NewAgent) {
        self.call(new, |agent_id| Some(Incoming::Created(agent_id)));
    }

    /// Sends `command` on a call stream of its own. Only a refusal says
    /// anything.
    pub fn send(&self, command: AgentCommand) {
        self.call(command, |()| None);
    }

    fn call<C: rho_rpc::protocol::Call>(
        &self,
        call: C,
        answered: fn(C::Reply) -> Option<Incoming>,
    ) {
        let socket = self.socket.clone();
        let tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            let answer = async {
                let mut client = Client::connect(&socket).await?;
                client.open(&call.open()).await?;
                client.recv::<Answer<C::Reply>>().await
            }
            .await;
            // A refusal is an answer like any other here: the harness
            // decides what it means.
            let incoming = match answer {
                Ok(Answer::Done(reply)) => answered(reply),
                Ok(Answer::Failed { reason }) => Some(Incoming::Refused(reason)),
                Err(error) => {
                    let _ = tx.send(Err(error));
                    return;
                }
            };
            if let Some(incoming) = incoming {
                let _ = tx.send(Ok(incoming));
            }
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
