//! The fold: what a client makes of the mirror.
//!
//! The agent host sends the mirror, one stripped event per raw row: the
//! mirror is a pure function of the raw log. Everything a rail or a transcript
//! shows is folded from it here, on the client, so the wire carries facts and
//! never conclusions.

use std::sync::Arc;

use rho_agent_types::{AgentId, AgentPos, AgentRole, Place, PresentationField, SendKind, UnixMs};

use crate::HostId;
use crate::protocol::AgentUsageBucket;
use crate::protocol::transcript::{
    RuntimeKind, SpawnedBy, Speaker, ToolOutcome, ToolStatus, TranscriptEvent,
};
use crate::state::{
    UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock, UiNotebookActivity, UiToolStatus,
};

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

/// What a row put to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub enum Said {
    /// A send asking for something the work needs.
    Ask,
    /// The agent stopped on an error: as much an ask as a question.
    Stopped,
    /// A send delivering what the user asked for.
    Result,
    Other,
}

impl Said {
    /// How hard it pulls on the user: lower is stronger. A stop pulls as
    /// hard as a question.
    pub fn strength(self) -> u8 {
        match self {
            Self::Ask | Self::Stopped => 0,
            Self::Result => 1,
            Self::Other => 2,
        }
    }
}

/// One thing the agent put to the user since they last wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Unread {
    pub pos: AgentPos,
    pub at: UnixMs,
    pub said: Said,
}

/// What an agent is: its `Created` row, kept current by the rows that
/// change it. Never what has happened to it; that is the [`Digest`].
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct AgentIdentity {
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub runtime: RuntimeKind,
    pub place: Place,
    pub spawned_by: SpawnedBy,
    pub spawn_name: Option<String>,
    pub parent: Option<AgentId>,
    /// The model its binding names, for pricing.
    pub model: String,
    pub created_at: UnixMs,
}

/// Which fold made a stored digest. Bump when `Digest::tell` changes
/// what it makes of a row; a client finding another version on disk
/// folds that agent's rows again, once.
pub const DIGEST_VERSION: u32 = 3;

/// What the rails read of an agent, folded from its mirror. Incremental,
/// and kept on disk by the client, so a restart reads it back instead of
/// folding every event again. Only the conversation with the user counts:
/// what the agent says to other agents, and whether it is running, do not.
#[derive(Clone, Debug, Default, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Digest {
    /// One past the newest position folded.
    pub newest: AgentPos,
    /// The sidecar's title. A spawn name always beats it.
    pub title: Option<String>,
    /// The agent's newest status and where it said it, until any later
    /// message from either side hides it.
    pub status: Option<(AgentPos, String)>,
    pub last_active: UnixMs,
    pub last_user_message_at: UnixMs,
    pub last_user_message_text: String,
    /// When it last sent the user anything but a status.
    pub last_sent_at: Option<UnixMs>,
    /// What it put to the user since they last wrote, oldest first.
    pub unread: Vec<Unread>,
    pub notebook: Option<UiNotebookActivity>,
    /// Every reply's usage, summed.
    pub usage: AgentUsageBucket,
    /// The model the newest usage named.
    pub usage_model: String,
}

impl Digest {
    /// Folds one event. Positions already held are skipped, so a repeated
    /// run is harmless; returns whether anything was new.
    pub fn tell(&mut self, pos: AgentPos, event: &TranscriptEvent) -> bool {
        if pos < self.newest {
            return false;
        }
        self.newest = pos.next();
        self.last_active = self.last_active.max(event.at());
        match event {
            TranscriptEvent::Received {
                from: None,
                text,
                at,
                ..
            } => self.user_spoke(*at, text),
            TranscriptEvent::Message {
                from: None,
                text,
                at,
                ..
            } => self.user_spoke(*at, text),
            TranscriptEvent::ClaudeMessage {
                speaker: Speaker::User,
                text,
                at,
            } => self.user_spoke(*at, text),
            TranscriptEvent::MessageSent {
                to: None,
                text,
                kind: SendKind::Status,
                ..
            } => self.status = Some((pos, text.clone())),
            TranscriptEvent::MessageSent {
                to: None, kind, at, ..
            } => {
                let said = match kind {
                    SendKind::Ask => Said::Ask,
                    SendKind::Result => Said::Result,
                    SendKind::Status | SendKind::Other => Said::Other,
                };
                self.unread.push(Unread { pos, at: *at, said });
                self.status = None;
                self.last_sent_at = Some(*at);
            }
            TranscriptEvent::Stopped { at, .. } => self.unread.push(Unread {
                pos,
                at: *at,
                said: Said::Stopped,
            }),
            TranscriptEvent::NotebookActivity {
                responding,
                running_tasks,
                checkin_at,
                archived,
                ..
            } => {
                self.notebook = Some(UiNotebookActivity {
                    responding: *responding,
                    running_tasks: *running_tasks,
                    checkin_at: *checkin_at,
                    archived: *archived,
                });
            }
            TranscriptEvent::Presented { title, .. } => apply(&mut self.title, title),
            TranscriptEvent::Replied {
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
            TranscriptEvent::Rewound { to, .. } => {
                self.unread.retain(|unread| unread.pos < *to);
                if self.status.as_ref().is_some_and(|(at, _)| at >= to) {
                    self.status = None;
                }
            }
            TranscriptEvent::Message { .. }
            | TranscriptEvent::Received { .. }
            | TranscriptEvent::ClaudeMessage { .. }
            | TranscriptEvent::Created { .. }
            | TranscriptEvent::RoleChanged { .. }
            | TranscriptEvent::Notice { .. }
            | TranscriptEvent::CompactionRequested { .. }
            | TranscriptEvent::QueueCleared { .. }
            | TranscriptEvent::Sent { .. }
            | TranscriptEvent::NotebookReport { .. }
            | TranscriptEvent::MessageSent { .. }
            | TranscriptEvent::ExecObserved { .. }
            | TranscriptEvent::Results { .. }
            | TranscriptEvent::Replied { .. }
            | TranscriptEvent::Failed { .. } => {}
        }
        true
    }

    /// The strongest thing the agent put to the user past `seen`, counted
    /// from the oldest of that kind (`rho-dealer/cases.md`, A5).
    pub fn strongest_unread(&self, seen: AgentPos) -> Option<Unread> {
        self.unread
            .iter()
            .filter(|unread| unread.pos >= seen)
            .min_by_key(|unread| (unread.said.strength(), unread.pos))
            .copied()
    }

    /// Writing reads everything before it.
    fn user_spoke(&mut self, at: UnixMs, text: &str) {
        self.last_user_message_at = at;
        self.last_user_message_text = one_line(text);
        self.status = None;
        self.unread.clear();
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
    pub fn new(host: HostId, agent_id: AgentId, event: &TranscriptEvent) -> Option<Self> {
        let TranscriptEvent::Created {
            role,
            runtime,
            place,
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
                place: place.clone(),
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
    pub fn tell(&mut self, pos: AgentPos, event: &TranscriptEvent) -> bool {
        if !self.digest.tell(pos, event) {
            return false;
        }
        if let TranscriptEvent::RoleChanged { role, model, .. } = event {
            self.identity.role = *role;
            if let Some(model) = model {
                self.identity.model = model.clone();
            }
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
pub fn transcript(events: &[(AgentPos, TranscriptEvent)]) -> UiAgentState {
    TranscriptFold::new(events).state()
}

/// The transcript as a fold that takes one row at a time, so a `Log`
/// entry costs what it changes and never a walk of the whole mirror.
#[derive(Clone, Debug, Default)]
pub struct TranscriptFold {
    exec_timings: Arc<std::collections::BTreeMap<String, rho_agent_types::ExecTiming>>,
    timing_events: Vec<(AgentPos, String, rho_agent_types::ExecMilestone, UnixMs)>,
    /// One past the newest position folded.
    next: AgentPos,
    /// Shared with every state handed out, so a row costs the blocks it
    /// adds or changes and a state is a list of pointers.
    blocks: Vec<Arc<UiBlock>>,
    /// Where each block came from, so a rewind drops exactly what was
    /// told after it and keeps the rest.
    told_at: Vec<AgentPos>,
    /// Messages no request has carried yet; drawn after the blocks.
    queue: Vec<(AgentPos, Option<u64>, UiBlock)>,
    /// The agent stopped on an error and nobody has written since.
    errored: bool,
    /// What the last reply said the context holds, and where it said it.
    context_used: Option<(AgentPos, u64)>,
    digest: Digest,
    /// The lowest index of the folded transcript whose block has moved
    /// since a delta was last taken, or `None` when none has. Handing the
    /// whole state made every appended row cost the whole transcript;
    /// this is what a row costs instead.
    dirty_from: Option<usize>,
}

/// What one telling changed: where the transcript first differs, and the
/// blocks from there on. Everything before `from` is the same pointer it
/// already was, so a reader replaces a suffix rather than a state.
#[derive(Clone, Debug)]
pub struct FoldDelta {
    pub exec_timings: Arc<std::collections::BTreeMap<String, rho_agent_types::ExecTiming>>,
    pub from: usize,
    pub blocks: Vec<Arc<UiBlock>>,
    pub status: UiAgentStatus,
    pub context_used: Option<u64>,
    pub usage: UiAgentUsage,
}

impl TranscriptFold {
    pub fn new(events: &[(AgentPos, TranscriptEvent)]) -> Self {
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
        // The queue is drawn after the blocks, so a block landing moves
        // every queued row along with it.
        self.touch(self.blocks.len());
        self.blocks.push(Arc::new(block));
        self.told_at.push(pos);
    }

    /// Notes that the transcript differs from `at` on.
    fn touch(&mut self, at: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(at, |held| held.min(at)));
    }

    /// The composed transcript from one index on: the folded blocks, then
    /// the messages no request has carried yet.
    fn composed_from(&self, from: usize) -> Vec<Arc<UiBlock>> {
        let queued = self
            .queue
            .iter()
            .map(|(_, _, queued)| Arc::new(queued.clone()));
        if from < self.blocks.len() {
            self.blocks[from..].iter().cloned().chain(queued).collect()
        } else {
            queued.skip(from - self.blocks.len()).collect()
        }
    }

    /// What has moved since this was last asked. `None` when nothing has,
    /// which is what a row the reader had already seen answers.
    pub fn delta(&mut self) -> Option<FoldDelta> {
        let from = self.dirty_from.take()?;
        let state = self.state();
        Some(FoldDelta {
            exec_timings: state.exec_timings,
            from,
            blocks: self.composed_from(from),
            status: state.status,
            context_used: state.context_used,
            usage: state.usage,
        })
    }

    /// Each result lands on the call it answers: its status and its two
    /// timestamps. Where the log holds the output is not kept, because
    /// nothing in the transcript draws it.
    fn report_calls(&mut self, calls: &[String]) {
        for id in calls {
            if let Some(index) = self
                .blocks
                .iter()
                .rposition(|block| matches!(&**block, UiBlock::Tool(tool) if &tool.id == id))
            {
                self.touch(index);
                if let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index]) {
                    tool.status = UiToolStatus::Reported;
                    tool.started_at = None;
                    tool.finished_at = None;
                }
            }
        }
    }

    fn finish_calls(&mut self, results: &[ToolOutcome]) {
        for result in results {
            let called = self
                .blocks
                .iter()
                .rposition(|block| matches!(&**block, UiBlock::Tool(tool) if tool.id == result.id));
            if let Some(index) = called {
                self.touch(index);
            }
            if let Some(index) = called
                && let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index])
            {
                tool.status = match result.status {
                    ToolStatus::Success => UiToolStatus::Success,
                    ToolStatus::Reported => UiToolStatus::Reported,
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
    pub fn tell(&mut self, pos: AgentPos, event: &TranscriptEvent) -> bool {
        if pos < self.next {
            return false;
        }
        self.next = pos.next();
        self.digest.tell(pos, event);
        match event {
            TranscriptEvent::ExecObserved { id, milestone, at } => {
                self.timing_events.push((pos, id.clone(), *milestone, *at));
                Arc::make_mut(&mut self.exec_timings)
                    .entry(id.clone())
                    .or_default()
                    .observe(*milestone, *at);
                let changed = self
                    .blocks
                    .iter()
                    .rposition(|block| matches!(&**block, UiBlock::Tool(tool) if &tool.id == id));
                self.touch(changed.unwrap_or(self.blocks.len()));
                if let Some(index) = changed
                    && let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index])
                {
                    tool.timing = self.exec_timings[id];
                }
            }

            TranscriptEvent::Message { from, text, .. } => {
                self.errored = false;
                self.touch(self.blocks.len() + self.queue.len());
                self.queue.push((
                    pos,
                    None,
                    UiBlock::QueuedMessage {
                        text: text.clone(),
                        sender: *from,
                    },
                ));
            }
            TranscriptEvent::Received { id, from, text, .. } => {
                self.errored = false;
                self.touch(self.blocks.len() + self.queue.len());
                self.queue.push((
                    pos,
                    Some(*id),
                    UiBlock::QueuedMessage {
                        text: text.clone(),
                        sender: *from,
                    },
                ));
            }
            TranscriptEvent::CompactionRequested { .. } => {
                self.touch(self.blocks.len() + self.queue.len());
                self.queue.push((
                    pos,
                    None,
                    UiBlock::Notice {
                        text: "compacting context".to_owned(),
                    },
                ));
            }
            TranscriptEvent::QueueCleared { .. } => {
                self.touch(self.blocks.len());
                self.queue.clear();
            }
            TranscriptEvent::Sent {
                results,
                compaction,
                ..
            } => {
                for (_, _, queued) in std::mem::take(&mut self.queue) {
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
            TranscriptEvent::NotebookReport {
                calls,
                delivered: delivered_ids,
                acknowledged,
                compaction,
                ..
            } => {
                let queue = std::mem::take(&mut self.queue);
                for (queued_at, id, queued) in queue {
                    if id.is_some_and(|id| delivered_ids.contains(&id)) {
                        self.push(pos, delivered(queued));
                    } else if id.is_some_and(|id| acknowledged.contains(&id))
                        || (*compaction && id.is_none() && matches!(queued, UiBlock::Notice { .. }))
                    {
                        self.touch(self.blocks.len() + self.queue.len());
                    } else {
                        self.queue.push((queued_at, id, queued));
                    }
                }
                if *compaction {
                    self.push(
                        pos,
                        UiBlock::Notice {
                            text: "compacting context".to_owned(),
                        },
                    );
                }
                self.report_calls(calls);
            }
            // A status is the status line, not a message.
            TranscriptEvent::MessageSent {
                kind: SendKind::Status,
                ..
            } => {}
            TranscriptEvent::MessageSent { to, text, .. } => {
                self.push(
                    pos,
                    UiBlock::MessageSent {
                        to: *to,
                        text: text.clone(),
                    },
                );
            }
            TranscriptEvent::Results { results, .. } => self.finish_calls(results),
            TranscriptEvent::Replied {
                items,
                compacted,
                context_used,
                at,
                ..
            } => {
                if let Some(used) = context_used {
                    self.context_used = Some((pos, *used));
                }
                for item in items {
                    let mut block = crate::store::block(item);
                    if let UiBlock::Tool(tool) = &mut block {
                        tool.timing = self.exec_timings.get(&tool.id).copied().unwrap_or_default();
                        tool.started_at = Some(*at);
                    }
                    self.push(pos, block);
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
            // Claude's transcript is mirrored message by message. Its queue
            // is live state (Claude Code never persists one), never rows.
            TranscriptEvent::ClaudeMessage { speaker, text, .. } => {
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
            TranscriptEvent::Stopped { error, .. } => {
                self.errored = true;
                // A call the agent never answered is over too. Only a
                // running one is copied out of its sharing.
                for block in &mut self.blocks {
                    if matches!(&**block, UiBlock::Tool(tool) if tool.status == UiToolStatus::Running)
                        && let UiBlock::Tool(tool) = Arc::make_mut(block)
                    {
                        tool.status = UiToolStatus::Cancelled;
                    }
                }
                self.push(
                    pos,
                    UiBlock::Notice {
                        text: error.clone(),
                    },
                );
            }
            // What the model said before its request failed stays
            // readable; a retry says so, a final failure is `Stopped`
            // right after.
            TranscriptEvent::Failed {
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
            TranscriptEvent::Rewound { to, .. } => {
                self.timing_events.retain(|(pos, ..)| pos < to);
                let timings = Arc::make_mut(&mut self.exec_timings);
                timings.clear();
                for (_, id, milestone, at) in &self.timing_events {
                    timings
                        .entry(id.clone())
                        .or_default()
                        .observe(*milestone, *at);
                }
                // Earlier tool rows may have been updated by a now-rewound
                // handoff.
                for block in &mut self.blocks {
                    if let UiBlock::Tool(tool) = Arc::make_mut(block) {
                        tool.timing = timings.get(&tool.id).copied().unwrap_or_default();
                    }
                }
                self.touch(0);

                let kept = self.told_at.iter().take_while(|told| *told < to).count();
                self.touch(kept);
                self.blocks.truncate(kept);
                self.told_at.truncate(kept);
                self.queue.retain(|(queued_at, _, _)| queued_at < to);
                if self.context_used.is_some_and(|(said_at, _)| said_at >= *to) {
                    self.context_used = None;
                }
            }
            TranscriptEvent::Created { .. }
            | TranscriptEvent::RoleChanged { .. }
            | TranscriptEvent::Notice { .. }
            | TranscriptEvent::Presented { .. } => {}
            TranscriptEvent::NotebookActivity { .. } => {
                self.touch(self.blocks.len() + self.queue.len());
            }
        }
        true
    }

    /// The transcript as it stands: the blocks, then what is queued.
    pub fn state(&self) -> UiAgentState {
        let mut blocks = self.blocks.clone();
        blocks.extend(
            self.queue
                .iter()
                .map(|(_, _, queued)| Arc::new(queued.clone())),
        );
        UiAgentState {
            exec_timings: self.exec_timings.clone(),
            blocks,
            runtime: None,
            // Never `Streaming`: this is the mirror, not the live tail.
            // Whether the agent is working is the agent host's to report.
            status: if self.errored {
                UiAgentStatus::Error
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

    use super::*;
    use crate::protocol::transcript::{ArgumentsFormat, Item, ToolOutcome, Usage};
    use crate::state::UiTool;

    pub(crate) fn test_place() -> Place {
        Place {
            workset: "0123456789ab".into(),
            cwd: "/src/repo".into(),
            origin: None,
        }
    }

    fn told(events: Vec<TranscriptEvent>) -> UiAgentState {
        transcript(
            &events
                .into_iter()
                .enumerate()
                .map(|(pos, event)| (AgentPos(pos as u64), event))
                .collect::<Vec<_>>(),
        )
    }

    fn user(text: &str, at: u64) -> TranscriptEvent {
        TranscriptEvent::Message {
            from: None,
            text: text.to_owned(),
            at: UnixMs(at),
        }
    }

    /// A long transcript, one page appended: the reader is told where the
    /// transcript first differs, and that is the end of what it already
    /// had. Handing the state whole made a row cost every row above it.
    #[test]
    fn committed_items_keep_live_order_and_phase_and_rewind_together() {
        use crate::protocol::transcript::{ArgumentsFormat, Item, TextPhase};
        let items = vec![
            Item::Text {
                text: "before".into(),
                phase: Some(TextPhase::Commentary),
            },
            Item::ToolCall {
                id: "middle".into(),
                name: "exec".into(),
                arguments: "print(42)".into(),
                format: ArgumentsFormat::Text,
            },
            Item::Reasoning {
                text: "after call".into(),
            },
            Item::Text {
                text: "last".into(),
                phase: Some(TextPhase::FinalAnswer),
            },
        ];
        let event = TranscriptEvent::Replied {
            items: items.clone(),
            compacted: false,
            usage: None,
            context_used: Some(73),
            at: UnixMs(19),
        };
        let mut fold = TranscriptFold::new(&[(AgentPos(0), user("question", 1))]);
        fold.tell(
            AgentPos(1),
            &TranscriptEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at: UnixMs(2),
            },
        );
        fold.tell(AgentPos(2), &event);
        for (index, item) in items.iter().enumerate() {
            let expected = crate::store::block(item);
            let actual = &*fold.blocks[index + 1];
            match (actual, expected) {
                (UiBlock::Tool(tool), UiBlock::Tool(expected)) => {
                    assert_eq!(tool.id, expected.id);
                    assert_eq!(tool.arguments, "print(42)");
                    assert_eq!(tool.started_at, Some(UnixMs(19)));
                }
                (actual, expected) => assert_eq!(actual, &expected),
            }
        }
        fold.tell(
            AgentPos(3),
            &TranscriptEvent::Rewound {
                to: AgentPos(2),
                at: UnixMs(20),
            },
        );
        assert_eq!(fold.blocks.len(), 1);
    }

    #[test]
    fn a_page_for_the_open_agent_costs_its_rows() {
        let mut fold = TranscriptFold::default();
        let mut pos = 0u64;
        let mut tell = |fold: &mut TranscriptFold, event: TranscriptEvent| {
            fold.tell(AgentPos(pos), &event);
            pos += 1;
        };
        for nth in 0..64 {
            tell(&mut fold, user(&format!("row {nth}"), nth as u64));
            tell(
                &mut fold,
                TranscriptEvent::Sent {
                    results: Vec::new(),
                    compaction: false,
                    at: UnixMs(nth as u64),
                },
            );
        }
        // The one hand-off that is whole: the reader had nothing.
        let whole = fold.state();
        let held = whole.blocks.len();
        assert_eq!(held, 64, "one block per delivered message");
        fold.delta().expect("the first read hands everything");
        assert!(fold.delta().is_none(), "nothing has moved since");

        tell(&mut fold, user("one more", 100));
        tell(
            &mut fold,
            TranscriptEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at: UnixMs(100),
            },
        );
        let delta = fold.delta().expect("a row moved the transcript");
        assert_eq!(
            delta.from, held,
            "the transcript first differs where it used to end"
        );
        assert_eq!(
            delta.blocks.len(),
            1,
            "and what differs is the row appended"
        );

        let mut store = crate::store::AgentStore::default();
        let agent =
            AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).expect("an agent id");
        // The reader opened the agent and was handed the transcript whole,
        // once; the page arrives after that.
        store.set_fold(agent, whole);
        let summary = store.apply_fold_delta(agent, delta);
        assert_eq!(
            summary.first_changed_block,
            Some(held),
            "the reader renders from there, not from the top"
        );
        assert_eq!(
            store.get(&agent).map(|state| state.blocks.len()),
            Some(held + 1),
            "and the transcript it holds is the whole of it"
        );
    }

    fn only_tool(state: &UiAgentState) -> &UiTool {
        state
            .blocks
            .iter()
            .find_map(|block| match &**block {
                UiBlock::Tool(tool) => Some(tool),
                _ => None,
            })
            .expect("the call is a tool block")
    }

    /// A code-mode `exec` call's arguments are JavaScript source, not JSON,
    /// so the mirror must preserve the complete source. The
    /// call has to carry them itself or the transcript shows the word
    /// "exec" and the code is gone, which is what the user saw.
    #[test]
    fn an_exec_call_folds_to_a_tool_whose_arguments_are_the_code() {
        let code = "const files = await tools.exec_command({ cmd: 'ls' });\nconsole.log(files);\n";
        let state = told(vec![
            user("what is in there", 1),
            TranscriptEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at: UnixMs(2),
            },
            TranscriptEvent::Replied {
                items: vec![Item::ToolCall {
                    id: "call-1".to_owned(),
                    name: "exec".to_owned(),
                    arguments: code.to_owned(),
                    format: ArgumentsFormat::Text,
                }],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(2),
            },
        ]);
        let tool = only_tool(&state);
        assert_eq!(tool.arguments, code, "the whole cell, every line of it");
    }

    /// And the reduction that lost it still leaves the label its command: a
    /// shell call carries its arguments whole, which is what the transcript
    /// reads its label out of.
    #[test]
    fn a_shell_call_still_carries_its_command() {
        let state = told(vec![
            user("build it", 1),
            TranscriptEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at: UnixMs(2),
            },
            TranscriptEvent::Replied {
                items: vec![Item::ToolCall {
                    id: "call-1".to_owned(),
                    name: "shell_command".to_owned(),
                    arguments: r#"{"command":"cargo build"}"#.to_owned(),
                    format: ArgumentsFormat::Json,
                }],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(2),
            },
        ]);
        let tool = only_tool(&state);
        assert_eq!(tool.name, "shell_command");
        assert_eq!(tool.arguments, r#"{"command":"cargo build"}"#);
    }

    #[test]
    fn a_told_turn_reads_as_a_transcript() {
        let state = told(vec![
            user("have a look", 1),
            TranscriptEvent::Sent {
                results: Vec::new(),
                compaction: false,
                at: UnixMs(2),
            },
            TranscriptEvent::Replied {
                items: vec![Item::ToolCall {
                    id: "call-1".to_owned(),
                    name: "Read".to_owned(),
                    arguments: r#"{"file_path":"/tmp/README.md"}"#.to_owned(),
                    format: ArgumentsFormat::Json,
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
            TranscriptEvent::Sent {
                results: vec![ToolOutcome {
                    id: "call-1".to_owned(),
                    status: ToolStatus::Error,
                    started_at: UnixMs(3),
                    finished_at: UnixMs(4),
                }],
                compaction: false,
                at: UnixMs(4),
            },
            TranscriptEvent::Replied {
                items: vec![Item::Text {
                    text: "done looking".to_owned(),
                    phase: None,
                }],
                compacted: false,
                usage: Some(Usage {
                    model: "sol".to_owned(),
                    output_tokens: 7,
                    ..Default::default()
                }),
                context_used: Some(12),
                at: UnixMs(5),
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
        assert_eq!(
            tool.arguments, r#"{"file_path":"/tmp/README.md"}"#,
            "the call carries what the model sent; the label reads the path \
             out of it at render time"
        );
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
            TranscriptEvent::QueueCleared { at: UnixMs(2) },
        ]);
        assert!(state.blocks.is_empty());
    }

    /// A Claude agent's tool results carry nothing out of the queue: the
    /// message waits until Claude's own echo of it.
    #[test]
    fn results_alone_leave_the_queue_where_it_is() {
        let state = told(vec![
            user("later", 1),
            TranscriptEvent::Results {
                results: Vec::new(),
                at: UnixMs(2),
            },
        ]);
        assert_eq!(state.blocks.len(), 1);
        assert!(matches!(*state.blocks[0], UiBlock::QueuedMessage { .. }));
    }

    fn replied_with_context(used: u64, at: u64) -> TranscriptEvent {
        TranscriptEvent::Replied {
            items: Vec::new(),
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
            TranscriptEvent::Rewound {
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
    fn a_stopped_agent_ends_in_a_notice() {
        let state = told(vec![TranscriptEvent::Stopped {
            error: "the deploy script exited 1".to_owned(),
            at: UnixMs(2),
        }]);
        assert_eq!(state.status, UiAgentStatus::Error);
        assert_eq!(
            state.blocks,
            vec![Arc::new(UiBlock::Notice {
                text: "the deploy script exited 1".to_owned()
            })]
        );
    }
    /// What the model said before its request failed stays on screen:
    /// a retry says so after it, a final failure is the stop's notice.
    #[test]
    fn a_failed_request_keeps_what_was_said() {
        let state = told(vec![
            TranscriptEvent::Failed {
                text: "half an answer".to_owned(),
                error: "overloaded".to_owned(),
                retrying: true,
                at: UnixMs(2),
            },
            TranscriptEvent::Failed {
                text: "another half".to_owned(),
                error: "quota".to_owned(),
                retrying: false,
                at: UnixMs(3),
            },
            TranscriptEvent::Stopped {
                error: "quota".to_owned(),
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

    fn sent(kind: SendKind, at: u64) -> TranscriptEvent {
        TranscriptEvent::MessageSent {
            to: None,
            text: format!("sent at {at}"),
            kind,
            at: UnixMs(at),
        }
    }

    /// The card counts from the oldest unread of the strongest kind, and a
    /// stop is as strong as a question (`rho-dealer/cases.md`, A5, A8).
    #[test]
    fn the_strongest_unread_counts_from_its_oldest() {
        let mut digest = Digest::default();
        digest.tell(AgentPos(0), &user("go", 1));
        digest.tell(AgentPos(1), &sent(SendKind::Result, 2));
        digest.tell(
            AgentPos(2),
            &TranscriptEvent::Stopped {
                error: "quota".to_owned(),
                at: UnixMs(3),
            },
        );
        digest.tell(AgentPos(3), &sent(SendKind::Other, 4));
        digest.tell(AgentPos(4), &sent(SendKind::Ask, 5));
        let strongest = |seen| {
            digest
                .strongest_unread(AgentPos(seen))
                .map(|unread| (unread.said, unread.pos.0))
        };
        assert_eq!(strongest(0), Some((Said::Stopped, 2)));
        assert_eq!(strongest(3), Some((Said::Ask, 4)));
        assert_eq!(strongest(5), None);

        let mut digest = Digest::default();
        digest.tell(AgentPos(0), &sent(SendKind::Other, 1));
        digest.tell(AgentPos(1), &sent(SendKind::Result, 2));
        assert_eq!(
            digest
                .strongest_unread(AgentPos::ZERO)
                .map(|unread| unread.said),
            Some(Said::Result),
            "a result outranks an older aside"
        );
    }

    /// Only the conversation counts: a status, a retried error and mail to
    /// another agent put nothing to the user, and writing reads it all.
    #[test]
    fn unread_is_what_the_agent_put_to_the_user_since_they_wrote() {
        let mut digest = Digest::default();
        digest.tell(AgentPos(0), &sent(SendKind::Status, 1));
        digest.tell(
            AgentPos(1),
            &TranscriptEvent::Failed {
                text: String::new(),
                error: "overloaded".to_owned(),
                retrying: true,
                at: UnixMs(2),
            },
        );
        digest.tell(
            AgentPos(2),
            &TranscriptEvent::MessageSent {
                to: Some(AgentId::from_counter(9, &rho_agent_types::AgentIdDomain(0)).unwrap()),
                text: "mail".to_owned(),
                kind: SendKind::Ask,
                at: UnixMs(3),
            },
        );
        assert!(digest.unread.is_empty());
        assert_eq!(
            digest.status,
            Some((AgentPos(0), "sent at 1".to_owned())),
            "mail to an agent is not the conversation and leaves the status"
        );
        assert_eq!(digest.last_sent_at, None);

        digest.tell(AgentPos(3), &sent(SendKind::Result, 4));
        assert_eq!(digest.status, None, "a later send hides the status");
        assert_eq!(digest.last_sent_at, Some(UnixMs(4)));
        digest.tell(AgentPos(4), &sent(SendKind::Status, 5));
        assert_eq!(digest.unread.len(), 1);
        digest.tell(AgentPos(5), &user("thanks", 6));
        assert!(
            digest.unread.is_empty(),
            "writing reads everything before it"
        );
        assert_eq!(digest.status, None, "and hides the status");
    }
    #[test]
    fn report_delivers_exact_messages_and_reports_call_while_task_runs() {
        let mut fold = TranscriptFold::default();
        let mut tell = |pos, event| {
            fold.tell(AgentPos(pos), &event);
        };
        tell(
            0,
            TranscriptEvent::NotebookActivity {
                responding: true,
                running_tasks: 2,
                checkin_at: None,
                archived: false,
                at: UnixMs(1),
            },
        );
        tell(
            1,
            TranscriptEvent::Replied {
                items: vec![Item::ToolCall {
                    id: "exec-1".into(),
                    name: "exec".into(),
                    arguments: "print(1)".into(),
                    format: ArgumentsFormat::Text,
                }],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(2),
            },
        );
        for (pos, id) in [(2, 41), (3, 73), (4, 82)] {
            tell(
                pos,
                TranscriptEvent::Received {
                    id,
                    from: None,
                    text: format!("message-{id}"),
                    at: UnixMs(pos),
                },
            );
        }
        tell(
            5,
            TranscriptEvent::NotebookReport {
                calls: vec!["exec-1".into()],
                delivered: vec![41],
                acknowledged: vec![73],
                compaction: false,
                at: UnixMs(5),
            },
        );
        let state = fold.state();
        assert!(state.runtime.is_none());
        assert_eq!(fold.digest.notebook.unwrap().running_tasks, 2);
        let UiBlock::Tool(tool) = &*state.blocks[0] else {
            panic!("expected tool")
        };
        assert_eq!(tool.status, UiToolStatus::Reported);
        assert_eq!((tool.started_at, tool.finished_at), (None, None));
        assert_eq!(
            *state.blocks[1],
            UiBlock::UserMessage {
                text: "message-41".into()
            }
        );
        assert!(
            matches!(&*state.blocks[2], UiBlock::QueuedMessage { text, .. } if text == "message-82")
        );
        assert_eq!(
            state.blocks.len(),
            3,
            "acknowledged message is not delivered"
        );
    }

    /// A rewind hides what it undid and keeps what came before it.
    #[test]
    fn a_rewind_hides_what_it_undid() {
        let state = told(vec![
            TranscriptEvent::ClaudeMessage {
                speaker: Speaker::User,
                text: "first".to_owned(),
                at: UnixMs(1),
            },
            TranscriptEvent::ClaudeMessage {
                speaker: Speaker::Assistant,
                text: "second".to_owned(),
                at: UnixMs(2),
            },
            TranscriptEvent::Rewound {
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
        let agent_id = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let created = TranscriptEvent::Created {
            role: AgentRole::default(),
            runtime: RuntimeKind::Rho,
            place: test_place(),
            spawned_by: SpawnedBy::Direct,
            spawn_name: None,
            parent: None,
            model: "sol".to_owned(),
            at: UnixMs(1),
        };
        assert!(MirroredAgent::new(host, agent_id, &user("x", 1)).is_none());
        let mut mirrored = MirroredAgent::new(host, agent_id, &created).unwrap();
        assert!(mirrored.tell(AgentPos(1), &user("do the thing\nand then some", 10)));
        assert_eq!(mirrored.digest.last_user_message_text, "do the thing");
        assert!(mirrored.tell(AgentPos(2), &sent(SendKind::Status, 11)));
        assert!(mirrored.tell(AgentPos(3), &sent(SendKind::Ask, 12)));
        assert_eq!(mirrored.digest.unread.len(), 1);
        // Replaying a position the fold already holds changes nothing.
        assert!(!mirrored.tell(AgentPos(3), &user("again", 99)));
        assert_eq!(mirrored.digest.newest, AgentPos(4));
        assert_eq!(mirrored.digest.last_active, UnixMs(12));
        assert!(mirrored.tell(
            AgentPos(4),
            &TranscriptEvent::Rewound {
                to: AgentPos(2),
                at: UnixMs(14),
            }
        ));
        assert!(mirrored.digest.unread.is_empty());
        assert_eq!(mirrored.digest.status, None);
    }
    #[test]
    fn exec_observations_survive_commit_and_rewind_without_retiming() {
        use rho_agent_types::ExecMilestone::*;
        let mut fold = TranscriptFold::default();
        let tell = |fold: &mut TranscriptFold, pos, milestone, at| {
            fold.tell(
                AgentPos(pos),
                &TranscriptEvent::ExecObserved {
                    id: "exec-1".into(),
                    milestone,
                    at: UnixMs(at),
                },
            );
        };
        tell(&mut fold, 0, FirstBlock, 10);
        tell(&mut fold, 1, ArgumentsFinished, 20);
        let early = fold.state();
        assert_eq!(
            early.exec_timings["exec-1"].first_block_at,
            Some(UnixMs(10))
        );
        tell(&mut fold, 2, ResponseFinished, 30);
        tell(&mut fold, 3, Boundary, 40);
        tell(&mut fold, 4, HandedOff, 50);
        // Redelivery of one observation is idempotent even at a new log
        // position.
        tell(&mut fold, 5, FirstBlock, 99);
        assert_eq!(
            fold.state().exec_timings["exec-1"].first_block_at,
            Some(UnixMs(10))
        );
        fold.tell(
            AgentPos(6),
            &TranscriptEvent::Rewound {
                to: AgentPos(3),
                at: UnixMs(60),
            },
        );
        let timing = fold.state().exec_timings["exec-1"];
        assert_eq!(timing.response_finished_at, Some(UnixMs(30)));
        assert_eq!(timing.boundary_at, None);
        assert_eq!(timing.handed_off_at, None);
    }
}
