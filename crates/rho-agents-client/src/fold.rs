//! The fold: what a client makes of mail, the directory, and the workset
//! rows it reads.
//!
//! The host sends facts: the mail log, the directory's whole entries, and a
//! workset's rows when a transcript is open. Everything a rail or a
//! transcript shows is folded from them here, on the client, so the wire
//! carries facts and never conclusions.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rho_agent_types::{
    Address, AgentId, Mail, MailId, Place, SendKind, UnixMs, WorksetKind, Wpos, props,
};

use crate::HostId;
use crate::protocol::AgentUsageBucket;
use crate::protocol::rho2::{AgentEntry, LogEvent};
use crate::protocol::transcript::{
    InferenceState, RuntimeKind, RuntimeState, Speaker, ToolOutcome, ToolStatus,
};
use crate::state::{UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock, UiToolStatus};

/// A place in an agent's conversation with the user, which is how far the
/// user has dealt with it. Mail `m` sits at `2m`. A stop has no mail id, so
/// it sits half a step after the host's newest mail when it was seen:
/// after everything said before it, and before anything said later.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
pub struct ConversationPos(pub u64);

impl ConversationPos {
    pub fn of_mail(mail: MailId) -> Self {
        Self(mail.0.checked_mul(2).expect("mail log overflow"))
    }

    /// A stop seen while `head` was the host's newest mail.
    pub fn of_stop(head: MailId) -> Self {
        Self(Self::of_mail(head).0 + 1)
    }

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("conversation overflow"))
    }
}

/// What the user last said about an agent: how far they have dealt with
/// its conversation, and whether they muted it. The only facts attention
/// needs that the host does not carry; kept by the client.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct Verdict {
    /// One past the newest place the user has dealt with.
    pub handled_through: ConversationPos,
    pub muted: bool,
}

/// What the agent put to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub enum Said {
    /// Mail asking for something the work needs.
    Ask,
    /// The agent stopped on an error: as much an ask as a question.
    Stopped,
    /// Mail delivering what the user asked for.
    Result,
}

impl Said {
    /// How hard it pulls on the user: lower is stronger. A stop pulls as
    /// hard as a question.
    pub fn strength(self) -> u8 {
        match self {
            Self::Ask | Self::Stopped => 0,
            Self::Result => 1,
        }
    }
}

/// One thing the agent put to the user since they last wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Unread {
    pub pos: ConversationPos,
    pub at: UnixMs,
    pub said: Said,
}

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

    /// Whether the agent belongs in the user's list and the dealer.
    pub fn user_facing(&self) -> bool {
        self.prop(props::USER_FACING) == Some("true")
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
    /// runtime: what a test that folds mail into an agent starts from.
    pub fn for_test(agent: AgentId, props: rho_agent_types::Props, created_at: UnixMs) -> Self {
        Self {
            entry: AgentEntry {
                agent,
                workset: rho_agent_types::WorksetId([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]),
                spec: crate::protocol::rho2::SpecView {
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

/// What the rails read of an agent, folded from its mail and its runtime's
/// status. Only the conversation with the user counts: what the agent says
/// to other agents, and whether it is running, do not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Digest {
    /// One past the newest place folded.
    pub newest: ConversationPos,
    /// One past the newest mail folded; older mail is a replay.
    mail_next: MailId,
    /// The agent's newest status and the mail that said it, until any
    /// later mail from either side hides it.
    pub status: Option<(MailId, String)>,
    pub last_active: UnixMs,
    pub last_user_message_at: UnixMs,
    pub last_user_message_text: String,
    /// When it last sent the user anything but a status.
    pub last_sent_at: Option<UnixMs>,
    /// What it put to the user since they last wrote, oldest first.
    pub unread: Vec<Unread>,
}

impl Digest {
    /// Folds one mail of `agent`'s. Mail already held is skipped, so a
    /// replayed page is harmless; returns whether anything was new.
    pub fn tell(&mut self, agent: AgentId, mail: &Mail) -> bool {
        if mail.id < self.mail_next {
            return false;
        }
        self.mail_next = mail.id.next();
        let pos = ConversationPos::of_mail(mail.id);
        self.newest = self.newest.max(pos.next());
        self.last_active = self.last_active.max(mail.at);
        let me = Address::Agent(agent);
        if mail.from == Address::User && mail.to == me {
            self.user_spoke(mail.at, &text(mail));
        } else if mail.from == me && mail.to == Address::User {
            match mail.kind {
                Some(SendKind::Status) => self.status = Some((mail.id, text(mail))),
                kind => {
                    let said = match kind {
                        Some(SendKind::Ask) => Some(Said::Ask),
                        Some(SendKind::Result) => Some(Said::Result),
                        // Kept in the conversation, but nothing the user
                        // is owed.
                        Some(SendKind::Fyi | SendKind::Status) | None => None,
                    };
                    if let Some(said) = said {
                        self.unread.push(Unread {
                            pos,
                            at: mail.at,
                            said,
                        });
                    }
                    self.status = None;
                    self.last_sent_at = Some(mail.at);
                }
            }
        }
        true
    }

    /// What the agent's runtime is doing, seen while `head` was the host's
    /// newest mail: a failure that will not retry is a stop put to the
    /// user, anything else (a retry among it) ends one. Returns whether
    /// anything changed.
    pub fn observe(&mut self, state: &RuntimeState, head: MailId, at: UnixMs) -> bool {
        match state.inference {
            InferenceState::Failed { .. } => self.stopped(head, at),
            InferenceState::Idle | InferenceState::Responding | InferenceState::Retrying { .. } => {
                self.running()
            }
        }
    }

    /// The agent's runtime stopped on an error while `head` was the host's
    /// newest mail. A stop already held is the same stop, told again by a
    /// reconnect; returns whether anything changed.
    fn stopped(&mut self, head: MailId, at: UnixMs) -> bool {
        if self.stop().is_some() {
            return false;
        }
        let pos = ConversationPos::of_stop(head);
        self.newest = self.newest.max(pos.next());
        self.unread.push(Unread {
            pos,
            at,
            said: Said::Stopped,
        });
        true
    }

    /// A stop the copy kept across a restart.
    pub fn restore_stop(&mut self, stop: Unread) {
        if self.stop().is_none() {
            self.newest = self.newest.max(stop.pos.next());
            self.unread.push(stop);
        }
    }

    /// The runtime is not stopped: whatever stop was held is over.
    fn running(&mut self) -> bool {
        let before = self.unread.len();
        self.unread.retain(|unread| unread.said != Said::Stopped);
        before != self.unread.len()
    }

    /// The stop the user has not dealt with by writing, if any.
    pub fn stop(&self) -> Option<Unread> {
        self.unread
            .iter()
            .find(|unread| unread.said == Said::Stopped)
            .copied()
    }

    /// The strongest thing the agent put to the user past `seen`, counted
    /// from the oldest of that kind (`rho-dealer/cases.md`, A5).
    pub fn strongest_unread(&self, seen: ConversationPos) -> Option<Unread> {
        self.unread
            .iter()
            .filter(|unread| unread.pos >= seen)
            .min_by_key(|unread| (unread.said.strength(), unread.pos))
            .copied()
    }

    /// The newest thing put to the user: where the card's source last moved.
    pub fn newest_unread(&self) -> Option<Unread> {
        self.unread.iter().max_by_key(|unread| unread.pos).copied()
    }

    /// Writing reads everything before it.
    fn user_spoke(&mut self, at: UnixMs, text: &str) {
        self.last_user_message_at = at;
        self.last_user_message_text = one_line(text);
        self.status = None;
        self.unread.clear();
    }
}

fn text(mail: &Mail) -> String {
    rho_agent_types::transcript::text_content(&mail.content)
}

/// One agent as the client holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredAgent {
    pub host: HostId,
    pub identity: AgentIdentity,
    pub digest: Digest,
}

impl MirroredAgent {
    /// An agent the directory named, before any of its mail.
    pub fn new(host: HostId, identity: AgentIdentity) -> Self {
        Self {
            host,
            identity,
            digest: Digest::default(),
        }
    }

    /// Folds one of its mail; returns whether it was new.
    pub fn tell(&mut self, mail: &Mail) -> bool {
        self.digest.tell(self.agent_id(), mail)
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
/// that is about it, or a mail from or to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptRow {
    Log(Wpos, LogEvent),
    Mail(Mail),
}

impl TranscriptRow {
    pub fn at(&self) -> UnixMs {
        match self {
            Self::Log(_, event) => event.at(),
            Self::Mail(mail) => mail.at,
        }
    }
}

/// An agent's workset rows and its mail as one transcript, in time order:
/// each is in its own order already, and the two meet where their clocks
/// say. Rows about other agents are left out.
pub fn interleave(
    agent: AgentId,
    rows: impl IntoIterator<Item = (Wpos, LogEvent)>,
    mails: impl IntoIterator<Item = Mail>,
) -> Vec<TranscriptRow> {
    let mut rows = rows
        .into_iter()
        .filter(|(_, event)| event.agent() == Some(agent))
        .map(|(pos, event)| TranscriptRow::Log(pos, event))
        .peekable();
    let mut mails = mails.into_iter().map(TranscriptRow::Mail).peekable();
    let mut merged = Vec::new();
    loop {
        let next = match (rows.peek(), mails.peek()) {
            (Some(row), Some(mail)) if mail.at() < row.at() => mails.next(),
            (Some(_), _) => rows.next(),
            (None, _) => mails.next(),
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
    /// One past the newest mail folded.
    mail_next: MailId,
    /// Shared with every state handed out, so a row costs the blocks it
    /// adds or changes and a state is a list of pointers.
    blocks: Vec<Arc<UiBlock>>,
    /// The row each block came from, so a rewind drops exactly what was
    /// told after it. `None` for mail, which no rewind takes back.
    told_at: Vec<Option<Wpos>>,
    /// Inputs no request has carried yet; drawn after the blocks.
    queue: Vec<(Wpos, UiBlock)>,
    /// Mail to the agent that no input has handed it yet; drawn last.
    mailbox: Vec<(MailId, UiBlock)>,
    /// Mail an input has handed over, so mail heard after its input is
    /// not shown waiting.
    handed: BTreeSet<MailId>,
    /// Who wrote each mail to the agent, for the input that hands it over.
    senders: HashMap<MailId, Address>,
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
            mail_next: MailId::default(),
            blocks: Vec::new(),
            told_at: Vec::new(),
            queue: Vec::new(),
            mailbox: Vec::new(),
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

    /// The end of the queue, where a new input lands.
    fn queue_end(&self) -> usize {
        self.blocks.len() + self.queue.len()
    }

    fn waiting(&self) -> impl Iterator<Item = Arc<UiBlock>> + '_ {
        self.queue
            .iter()
            .map(|(_, queued)| queued)
            .chain(self.mailbox.iter().map(|(_, waiting)| waiting))
            .map(|block| Arc::new(block.clone()))
    }

    /// The composed transcript from one index on: the folded blocks, then
    /// the inputs no request has carried yet, then mail not yet handed over.
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
    fn deliver_queue(&mut self, pos: Wpos) {
        for (_, queued) in std::mem::take(&mut self.queue) {
            self.push(Some(pos), delivered(queued));
        }
    }

    /// Folds one row. Rows and mail already held are skipped, as are rows
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
            TranscriptRow::Mail(mail) => {
                if mail.id < self.mail_next {
                    return false;
                }
                self.mail_next = mail.id.next();
                self.tell_mail(mail);
            }
        }
        true
    }

    fn tell_mail(&mut self, mail: &Mail) {
        let me = Address::Agent(self.agent);
        if mail.from == me {
            // A status is the status line, not a message.
            if mail.kind == Some(SendKind::Status) && mail.to == Address::User {
                return;
            }
            let to = match mail.to {
                Address::User => None,
                Address::Agent(agent) => Some(agent),
            };
            self.push(
                None,
                UiBlock::MessageSent {
                    to,
                    text: text(mail),
                },
            );
        } else if mail.to == me {
            self.senders.insert(mail.id, mail.from);
            if self.handed.contains(&mail.id) {
                return;
            }
            self.errored = false;
            self.touch(self.queue_end() + self.mailbox.len());
            self.mailbox.push((
                mail.id,
                UiBlock::QueuedMessage {
                    text: text(mail),
                    sender: sender(mail.from),
                },
            ));
        }
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
            LogEvent::Input { mail, text, .. } => {
                self.errored = false;
                let block = match mail {
                    Some(id) => {
                        self.handed.insert(*id);
                        if let Some(index) =
                            self.mailbox.iter().position(|(waiting, _)| waiting == id)
                        {
                            self.touch(self.queue_end() + index);
                            self.mailbox.remove(index);
                        }
                        UiBlock::QueuedMessage {
                            text: text.clone(),
                            sender: self.senders.get(id).copied().and_then(sender),
                        }
                    }
                    // Not mail: what the policy has to tell the agent.
                    None => UiBlock::Notice { text: text.clone() },
                };
                self.touch(self.queue_end());
                self.queue.push((pos, block));
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
            LogEvent::Sent {
                results,
                compaction,
                ..
            } => {
                self.deliver_queue(pos);
                if *compaction {
                    self.push(
                        Some(pos),
                        UiBlock::Notice {
                            text: "compacting context".to_owned(),
                        },
                    );
                }
                self.finish_calls(results);
            }
            LogEvent::NotebookReport {
                calls, compaction, ..
            } => {
                self.deliver_queue(pos);
                if *compaction {
                    self.push(
                        Some(pos),
                        UiBlock::Notice {
                            text: "compacting context".to_owned(),
                        },
                    );
                }
                self.report_calls(calls);
            }
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
            // one that hides what it undid. Mail stays: it was sent.
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
                self.queue.retain(|(queued_at, _)| queued_at < to);
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

    fn input(text: &str, mail: Option<u64>, at: u64) -> LogEvent {
        LogEvent::Input {
            agent: me(),
            mail: mail.map(MailId),
            text: text.to_owned(),
            wake: rho_agent_types::InputWake::Person,
            at: UnixMs(at),
        }
    }

    fn sent_row(at: u64) -> LogEvent {
        LogEvent::Sent {
            agent: me(),
            results: Vec::new(),
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

    fn mail(id: u64, from: Address, to: Address, kind: Option<SendKind>, at: u64) -> Mail {
        Mail {
            id: MailId(id),
            at: UnixMs(at),
            from,
            to,
            re: None,
            kind,
            content: vec![rho_agent_types::ContentPart::Text {
                text: format!("mail {id}"),
            }],
        }
    }

    fn to_user(id: u64, kind: SendKind, at: u64) -> Mail {
        mail(id, Address::Agent(me()), Address::User, Some(kind), at)
    }

    fn from_user(id: u64, at: u64) -> Mail {
        mail(id, Address::User, Address::Agent(me()), None, at)
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
            LogEvent::Sent {
                agent: me(),
                results: vec![ToolOutcome {
                    id: "call-1".to_owned(),
                    status: ToolStatus::Error,
                    started_at: UnixMs(3),
                    finished_at: UnixMs(4),
                }],
                compaction: false,
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

    /// Results alone carry nothing out of the queue; a request does.
    #[test]
    fn an_input_waits_as_queued_until_a_request_carries_it() {
        let state = told(vec![
            input("later", None, 1),
            LogEvent::Results {
                agent: me(),
                results: Vec::new(),
                at: UnixMs(2),
            },
        ]);
        assert_eq!(state.blocks.len(), 1);
        assert!(matches!(&*state.blocks[0], UiBlock::Notice { text } if text == "later"));
    }

    /// The workset's log holds every agent's rows; only this agent's are
    /// its transcript.
    #[test]
    fn rows_about_another_agent_are_not_this_transcript() {
        let other = LogEvent::ClaudeMessage {
            agent: agent(2),
            speaker: Speaker::Assistant,
            text: "not mine".into(),
            at: UnixMs(1),
        };
        let mut fold = TranscriptFold::new(me(), &[]);
        assert!(!fold.tell(&TranscriptRow::Log(Wpos(0), other.clone())));
        assert!(fold.state().blocks.is_empty());
        let merged = interleave(me(), [(Wpos(0), other)], []);
        assert!(merged.is_empty());
    }

    /// Mail to the agent waits until an input hands it over, then reads as
    /// the input does, from whoever wrote it, and is never shown twice;
    /// what the agent mails out is in its transcript, except a status.
    #[test]
    fn mail_waits_until_an_input_hands_it_over() {
        let peer = agent(2);
        let mut fold = TranscriptFold::new(me(), &[]);
        fold.tell(&TranscriptRow::Mail(mail(
            3,
            Address::Agent(peer),
            Address::Agent(me()),
            None,
            1,
        )));
        fold.tell(&TranscriptRow::Mail(from_user(4, 2)));
        let state = fold.state();
        assert_eq!(state.blocks.len(), 2, "both wait");
        assert!(matches!(&*state.blocks[0],
            UiBlock::QueuedMessage { sender: Some(sender), .. } if *sender == peer));

        fold.tell(&TranscriptRow::Log(Wpos(0), input("from peer", Some(3), 3)));
        fold.tell(&TranscriptRow::Log(Wpos(1), sent_row(4)));
        let state = fold.state();
        assert_eq!(
            *state.blocks[0],
            UiBlock::AgentMessage {
                sender: peer,
                text: "from peer".into()
            }
        );
        assert_eq!(state.blocks.len(), 2, "the user's mail still waits, once");
        assert!(matches!(&*state.blocks[1],
            UiBlock::QueuedMessage { sender: None, text } if text == "mail 4"));

        fold.tell(&TranscriptRow::Mail(to_user(5, SendKind::Status, 5)));
        fold.tell(&TranscriptRow::Mail(to_user(6, SendKind::Ask, 6)));
        let state = fold.state();
        assert_eq!(state.blocks.len(), 3, "a status is not a message");
        assert!(matches!(&*state.blocks[1],
            UiBlock::MessageSent { to: None, text } if text == "mail 6"));
        assert!(
            matches!(&*state.blocks[2], UiBlock::QueuedMessage { .. }),
            "waiting mail stays last"
        );
    }

    /// An input may be heard before the mail it hands over: the mail then
    /// never waits.
    #[test]
    fn mail_heard_after_its_input_does_not_wait() {
        let mut fold = TranscriptFold::new(me(), &[]);
        fold.tell(&TranscriptRow::Log(Wpos(0), input("hello", Some(9), 2)));
        fold.tell(&TranscriptRow::Mail(from_user(9, 1)));
        let state = fold.state();
        assert_eq!(state.blocks.len(), 1);
        assert!(
            !fold.tell(&TranscriptRow::Mail(from_user(9, 1))),
            "a replay"
        );
    }

    /// A rewind takes back rows, not mail: what was mailed was sent.
    #[test]
    fn a_rewind_hides_rows_but_not_mail() {
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
            [to_user(7, SendKind::Result, 3)],
        );
        assert!(matches!(rows[2], TranscriptRow::Mail(_)), "merged by time");
        let state = transcript(me(), &rows);
        assert_eq!(
            state.blocks,
            vec![
                Arc::new(UiBlock::UserMessage {
                    text: "first".to_owned()
                }),
                Arc::new(UiBlock::MessageSent {
                    to: None,
                    text: "mail 7".to_owned()
                }),
            ]
        );
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

    fn digest(mails: &[Mail]) -> Digest {
        let mut digest = Digest::default();
        for mail in mails {
            digest.tell(me(), mail);
        }
        digest
    }

    /// The card counts from the oldest unread of the strongest kind
    /// (`rho-dealer/cases.md`, A5); a status, an fyi and mail between
    /// agents are never owed to the user.
    #[test]
    fn the_strongest_unread_counts_from_its_oldest() {
        let peer = Address::Agent(agent(2));
        let digest = digest(&[
            from_user(1, 1),
            to_user(2, SendKind::Result, 2),
            to_user(3, SendKind::Status, 3),
            to_user(4, SendKind::Fyi, 4),
            mail(5, Address::Agent(me()), peer, Some(SendKind::Ask), 5),
            to_user(6, SendKind::Ask, 6),
            to_user(7, SendKind::Ask, 7),
        ]);
        let strongest = |seen| {
            digest
                .strongest_unread(seen)
                .map(|unread| (unread.said, unread.pos))
        };
        let at = |id| ConversationPos::of_mail(MailId(id));
        assert_eq!(strongest(ConversationPos(0)), Some((Said::Ask, at(6))));
        assert_eq!(strongest(at(7)), Some((Said::Ask, at(7))));
        assert_eq!(strongest(digest.newest), None);
        assert_eq!(
            digest.unread.len(),
            3,
            "a status, an fyi and agent mail are no card"
        );
        assert_eq!(digest.last_sent_at, Some(UnixMs(7)));
    }

    /// A status is the status line until any later mail to the user.
    #[test]
    fn a_status_is_never_unread() {
        let mut digest = digest(&[to_user(1, SendKind::Status, 1)]);
        assert!(digest.unread.is_empty());
        assert_eq!(digest.status, Some((MailId(1), "mail 1".into())));
        assert_eq!(digest.last_sent_at, None, "a status is not a send");
        digest.tell(me(), &to_user(2, SendKind::Fyi, 2));
        assert_eq!(digest.status, None);
        assert!(digest.unread.is_empty());
    }

    /// The user writing reads everything before it, once: the same mail
    /// heard again (a reconnect replays the log from a cursor) does not
    /// read what the agent said after it.
    #[test]
    fn writing_reads_what_came_before_it_and_a_replay_reads_nothing() {
        let mut digest = digest(&[to_user(1, SendKind::Ask, 1), from_user(2, 2)]);
        assert!(
            digest.unread.is_empty(),
            "writing reads everything before it"
        );
        assert_eq!(digest.last_user_message_text, "mail 2");
        assert!(digest.tell(me(), &to_user(3, SendKind::Result, 3)));
        assert!(!digest.tell(me(), &from_user(2, 2)), "a replay is not news");
        assert_eq!(digest.unread.len(), 1, "the result after it is still owed");
        assert_eq!(digest.newest, ConversationPos::of_mail(MailId(3)).next());
    }

    /// A stop sits after the mail before it and before the mail after it,
    /// so dealing with the conversation up to a stop deals with the stop,
    /// and mail after a dealt-with stop is owed again.
    #[test]
    fn a_stop_is_ordered_between_the_mail_around_it() {
        let mut digest = digest(&[to_user(4, SendKind::Result, 1)]);
        let seen_mail = digest.newest;
        assert!(digest.stopped(MailId(9), UnixMs(2)));
        assert!(
            !digest.stopped(MailId(12), UnixMs(3)),
            "told again by a reconnect"
        );
        let stop = digest
            .strongest_unread(seen_mail)
            .expect("the stop is owed");
        assert_eq!(stop.said, Said::Stopped);
        let seen_stop = digest.newest;
        assert_eq!(digest.strongest_unread(seen_stop), None);
        digest.tell(me(), &to_user(10, SendKind::Result, 4));
        assert_eq!(
            digest.strongest_unread(seen_stop).map(|unread| unread.said),
            Some(Said::Result),
            "mail after the stop is new"
        );
        assert!(
            digest.observe(&retrying, MailId(12), UnixMs(5)),
            "a retry ends it"
        );
        assert!(digest.stop().is_none());
    }

    #[test]
    fn identity_reads_the_props_the_gui_gives_meaning() {
        let parent = agent(7);
        let mut entry = AgentEntry {
            agent: me(),
            workset: "0123456789ab".parse().unwrap(),
            spec: crate::protocol::rho2::SpecView {
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
        assert!(!bare.user_facing());
        assert_eq!(bare.handle_prefix(), "eng");
        assert_eq!(bare.place().cwd, "/src");
        assert_eq!(bare.place().workset, Some(entry.workset));

        for (key, value) in [
            (props::TITLE, "a title"),
            (props::NAME, " "),
            (props::PARENT, &parent.encoded()),
            (props::USER_FACING, "true"),
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
        assert!(full.user_facing());
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
