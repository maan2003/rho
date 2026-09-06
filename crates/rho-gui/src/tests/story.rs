//! What the old `Ready` and story said about an agent, in the words the
//! tests still use, and the log rows each stands for now.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rho_core::{MessageDelivery, UnixMs};
use rho_hosts::connection::ConnEvent;
use rho_ui_proto::mirror::{AgentPos, LogEntry, MirrorEvent, PresentationField, Seq, TurnEdge};
use rho_ui_proto::{AgentId, AgentRole, WorkspaceInfo};

pub type UiRuntimeKind = rho_ui_proto::mirror::RuntimeKind;
pub type UiSpawnedBy = rho_ui_proto::mirror::SpawnedBy;
pub type UiAgentWant = rho_ui_proto::mirror::AgentWant;
pub type UiTurnOutcome = rho_ui_proto::mirror::TurnOutcome;

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
    pub workdirs: Vec<WorkspaceInfo>,
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
    fn mirror(self) -> MirrorEvent {
        match self {
            Self::UserMessage { text, at } => MirrorEvent::Message {
                from: None,
                text,
                delivery: MessageDelivery::Immediate,
                at,
            },
            Self::TurnStarted { at } => MirrorEvent::Turn {
                edge: TurnEdge::Started,
                at,
            },
            Self::TurnEnded { outcome, at } => MirrorEvent::Turn {
                edge: TurnEdge::Ended(outcome),
                at,
            },
            Self::Wants { want, summary, at } => MirrorEvent::Wants { want, summary, at },
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

fn entry(agent_id: AgentId, pos: u64, event: MirrorEvent) -> LogEntry {
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
            events.push(MirrorEvent::Presented {
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
            events.push(MirrorEvent::Turn {
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
    let mut events = vec![MirrorEvent::Created {
        role: head.role,
        runtime: head.runtime_kind,
        workdirs: head.workdirs,
        spawned_by: head.spawned_by,
        spawn_name: head.spawn_name,
        parent: head.parent,
        model: "test-model".to_owned(),
        at: head.created_at,
    }];
    if head.generated_title.is_some() || head.activity.is_some() {
        events.push(MirrorEvent::Presented {
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
        events.push(MirrorEvent::Turn {
            edge: TurnEdge::Started,
            at: head.created_at,
        });
    }
    let told = events.len() as u64;
    if head.story_pos.0 >= told {
        // The head stood past what these rows say; a row that changes
        // nothing carries the position.
        events.push(MirrorEvent::Presented {
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

/// `Ready` followed by the log rows these heads stand for.
pub fn ready_with(heads: Vec<UiAgentHead>, agent_counter: u64) -> ConnEvent {
    let entries = heads.into_iter().flat_map(head_entries).collect();
    // The head is the newest row handed out: a client that has heard every
    // row so far is caught up.
    let journal_head = NEXT_SEQ.with(|next| Seq(next.get() - 1));
    ConnEvent::Many(vec![
        ConnEvent::Ready {
            auth: rho_ui_proto::AuthState {
                namespaces: Vec::new(),
                disabled_namespaces: Vec::new(),
                active_namespace: None,
            },
            machine_seed: 0,
            agent_counter,
            journal_head,
        },
        ConnEvent::Log { entries },
    ])
}

/// A run of one agent's story, each row past the last one told.
pub fn story(agent_id: AgentId, events: Vec<UiStoryEvent>) -> ConnEvent {
    let entries = events
        .into_iter()
        .map(|event| {
            let pos = NEXT_POS.with(|next| next.borrow().get(&agent_id).copied().unwrap_or(1));
            entry(agent_id, pos, event.mirror())
        })
        .collect();
    ConnEvent::Log { entries }
}

thread_local! {
    /// The model this test drives. One per test thread, so a test's own
    /// fold and cursor are its own.
    static MODEL: RefCell<(crate::model::Model, std::collections::HashSet<crate::registry::HostId>)> =
        RefCell::new((crate::model::Model::new(), std::collections::HashSet::new()));
}

/// One frame, through the model and then into the workspace: the same
/// `ingest` the model thread runs, called inline so a test stays in one
/// thread and can assert in the frame it fed.
pub fn feed(
    workspace: &mut crate::workspace::Workspace,
    host: crate::registry::HostId,
    event: ConnEvent,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::workspace::Workspace>,
) {
    let followed = workspace.followed();
    let events = MODEL.with(|model| {
        let (model, attached) = &mut *model.borrow_mut();
        if attached.insert(host) {
            model.attach(host, format!("host-{}", attached.len()));
        }
        model.command(crate::model::ModelCommand::Follow(followed));
        model.ingest(host, event)
    });
    workspace.handle_model_events(events, window, cx);
}
