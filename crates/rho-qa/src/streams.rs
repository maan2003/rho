//! An agent host's session stream, with calls on streams of their own
//! beside it, for a harness that reads worksets and mail and drives agents
//! in one loop.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rho_agent_types::{AgentId, MailId, WorksetId};
use rho_agents_client::protocol::rho2::{
    ClientFrame, CreateWorkset, CreatedWorkset, Open, PostMail, ServerFrame,
};
use rho_rpc::protocol::client::Client;
use rho_rpc::protocol::{Answer, read_frame, write_frame, write_open};
use tokio::io::WriteHalf;
use tokio::sync::mpsc;

/// A frame from the session or an answer, in the order it was read.
pub enum Incoming {
    Session(ServerFrame),
    /// The workset [`Streams::create`] asked for, and the agent its
    /// message went to.
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
    incoming_tx: mpsc::UnboundedSender<Result<Incoming>>,
    incoming: mpsc::UnboundedReceiver<Result<Incoming>>,
    /// How far the mail ran when the session opened.
    pub mail_head: MailId,
}

impl Streams {
    /// Opens a session on `client`, a connection to the agent host at
    /// `socket`, and reads its `Hello`. Nothing is followed yet.
    pub async fn open(client: Client, socket: &Path) -> Result<Self> {
        let mut session = client.into_stream();
        write_open(&mut session, &Open::Session).await?;
        let ServerFrame::Hello { mail_head, .. } = read_frame(&mut session).await? else {
            bail!("the session did not open with its hello");
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
        Ok(Self {
            socket: socket.to_owned(),
            session,
            incoming_tx,
            incoming,
            mail_head,
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

    /// Mails an agent as the user, on a call stream of its own. Only a
    /// refusal says anything.
    pub fn post(&self, mail: PostMail) {
        self.call(mail, |_| None);
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

    pub async fn recv(&mut self) -> Result<Incoming> {
        self.incoming
            .recv()
            .await
            .context("the agent host's streams closed")?
    }
}
