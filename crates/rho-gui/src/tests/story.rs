//! What the old `Ready` and story said about an agent, in the words the
//! tests still use, and the log rows each stands for now.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rho_agent_host_proto::transcript::{LogEntry, TranscriptEvent};
use rho_agent_host_proto::{Answer, Open, agents, read_frame, write_frame};
use rho_agent_types::{
    AgentId, AgentPos, AgentRole, MessageDelivery, Place, PresentationField, Seq, TurnEdge, UnixMs,
};
use rho_agents_client::stream::AgentFrame;
use rho_desk_client::stream::DeskFrame;
use rho_hosts::connection::ConnEvent;
use senax_encoder::{Packer, Unpacker};

pub type UiRuntimeKind = rho_agent_host_proto::transcript::RuntimeKind;
pub type UiSpawnedBy = rho_agent_host_proto::transcript::SpawnedBy;
pub type UiAgentWant = rho_agent_types::AgentWant;
pub type UiTurnOutcome = rho_agent_types::TurnOutcome;

/// A position in an agent's story, as the old `Ready` named it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct UiStoryPos(pub u64);

/// What the old `Ready` said about one agent. Tests build these; the helper
/// below turns each into the log rows that say the same thing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiAgentHead {
    pub agent_id: AgentId,
    pub story_pos: UiStoryPos,
    pub role: AgentRole,
    pub runtime_kind: UiRuntimeKind,
    pub place: Place,
    pub spawned_by: UiSpawnedBy,
    pub parent: Option<AgentId>,
    pub spawn_name: Option<String>,
    pub generated_title: Option<String>,
    pub activity: Option<String>,
    pub turn_running: bool,
    pub created_at: UnixMs,
}

/// The story events tests tell, in the old story's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiStoryEvent {
    UserMessage {
        text: String,
        at: UnixMs,
    },
    TurnStarted {
        at: UnixMs,
    },
    TurnEnded {
        outcome: UiTurnOutcome,
        at: UnixMs,
    },
    Wants {
        want: UiAgentWant,
        summary: Option<String>,
        at: UnixMs,
    },
}

impl UiStoryEvent {
    fn mirror(self) -> TranscriptEvent {
        match self {
            Self::UserMessage { text, at } => TranscriptEvent::Message {
                from: None,
                text,
                delivery: MessageDelivery::Immediate,
                at,
            },
            Self::TurnStarted { at } => TranscriptEvent::Turn {
                edge: TurnEdge::Started,
                at,
            },
            Self::TurnEnded { outcome, at } => TranscriptEvent::Turn {
                edge: TurnEdge::Ended(outcome),
                at,
            },
            Self::Wants { want, summary, at } => TranscriptEvent::Wants { want, summary, at },
        }
    }
}

thread_local! {
    /// Where each agent's log stands, so every row a test tells lands past
    /// the last: the fold ignores rows behind its newest.
    static NEXT_POS: RefCell<HashMap<AgentId, u64>> = RefCell::new(HashMap::new());
    static NEXT_SEQ: Cell<u64> = const { Cell::new(1) };
}

/// Forgets every position and seq handed out, for a test starting fresh.
pub fn reset() {
    NEXT_POS.with(|next| next.borrow_mut().clear());
    NEXT_SEQ.with(|next| next.set(1));
}

fn known(agent_id: AgentId) -> bool {
    NEXT_POS.with(|next| next.borrow().contains_key(&agent_id))
}

fn entry(agent_id: AgentId, pos: u64, event: TranscriptEvent) -> LogEntry {
    let seq = NEXT_SEQ.with(|next| {
        let seq = next.get();
        next.set(seq + 1);
        Seq(seq)
    });
    NEXT_POS.with(|next| {
        let mut next = next.borrow_mut();
        let slot = next.entry(agent_id).or_default();
        *slot = (*slot).max(pos + 1);
    });
    LogEntry {
        seq,
        agent_id,
        pos: AgentPos(pos),
        event,
    }
}

/// The rows that say what a head said: creation at row zero, then the
/// title and the running turn, the last of them at the head's position.
/// A head told again for an agent already known is what it says now, as
/// rows past the last.
pub fn head_entries(head: UiAgentHead) -> Vec<LogEntry> {
    let agent_id = head.agent_id;
    if known(agent_id) {
        let mut events = Vec::new();
        if head.generated_title.is_some() || head.activity.is_some() {
            events.push(TranscriptEvent::Presented {
                title: head
                    .generated_title
                    .map_or(PresentationField::Unchanged, PresentationField::Set),
                activity: head
                    .activity
                    .map_or(PresentationField::Unchanged, PresentationField::Set),
                at: head.created_at,
            });
        }
        if head.turn_running {
            events.push(TranscriptEvent::Turn {
                edge: TurnEdge::Started,
                at: head.created_at,
            });
        }
        return events
            .into_iter()
            .map(|event| {
                let pos = NEXT_POS.with(|next| next.borrow()[&agent_id]);
                entry(agent_id, pos, event)
            })
            .collect();
    }
    let mut events = vec![TranscriptEvent::Created {
        role: head.role,
        runtime: head.runtime_kind,
        place: head.place,
        spawned_by: head.spawned_by,
        spawn_name: head.spawn_name,
        parent: head.parent,
        model: "test-model".to_owned(),
        at: head.created_at,
    }];
    if head.generated_title.is_some() || head.activity.is_some() {
        events.push(TranscriptEvent::Presented {
            title: head
                .generated_title
                .map_or(PresentationField::Unchanged, PresentationField::Set),
            activity: head
                .activity
                .map_or(PresentationField::Unchanged, PresentationField::Set),
            at: head.created_at,
        });
    }
    if head.turn_running {
        events.push(TranscriptEvent::Turn {
            edge: TurnEdge::Started,
            at: head.created_at,
        });
    }
    let told = events.len() as u64;
    if head.story_pos.0 >= told {
        // The head stood past what these rows say; a row that changes
        // nothing carries the position.
        events.push(TranscriptEvent::Presented {
            title: PresentationField::Unchanged,
            activity: PresentationField::Unchanged,
            at: head.created_at,
        });
    }
    let last = events.len() - 1;
    events
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            let pos = if index == last {
                (index as u64).max(head.story_pos.0)
            } else {
                index as u64
            };
            entry(agent_id, pos, event)
        })
        .collect()
}

/// `Ready`, then an agents stream opening on the log rows these heads stand
/// for.
pub fn ready_with(heads: Vec<UiAgentHead>, agent_counter: u64) -> Frame {
    let entries = heads.into_iter().flat_map(head_entries).collect();
    // The head is the newest row handed out: a client that has heard every
    // row so far is caught up.
    let journal_head = NEXT_SEQ.with(|next| Seq(next.get() - 1));
    Frame::Many(vec![
        ConnEvent::Ready.into(),
        AgentFrame::JournalHead {
            machine_seed: 0,
            journal_head,
            agent_counter,
        }
        .into(),
        AgentFrame::Auth {
            auth: rho_agent_host_proto::AuthState {
                namespaces: Vec::new(),
                disabled_namespaces: Vec::new(),
                active_namespace: None,
            },
        }
        .into(),
        AgentFrame::Log { entries }.into(),
    ])
}

/// The calls a host in this process has been asked so far
/// ([`crate::workspace::Workspace::host_in_process_for_test`]), each with
/// the stream to answer it on.
pub fn calls(
    streams: &mut tokio::sync::mpsc::UnboundedReceiver<rho_rpc::Stream>,
) -> Vec<(agents::Request, rho_rpc::Stream)> {
    let mut calls = Vec::new();
    while let Ok(mut stream) = streams.try_recv() {
        let open = futures::executor::block_on(read_frame::<_, Open>(&mut stream))
            .expect("a stream opens by saying what it is for");
        if let Open::Agents(agents::Open::Request(request)) = open {
            calls.push((request, stream));
        }
    }
    calls
}

/// Answers a call as the daemon would.
pub fn answer<T: Packer + Unpacker>(stream: &mut rho_rpc::Stream, answer: Answer<T>) {
    futures::executor::block_on(write_frame(stream, &answer)).expect("answer the call");
}

/// A run of one agent's story, each row past the last one told.
pub fn story(agent_id: AgentId, events: Vec<UiStoryEvent>) -> AgentFrame {
    let entries = events
        .into_iter()
        .map(|event| {
            let pos = NEXT_POS.with(|next| next.borrow().get(&agent_id).copied().unwrap_or(1));
            entry(agent_id, pos, event.mirror())
        })
        .collect();
    AgentFrame::Log { entries }
}

thread_local! {
    /// The model this test drives. One per test thread, so a test's own
    /// fold and cursor are its own.
    static MODEL: RefCell<(rho_agents_client::model::Model, std::collections::HashSet<rho_agents_client::HostId>)> =
        RefCell::new((rho_agents_client::model::Model::new(), std::collections::HashSet::new()));
}

/// What a host says, on any of its streams.
pub enum Frame {
    Control(ConnEvent),
    Agents(AgentFrame),
    Desk(DeskFrame),
    Many(Vec<Frame>),
}

impl From<ConnEvent> for Frame {
    fn from(event: ConnEvent) -> Self {
        Self::Control(event)
    }
}

impl From<AgentFrame> for Frame {
    fn from(frame: AgentFrame) -> Self {
        Self::Agents(frame)
    }
}

impl From<DeskFrame> for Frame {
    fn from(frame: DeskFrame) -> Self {
        Self::Desk(frame)
    }
}

/// One frame into the workspace: a control- or desk-stream event straight in,
/// an agents-stream frame through the same `ingest` the model thread runs,
/// called inline so a test stays in one thread and can assert in the frame
/// it fed.
pub fn feed(
    workspace: &mut crate::workspace::Workspace,
    host: rho_agents_client::HostId,
    frame: impl Into<Frame>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::workspace::Workspace>,
) {
    match frame.into() {
        Frame::Control(event) => workspace.handle_event(host, event, window, cx),
        Frame::Desk(frame) => workspace.handle_desk_event(host, frame, window, cx),
        Frame::Agents(frame) => {
            let followed = workspace.followed();
            let events = MODEL.with(|model| {
                let (model, attached) = &mut *model.borrow_mut();
                if attached.insert(host) {
                    model.attach(host, format!("host-{}", attached.len()));
                }
                model.command(rho_agents_client::model::ModelCommand::Follow(followed));
                model.ingest(host, frame)
            });
            workspace.handle_model_events(events, window, cx);
        }
        Frame::Many(frames) => {
            for frame in frames {
                feed(workspace, host, frame, window, cx);
            }
        }
    }
}
