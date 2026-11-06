//! The fold: what a client makes of the directory, chat messages, and the
//! workset rows it reads.
//!
//! The host sends facts: the chat log, the directory's whole entries, and a
//! workset's rows when a transcript is open. Everything a rail or a
//! transcript shows is folded from them here, on the client, so the wire
//! carries facts and never conclusions.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rho_agent_types::{
    Address, AgentId, Message, MessageId, Place, UnixMs, WorksetKind, Wpos, props,
};

use crate::HostId;
use crate::protocol::AgentUsageBucket;
use crate::protocol::transcript::{RuntimeKind, Speaker, ToolOutcome, ToolStatus};
use crate::protocol::worksets::{AgentEntry, LogEvent};
use crate::state::{UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock, UiToolStatus};

/// What an agent is: its directory entry and the kind of workset it is
/// in. Never what has happened to it; that is the [`Digest`].
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct AgentIdentity {
    pub entry: AgentEntry,
    pub kind: WorksetKind,
}

impl AgentIdentity {
    pub fn agent_id(&self) -> AgentId {
        self.entry.agent
    }

    fn prop(&self, key: &str) -> Option<&str> {
        self.entry
            .spec
            .props
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    }

    /// The agent's name for a reader: what Python named it, else the title
    /// generated for it.
    pub fn name(&self) -> Option<&str> {
        self.prop(props::NAME).or_else(|| self.prop(props::TITLE))
    }

    /// The agent this one answers to. A parent that does not parse is
    /// none: the prop is Python's to write, and the GUI does not trust it.
    pub fn parent(&self) -> Option<AgentId> {
        AgentId::from_encoded(self.prop(props::PARENT)?).ok()
    }

    /// Whether the user manages the agent: one no other agent made.
    pub fn user_facing(&self) -> bool {
        self.parent().is_none()
    }

    /// The prefix of the agent's label: `adv` for an advisor, `eng` for
    /// everything else.
    pub fn handle_prefix(&self) -> &'static str {
        match self.prop(PRESET) {
            Some("advisor") => "adv",
            _ => "eng",
        }
    }

    pub fn is_advisor(&self) -> bool {
        self.handle_prefix() == "adv"
    }

    /// Where the agent works: its `cwd` prop, else where its workset
    /// starts. An isolated workset is reached by its id; a host one is the
    /// host's own directory.
    pub fn place(&self) -> Place {
        let start = match &self.kind {
            WorksetKind::Isolated => camino::Utf8Path::new("/src"),
            WorksetKind::Host { cwd } => cwd.as_path(),
        };
        let cwd = self.prop(props::CWD).map_or(start, camino::Utf8Path::new);
        Place {
            cwd: cwd.to_owned(),
            workset: match self.kind {
                WorksetKind::Isolated => Some(self.entry.workset),
                WorksetKind::Host { .. } => None,
            },
        }
    }

    pub fn runtime(&self) -> RuntimeKind {
        self.entry.spec.runtime
    }
}

impl AgentIdentity {
    /// An agent of an isolated workset with these props, on the default
    /// runtime: what a test that folds messages into an agent starts from.
    pub fn for_test(agent: AgentId, props: rho_agent_types::Props, created_at: UnixMs) -> Self {
        Self {
            entry: AgentEntry {
                agent,
                workset: rho_agent_types::WorksetId([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]),
                spec: crate::protocol::worksets::SpecView {
                    runtime: RuntimeKind::Rho,
                    model: "sol".into(),
                    effort: "medium".into(),
                    props,
                },
                created_at,
            },
            kind: WorksetKind::Isolated,
        }
    }
}

/// The prop that names the preset an agent was made from; the label's
/// prefix reads it.
pub const PRESET: &str = "preset";

/// What the rails read of an agent, folded from the messages it posted or
/// received. Only messages count: whether it is running does not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Digest {
    pub last_active: UnixMs,
    pub last_user_message_at: UnixMs,
    pub last_user_message_text: String,
}

impl Digest {
    /// Folds one message the agent posted or received. The chat fold hands
    /// each message over once ([`crate::chat::HostChat::tell`]).
    pub fn tell(&mut self, message: &Message) {
        self.last_active = self.last_active.max(message.at);
        if message.from == Address::User {
            self.last_user_message_at = message.at;
            self.last_user_message_text = one_line(&text(message));
        }
    }
}

fn text(message: &Message) -> String {
    rho_agent_types::transcript::text_content(&message.content)
}

/// One agent as the client holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredAgent {
    pub host: HostId,
    pub identity: AgentIdentity,
    pub digest: Digest,
}

impl MirroredAgent {
    /// An agent the directory named, before any of its messages.
    pub fn new(host: HostId, identity: AgentIdentity) -> Self {
        Self {
            host,
            identity,
            digest: Digest::default(),
        }
    }

    pub fn agent_id(&self) -> AgentId {
        self.identity.agent_id()
    }

    /// The agent's name for a reader.
    pub fn title(&self) -> Option<&str> {
        self.identity.name()
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

/// One thing a transcript is folded from: a row of the agent's workset log
/// that is about it, or a message it posted or received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptRow {
    Log(Wpos, LogEvent),
    Message(Message),
}

impl TranscriptRow {
    pub fn at(&self) -> UnixMs {
        match self {
            Self::Log(_, event) => event.at(),
            Self::Message(message) => message.at,
        }
    }
}

/// An agent's workset rows and its messages as one transcript, in time order:
/// each is in its own order already, and the two meet where their clocks
/// say. Rows about other agents are left out.
pub fn interleave(
    agent: AgentId,
    rows: impl IntoIterator<Item = (Wpos, LogEvent)>,
    messages: impl IntoIterator<Item = Message>,
) -> Vec<TranscriptRow> {
    let mut rows = rows
        .into_iter()
        .filter(|(_, event)| event.agent() == Some(agent))
        .map(|(pos, event)| TranscriptRow::Log(pos, event))
        .peekable();
    let mut messages = messages.into_iter().map(TranscriptRow::Message).peekable();
    let mut merged = Vec::new();
    loop {
        let next = match (rows.peek(), messages.peek()) {
            (Some(row), Some(message)) if message.at() < row.at() => messages.next(),
            (Some(_), _) => rows.next(),
            (None, _) => messages.next(),
        };
        match next {
            Some(row) => merged.push(row),
            None => return merged,
        }
    }
}

/// The transcript of one agent, oldest first. Nothing here is invented: a
/// call is its name and its line, a reply is what was said, and an input
/// waits as queued until a request carries it.
pub fn transcript(agent: AgentId, rows: &[TranscriptRow]) -> UiAgentState {
    TranscriptFold::new(agent, rows).state()
}

/// The transcript as a fold that takes one row at a time, so a row costs
/// what it changes and never a walk of the whole transcript.
#[derive(Clone, Debug)]
pub struct TranscriptFold {
    agent: AgentId,
    exec_timings: Arc<std::collections::BTreeMap<String, rho_agent_types::ExecTiming>>,
    timing_events: Vec<(Wpos, String, rho_agent_types::ExecMilestone, UnixMs)>,
    /// One past the newest row folded.
    next: Wpos,
    /// The newest message folded.
    message_newest: Option<MessageId>,
    /// Shared with every state handed out, so a row costs the blocks it
    /// adds or changes and a state is a list of pointers.
    blocks: Vec<Arc<UiBlock>>,
    /// The row each block came from, so a rewind drops exactly what was
    /// told after it. `None` for a message, which no rewind takes back.
    told_at: Vec<Option<Wpos>>,
    /// Compactions no request has carried yet; drawn after the blocks.
    queue: Vec<(Wpos, UiBlock)>,
    /// Messages the agent received that no input has handed it yet; drawn
    /// last.
    inbox: Vec<(MessageId, UiBlock)>,
    /// Messages an input has handed over, so a message heard after its
    /// input is not shown waiting.
    handed: BTreeSet<MessageId>,
    /// Who wrote each message the agent received, for the input that
    /// hands it over.
    senders: HashMap<MessageId, Address>,
    /// The agent stopped on an error and nobody has written since.
    errored: bool,
    /// What the last reply said the context holds, and where it said it.
    context_used: Option<(Wpos, u64)>,
    /// Every reply's usage, summed, and the model the newest one named.
    usage: AgentUsageBucket,
    usage_model: String,
    /// The lowest index of the folded transcript whose block has moved
    /// since a delta was last taken, or `None` when none has.
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
    pub fn new(agent: AgentId, rows: &[TranscriptRow]) -> Self {
        let mut fold = Self {
            agent,
            exec_timings: Arc::default(),
            timing_events: Vec::new(),
            next: Wpos::ZERO,
            message_newest: None,
            blocks: Vec::new(),
            told_at: Vec::new(),
            queue: Vec::new(),
            inbox: Vec::new(),
            handed: BTreeSet::new(),
            senders: HashMap::new(),
            errored: false,
            context_used: None,
            usage: AgentUsageBucket::default(),
            usage_model: String::new(),
            dirty_from: None,
        };
        for row in rows {
            fold.tell(row);
        }
        fold
    }

    fn push(&mut self, told_at: Option<Wpos>, block: UiBlock) {
        // The queue is drawn after the blocks, so a block landing moves
        // every queued row along with it.
        self.touch(self.blocks.len());
        self.blocks.push(Arc::new(block));
        self.told_at.push(told_at);
    }

    /// Notes that the transcript differs from `at` on.
    fn touch(&mut self, at: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(at, |held| held.min(at)));
    }

    /// The end of the queue, where the inbox starts.
    fn queue_end(&self) -> usize {
        self.blocks.len() + self.queue.len()
    }

    fn waiting(&self) -> impl Iterator<Item = Arc<UiBlock>> + '_ {
        self.queue
            .iter()
            .map(|(_, queued)| queued)
            .chain(self.inbox.iter().map(|(_, waiting)| waiting))
            .map(|block| Arc::new(block.clone()))
    }

    /// The composed transcript from one index on: the folded blocks, then
    /// the compactions no request has carried yet, then messages not yet
    /// handed over.
    fn composed_from(&self, from: usize) -> Vec<Arc<UiBlock>> {
        if from < self.blocks.len() {
            self.blocks[from..]
                .iter()
                .cloned()
                .chain(self.waiting())
                .collect()
        } else {
            self.waiting().skip(from - self.blocks.len()).collect()
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

    /// Each reported call is marked reported, its task still running.
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

    /// Each result lands on the call it answers: its status and its two
    /// timestamps. The output is not kept, because nothing in the
    /// transcript draws it.
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

    /// A request went out: every input before it went with it.
    /// Moves what waited into the transcript; returns whether a request
    /// for compaction was among it.
    fn deliver_queue(&mut self, pos: Wpos) -> bool {
        let queue = std::mem::take(&mut self.queue);
        let compaction = !queue.is_empty();
        for (_, queued) in queue {
            self.push(Some(pos), queued);
        }
        compaction
    }

    /// Folds one row. Rows and messages already held are skipped, as are rows
    /// about another agent, so a repeated page is harmless; returns
    /// whether anything was new.
    pub fn tell(&mut self, row: &TranscriptRow) -> bool {
        match row {
            TranscriptRow::Log(pos, event) => {
                if *pos < self.next || event.agent() != Some(self.agent) {
                    return false;
                }
                self.next = pos.next();
                self.tell_row(*pos, event);
            }
            TranscriptRow::Message(message) => {
                if self.message_newest >= Some(message.id) {
                    return false;
                }
                self.message_newest = Some(message.id);
                self.tell_message(message);
            }
        }
        true
    }

    /// A message the agent posted, or one it received: the model hands a
    /// transcript only those.
    fn tell_message(&mut self, message: &Message) {
        if message.from == Address::Agent(self.agent) {
            self.push(
                None,
                UiBlock::MessageSent {
                    to: None,
                    text: text(message),
                },
            );
            return;
        }
        self.senders.insert(message.id, message.from);
        if self.handed.contains(&message.id) {
            return;
        }
        self.errored = false;
        self.touch(self.queue_end() + self.inbox.len());
        self.inbox.push((
            message.id,
            UiBlock::QueuedMessage {
                text: text(message),
                sender: sender(message.from),
            },
        ));
    }

    fn tell_row(&mut self, pos: Wpos, event: &LogEvent) {
        match event {
            LogEvent::ExecObserved {
                id, milestone, at, ..
            } => {
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
            LogEvent::CompactionRequested { .. } => {
                self.touch(self.queue_end());
                self.queue.push((
                    pos,
                    UiBlock::Notice {
                        text: "compacting context".to_owned(),
                    },
                ));
            }
            LogEvent::Request {
                inputs,
                calls,
                compaction,
                ..
            } => {
                if !inputs.is_empty() {
                    self.errored = false;
                }
                for input in inputs {
                    let block = match input.message {
                        Some(id) => {
                            self.handed.insert(id);
                            // The message as its sender wrote it; the row's
                            // text is what the policy made of it for the model.
                            match self.inbox.iter().position(|(waiting, _)| *waiting == id) {
                                Some(index) => {
                                    self.touch(self.queue_end() + index);
                                    self.inbox.remove(index).1
                                }
                                None => UiBlock::QueuedMessage {
                                    text: input.text.clone(),
                                    sender: self.senders.get(&id).copied().and_then(sender),
                                },
                            }
                        }
                        // No message: what the policy has to tell the agent.
                        None => UiBlock::Notice {
                            text: input.text.clone(),
                        },
                    };
                    // A message is still a message: a rewind of the turn it
                    // started does not unsay what was written.
                    self.push(input.message.is_none().then_some(pos), delivered(block));
                }
                let requested = self.deliver_queue(pos);
                // A requested compaction already shows as its request.
                if *compaction && !requested {
                    self.push(
                        Some(pos),
                        UiBlock::Notice {
                            text: "compacting context".to_owned(),
                        },
                    );
                }
                self.report_calls(calls);
            }
            LogEvent::NotebookReport { calls, .. } => self.report_calls(calls),
            LogEvent::Results { results, .. } => self.finish_calls(results),
            LogEvent::Replied {
                items,
                compacted,
                usage,
                context_used,
                at,
                ..
            } => {
                if let Some(used) = context_used {
                    self.context_used = Some((pos, *used));
                }
                if let Some(usage) = usage {
                    self.usage.input_tokens += usage.input_tokens;
                    self.usage.cache_read_tokens += usage.cache_read_tokens;
                    self.usage.cache_write_tokens += usage.cache_write_tokens;
                    self.usage.cache_write_1h_tokens += usage.cache_write_1h_tokens;
                    self.usage.output_tokens += usage.output_tokens;
                    self.usage.requests += 1;
                    self.usage_model = usage.model.clone();
                }
                for item in items {
                    let mut block = crate::store::block(item);
                    if let UiBlock::Tool(tool) = &mut block {
                        tool.timing = self.exec_timings.get(&tool.id).copied().unwrap_or_default();
                        tool.started_at = Some(*at);
                    }
                    self.push(Some(pos), block);
                }
                if *compacted {
                    self.push(
                        Some(pos),
                        UiBlock::Notice {
                            text: "compacted".to_owned(),
                        },
                    );
                }
            }
            // Claude's transcript is mirrored message by message.
            LogEvent::ClaudeMessage { speaker, text, .. } => {
                self.push(
                    Some(pos),
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
            LogEvent::Stopped { error, .. } => {
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
                    Some(pos),
                    UiBlock::Notice {
                        text: error.clone(),
                    },
                );
            }
            // What the model said before its request failed stays
            // readable; a retry says so, a final failure is `Stopped`
            // right after.
            LogEvent::Failed {
                text,
                error,
                retrying,
                ..
            } => {
                if !text.is_empty() {
                    self.push(
                        Some(pos),
                        UiBlock::AssistantMessage {
                            text: text.clone(),
                            phase: None,
                        },
                    );
                }
                if *retrying {
                    self.push(
                        Some(pos),
                        UiBlock::Notice {
                            text: format!("temporary inference error: {error}; retrying"),
                        },
                    );
                }
            }
            // A rewind is told rather than unwritten, so the reader is the
            // one that hides what it undid. Messages stay: they were sent.
            LogEvent::Rewound { to, .. } => {
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

                let undone = |told: &Option<Wpos>| told.is_some_and(|told| told >= *to);
                let blocks = std::mem::take(&mut self.blocks);
                let told_at = std::mem::take(&mut self.told_at);
                for (block, told) in blocks.into_iter().zip(told_at) {
                    if !undone(&told) {
                        self.blocks.push(block);
                        self.told_at.push(told);
                    }
                }
                self.queue.retain(|(queued_at, ..)| queued_at < to);
                if self.context_used.is_some_and(|(said_at, _)| said_at >= *to) {
                    self.context_used = None;
                }
            }
            LogEvent::WorksetCreated { .. }
            | LogEvent::Evaluated { .. }
            | LogEvent::AgentCreated { .. }
            | LogEvent::AgentChanged { .. } => {}
        }
    }

    /// The transcript as it stands: the blocks, then what waits.
    pub fn state(&self) -> UiAgentState {
        let mut blocks = self.blocks.clone();
        blocks.extend(self.waiting());
        UiAgentState {
            exec_timings: self.exec_timings.clone(),
            blocks,
            runtime: None,
            // Never `Streaming`: this is the log, not the live tail.
            // Whether the agent is working is the host's to report.
            status: if self.errored {
                UiAgentStatus::Error
            } else {
                UiAgentStatus::Idle
            },
            context_used: self.context_used.map(|(_, used)| used),
            usage: UiAgentUsage {
                provider: self.usage_model.clone(),
                total: self.usage.clone(),
            },
        }
    }
}

fn sender(from: Address) -> Option<AgentId> {
    match from {
        Address::User => None,
        Address::Agent(agent) => Some(agent),
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
    use crate::protocol::transcript::{ArgumentsFormat, Item, Usage};
    use crate::protocol::worksets::InputView;
    use crate::state::UiTool;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    const ME: u64 = 1;

    fn me() -> AgentId {
        agent(ME)
    }

    /// Rows about the agent under test, at consecutive positions.
    fn told(events: Vec<LogEvent>) -> UiAgentState {
        transcript(
            me(),
            &events
                .into_iter()
                .enumerate()
                .map(|(pos, event)| TranscriptRow::Log(Wpos(pos as u64), event))
                .collect::<Vec<_>>(),
        )
    }

    fn input(text: &str, message: Option<u64>, at: u64) -> LogEvent {
        LogEvent::Request {
            agent: me(),
            inputs: vec![InputView {
                message: message.map(|id| MessageId(rho_agent_types::ChatSeq(id))),
                text: text.to_owned(),
                role: rho_agent_types::Role::User,
            }],
            calls: Vec::new(),
            compaction: false,
            at: UnixMs(at),
        }
    }

    fn sent_row(at: u64) -> LogEvent {
        LogEvent::Request {
            agent: me(),
            inputs: Vec::new(),
            calls: Vec::new(),
            compaction: false,
            at: UnixMs(at),
        }
    }

    fn replied(items: Vec<Item>, usage: Option<Usage>, context: Option<u64>, at: u64) -> LogEvent {
        LogEvent::Replied {
            agent: me(),
            items,
            compacted: false,
            usage,
            context_used: context,
            at: UnixMs(at),
        }
    }

    fn message(id: u64, from: Address, at: u64) -> Message {
        Message {
            id: MessageId(rho_agent_types::ChatSeq(id)),
            at: UnixMs(at),
            topic: rho_agent_types::TopicId(rho_agent_types::ChatSeq(1)),
            from,
            mentions: Vec::new(),
            content: vec![rho_agent_types::ContentPart::Text {
                text: format!("message {id}"),
            }],
        }
    }

    fn to_user(id: u64, kind: rho_agent_types::MentionKind, at: u64) -> Message {
        Message {
            mentions: vec![rho_agent_types::Mention {
                to: Address::User,
                kind,
            }],
            ..message(id, Address::Agent(me()), at)
        }
    }

    fn from_user(id: u64, at: u64) -> Message {
        message(id, Address::User, at)
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

    #[test]
    fn committed_items_keep_live_order_and_phase_and_rewind_together() {
        use crate::protocol::transcript::TextPhase;
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
        ];
        let mut fold = TranscriptFold::new(
            me(),
            &[
                TranscriptRow::Log(Wpos(0), input("question", None, 1)),
                TranscriptRow::Log(Wpos(1), sent_row(2)),
                TranscriptRow::Log(Wpos(2), replied(items.clone(), None, Some(73), 19)),
            ],
        );
        for (index, item) in items.iter().enumerate() {
            let expected = crate::store::block(item);
            match (&*fold.blocks[index + 1], expected) {
                (UiBlock::Tool(tool), UiBlock::Tool(expected)) => {
                    assert_eq!(tool.id, expected.id);
                    assert_eq!(tool.started_at, Some(UnixMs(19)));
                }
                (actual, expected) => assert_eq!(actual, &expected),
            }
        }
        fold.tell(&TranscriptRow::Log(
            Wpos(3),
            LogEvent::Rewound {
                agent: me(),
                to: Wpos(2),
                at: UnixMs(20),
            },
        ));
        assert_eq!(fold.blocks.len(), 1);
    }

    /// A long transcript, one row appended: the reader is told where the
    /// transcript first differs, and that is the end of what it had.
    #[test]
    fn a_page_for_the_open_agent_costs_its_rows() {
        let mut fold = TranscriptFold::new(me(), &[]);
        let mut pos = 0u64;
        let mut tell = |fold: &mut TranscriptFold, event: LogEvent| {
            fold.tell(&TranscriptRow::Log(Wpos(pos), event));
            pos += 1;
        };
        for nth in 0..64 {
            tell(&mut fold, input(&format!("row {nth}"), None, nth));
            tell(&mut fold, sent_row(nth));
        }
        let whole = fold.state();
        let held = whole.blocks.len();
        assert_eq!(held, 64, "one block per delivered input");
        fold.delta().expect("the first read hands everything");
        assert!(fold.delta().is_none(), "nothing has moved since");

        tell(&mut fold, input("one more", None, 100));
        tell(&mut fold, sent_row(100));
        let delta = fold.delta().expect("a row moved the transcript");
        assert_eq!(
            delta.from, held,
            "the transcript first differs where it ended"
        );
        assert_eq!(delta.blocks.len(), 1);

        let mut store = crate::store::AgentStore::default();
        store.set_fold(me(), whole);
        let summary = store.apply_fold_delta(me(), delta);
        assert_eq!(summary.first_changed_block, Some(held));
        assert_eq!(
            store.get(&me()).map(|state| state.blocks.len()),
            Some(held + 1)
        );
    }

    /// A code cell's arguments are its source, kept whole.
    #[test]
    fn an_exec_call_folds_to_a_tool_whose_arguments_are_the_code() {
        let code = "files = os.listdir('.')\nprint(files)\n";
        let state = told(vec![replied(
            vec![Item::ToolCall {
                id: "call-1".to_owned(),
                name: "exec".to_owned(),
                arguments: code.to_owned(),
                format: ArgumentsFormat::Text,
            }],
            None,
            None,
            2,
        )]);
        assert_eq!(only_tool(&state).arguments, code);
    }

    #[test]
    fn a_told_turn_reads_as_a_transcript() {
        let state = told(vec![
            input("have a look", Some(1), 1),
            sent_row(2),
            replied(
                vec![Item::ToolCall {
                    id: "call-1".to_owned(),
                    name: "Read".to_owned(),
                    arguments: r#"{"file_path":"/tmp/README.md"}"#.to_owned(),
                    format: ArgumentsFormat::Json,
                }],
                Some(Usage {
                    model: "sol".to_owned(),
                    output_tokens: 5,
                    ..Default::default()
                }),
                Some(10),
                3,
            ),
            LogEvent::Results {
                agent: me(),
                results: vec![ToolOutcome {
                    id: "call-1".to_owned(),
                    status: ToolStatus::Error,
                    started_at: UnixMs(3),
                    finished_at: UnixMs(4),
                }],
                at: UnixMs(4),
            },
            replied(
                vec![Item::Text {
                    text: "done looking".to_owned(),
                    phase: None,
                }],
                Some(Usage {
                    model: "sol".to_owned(),
                    output_tokens: 7,
                    ..Default::default()
                }),
                Some(12),
                5,
            ),
        ]);
        assert_eq!(state.status, UiAgentStatus::Idle);
        assert_eq!(state.blocks.len(), 3);
        assert_eq!(
            *state.blocks[0],
            UiBlock::UserMessage {
                text: "have a look".to_owned()
            }
        );
        let tool = only_tool(&state);
        assert_eq!(tool.status, UiToolStatus::Error);
        assert_eq!(tool.finished_at, Some(UnixMs(4)));
        assert!(matches!(*state.blocks[2], UiBlock::AssistantMessage { .. }));
        assert_eq!(state.usage.total.output_tokens, 12);
        assert_eq!(state.usage.total.requests, 2);
        assert_eq!(state.usage.provider, "sol");
        assert_eq!(state.context_used, Some(12));
    }

    /// An input may be heard before the message it hands over: the message
    /// then never waits.
    #[test]
    fn a_message_heard_after_its_input_does_not_wait() {
        let mut fold = TranscriptFold::new(me(), &[]);
        fold.tell(&TranscriptRow::Log(Wpos(0), input("hello", Some(9), 2)));
        fold.tell(&TranscriptRow::Message(from_user(9, 1)));
        let state = fold.state();
        assert_eq!(state.blocks.len(), 1);
        assert!(
            !fold.tell(&TranscriptRow::Message(from_user(9, 1))),
            "a replay"
        );
    }

    /// A rewind takes back rows, not messages: what was posted was sent.
    #[test]
    fn a_rewind_hides_rows_but_not_messages() {
        let rows = interleave(
            me(),
            [
                (
                    Wpos(0),
                    LogEvent::ClaudeMessage {
                        agent: me(),
                        speaker: Speaker::User,
                        text: "first".into(),
                        at: UnixMs(1),
                    },
                ),
                (
                    Wpos(1),
                    LogEvent::ClaudeMessage {
                        agent: me(),
                        speaker: Speaker::Assistant,
                        text: "second".into(),
                        at: UnixMs(2),
                    },
                ),
                (
                    Wpos(2),
                    LogEvent::Rewound {
                        agent: me(),
                        to: Wpos(1),
                        at: UnixMs(4),
                    },
                ),
            ],
            [to_user(7, rho_agent_types::MentionKind::Result, 3)],
        );
        assert!(
            matches!(rows[2], TranscriptRow::Message(_)),
            "merged by time"
        );
        let state = transcript(me(), &rows);
        assert_eq!(
            state.blocks,
            vec![
                Arc::new(UiBlock::UserMessage {
                    text: "first".to_owned()
                }),
                Arc::new(UiBlock::MessageSent {
                    to: None,
                    text: "message 7".to_owned()
                }),
            ]
        );
    }

    /// The user's message an input handed over stays when the turn it
    /// started is rewound; the input's own notice and the reply go.
    #[test]
    fn a_rewind_keeps_the_messages_its_inputs_handed_over() {
        let mut fold = TranscriptFold::new(me(), &[]);
        fold.tell(&TranscriptRow::Message(from_user(1, 1)));
        let rows = [
            input("Message from the user:\nmessage 1", Some(1), 2),
            sent_row(3),
            replied(
                vec![Item::Text {
                    text: "answer".into(),
                    phase: None,
                }],
                None,
                None,
                4,
            ),
            input("a policy note", None, 5),
            sent_row(6),
            LogEvent::Rewound {
                agent: me(),
                to: Wpos(0),
                at: UnixMs(7),
            },
        ];
        for (pos, event) in rows.into_iter().enumerate() {
            fold.tell(&TranscriptRow::Log(Wpos(pos as u64), event));
        }
        assert_eq!(
            fold.state().blocks,
            vec![Arc::new(UiBlock::UserMessage {
                text: "message 1".to_owned()
            })]
        );
    }

    /// A compaction the user asked for says so once: the request, carried
    /// by the request that compacts. One nobody asked for still shows.
    #[test]
    fn a_compaction_shows_once() {
        let report = |at| LogEvent::Request {
            agent: me(),
            inputs: Vec::new(),
            calls: Vec::new(),
            compaction: true,
            at: UnixMs(at),
        };
        let notices = |state: UiAgentState| {
            state
                .blocks
                .iter()
                .filter(|block| matches!(&***block, UiBlock::Notice { .. }))
                .count()
        };
        let requested = told(vec![
            LogEvent::CompactionRequested {
                agent: me(),
                at: UnixMs(1),
            },
            report(2),
        ]);
        assert_eq!(notices(requested), 1);
        assert_eq!(notices(told(vec![report(1)])), 1);
    }

    #[test]
    fn the_last_reply_says_how_full_the_context_is_until_rewound() {
        let state = told(vec![
            replied(Vec::new(), None, Some(10), 1),
            replied(Vec::new(), None, Some(12), 2),
            LogEvent::Rewound {
                agent: me(),
                to: Wpos(1),
                at: UnixMs(3),
            },
        ]);
        assert_eq!(
            state.context_used, None,
            "said by a reply the rewind took back"
        );
    }

    /// What the model said before its request failed stays on screen:
    /// a retry says so after it, a final failure is the stop's notice.
    #[test]
    fn a_failed_request_keeps_what_was_said() {
        let state = told(vec![
            LogEvent::Failed {
                agent: me(),
                text: "half an answer".to_owned(),
                error: "overloaded".to_owned(),
                retrying: true,
                at: UnixMs(2),
            },
            LogEvent::Stopped {
                agent: me(),
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
                Arc::new(UiBlock::Notice {
                    text: "quota".to_owned()
                }),
            ]
        );
    }

    #[test]
    fn exec_observations_survive_commit_and_rewind_without_retiming() {
        use rho_agent_types::ExecMilestone::*;
        let mut fold = TranscriptFold::new(me(), &[]);
        let tell = |fold: &mut TranscriptFold, pos, milestone, at| {
            fold.tell(&TranscriptRow::Log(
                Wpos(pos),
                LogEvent::ExecObserved {
                    agent: me(),
                    id: "exec-1".into(),
                    milestone,
                    at: UnixMs(at),
                },
            ));
        };
        tell(&mut fold, 0, FirstBlock, 10);
        tell(&mut fold, 1, ResponseFinished, 30);
        tell(&mut fold, 2, Boundary, 40);
        tell(&mut fold, 3, FirstBlock, 99);
        assert_eq!(
            fold.state().exec_timings["exec-1"].first_block_at,
            Some(UnixMs(10))
        );
        fold.tell(&TranscriptRow::Log(
            Wpos(4),
            LogEvent::Rewound {
                agent: me(),
                to: Wpos(2),
                at: UnixMs(60),
            },
        ));
        let timing = fold.state().exec_timings["exec-1"];
        assert_eq!(timing.response_finished_at, Some(UnixMs(30)));
        assert_eq!(timing.boundary_at, None);
    }

    /// What the user last wrote to the agent is what the rails find it
    /// by; what the agent posts moves only when it was last active.
    #[test]
    fn a_digest_keeps_what_the_user_last_wrote() {
        let mut digest = Digest::default();
        digest.tell(&from_user(1, 1));
        digest.tell(&to_user(2, rho_agent_types::MentionKind::Ask, 5));
        assert_eq!(digest.last_user_message_text, "message 1");
        assert_eq!(digest.last_user_message_at, UnixMs(1));
        assert_eq!(digest.last_active, UnixMs(5));
    }

    #[test]
    fn identity_reads_the_props_the_gui_gives_meaning() {
        let parent = agent(7);
        let mut entry = AgentEntry {
            agent: me(),
            workset: "0123456789ab".parse().unwrap(),
            spec: crate::protocol::worksets::SpecView {
                runtime: RuntimeKind::Rho,
                model: "sol".into(),
                effort: "medium".into(),
                props: Default::default(),
            },
            created_at: UnixMs(1),
        };
        let identity = |entry: &AgentEntry, kind| AgentIdentity {
            entry: entry.clone(),
            kind,
        };
        let bare = identity(&entry, WorksetKind::Isolated);
        assert_eq!(bare.name(), None);
        assert!(bare.user_facing(), "no agent made it");
        assert_eq!(bare.handle_prefix(), "eng");
        assert_eq!(bare.place().cwd, "/src");
        assert_eq!(bare.place().workset, Some(entry.workset));

        for (key, value) in [
            (props::TITLE, "a title"),
            (props::NAME, " "),
            (props::PARENT, &parent.encoded()),
            (props::CWD, "/src/rho"),
            (PRESET, "advisor"),
        ] {
            entry.spec.props.insert(key.into(), value.into());
        }
        let full = identity(
            &entry,
            WorksetKind::Host {
                cwd: "/home/me".into(),
            },
        );
        assert_eq!(full.name(), Some("a title"), "a blank name is no name");
        assert_eq!(full.parent(), Some(parent));
        assert!(!full.user_facing(), "another agent made it");
        assert_eq!(full.handle_prefix(), "adv");
        assert_eq!(full.place().cwd, "/src/rho");
        assert_eq!(full.place().workset, None, "a host workset is the host");
        entry
            .spec
            .props
            .insert(props::PARENT.into(), "not an id".into());
        assert_eq!(identity(&entry, WorksetKind::Isolated).parent(), None);
    }
}
