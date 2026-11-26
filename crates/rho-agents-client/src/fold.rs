//! The fold: what a client makes of the directory and the workset rows it
//! reads.
//!
//! The host sends facts: the directory's whole entries, and a workset's
//! rows when a transcript is open. Everything a rail or a transcript shows
//! is folded from them here, on the client, so the wire carries facts and
//! never conclusions. Chat is not here: a row names the message an input
//! handed over, and the chat store says what it is.

use std::sync::Arc;

use rho_agent_types::{AgentId, ChatSeq, MessageId, Place, UnixMs, WorksetKind, Wpos, props};

use crate::HostId;
use crate::protocol::AgentUsageBucket;
use crate::protocol::transcript::{RuntimeKind, Speaker};
use crate::protocol::worksets::{AgentEntry, LogEvent};
use crate::state::{FailureKind, UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock, UiToolStatus};

/// What an agent is: its directory entry and the kind of workset it is
/// in. Never what has happened to it.
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

    /// The chat message the agent was started from, when policy kept it as
    /// `origin`: the thread it works in, and the first thing it read.
    pub fn origin(&self) -> Option<MessageId> {
        brief_origin(&self.entry.spec)
    }

    /// The repository its workset was made on, as the user named it, when
    /// it was cloned from one.
    pub fn repository(&self) -> Option<&str> {
        self.prop(props::REPO)
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
        let cwd = rho_agent_types::agent_cwd(&self.entry.spec.props).unwrap_or(start);
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
    /// runtime: what a test of the directory starts from.
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

/// One agent as the client holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredAgent {
    pub host: HostId,
    pub identity: AgentIdentity,
}

impl MirroredAgent {
    /// An agent the directory named.
    pub fn new(host: HostId, identity: AgentIdentity) -> Self {
        Self { host, identity }
    }

    pub fn agent_id(&self) -> AgentId {
        self.identity.agent_id()
    }

    /// The agent's name for a reader.
    pub fn title(&self) -> Option<&str> {
        self.identity.name()
    }
}

/// The transcript of one agent, oldest first. Nothing here is invented: a
/// call is its name and its line, a reply is what was said, and an input
/// waits as queued until a request carries it.
/// Rows about other agents are left out.
pub fn transcript(agent: AgentId, rows: &[(Wpos, LogEvent)]) -> UiAgentState {
    TranscriptFold::new(agent, rows).state()
}

/// The row a block of the transcript came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
    /// What the agent said or did at this row: a rewind to before it
    /// takes it back.
    Said(Wpos),
    /// A message, delivered or written at this row: it was sent, so no
    /// rewind unsays it.
    Sent(Wpos),
}

impl Told {
    fn at(self) -> Wpos {
        match self {
            Self::Said(at) | Self::Sent(at) => at,
        }
    }
}

/// The transcript as a fold that takes one row at a time, so a row costs
/// what it changes and never a walk of the whole transcript.
#[derive(Clone, Debug)]
pub struct TranscriptFold {
    agent: AgentId,
    /// One past the newest row folded.
    next: Wpos,
    /// Shared with every state handed out, so a row costs the blocks it
    /// adds or changes and a state is a list of pointers.
    blocks: Vec<Arc<UiBlock>>,
    /// The row each block came from: where it stands among the workset's
    /// rows, and, so a rewind drops exactly what was told after it, whether
    /// a rewind may take it back.
    told_at: Vec<Told>,
    /// Blocks before this one came before the last request that answered.
    unanswered: usize,
    /// Compactions no request has carried yet; drawn after the blocks.
    queue: Vec<(Wpos, UiBlock)>,
    /// The agent stopped on an error and nobody has written since.
    errored: bool,
    origin: Option<MessageId>,
    runtime_label: Option<String>,
    /// The next successful response needs to close the visible error run.
    after_error: bool,
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
    pub from: usize,
    pub blocks: Vec<Arc<UiBlock>>,
    pub status: UiAgentStatus,
    pub context_used: Option<u64>,
    pub usage: UiAgentUsage,
}

fn brief_origin(spec: &crate::protocol::worksets::SpecView) -> Option<MessageId> {
    let id = spec
        .props
        .get(props::ORIGIN)?
        .trim()
        .strip_prefix("chat:")?;
    Some(MessageId(ChatSeq(id.parse().ok()?)))
}

impl TranscriptFold {
    pub fn new(agent: AgentId, rows: &[(Wpos, LogEvent)]) -> Self {
        let mut fold = Self {
            agent,
            next: Wpos::ZERO,
            blocks: Vec::new(),
            told_at: Vec::new(),
            unanswered: 0,
            queue: Vec::new(),
            errored: false,
            origin: None,
            runtime_label: None,
            after_error: false,
            context_used: None,
            usage: AgentUsageBucket::default(),
            usage_model: String::new(),
            dirty_from: None,
        };
        for (pos, event) in rows {
            fold.tell(*pos, event);
        }
        fold
    }

    fn push(&mut self, told_at: Told, block: UiBlock) {
        self.after_error |= matches!(block, UiBlock::Failure { .. });
        // The same notice again, such as a retry failing as the last one
        // did, is counted on the one before.
        if let UiBlock::Notice { text, times } = &block
            && let Some(last) = self.blocks.last_mut()
            && matches!(&**last, UiBlock::Notice { text: said, .. } if said == text)
        {
            if let UiBlock::Notice { times: said, .. } = Arc::make_mut(last) {
                *said += times;
            }
            self.touch(self.blocks.len() - 1);
            return;
        }
        if let UiBlock::Failure {
            kind,
            text,
            times,
            at,
            ..
        } = &block
            && let Some(last) = self.blocks.last_mut()
            && matches!(&**last, UiBlock::Failure { kind: was, text: said, .. } if was == kind && said == text)
        {
            if let UiBlock::Failure {
                times: said,
                at: latest,
                ..
            } = Arc::make_mut(last)
            {
                *said += times;
                *latest = *at;
            }
            self.touch(self.blocks.len() - 1);
            return;
        }
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

    /// The end of the queue.
    fn queue_end(&self) -> usize {
        self.blocks.len() + self.queue.len()
    }

    fn waiting(&self) -> impl Iterator<Item = Arc<UiBlock>> + '_ {
        self.queue
            .iter()
            .map(|(_, queued)| Arc::new(queued.clone()))
    }

    /// The composed transcript from one index on: the folded blocks, then
    /// the compactions no request has carried yet.
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
            from,
            blocks: self.composed_from(from),
            status: state.status,
            context_used: state.context_used,
            usage: state.usage,
        })
    }

    /// Each reported call is marked reported, its task still running.
    /// A request answered what was open: every call since the last one that
    /// did. A call Rho refused ended on its own, and one cut off went with it.
    fn answer_calls(&mut self, timing: Option<&rho_agent_types::CallTiming>, output: &str) {
        let reported = (self.unanswered..self.blocks.len()).rev().find(|&index| {
            matches!(&*self.blocks[index], UiBlock::Tool(tool)
                if timing.is_none_or(|timing| tool.id == timing.call.as_str()))
        });
        for index in self.unanswered..self.blocks.len() {
            if matches!(&*self.blocks[index], UiBlock::Tool(_)) {
                self.report_block(index);
                let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index]) else {
                    continue;
                };
                if let Some(timing) = timing
                    && tool.id == timing.call.as_str()
                {
                    tool.timing = timing.timing;
                }
                if Some(index) == reported && !output.is_empty() {
                    tool.output = Some(output.to_owned());
                }
            }
        }
        self.unanswered = self.blocks.len();
    }

    /// A call whose cell ended or was cancelled stays so once reported.
    fn report_block(&mut self, index: usize) {
        self.touch(index);
        if let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index])
            && !matches!(
                tool.status,
                UiToolStatus::Success | UiToolStatus::Error | UiToolStatus::Cancelled
            )
        {
            tool.status = UiToolStatus::Reported;
            tool.finished_at = None;
        }
    }

    /// The call's cell ended: `ok` or `error`, and how long it ran. One the
    /// user cancelled stays cancelled.
    fn end_cell(&mut self, end: &rho_agent_types::CellEnd) {
        let Some(index) = self.blocks.iter().rposition(|block| {
            matches!(&**block, UiBlock::Tool(tool)
                if tool.id == end.call.as_str() && tool.status != UiToolStatus::Cancelled)
        }) else {
            return;
        };
        self.touch(index);
        if let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index]) {
            tool.status = match end.failed {
                true => UiToolStatus::Error,
                false => UiToolStatus::Success,
            };
            tool.finished_at = Some(end.at);
            self.after_error |= end.failed;
        }
    }

    /// Every call still running is over. Only a running one is copied out
    /// of its sharing.
    fn cancel_running(&mut self) {
        for (index, block) in self.blocks.iter_mut().enumerate() {
            if matches!(&**block, UiBlock::Tool(tool) if tool.status == UiToolStatus::Running)
                && let UiBlock::Tool(tool) = Arc::make_mut(block)
            {
                tool.status = UiToolStatus::Cancelled;
                self.dirty_from = Some(self.dirty_from.map_or(index, |held| held.min(index)));
            }
        }
    }

    fn report_calls(&mut self, calls: &[crate::protocol::worksets::CallView], at: UnixMs) {
        for call in calls {
            let Some(index) = self
                .blocks
                .iter()
                .rposition(|block| matches!(&**block, UiBlock::Tool(tool) if tool.id == call.id))
            else {
                continue;
            };
            self.report_block(index);
            if let UiBlock::Tool(tool) = Arc::make_mut(&mut self.blocks[index]) {
                if !call.output.is_empty() {
                    tool.output = Some(call.output.clone());
                }
                if tool.status == UiToolStatus::Reported
                    && let Some(failed) = call.failed
                {
                    tool.status = if failed {
                        UiToolStatus::Error
                    } else {
                        UiToolStatus::Success
                    };
                    tool.finished_at = Some(at);
                    self.after_error |= failed;
                }
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
            self.push(Told::Said(pos), queued);
        }
        compaction
    }

    /// Folds one row. Rows already held are skipped, as are rows about
    /// another agent, so a repeated page is harmless; returns whether
    /// anything was new.
    pub fn tell(&mut self, pos: Wpos, event: &LogEvent) -> bool {
        if pos < self.next || event.agent() != Some(self.agent) {
            return false;
        }
        self.next = pos.next();
        self.tell_row(pos, event);
        true
    }

    fn successful_response(&mut self, pos: Wpos) {
        if std::mem::take(&mut self.after_error) {
            self.push(Told::Said(pos), UiBlock::Responded);
        }
    }

    fn tell_row(&mut self, pos: Wpos, event: &LogEvent) {
        match event {
            LogEvent::CompactionRequested { .. } => {
                self.touch(self.queue_end());
                self.queue.push((
                    pos,
                    UiBlock::Notice {
                        text: "compacting context".to_owned(),
                        times: 1,
                    },
                ));
            }
            LogEvent::Request {
                inputs,
                answers,
                compaction,
                timing,
                output,
                ..
            } => {
                if !inputs.is_empty() {
                    self.errored = false;
                }
                for input in inputs {
                    match input.message {
                        // The brief has its own pinned row, not a second read here.
                        Some(message) if Some(message) == self.origin => {}
                        // A message is still a message: a rewind of the
                        // turn it started does not unsay what was written.
                        Some(message) => self.push(
                            Told::Sent(pos),
                            UiBlock::Delivered {
                                message,
                                text: input.text.clone(),
                            },
                        ),
                        // No message: what the policy has to tell the agent.
                        None => self.push(
                            Told::Said(pos),
                            UiBlock::Notice {
                                text: input.text.clone(),
                                times: 1,
                            },
                        ),
                    }
                }
                let requested = self.deliver_queue(pos);
                // A requested compaction already shows as its request.
                if *compaction && !requested {
                    self.push(
                        Told::Said(pos),
                        UiBlock::Notice {
                            text: "compacting context".to_owned(),
                            times: 1,
                        },
                    );
                }
                if *answers {
                    self.answer_calls(timing.as_ref(), output);
                }
            }
            LogEvent::MailWritten {
                message, version, ..
            } => {
                self.push(
                    Told::Sent(pos),
                    UiBlock::MailWritten {
                        message: *message,
                        version: *version,
                    },
                );
            }
            LogEvent::CellEnded { end, .. } => self.end_cell(end),
            LogEvent::CallsEnded { calls, at, .. } => self.report_calls(calls, *at),
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
                self.successful_response(pos);
                for item in items {
                    let mut block = crate::store::block(item);
                    if let UiBlock::Tool(tool) = &mut block {
                        tool.started_at = Some(*at);
                    }
                    self.push(Told::Said(pos), block);
                }
                if *compacted {
                    self.push(
                        Told::Said(pos),
                        UiBlock::Notice {
                            text: "compacted".to_owned(),
                            times: 1,
                        },
                    );
                }
            }
            // Claude's transcript is mirrored message by message.
            LogEvent::ClaudeMessage { speaker, text, .. } => {
                if *speaker == Speaker::Assistant {
                    self.successful_response(pos);
                }
                self.push(
                    Told::Said(pos),
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
            LogEvent::Cancelled { .. } => {
                self.cancel_running();
                self.push(
                    Told::Said(pos),
                    UiBlock::Notice {
                        text: "you cancelled the request".to_owned(),
                        times: 1,
                    },
                );
            }
            LogEvent::Stopped { error, at, .. } => {
                self.errored = true;
                // A call the agent never answered is over too.
                self.cancel_running();
                self.push(
                    Told::Said(pos),
                    UiBlock::Failure {
                        kind: FailureKind::Stopped,
                        text: error.clone(),
                        times: 1,
                        since: *at,
                        at: *at,
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
                at,
                ..
            } => {
                if !text.is_empty() {
                    self.push(
                        Told::Said(pos),
                        UiBlock::AssistantMessage {
                            text: text.clone(),
                            phase: None,
                        },
                    );
                }
                if *retrying {
                    self.push(
                        Told::Said(pos),
                        UiBlock::Failure {
                            kind: FailureKind::Retrying {
                                runtime: self.runtime_label.clone(),
                            },
                            text: error.clone(),
                            times: 1,
                            since: *at,
                            at: *at,
                        },
                    );
                }
            }
            // A rewind is told rather than unwritten, so the reader is the
            // one that hides what it undid. Delivered messages stay: they
            // were sent.
            LogEvent::Rewound { to, .. } => {
                self.touch(0);
                self.unanswered = 0;

                let undone = |told: &Told| matches!(told, Told::Said(told) if told >= to);
                let blocks = std::mem::take(&mut self.blocks);
                let told_at = std::mem::take(&mut self.told_at);
                for (block, told) in blocks.into_iter().zip(told_at) {
                    if !undone(&told) {
                        self.blocks.push(block);
                        self.told_at.push(told);
                    }
                }
                self.after_error = self.blocks.iter().rev()
                    .take_while(|block| !matches!(&***block, UiBlock::Responded))
                    .any(|block| matches!(&**block, UiBlock::Failure { .. })
                        || matches!(&**block, UiBlock::Tool(tool) if tool.status == UiToolStatus::Error));
                self.queue.retain(|(queued_at, ..)| queued_at < to);
                if self.context_used.is_some_and(|(said_at, _)| said_at >= *to) {
                    self.context_used = None;
                }
            }
            LogEvent::AgentCreated { spec, .. } => {
                self.origin = brief_origin(spec);
                self.runtime_label = Some(spec.runtime_label());
            }
            LogEvent::AgentChanged { spec, .. } => {
                self.runtime_label = Some(spec.runtime_label());
            }
            LogEvent::WorksetCreated { .. } | LogEvent::Evaluated { .. } => {}
        }
    }

    /// The workset row block `index` of [`Self::state`] came from, which
    /// orders it among the rows of the workset's other agents. A queued
    /// compaction stands where it was asked for.
    pub fn position(&self, index: usize) -> Option<Wpos> {
        match self.told_at.get(index) {
            Some(told) => Some(told.at()),
            None => self
                .queue
                .get(index - self.told_at.len())
                .map(|(queued_at, _)| *queued_at),
        }
    }

    /// The transcript as it stands: the blocks, then what waits.
    pub fn state(&self) -> UiAgentState {
        let mut blocks = self.blocks.clone();
        blocks.extend(self.waiting());
        UiAgentState {
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

#[cfg(test)]
mod tests {
    use rho_agent_types::MessageId;

    use super::*;
    use crate::protocol::transcript::{ArgumentsFormat, Item, Usage};
    use crate::protocol::worksets::{CallView, InputView};
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
                .map(|(pos, event)| (Wpos(pos as u64), event))
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
            answers: true,
            compaction: false,
            timing: None,
            output: String::new(),
            at: UnixMs(at),
        }
    }

    fn sent_row(at: u64) -> LogEvent {
        LogEvent::Request {
            agent: me(),
            inputs: Vec::new(),
            answers: true,
            compaction: false,
            timing: None,
            output: String::new(),
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
                (Wpos(0), input("question", None, 1)),
                (Wpos(1), sent_row(2)),
                (Wpos(2), replied(items.clone(), None, Some(73), 19)),
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
        fold.tell(
            Wpos(3),
            &LogEvent::Rewound {
                agent: me(),
                to: Wpos(2),
                at: UnixMs(20),
            },
        );
        assert_eq!(fold.blocks.len(), 1);
    }

    /// A long transcript, one row appended: the reader is told where the
    /// transcript first differs, and that is the end of what it had.
    #[test]
    fn a_page_for_the_open_agent_costs_its_rows() {
        let mut fold = TranscriptFold::new(me(), &[]);
        let mut pos = 0u64;
        let mut tell = |fold: &mut TranscriptFold, event: LogEvent| {
            fold.tell(Wpos(pos), &event);
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
            LogEvent::CallsEnded {
                agent: me(),
                calls: vec![CallView {
                    id: "call-1".to_owned(),
                    output: "permission denied".to_owned(),
                    failed: Some(true),
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
        assert_eq!(state.blocks.len(), 4);
        assert!(matches!(*state.blocks[2], UiBlock::Responded));
        assert_eq!(
            *state.blocks[0],
            UiBlock::Delivered {
                message: MessageId(rho_agent_types::ChatSeq(1)),
                text: "have a look".to_owned()
            }
        );
        let tool = only_tool(&state);
        assert_eq!(tool.status, UiToolStatus::Error);
        assert_eq!(tool.output.as_deref(), Some("permission denied"));
        assert_eq!(tool.finished_at, Some(UnixMs(4)));
        assert!(matches!(*state.blocks[3], UiBlock::AssistantMessage { .. }));
        assert_eq!(state.usage.total.output_tokens, 12);
        assert_eq!(state.usage.total.requests, 2);
        assert_eq!(state.usage.provider, "sol");
        assert_eq!(state.context_used, Some(12));
    }

    fn tool_statuses(fold: &TranscriptFold) -> Vec<UiToolStatus> {
        fold.state()
            .blocks
            .iter()
            .filter_map(|block| match &**block {
                UiBlock::Tool(tool) => Some(tool.status),
                _ => None,
            })
            .collect()
    }

    fn call(id: &str) -> Item {
        Item::ToolCall {
            id: id.to_owned(),
            name: "exec".to_owned(),
            arguments: "pass".to_owned(),
            format: ArgumentsFormat::Text,
        }
    }

    fn request_answering(answers: bool, at: u64) -> LogEvent {
        LogEvent::Request {
            agent: me(),
            inputs: Vec::new(),
            answers,
            compaction: false,
            timing: None,
            output: String::new(),
            at: UnixMs(at),
        }
    }

    fn cell_ended(call: &str, at: u64, failed: bool) -> LogEvent {
        LogEvent::CellEnded {
            agent: me(),
            end: rho_agent_types::CellEnd {
                call: rho_agent_types::transcript::ExecId::try_from(call).unwrap(),
                at: UnixMs(at),
                failed,
            },
            at: UnixMs(at + 1),
        }
    }

    /// A cell that ended says how and how long it ran, whether a request
    /// answered it first, after, or never (the turn ended with it). A
    /// request does not undo it; one still running when answered is only
    /// reported, and a cancelled one stays cancelled.
    #[test]
    fn a_call_says_how_its_cell_ended_and_how_long_it_ran() {
        use UiToolStatus::{Cancelled, Error, Reported, Success};
        let mut fold = TranscriptFold::new(
            me(),
            &[
                // Ended, then answered.
                (Wpos(0), replied(vec![call("one")], None, None, 1_000)),
                (Wpos(1), cell_ended("one", 1_240, false)),
                (Wpos(2), request_answering(true, 1_300)),
                // Answered while running, then ended.
                (Wpos(3), replied(vec![call("two")], None, None, 2_000)),
                (Wpos(4), request_answering(true, 2_100)),
                (Wpos(5), cell_ended("two", 9_000, true)),
                // Ended the turn: no request follows.
                (Wpos(6), replied(vec![call("three")], None, None, 10_000)),
                (Wpos(7), cell_ended("three", 10_050, false)),
                // Answered, still running.
                (Wpos(8), replied(vec![call("four")], None, None, 11_000)),
                (Wpos(9), request_answering(true, 11_100)),
            ],
        );
        assert_eq!(tool_statuses(&fold), [Success, Error, Success, Reported]);
        let times: Vec<_> = fold
            .state()
            .blocks
            .iter()
            .filter_map(|block| match &**block {
                UiBlock::Tool(tool) => Some((tool.started_at, tool.finished_at)),
                _ => None,
            })
            .collect();
        assert_eq!(
            times,
            [
                (Some(UnixMs(1_000)), Some(UnixMs(1_240))),
                (Some(UnixMs(2_000)), Some(UnixMs(9_000))),
                (Some(UnixMs(10_000)), Some(UnixMs(10_050))),
                (Some(UnixMs(11_000)), None),
            ]
        );
        // An end that names no call ends nothing; a cancelled call stays so.
        fold.tell(Wpos(10), &cell_ended("nine", 11_500, false));
        assert_eq!(tool_statuses(&fold), [Success, Error, Success, Reported]);
        fold.tell(Wpos(11), &replied(vec![call("five")], None, None, 12_000));
        fold.tell(
            Wpos(12),
            &LogEvent::Cancelled {
                agent: me(),
                at: UnixMs(12_100),
            },
        );
        fold.tell(Wpos(13), &cell_ended("five", 12_200, true));
        assert_eq!(
            tool_statuses(&fold),
            [Success, Error, Success, Reported, Cancelled]
        );
    }

    /// A request that answers closes every call since the last one that did,
    /// past a later call that ended on its own. One that does not, the
    /// compaction of a quiet agent, leaves the call open.
    #[test]
    fn a_request_answers_what_is_open_only_when_it_says_so() {
        use UiToolStatus::{Reported, Running};
        let mut pos = 0;
        let mut fold = TranscriptFold::new(me(), &[]);
        let mut tell = |fold: &mut TranscriptFold, event: LogEvent| {
            fold.tell(Wpos(pos), &event);
            pos += 1;
        };
        tell(&mut fold, replied(vec![call("admitted")], None, None, 1));
        tell(&mut fold, replied(vec![call("refused")], None, None, 2));
        tell(
            &mut fold,
            LogEvent::CallsEnded {
                agent: me(),
                calls: vec![CallView {
                    id: "refused".to_owned(),
                    output: String::new(),
                    failed: None,
                }],
                at: UnixMs(3),
            },
        );
        assert_eq!(tool_statuses(&fold), [Running, Reported]);
        tell(&mut fold, request_answering(false, 4));
        assert_eq!(tool_statuses(&fold), [Running, Reported]);
        tell(&mut fold, request_answering(true, 5));
        assert_eq!(tool_statuses(&fold), [Reported, Reported]);

        tell(&mut fold, replied(vec![call("next")], None, None, 6));
        assert_eq!(tool_statuses(&fold), [Reported, Reported, Running]);
        tell(&mut fold, request_answering(true, 7));
        assert_eq!(tool_statuses(&fold), [Reported, Reported, Reported]);
    }

    /// A request's timing lands on the call it names, among the calls it
    /// answers, and goes with that call when a rewind drops it.
    #[test]
    fn a_request_times_the_call_it_answers() {
        let timing = rho_agent_types::ExecTiming {
            first_block_at: Some(UnixMs(10)),
            handed_off_at: Some(UnixMs(40)),
            ..Default::default()
        };
        let mut fold = TranscriptFold::new(me(), &[]);
        let mut answering = request_answering(true, 4);
        let LogEvent::Request { timing: slot, .. } = &mut answering else {
            unreachable!()
        };
        *slot = Some(rho_agent_types::CallTiming {
            call: rho_agent_types::transcript::ExecId::try_from("two").unwrap(),
            timing,
        });
        let rows = [
            replied(vec![call("one")], None, None, 1),
            replied(vec![call("two")], None, None, 2),
            answering,
        ];
        for (pos, event) in rows.into_iter().enumerate() {
            fold.tell(Wpos(pos as u64), &event);
        }
        let timings = |fold: &TranscriptFold| -> Vec<(String, rho_agent_types::ExecTiming)> {
            fold.state()
                .blocks
                .iter()
                .filter_map(|block| match &**block {
                    UiBlock::Tool(tool) => Some((tool.id.clone(), tool.timing)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            timings(&fold),
            [
                ("one".to_owned(), Default::default()),
                ("two".to_owned(), timing),
            ]
        );
        fold.tell(
            Wpos(3),
            &LogEvent::Rewound {
                agent: me(),
                to: Wpos(1),
                at: UnixMs(5),
            },
        );
        assert_eq!(timings(&fold), [("one".to_owned(), Default::default())]);
    }

    /// A rewind drops blocks, so what the fold remembers of the last answer
    /// must not point past them: the next call is still answered.
    #[test]
    fn a_call_after_a_rewind_is_answered() {
        use UiToolStatus::{Reported, Running};
        let mut fold = TranscriptFold::new(me(), &[]);
        let rows = [
            replied(vec![call("one")], None, None, 1),
            request_answering(true, 2),
            replied(vec![call("two")], None, None, 3),
            request_answering(true, 4),
            replied(vec![call("three")], None, None, 5),
            request_answering(true, 6),
            LogEvent::Rewound {
                agent: me(),
                to: Wpos(2),
                at: UnixMs(7),
            },
            replied(vec![call("after")], None, None, 8),
        ];
        for (pos, event) in rows.into_iter().enumerate() {
            fold.tell(Wpos(pos as u64), &event);
        }
        assert_eq!(tool_statuses(&fold), [Reported, Running]);
        fold.tell(Wpos(8), &request_answering(true, 9));
        assert_eq!(tool_statuses(&fold), [Reported, Reported]);
    }

    /// The message an input handed over stays, under the id the chat
    /// knows it by, when the turn it started is rewound; the policy's
    /// notice and the reply go.
    #[test]
    fn a_rewind_keeps_the_messages_its_inputs_handed_over() {
        let mut fold = TranscriptFold::new(me(), &[]);
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
            fold.tell(Wpos(pos as u64), &event);
        }
        assert_eq!(
            fold.state().blocks,
            vec![Arc::new(UiBlock::Delivered {
                message: MessageId(rho_agent_types::ChatSeq(1)),
                text: "Message from the user:\nmessage 1".to_owned()
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
            answers: true,
            compaction: true,
            timing: None,
            output: String::new(),
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
    fn separately_sent_mail_survives_a_rewind_of_the_model_turn() {
        let first = MessageId(rho_agent_types::ChatSeq(91));
        let later = MessageId(rho_agent_types::ChatSeq(24));
        let result = told(vec![
            LogEvent::MailWritten {
                agent: me(),
                message: first,
                version: Some(first.0),
                at: UnixMs(6),
            },
            LogEvent::MailWritten {
                agent: me(),
                message: later,
                version: Some(later.0),
                at: UnixMs(6),
            },
            LogEvent::Rewound {
                agent: me(),
                to: Wpos(1),
                at: UnixMs(8),
            },
        ]);
        let messages: Vec<_> = result
            .blocks
            .iter()
            .filter_map(|block| match &**block {
                UiBlock::MailWritten { message, .. } => Some(*message),
                _ => None,
            })
            .collect();
        assert_eq!(messages, [first, later]);
    }

    #[test]
    fn a_brief_does_not_reappear_as_a_delivered_mail() {
        let mut props = rho_agent_types::Props::new();
        props.insert("origin".into(), "chat:27".into());
        let spec = crate::protocol::worksets::SpecView {
            runtime: RuntimeKind::Rho,
            model: "sol".to_owned(),
            effort: "low".to_owned(),
            props,
        };
        let input = |number, text: &str| crate::protocol::worksets::InputView {
            message: Some(MessageId(ChatSeq(number))),
            text: text.into(),
            role: rho_agent_types::Role::User,
        };
        let state = told(vec![
            LogEvent::AgentCreated {
                agent: me(),
                spec,
                at: UnixMs(1),
            },
            LogEvent::Request {
                agent: me(),
                inputs: vec![input(27, "brief"), input(28, "same thread, new mail")],
                answers: false,
                compaction: false,
                timing: None,
                output: String::new(),
                at: UnixMs(2),
            },
        ]);
        assert_eq!(state.blocks.len(), 1);
        assert!(
            matches!(&*state.blocks[0], UiBlock::Delivered { message, text }
            if *message == MessageId(ChatSeq(28)) && text == "same thread, new mail")
        );
    }

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
                Arc::new(UiBlock::Failure {
                    kind: FailureKind::Retrying { runtime: None },
                    text: "overloaded".to_owned(),
                    times: 1,
                    since: UnixMs(2),
                    at: UnixMs(2),
                }),
                Arc::new(UiBlock::Failure {
                    kind: FailureKind::Stopped,
                    text: "quota".to_owned(),
                    times: 1,
                    since: UnixMs(3),
                    at: UnixMs(3),
                }),
            ]
        );
    }

    /// A cancel is said where it happened, and ends the call that ran.
    #[test]
    fn a_cancel_is_said_and_ends_what_ran() {
        let state = told(vec![
            replied(vec![call("long")], None, None, 1),
            LogEvent::Cancelled {
                agent: me(),
                at: UnixMs(2),
            },
        ]);
        assert_eq!(only_tool(&state).status, UiToolStatus::Cancelled);
        assert_eq!(
            *state.blocks[1],
            UiBlock::Notice {
                text: "you cancelled the request".to_owned(),
                times: 1,
            }
        );
        assert_ne!(state.status, UiAgentStatus::Error, "a cancel is no failure");
    }

    /// A retry that fails as the one before did reads once, counted; a
    /// different error, or the same after something else, reads again.
    #[test]
    fn the_same_error_again_is_counted_on_one_line() {
        let failed = |error: &str, at: u64| LogEvent::Failed {
            agent: me(),
            text: String::new(),
            error: error.to_owned(),
            retrying: true,
            at: UnixMs(at),
        };
        let state = told(vec![
            failed("overloaded", 1),
            failed("overloaded", 2),
            failed("overloaded", 3),
            failed("timed out", 4),
            failed("overloaded", 5),
        ]);
        let notices: Vec<_> = state
            .blocks
            .iter()
            .map(|block| match &**block {
                UiBlock::Failure { text, times, .. } => (text.as_str(), *times),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            notices,
            [("overloaded", 3), ("timed out", 1), ("overloaded", 1),]
        );
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
