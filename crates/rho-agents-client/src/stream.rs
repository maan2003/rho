//! One host's agents stream: the daemon's journal and live tails to the
//! model, the model's follow and the window's focus back to the daemon.
//!
//! The host opens it on every connection
//! ([`rho_hosts::HostStream`]); what is said on it and where its frames go
//! are this crate's.

use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_agent_host_proto::agents::{self, ClientFrame, ServerFrame};
use rho_agent_host_proto::transcript::{Live, LogEntry};
use rho_agent_host_proto::{AuthState, QuotaSummary, read_frame, write_frame, write_open};
use rho_agent_types::{AgentId, Seq};
use rho_hosts::{Dialer, HostStream};

use crate::HostId;
use crate::model::ToModel;

/// What a host says on its agents stream.
pub enum AgentFrame {
    /// The stream is open: whose journal this is and how far it runs.
    /// Every (re)opened stream starts with one, and a follow is asked for
    /// only after it.
    JournalHead {
        machine_seed: u64,
        journal_head: Seq,
        agent_counter: u64,
    },
    /// Which provider accounts the host's agents may run on.
    Auth { auth: AuthState },
    /// A run of the host's journal, contiguous by seq: the answer to
    /// `Follow` and everything appended since.
    Log { entries: Vec<LogEntry> },
    /// What changed in the runtime's tail past the log, for an agent some
    /// client is looking at.
    Live { agent_id: AgentId, live: Live },
    /// An agent was created on the host, by any client or agent, and the
    /// agent-id counter moved to `agent_counter`.
    AgentCreated {
        agent_id: AgentId,
        agent_counter: u64,
    },
    /// The host's quota: every account's latest usage.
    QuotaUsage { summaries: Vec<QuotaSummary> },
}

/// An agents-stream frame tagged with the host it came from.
pub struct AgentEvent {
    pub host: HostId,
    pub frame: AgentFrame,
}

/// What this client says down one host's agents stream. Whatever is
/// queued while the stream is down is dropped when it opens again: a
/// follow was asked of the last stream's journal head, and the new stream
/// says its own. The focus is state, so each new stream is told it whole.
#[derive(Clone)]
pub struct AgentCommands {
    commands: futures_mpsc::UnboundedSender<ClientFrame>,
    focus: Arc<Mutex<Option<Vec<AgentId>>>>,
}

impl AgentCommands {
    pub fn send(&self, frame: ClientFrame) {
        let _ = self.commands.unbounded_send(frame);
    }

    /// The agents whose live frames this host should stream: every open
    /// pane, replaced wholesale. Everything durable is on the journal
    /// regardless, so this only decides who streams.
    pub fn focus(&self, agent_ids: Vec<AgentId>) {
        *self.focus.lock().unwrap() = Some(agent_ids.clone());
        self.send(ClientFrame::Focus { agent_ids });
    }
}

/// One host's agents stream, as the host keeps it across reconnects.
pub(crate) struct AgentStream {
    host: HostId,
    model: futures_mpsc::UnboundedSender<ToModel>,
    commands: Arc<tokio::sync::Mutex<futures_mpsc::UnboundedReceiver<ClientFrame>>>,
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

impl HostStream for AgentStream {
    fn name(&self) -> &'static str {
        "agents"
    }

    /// The daemon's frames to the model, this client's frames to the
    /// daemon, for as long as the connection lasts. Ends with an error
    /// when either direction does.
    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let model = self.model.clone();
        let commands = self.commands.clone();
        let focus = self.focus.clone();
        Box::pin(async move {
            // Bulk priority: a catch-up must not hold up anything interactive.
            let mut socket = dialer.open(None).await?;
            write_open(&mut socket, &agents::Open::Session).await?;
            let (mut reader, mut writer) = tokio::io::split(socket);
            let mut commands = commands.lock().await;
            while commands.try_recv().is_ok() {}
            let focus = focus.lock().unwrap().clone();
            if let Some(agent_ids) = focus {
                write_frame(&mut writer, &ClientFrame::Focus { agent_ids }).await?;
            }
            let read = async {
                loop {
                    let frame = match read_frame::<_, ServerFrame>(&mut reader).await? {
                        ServerFrame::JournalHead {
                            machine_seed,
                            journal_head,
                            agent_counter,
                        } => AgentFrame::JournalHead {
                            machine_seed,
                            journal_head,
                            agent_counter,
                        },
                        ServerFrame::Auth { auth } => AgentFrame::Auth { auth },
                        ServerFrame::Log { entries } => AgentFrame::Log { entries },
                        ServerFrame::Live { agent_id, live } => AgentFrame::Live { agent_id, live },
                        ServerFrame::AgentCreated {
                            agent_id,
                            agent_counter,
                        } => AgentFrame::AgentCreated {
                            agent_id,
                            agent_counter,
                        },
                        ServerFrame::QuotaUsage { summaries } => {
                            AgentFrame::QuotaUsage { summaries }
                        }
                        // Nothing here asks for a detail body: the
                        // transcript draws a call's line and never its
                        // output.
                        ServerFrame::Detail { .. } => continue,
                    };
                    let event = ToModel::Event(AgentEvent { host, frame });
                    if model.unbounded_send(event).is_err() {
                        return Ok(());
                    }
                }
            };
            let write = async {
                while let Some(frame) = commands.next().await {
                    write_frame(&mut writer, &frame).await?;
                }
                anyhow::Ok(())
            };
            tokio::select! {
                result = read => result,
                result = write => result,
            }
        })
    }
}
