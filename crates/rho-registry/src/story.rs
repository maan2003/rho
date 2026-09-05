//! The client's fold of an agent's story: what a reader needs about an
//! agent without holding its whole log.
//!
//! Slice B keeps only the fold in memory. The events themselves land in the
//! GUI's own redb mirror in the next change, which is what the transcript
//! renders from; nothing here changes when they do.

use rho_ui_proto::AgentId;
use rho_ui_proto::story::{UiAgentHead, UiAgentWant, UiStoryEvent, UiStoryPos, UiTurnOutcome};

/// How urgently an agent wants the user, in ascending order. Derived on
/// the client from the story and the user's own verdicts; the daemon has
/// no opinion about it (`AGENT-LOG-DESIGN.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attention {
    /// Nothing is owed: handled, muted, deferred, or never asked anything.
    #[default]
    Quiet,
    /// A turn is running; the agent's court.
    Working,
    /// The last turn asked something of the user, past their cursor.
    Pending,
    /// The turn errored or stopped unfinished: blocked on the user.
    NeedsInput,
}

/// What the agent said its last finished turn asks of the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wants {
    pub want: UiAgentWant,
    pub summary: Option<String>,
    /// Where it was told, so a cursor past it settles the card.
    pub at: UiStoryPos,
}

/// Uninterpreted chronology, folded from the story. Every field is a fact
/// the story told; what they mean for the rails is decided in one place in
/// `desk_view`, not here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoryDigest {
    /// One past the newest event folded: the cursor a verdict writes.
    pub newest: UiStoryPos,
    pub turn_running: bool,
    pub last_active: rho_core::UnixMs,
    pub last_user_message_at: rho_core::UnixMs,
    pub last_user_message_text: String,
    pub last_turn_ended: Option<rho_core::UnixMs>,
    /// The last turn ended badly, and nothing has happened since.
    pub errored: Option<UiStoryPos>,
    /// What the last finished turn asks for, cleared when the user speaks
    /// again: the ball is theirs once they answer.
    pub wants: Option<Wants>,
}

impl StoryDigest {
    /// Folds one event, in position order.
    pub fn tell(&mut self, pos: UiStoryPos, event: &UiStoryEvent) {
        self.newest = pos.next();
        let at = event.at();
        if at > self.last_active {
            self.last_active = at;
        }
        match event {
            UiStoryEvent::UserMessage { text, .. } => {
                self.last_user_message_at = at;
                self.last_user_message_text = one_line(text);
                // Answering is the user taking the ball back.
                self.wants = None;
                self.errored = None;
            }
            UiStoryEvent::TurnStarted { .. } => {
                self.turn_running = true;
                self.errored = None;
            }
            UiStoryEvent::TurnEnded { outcome, .. } => {
                self.turn_running = false;
                self.last_turn_ended = Some(at);
                self.errored = matches!(outcome, UiTurnOutcome::Errored { .. }).then_some(pos);
            }
            UiStoryEvent::Wants { want, summary, .. } => {
                self.wants = Some(Wants {
                    want: *want,
                    summary: summary.clone(),
                    at: pos,
                });
            }
            UiStoryEvent::Created { .. }
            | UiStoryEvent::Parented { .. }
            | UiStoryEvent::AgentMail { .. }
            | UiStoryEvent::Reply { .. }
            | UiStoryEvent::ToolCall { .. }
            | UiStoryEvent::Titled { .. }
            | UiStoryEvent::Activity { .. }
            | UiStoryEvent::Cost { .. }
            | UiStoryEvent::Rewound { .. }
            | UiStoryEvent::Compacted { .. }
            | UiStoryEvent::RoleChanged { .. }
            | UiStoryEvent::WorkdirAdded { .. }
            | UiStoryEvent::HistoryUnavailableBefore { .. } => {}
        }
    }
}

/// One agent as the client holds it: what the daemon says it is, and what
/// its story adds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredAgent {
    /// Which daemon it lives on; agents from two hosts share one mirror.
    pub host: crate::HostId,
    pub head: UiAgentHead,
    pub digest: StoryDigest,
}

impl MirroredAgent {
    pub fn new(host: crate::HostId, head: UiAgentHead) -> Self {
        let digest = StoryDigest {
            last_active: head.created_at,
            turn_running: head.turn_running,
            ..StoryDigest::default()
        };
        Self { host, head, digest }
    }

    pub fn agent_id(&self) -> AgentId {
        self.head.agent_id
    }

    /// Folds a run of story events starting at `from`. Events already
    /// folded are skipped, so a repeated range is harmless.
    pub fn tell(&mut self, from: UiStoryPos, events: &[UiStoryEvent]) {
        for (offset, event) in events.iter().enumerate() {
            let pos = UiStoryPos(from.0 + offset as u64);
            if pos.0 < self.digest.newest.0 {
                continue;
            }
            self.digest.tell(pos, event);
        }
    }
}

/// The first line, trimmed: what a rail row shows of a message.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}
