//! An agent host's session streams, with calls on streams of their own
//! beside them, for a harness that reads worksets and chat and drives
//! agents in one loop.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rho_agent_types::{AgentId, ChatRecord, ChatSeq, WorksetId};
use rho_agents_client::protocol::worksets::{ClientFrame, CreateWorkset, CreatedWorkset, Open, ServerFrame};
use rho_chat::protocol as chat;
use rho_rpc::protocol::client::Client;
use rho_rpc::protocol::{Answer, Call as _, read_frame, write_frame, write_open};
use tokio::io::WriteHalf;
use tokio::sync::mpsc;

/// A frame from a session or an answer, in the order it was read.
pub enum Incoming {
    Session(ServerFrame),
    /// Chat events in log order, from the chat session.
    Chat(Vec<ChatRecord>),
    /// The workset [`Streams::create`] asked for, and the first agent its
    /// code created.
    Created {
        workset: WorksetId,
        agent: Option<AgentId>,
        error: Option<String>,
    },
    /// A call the agent host would not make, and why.
    Refused(String),
}

pub struct Streams {
    socket: PathBuf,
    session: WriteHalf<rho_rpc::Stream>,
    chat: WriteHalf<rho_rpc::Stream>,
    incoming_tx: mpsc::UnboundedSender<Result<Incoming>>,
    incoming: mpsc::UnboundedReceiver<Result<Incoming>>,
    /// How far the chat ran when the session opened.
    pub chat_head: ChatSeq,
}

impl Streams {
    /// Opens a worksets session on `client`, a connection to the agent host
    /// at `socket`, and a chat session beside it, and reads their hellos.
    /// Nothing is followed yet.
    pub async fn open(client: Client, socket: &Path) -> Result<Self> {
        let mut session = client.into_stream();
        write_open(&mut session, &Open::Session).await?;
        let ServerFrame::Hello { .. } = read_frame(&mut session).await? else {
            bail!("the session did not open with its hello");
        };
        let mut chat = Client::connect(socket).await?.into_stream();
        write_open(&mut chat, &chat::Open::Session).await?;
        let chat::ServerFrame::Hello { head: chat_head } = read_frame(&mut chat).await? else {
            bail!("the chat session did not open with its hello");
        };
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let (mut session_read, session) = tokio::io::split(session);
        let tx = incoming_tx.clone();
        tokio::spawn(async move {
            loop {
                let read = read_frame(&mut session_read).await.map(Incoming::Session);
                let failed = read.is_err();
                if tx.send(read).is_err() || failed {
                    break;
                }
            }
        });
        let (mut chat_read, chat) = tokio::io::split(chat);
        let tx = incoming_tx.clone();
        tokio::spawn(async move {
            loop {
                let read = read_frame(&mut chat_read).await.map(|frame| match frame {
                    chat::ServerFrame::Chat { records } => Incoming::Chat(records),
                    chat::ServerFrame::Hello { .. } => Incoming::Chat(Vec::new()),
                });
                let failed = read.is_err();
                if tx.send(read).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            socket: socket.to_owned(),
            session,
            chat,
            incoming_tx,
            incoming,
            chat_head,
        })
    }

    /// Creates a workset on a call stream of its own; it arrives as
    /// [`Incoming::Created`].
    pub fn create(&self, create: CreateWorkset) {
        self.call(create, |CreatedWorkset { workset, result }| {
            Some(Incoming::Created {
                workset,
                agent: result.agent,
                error: result.error,
            })
        });
    }

    /// Posts as the user, on a call stream of its own. Only a refusal says
    /// anything: the message arrives in the chat.
    pub fn post(&self, post: chat::Post) {
        self.call(post, |_| None);
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

    pub async fn send(&mut self, frame: &ClientFrame) -> Result<()> {
        write_frame(&mut self.session, frame).await
    }

    /// Asks for every chat event after `since`, then each new one.
    pub async fn follow_chat(&mut self, since: ChatSeq) -> Result<()> {
        write_frame(&mut self.chat, &chat::ClientFrame::Follow { since }).await
    }

    pub async fn recv(&mut self) -> Result<Incoming> {
        self.incoming
            .recv()
            .await
            .context("the agent host's streams closed")?
    }
}
