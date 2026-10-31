//! One host's agents session: the host's mail, directory, rows and live
//! tails to the model, and the model's follows and the window's focus back
//! to the host. The workset log pages a follow needs are read here too, on
//! streams of their own, so everything the model hears of a host comes in
//! one order.
//!
//! The host opens it on every connection
//! ([`rho_agent_hosts::HostStream`]); what is said on it and where its frames
//! go are this crate's.

use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use rho_agent_hosts::{Dialer, HostStream};
use rho_agent_types::{AgentId, WorksetId, Wpos};
use rho_rpc::protocol::{read_frame, write_frame, write_open};

use crate::HostId;
use crate::model::ToModel;
use crate::protocol::rho2::{self, ClientFrame, ServerFrame, WorksetLogPage};

/// What reaches the model from one host.
#[derive(Debug)]
pub enum AgentFrame {
    /// A frame of the host's session.
    Server(ServerFrame),
    /// One page of a workset's log that the model asked for.
    Log {
        workset: WorksetId,
        page: WorksetLogPage,
    },
    /// Reading a workset's log stopped short; the model asks again when
    /// the stream opens again.
    LogFailed { workset: WorksetId },
}

impl From<ServerFrame> for AgentFrame {
    fn from(frame: ServerFrame) -> Self {
        Self::Server(frame)
    }
}

/// An agents-stream frame tagged with the host it came from.
pub struct AgentEvent {
    pub host: HostId,
    pub frame: AgentFrame,
}

pub(crate) enum Command {
    Frame(ClientFrame),
    ReadLog { workset: WorksetId, from: Wpos },
}

/// What this client says down one host's agents stream. Whatever is
/// queued while the stream is down is dropped when it opens again: the
/// model says its follows again to each new stream. The focus is state,
/// so each new stream is told it whole.
#[derive(Clone)]
pub struct AgentCommands {
    commands: futures_mpsc::UnboundedSender<Command>,
    focus: Arc<Mutex<Option<Vec<AgentId>>>>,
}

impl AgentCommands {
    pub fn send(&self, frame: ClientFrame) {
        let _ = self.commands.unbounded_send(Command::Frame(frame));
    }

    /// Reads a workset's log from `from` to its end, page by page, each
    /// page to the model as [`AgentFrame::Log`].
    pub fn read_log(&self, workset: WorksetId, from: Wpos) {
        let _ = self
            .commands
            .unbounded_send(Command::ReadLog { workset, from });
    }

    /// The agents whose live frames this host should stream: every open
    /// pane, replaced wholesale.
    pub fn focus(&self, agent_ids: Vec<AgentId>) {
        *self.focus.lock().unwrap() = Some(agent_ids.clone());
        self.send(ClientFrame::Focus { agent_ids });
    }
}

/// One host's agents stream, as the host keeps it across reconnects.
pub(crate) struct AgentStream {
    host: HostId,
    model: futures_mpsc::UnboundedSender<ToModel>,
    pub(crate) commands: Arc<tokio::sync::Mutex<futures_mpsc::UnboundedReceiver<Command>>>,
    focus: Arc<Mutex<Option<Vec<AgentId>>>>,
}

impl AgentStream {
    pub(crate) fn new(
        host: HostId,
        model: futures_mpsc::UnboundedSender<ToModel>,
    ) -> (Self, AgentCommands) {
        let (commands, commands_rx) = futures_mpsc::unbounded();
        let focus = Arc::new(Mutex::new(None));
        let stream = Self {
            host,
            model,
            commands: Arc::new(tokio::sync::Mutex::new(commands_rx)),
            focus: focus.clone(),
        };
        (stream, AgentCommands { commands, focus })
    }
}

/// A workset's log from `from` to its end, each page to the model as it
/// comes. Ends early, with `LogFailed`, when a page cannot be read.
async fn read_log(
    dialer: Dialer,
    host: HostId,
    model: futures_mpsc::UnboundedSender<ToModel>,
    workset: WorksetId,
    mut from: Wpos,
) {
    loop {
        let page = async {
            let mut stream = dialer
                .open(<rho2::WorksetLog as rho_rpc::protocol::Call>::PRIORITY)
                .await?;
            rho_rpc::protocol::call(&mut stream, rho2::WorksetLog { workset, from }).await
        };
        let frame = match page.await {
            Ok(page) => {
                let next = page.next;
                let frame = AgentFrame::Log { workset, page };
                if model
                    .unbounded_send(ToModel::Event(AgentEvent { host, frame }))
                    .is_err()
                {
                    return;
                }
                match next {
                    Some(next) => {
                        from = next;
                        continue;
                    }
                    None => return,
                }
            }
            Err(error) => {
                tracing::warn!(%workset, "reading the workset log failed: {error:#}");
                AgentFrame::LogFailed { workset }
            }
        };
        let _ = model.unbounded_send(ToModel::Event(AgentEvent { host, frame }));
        return;
    }
}

impl HostStream for AgentStream {
    fn name(&self) -> &'static str {
        "agents"
    }

    /// The host's frames to the model, this client's frames to the host,
    /// for as long as the connection lasts. Ends with an error when either
    /// direction does.
    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let model = self.model.clone();
        let commands = self.commands.clone();
        let focus = self.focus.clone();
        Box::pin(async move {
            // Bulk priority: a catch-up must not hold up anything interactive.
            let mut socket = dialer.open(None).await?;
            write_open(&mut socket, &rho2::Open::Session).await?;
            let (mut reader, mut writer) = tokio::io::split(socket);
            let mut commands = commands.lock().await;
            while commands.try_recv().is_ok() {}
            let focus = focus.lock().unwrap().clone();
            if let Some(agent_ids) = focus {
                write_frame(&mut writer, &ClientFrame::Focus { agent_ids }).await?;
            }
            let read = async {
                loop {
                    let frame = read_frame::<_, ServerFrame>(&mut reader).await?;
                    let event = ToModel::Event(AgentEvent {
                        host,
                        frame: frame.into(),
                    });
                    if model.unbounded_send(event).is_err() {
                        return Ok(());
                    }
                }
            };
            let write = async {
                let mut reads = FuturesUnordered::new();
                loop {
                    tokio::select! {
                        command = commands.next() => match command {
                            Some(Command::Frame(frame)) => write_frame(&mut writer, &frame).await?,
                            Some(Command::ReadLog { workset, from }) => reads.push(read_log(
                                dialer.clone(),
                                host,
                                model.clone(),
                                workset,
                                from,
                            )),
                            None => return anyhow::Ok(()),
                        },
                        Some(()) = reads.next(), if !reads.is_empty() => {}
                    }
                }
            };
            tokio::select! {
                result = read => result,
                result = write => result,
            }
        })
    }
}
