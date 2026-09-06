//! The fold: what a client makes of the mirror.
//!
//! The daemon sends the mirror, one stripped event per raw row
//! (`AGENT-LOG-DESIGN.md`, "the mirror is a pure function of the raw
//! log"). Everything a rail or a transcript shows is folded from it here,
//! on the client, so the wire carries facts and never conclusions.

use std::sync::Arc;

use rho_core::{AgentId, AgentRole, UnixMs};
use rho_ui_proto::mirror::{
    AgentPos, AgentWant, MirrorEvent, PresentationField, RuntimeKind, SpawnedBy, Speaker,
    ToolOutcome, ToolStatus, TurnEdge, TurnOutcome,
};
use rho_ui_proto::{AgentUsageBucket, WorkspaceInfo};

use crate::HostId;
use crate::render::{UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock, UiTool, UiToolStatus};

/// How much an agent wants the user, as the view decided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    #[default]
    Quiet,
    Working,
    Pending,
    NeedsInput,
}

/// What the user last said about an agent: how far they have dealt with
/// its rows, and whether they muted it. The only facts attention needs
/// that no row carries; kept by the client with the digest.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct Verdict {
    /// One past the newest position the user has dealt with.
    pub handled_through: AgentPos,
    pub muted: bool,
}

/// What of a digest attention is decided from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttentionFacts {
    pub turn_running: bool,
    /// Where the last turn died, if nothing has happened since.
    pub errored: Option<AgentPos>,
    /// Where the last finished turn said what it wants.
    pub wants_at: Option<AgentPos>,
}

/// How badly an agent wants the user: the join of what its rows say and
/// the user's verdict. The one place attention is decided; every rail
/// reads the answer through the registry.
pub fn attention(facts: AttentionFacts, verdict: Verdict) -> Attention {
    let past = |pos: AgentPos| pos >= verdict.handled_through;
    // A running turn is the agent's court, whatever the user has said.
    if facts.turn_running {
        Attention::Working
    } else if verdict.muted {
        Attention::Quiet
    } else if facts.errored.is_some_and(past) {
        Attention::NeedsInput
    } else if facts.wants_at.is_some_and(past) {
        Attention::Pending
    } else {
        Attention::Quiet
    }
}

/// What the last finished turn asks of the user, and where it said so.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Wants {
    pub want: AgentWant,
    pub summary: Option<String>,
    pub at: AgentPos,
}

/// What an agent is: its `Created` row, kept current by the rows that
/// change it. Never what has happened to it; that is the [`Digest`].
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct AgentIdentity {
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub runtime: RuntimeKind,
    pub workdirs: Vec<WorkspaceInfo>,
    pub spawned_by: SpawnedBy,
    pub spawn_name: Option<String>,
    pub parent: Option<AgentId>,
    /// The model its binding names, for pricing.
    pub model: String,
    pub created_at: UnixMs,
}

impl AgentIdentity {
    /// The primary workdir; every agent is created with at least one.
    pub fn workspace(&self) -> Option<&WorkspaceInfo> {
        self.workdirs.first()
    }
}

/// Which fold made a stored digest. Bump when `Digest::tell` changes
/// what it makes of a row; a client finding another version on disk
/// folds that agent's rows again, once.
pub const DIGEST_VERSION: u32 = 1;

/// What the rails read of an agent, folded from its mirror. Incremental,
/// and kept on disk by the client, so a restart reads it back instead of
/// folding every event again.
#[derive(Clone, Debug, Default, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Digest {
    /// One past the newest position folded.
    pub newest: AgentPos,
    /// The sidecar's title. A spawn name always beats it.
    pub title: Option<String>,
    pub activity: Option<String>,
    pub turn_running: bool,
    /// When the running turn began, so a reader is told how long it has
    /// been working. `None` between turns, and while a turn the client
    /// never saw start is running.
    pub turn_started_at: Option<UnixMs>,
    pub last_active: UnixMs,
    pub last_user_message_at: UnixMs,
    pub last_user_message_text: String,
    pub last_turn_ended: Option<UnixMs>,
    /// Where the last turn died, if nothing has happened since.
    pub errored: Option<AgentPos>,
    pub wants: Option<Wants>,
    /// Every reply's usage, summed.
    pub usage: AgentUsageBucket,
    /// The model the newest usage named.
    pub usage_model: String,
}

impl Digest {
    pub fn attention_facts(&self) -> AttentionFacts {
        AttentionFacts {
            turn_running: self.turn_running,
            errored: self.errored,
            wants_at: self.wants.as_ref().map(|wants| wants.at),
        }
    }

    /// Folds one event. Positions already held are skipped, so a repeated
    /// run is harmless; returns whether anything was new.
    pub fn tell(&mut self, pos: AgentPos, event: &MirrorEvent) -> bool {
        if pos < self.newest {
            return false;
        }
        self.newest = pos.next();
        self.last_active = self.last_active.max(event.at());
        match event {
            MirrorEvent::Message {
                from: None,
                text,
                at,
                ..
            } => self.user_spoke(*at, text),
            MirrorEvent::ClaudeMessage {
                speaker: Speaker::User,
                text,
                at,
            } => self.user_spoke(*at, text),
            MirrorEvent::Turn {
                edge: TurnEdge::Started,
                at,
            } => {
                self.turn_running = true;
                self.turn_started_at = Some(*at);
                self.errored = None;
                self.wants = None;
            }
            MirrorEvent::Turn {
                edge: TurnEdge::Ended(outcome),
                at,
            } => {
                self.turn_running = false;
                self.turn_started_at = None;
                self.last_turn_ended = Some(*at);
                // The label described work that just stopped.
                self.activity = None;
                self.errored = matches!(outcome, TurnOutcome::Errored { .. }).then_some(pos);
            }
            MirrorEvent::Presented {
                title, activity, ..
            } => {
                apply(&mut self.title, title);
                apply(&mut self.activity, activity);
            }
            MirrorEvent::Wants { want, summary, .. } => {
                self.wants = Some(Wants {
                    want: *want,
                    summary: summary.clone(),
                    at: pos,
                });
            }
            MirrorEvent::Replied {
                usage: Some(usage), ..
            } => {
                self.usage.input_tokens += usage.input_tokens;
                self.usage.cache_read_tokens += usage.cache_read_tokens;
                self.usage.cache_write_tokens += usage.cache_write_tokens;
                self.usage.cache_write_1h_tokens += usage.cache_write_1h_tokens;
                self.usage.output_tokens += usage.output_tokens;
                self.usage.requests += 1;
                self.usage_model = usage.model.clone();
            }
            // History from `to` on is gone, and with it anything it said.
            MirrorEvent::Rewound { to, .. } => {
                if self.wants.as_ref().is_some_and(|wants| wants.at >= *to) {
                    self.wants = None;
                }
                if self.errored.is_some_and(|errored| errored >= *to) {
                    self.errored = None;
                }
            }
            MirrorEvent::Message { .. }
            | MirrorEvent::ClaudeMessage { .. }
            | MirrorEvent::Created { .. }
            | MirrorEvent::RoleChanged { .. }
            | MirrorEvent::WorkdirAdded { .. }
            | MirrorEvent::CompactionRequested { .. }
            | MirrorEvent::QueueCleared { .. }
            | MirrorEvent::Sent { .. }
            | MirrorEvent::Results { .. }
            | MirrorEvent::Replied { .. }
            | MirrorEvent::Failed { .. } => {}
        }
        true
    }

    fn user_spoke(&mut self, at: UnixMs, text: &str) {
        self.last_user_message_at = at;
        self.last_user_message_text = one_line(text);
        // The ball is the agent's again.
        self.wants = None;
        self.errored = None;
    }
}

fn apply(field: &mut Option<String>, change: &PresentationField) {
    match change {
        PresentationField::Unchanged => {}
        PresentationField::Set(value) => *field = Some(value.clone()),
        PresentationField::Clear => *field = None,
    }
}

/// One agent as the client holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredAgent {
    pub host: HostId,
    pub identity: AgentIdentity,
    pub digest: Digest,
}

impl MirroredAgent {
    /// From the agent's first row. `None` unless it is a `Created`.
    pub fn new(host: HostId, agent_id: AgentId, event: &MirrorEvent) -> Option<Self> {
        let MirrorEvent::Created {
            role,
            runtime,
            workdirs,
            spawned_by,
            spawn_name,
            parent,
            model,
            at,
        } = event
        else {
            return None;
        };
        let mut digest = Digest::default();
        digest.tell(AgentPos::ZERO, event);
        Some(Self {
            host,
            identity: AgentIdentity {
                agent_id,
                role: *role,
                runtime: *runtime,
                workdirs: workdirs.clone(),
                spawned_by: *spawned_by,
                spawn_name: spawn_name.clone(),
                parent: *parent,
                model: model.clone(),
                created_at: *at,
            },
            digest,
        })
    }

    pub fn agent_id(&self) -> AgentId {
        self.identity.agent_id
    }

    /// The agent's name for a reader: what the spawner called it, else
    /// what the sidecar made of it.
    pub fn title(&self) -> Option<&str> {
        self.identity
            .spawn_name
            .as_deref()
            .or(self.digest.title.as_deref())
    }

    /// Folds one event; returns whether it was new.
    pub fn tell(&mut self, pos: AgentPos, event: &MirrorEvent) -> bool {
        if !self.digest.tell(pos, event) {
            return false;
        }
        match event {
            MirrorEvent::RoleChanged { role, model, .. } => {
                self.identity.role = *role;
                if let Some(model) = model {
                    self.identity.model = model.clone();
                }
            }
            MirrorEvent::WorkdirAdded { workdir, .. } => {
                self.identity.workdirs.push(workdir.clone());
            }
            _ => {}
        }
        true
    }
}

/// The first line, whitespace folded: the words a person remembers.
pub fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The transcript of one agent's mirror, oldest first. Nothing here is
/// invented: a call is its name and its line, a reply is what was said,
/// and a message waits as queued until a request carries it.
pub fn transcript(events: &[(AgentPos, MirrorEvent)]) -> UiAgentState {
    TranscriptFold::new(events).state()
}

/// The transcript as a fold that takes one row at a time, so a `Log`
/// entry costs what it changes and never a walk of the whole mirror.
#[derive(Clone, Debug, Default)]
pub struct TranscriptFold {
    /// One past the newest position folded.
    next: AgentPos,
    /// Shared with every state handed out, so a row costs the blocks it
    /// adds or changes and a state is a list of pointers.
    blocks: Vec<Arc<UiBlock>>,
    /// Where each block came from, so a rewind drops exactly what was
    /// told after it and keeps the rest.
    told_at: Vec<AgentPos>,
    /// Messages no request has carried yet; drawn after the blocks.
    queue: Vec<(AgentPos, UiBlock)>,
    turn_running: bool,
    errored: bool,
    /// What the last reply said the context holds, and where it said it.
    context_used: Option<(AgentPos, u64)>,
    digest: Digest,
}

impl TranscriptFold {
    pub fn new(events: &[(AgentPos, MirrorEvent)]) -> Self {
        let mut fold = Self::default();
        for (pos, event) in events {
            fold.tell(*pos, event);
        }
        fold
    }

    /// One past the newest position folded.
    pub fn next(&self) -> AgentPos {
        self.next
    }

    fn push(&mut self, pos: AgentPos, block: UiBlock) {
        self.blocks.push(Arc::new(block));
        self.told_at.push(pos);
    }

    /// Each result lands on the call it answers.
    fn finish_calls(&mut self, results: &[ToolOutcome]) {
        for result in results {
            let called = self
                .blocks
                .iter()
                .rposition(|block| matches!(&**block, UiBlock::Tool(tool) if tool.id == result.id));
            if let Some(index) = called
                && let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index])
            {
                tool.status = match result.status {
                    ToolStatus::Success => UiToolStatus::Success,
                    ToolStatus::Error => UiToolStatus::Error,
                    ToolStatus::Cancelled => UiToolStatus::Cancelled,
                };
                tool.started_at = Some(result.started_at);
                tool.finished_at = Some(result.finished_at);
            }
        }
    }

    /// Folds one row. Positions already held are skipped, so a repeated
    /// row is harmless; returns whether anything was new.
    pub fn tell(&mut self, pos: AgentPos, event: &MirrorEvent) -> bool {
        if pos < self.next {
            return false;
        }
        self.next = pos.next();
        self.digest.tell(pos, event);
        match event {
            MirrorEvent::Message {
                from,
                text,
                delivery,
                ..
            } => {
                self.errored = false;
                self.queue.push((
                    pos,
                    UiBlock::QueuedMessage {
                        text: text.clone(),
                        delivery: *delivery,
                        sender: *from,
                    },
                ));
            }
            MirrorEvent::CompactionRequested { .. } => self.queue.push((
                pos,
                UiBlock::Notice {
                    text: "compacting context".to_owned(),
                },
            )),
            MirrorEvent::QueueCleared { .. } => self.queue.clear(),
            MirrorEvent::Sent {
                results,
                compaction,
                ..
            } => {
                for (_, queued) in std::mem::take(&mut self.queue) {
                    self.push(pos, delivered(queued));
                }
                if *compaction {
                    self.push(
                        pos,
                        UiBlock::Notice {
                            text: "compacting context".to_owned(),
                        },
                    );
                }
                self.finish_calls(results);
            }
            MirrorEvent::Results { results, .. } => self.finish_calls(results),
            MirrorEvent::Replied {
                text,
                calls,
                compacted,
                context_used,
                at,
                ..
            } => {
                if let Some(used) = context_used {
                    self.context_used = Some((pos, *used));
                }
                if !text.is_empty() {
                    self.push(
                        pos,
                        UiBlock::AssistantMessage {
                            text: text.clone(),
                            phase: None,
                        },
                    );
                }
                for call in calls {
                    self.push(
                        pos,
                        UiBlock::Tool(UiTool {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.what.text(),
                            preview: None,
                            // Until the `Sent` that carries its result says
                            // otherwise.
                            status: UiToolStatus::Running,
                            output: None,
                            error: None,
                            started_at: Some(*at),
                            finished_at: None,
                            metadata: None,
                        }),
                    );
                }
                if *compacted {
                    self.push(
                        pos,
                        UiBlock::Notice {
                            text: "compacted".to_owned(),
                        },
                    );
                }
            }
            // Claude's transcript is mirrored message by message; the
            // queued copy of the same text is what it confirms.
            MirrorEvent::ClaudeMessage { speaker, text, .. } => {
                if let Some(index) = self.queue.iter().position(|(_, queued)| {
                    matches!(queued, UiBlock::QueuedMessage { text: queued, .. } if queued.trim() == text.trim())
                }) {
                    self.queue.remove(index);
                }
                self.push(
                    pos,
                    match speaker {
                        Speaker::Assistant => UiBlock::AssistantMessage {
                            text: text.clone(),
                            phase: None,
                        },
                        Speaker::User | Speaker::Agent => {
                            UiBlock::UserMessage { text: text.clone() }
                        }
                    },
                );
            }
            MirrorEvent::Turn {
                edge: TurnEdge::Started,
                ..
            } => {
                self.turn_running = true;
                self.errored = false;
            }
            MirrorEvent::Turn {
                edge: TurnEdge::Ended(outcome),
                ..
            } => {
                self.turn_running = false;
                self.errored = matches!(outcome, TurnOutcome::Errored { .. });
                // A call the turn never answered is over too. Only a
                // running one is copied out of its sharing.
                for block in &mut self.blocks {
                    if matches!(&**block, UiBlock::Tool(tool) if tool.status == UiToolStatus::Running)
                        && let UiBlock::Tool(tool) = Arc::make_mut(block)
                    {
                        tool.status = UiToolStatus::Cancelled;
                    }
                }
                if let TurnOutcome::Errored { message } = outcome {
                    self.push(
                        pos,
                        UiBlock::Notice {
                            text: message.clone(),
                        },
                    );
                }
            }
            // What the model said before its request failed stays
            // readable; a retry says so, a final failure is the turn's
            // ending right after.
            MirrorEvent::Failed {
                text,
                error,
                retrying,
                ..
            } => {
                if !text.is_empty() {
                    self.push(
                        pos,
                        UiBlock::AssistantMessage {
                            text: text.clone(),
                            phase: None,
                        },
                    );
                }
                if *retrying {
                    self.push(
                        pos,
                        UiBlock::Notice {
                            text: format!("temporary inference error: {error}; retrying"),
                        },
                    );
                }
            }
            // A rewind is told rather than unwritten, so the reader is the
            // one that hides what it undid.
            MirrorEvent::Rewound { to, .. } => {
                let kept = self.told_at.iter().take_while(|told| *told < to).count();
                self.blocks.truncate(kept);
                self.told_at.truncate(kept);
                self.queue.retain(|(queued_at, _)| queued_at < to);
                if self.context_used.is_some_and(|(said_at, _)| said_at >= *to) {
                    self.context_used = None;
                }
            }
            MirrorEvent::Created { .. }
            | MirrorEvent::RoleChanged { .. }
            | MirrorEvent::WorkdirAdded { .. }
            | MirrorEvent::Presented { .. }
            | MirrorEvent::Wants { .. } => {}
        }
        true
    }

    /// The transcript as it stands: the blocks, then what is queued.
    pub fn state(&self) -> UiAgentState {
        let mut blocks = self.blocks.clone();
        blocks.extend(
            self.queue
                .iter()
                .map(|(_, queued)| Arc::new(queued.clone())),
        );
        UiAgentState {
            blocks,
            // Never `Streaming`: this is the mirror, not the live tail. A
            // turn that was running when the client last heard is the
            // daemon's to report again.
            status: if self.errored {
                UiAgentStatus::Error
            } else if self.turn_running {
                UiAgentStatus::Unloaded
            } else {
                UiAgentStatus::Idle
            },
            context_used: self.context_used.map(|(_, used)| used),
            usage: UiAgentUsage {
                provider: self.digest.usage_model.clone(),
                total: self.digest.usage.clone(),
            },
        }
    }
}

fn delivered(queued: UiBlock) -> UiBlock {
    match queued {
        UiBlock::QueuedMessage {
            text,
            sender: Some(sender),
            ..
        } => UiBlock::AgentMessage { sender, text },
        UiBlock::QueuedMessage { text, .. } => UiBlock::UserMessage { text },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use rho_core::MessageDelivery;
    use rho_ui_proto::mirror::{ToolCallLine, ToolLine, ToolOutcome, Usage};

    use super::*;

    fn told(events: Vec<MirrorEvent>) -> UiAgentState {
        transcript(
            &events
                .into_iter()
                .enumerate()
                .map(|(pos, event)| (AgentPos(pos as u64), event))
                .collect::<Vec<_>>(),
        )
    }

    fn user(text: &str, at: u64) -> MirrorEvent {
        MirrorEvent::Message {
            from: None,
            text: text.to_owned(),
            delivery: MessageDelivery::Immediate,
            at: UnixMs(at),
        }
    }

    #[test]
    fn a_told_turn_reads_as_a_transcript() {
        let state = told(vec![
            user("have a look", 1),
            MirrorEvent::Turn {
                edge: TurnEdge::Started,
                at: UnixMs(2),
            },
            MirrorEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at: UnixMs(2),
            },
            MirrorEvent::Replied {
                text: String::new(),
                calls: vec![ToolCallLine {
                    id: "call-1".to_owned(),
                    name: "Read".to_owned(),
                    what: ToolLine::Path("/tmp/README.md".into()),
                }],
                compacted: false,
                usage: Some(Usage {
                    model: "sol".to_owned(),
                    output_tokens: 5,
                    ..Default::default()
                }),
                context_used: Some(10),
                at: UnixMs(3),
            },
            MirrorEvent::Sent {
                results: vec![ToolOutcome {
                    id: "call-1".to_owned(),
                    status: ToolStatus::Error,
                    started_at: UnixMs(3),
                    finished_at: UnixMs(4),
                }],
                compaction: false,
                at: UnixMs(4),
            },
            MirrorEvent::Replied {
                text: "done looking".to_owned(),
                calls: Vec::new(),
                compacted: false,
                usage: Some(Usage {
                    model: "sol".to_owned(),
                    output_tokens: 7,
                    ..Default::default()
                }),
                context_used: Some(12),
                at: UnixMs(5),
            },
            MirrorEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Completed),
                at: UnixMs(6),
            },
        ]);
        assert_eq!(state.status, UiAgentStatus::Idle);
        assert_eq!(state.blocks.len(), 3);
        assert_eq!(
            *state.blocks[0],
            UiBlock::UserMessage {
                text: "have a look".to_owned()
            }
        );
        let UiBlock::Tool(tool) = &*state.blocks[1] else {
            panic!("the call is a tool block");
        };
        assert_eq!(tool.arguments, "/tmp/README.md");
        assert_eq!(tool.status, UiToolStatus::Error);
        assert_eq!(tool.finished_at, Some(UnixMs(4)));
        assert_eq!(tool.output, None, "the mirror never carries tool output");
        assert!(matches!(*state.blocks[2], UiBlock::AssistantMessage { .. }));
        assert_eq!(state.usage.total.output_tokens, 12);
        assert_eq!(state.usage.total.requests, 2);
        assert_eq!(state.usage.provider, "sol");
    }

    #[test]
    fn a_message_waits_as_queued_until_a_request_carries_it() {
        let state = told(vec![user("later", 1)]);
        assert!(matches!(*state.blocks[0], UiBlock::QueuedMessage { .. }));
        let state = told(vec![
            user("later", 1),
            MirrorEvent::QueueCleared { at: UnixMs(2) },
        ]);
        assert!(state.blocks.is_empty());
    }

    /// A Claude agent's tool results carry nothing out of the queue: the
    /// message waits until Claude's own echo of it.
    #[test]
    fn results_alone_leave_the_queue_where_it_is() {
        let state = told(vec![
            user("later", 1),
            MirrorEvent::Results {
                results: Vec::new(),
                at: UnixMs(2),
            },
        ]);
        assert_eq!(state.blocks.len(), 1);
        assert!(matches!(*state.blocks[0], UiBlock::QueuedMessage { .. }));
    }

    fn replied_with_context(used: u64, at: u64) -> MirrorEvent {
        MirrorEvent::Replied {
            text: String::new(),
            calls: Vec::new(),
            compacted: false,
            usage: None,
            context_used: Some(used),
            at: UnixMs(at),
        }
    }

    #[test]
    fn the_last_reply_says_how_full_the_context_is() {
        let state = told(vec![
            replied_with_context(10, 1),
            replied_with_context(12, 2),
        ]);
        assert_eq!(state.context_used, Some(12));
        let state = told(vec![
            replied_with_context(10, 1),
            replied_with_context(12, 2),
            MirrorEvent::Rewound {
                to: AgentPos(1),
                at: UnixMs(3),
            },
        ]);
        assert_eq!(
            state.context_used, None,
            "said by a reply the rewind took back"
        );
    }

    /// The error text is the trailing notice, which is what `Error` status
    /// means to every reader of a live frame.
    #[test]
    fn an_errored_turn_ends_in_a_notice() {
        let state = told(vec![
            MirrorEvent::Turn {
                edge: TurnEdge::Started,
                at: UnixMs(1),
            },
            MirrorEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Errored {
                    message: "the deploy script exited 1".to_owned(),
                }),
                at: UnixMs(2),
            },
        ]);
        assert_eq!(state.status, UiAgentStatus::Error);
        assert_eq!(
            state.blocks,
            vec![Arc::new(UiBlock::Notice {
                text: "the deploy script exited 1".to_owned()
            })]
        );
    }

    /// What the model said before its request failed stays on screen:
    /// a retry says so after it, a final failure is the turn's notice.
    #[test]
    fn a_failed_request_keeps_what_was_said() {
        let state = told(vec![
            MirrorEvent::Turn {
                edge: TurnEdge::Started,
                at: UnixMs(1),
            },
            MirrorEvent::Failed {
                text: "half an answer".to_owned(),
                error: "overloaded".to_owned(),
                retrying: true,
                at: UnixMs(2),
            },
            MirrorEvent::Failed {
                text: "another half".to_owned(),
                error: "quota".to_owned(),
                retrying: false,
                at: UnixMs(3),
            },
            MirrorEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Errored {
                    message: "quota".to_owned(),
                }),
                at: UnixMs(3),
            },
        ]);
        assert_eq!(state.status, UiAgentStatus::Error);
        assert_eq!(
            state.blocks,
            vec![
                Arc::new(UiBlock::AssistantMessage {
                    text: "half an answer".to_owned(),
                    phase: None
                }),
                Arc::new(UiBlock::Notice {
                    text: "temporary inference error: overloaded; retrying".to_owned()
                }),
                Arc::new(UiBlock::AssistantMessage {
                    text: "another half".to_owned(),
                    phase: None
                }),
                Arc::new(UiBlock::Notice {
                    text: "quota".to_owned()
                }),
            ]
        );
    }

    /// A rewind hides what it undid and keeps what came before it.
    #[test]
    fn a_rewind_hides_what_it_undid() {
        let state = told(vec![
            MirrorEvent::ClaudeMessage {
                speaker: Speaker::User,
                text: "first".to_owned(),
                at: UnixMs(1),
            },
            MirrorEvent::ClaudeMessage {
                speaker: Speaker::Assistant,
                text: "second".to_owned(),
                at: UnixMs(2),
            },
            MirrorEvent::Rewound {
                to: AgentPos(1),
                at: UnixMs(3),
            },
        ]);
        assert_eq!(
            state.blocks,
            vec![Arc::new(UiBlock::UserMessage {
                text: "first".to_owned()
            })]
        );
    }

    #[test]
    fn the_digest_reads_what_the_rails_need() {
        let host = HostId(1);
        let agent_id = AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        let created = MirrorEvent::Created {
            role: AgentRole::default(),
            runtime: RuntimeKind::Rho,
            workdirs: vec![WorkspaceInfo::UserCheckout {
                repo: "/repo".into(),
            }],
            spawned_by: SpawnedBy::Direct,
            spawn_name: None,
            parent: None,
            model: "sol".to_owned(),
            at: UnixMs(1),
        };
        assert!(MirroredAgent::new(host, agent_id, &user("x", 1)).is_none());
        let mut mirrored = MirroredAgent::new(host, agent_id, &created).unwrap();
        assert!(mirrored.tell(AgentPos(1), &user("do the thing\nand then some", 10)));
        assert!(mirrored.tell(
            AgentPos(2),
            &MirrorEvent::Turn {
                edge: TurnEdge::Started,
                at: UnixMs(11),
            }
        ));
        assert!(mirrored.digest.turn_running);
        assert_eq!(mirrored.digest.last_user_message_text, "do the thing");
        assert!(mirrored.tell(
            AgentPos(3),
            &MirrorEvent::Wants {
                want: AgentWant::Ask,
                summary: Some("needs a decision".to_owned()),
                at: UnixMs(12),
            }
        ));
        assert!(mirrored.tell(
            AgentPos(4),
            &MirrorEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Completed),
                at: UnixMs(13),
            }
        ));
        assert!(!mirrored.digest.turn_running);
        assert_eq!(mirrored.digest.last_turn_ended, Some(UnixMs(13)));
        assert_eq!(mirrored.digest.wants.as_ref().unwrap().at, AgentPos(3));
        // Replaying a position the fold already holds changes nothing.
        assert!(!mirrored.tell(AgentPos(3), &user("again", 99)));
        assert_eq!(mirrored.digest.newest, AgentPos(5));
        assert_eq!(mirrored.digest.last_active, UnixMs(13));
        assert!(mirrored.tell(
            AgentPos(5),
            &MirrorEvent::Rewound {
                to: AgentPos(3),
                at: UnixMs(14),
            }
        ));
        assert_eq!(mirrored.digest.wants, None);
    }
}
