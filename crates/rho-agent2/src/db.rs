//! Durable transcript and queue storage.
//!
//! History is append-only and the core is its sole writer, so the log is just
//! the order in which the core did things. Queue insertion is recorded before
//! it becomes live state, so a message that was accepted cannot be lost.
//!
//! The log belongs to a *lineage* rather than to an agent; an agent points at
//! the one it is currently on, and reading is that path walked back to the root
//! and then replayed forwards. `DECISION-history-only-branches`.

use std::borrow::Cow;
use std::path::Path;

use redb::{TableDefinition, TableHandle as _, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::{AgentId, AgentIdDomain, ContextBlock};
use rho_db::{RhoDb, Sen, SenValue, WriteTxn};
use rho_inference::PromptCacheKey;
use rho_inference::config::{InferenceModel, InferenceProfile};
use senax_encoder::{Decode, Encode};

use crate::source::QueuedInput;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("rho-agent2.counters.v1");
/// Singleton row holding this database's random machine seed, which keys
/// [`AgentId`] encoding. Written once, when the first agent is created.
const MACHINE: TableDefinition<u8, u64> = TableDefinition::new("rho-agent2.machine.v1");
const MACHINE_SEED_KEY: u8 = 0;
/// Where each branch left the lineage it came from. A lineage with no row here
/// is a root.
const LINEAGE_PARENTS: TableDefinition<LineageId, EventPos> =
    TableDefinition::new("rho-agent2.lineage_parents.v1");
const EVENTS: TableDefinition<EventPos, Sen<AgentEvent<'static>>> =
    TableDefinition::new("rho-agent2.events.v1");
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> =
    TableDefinition::new("rho-agent2.agents.v1");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    const LAST_AGENT_ID: Self = Self(1);
    const LAST_LINEAGE_ID: Self = Self(2);
}

/// One branch of history. Agents point at these; nothing points back, because
/// a branch outlives the agent's interest in it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct LineageId(u64);

/// Where an event sits: which branch, and how far along it.
///
/// This is the event key, so one lineage's events are a contiguous range in
/// order and a branch point is a bound rather than a search.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct EventPos {
    lineage_id: LineageId,
    seq: u32,
}

impl EventPos {
    fn root(lineage_id: LineageId) -> Self {
        Self { lineage_id, seq: 0 }
    }

    fn next(self) -> Self {
        Self {
            lineage_id: self.lineage_id,
            seq: self
                .seq
                .checked_add(1)
                .expect("agent timeline sequence overflow"),
        }
    }
}

/// An isolated redb-backed store. It may use its own database file or coexist
/// with unrelated `rho-db` tables.
#[derive(Clone, Debug)]
pub struct Store(RhoDb);

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Self {
        Self(RhoDb::open(path))
    }

    /// Mints an agent, the lineage its history starts on, and the position its
    /// first event goes at.
    pub(crate) async fn create_agent(
        &self,
        profile: InferenceProfile,
        model: PersistedModel,
        prompt_cache_key: PromptCacheKey,
    ) -> (AgentId, EventPos, AgentRecord) {
        let mut write = self.0.write().await;
        // Opened here so that an agent, once created, can always be read back
        // without asking whether the rest of the schema exists yet.
        write.open_table(LINEAGE_PARENTS);
        write.open_table(EVENTS);
        let mut machine = write.open_table(MACHINE);
        let stored = machine.get(&MACHINE_SEED_KEY).map(|seed| seed.value());
        let seed = stored.unwrap_or_else(rand::random::<u64>);
        if stored.is_none() {
            machine.insert(&MACHINE_SEED_KEY, &seed);
        }
        drop(machine);

        let id = AgentId::from_counter(
            next_counter(&mut write, CounterKey::LAST_AGENT_ID),
            &AgentIdDomain(seed),
        )
        .expect("agent id counter exceeds prefix-id capacity");
        let lineage_id = LineageId(next_counter(&mut write, CounterKey::LAST_LINEAGE_ID));
        let record = AgentRecord {
            profile,
            model,
            prompt_cache_key,
            current_lineage: lineage_id,
        };
        write
            .open_table(AGENTS)
            .insert(&id, SenValue::borrowed(&record));
        write.commit();
        (id, EventPos::root(lineage_id), record)
    }

    /// Everything this agent's branch inherits, in order, and where its next
    /// event goes.
    pub(crate) fn load(
        &self,
        id: AgentId,
    ) -> Option<(AgentRecord, EventPos, Vec<AgentEvent<'static>>)> {
        let read = self.0.read();
        if !read.has_table(AGENTS.name()) {
            return None;
        }
        let agent = read.open_table(AGENTS).get(&id)?.value().into_owned();

        // Back to the root first, remembering where each branch left its
        // parent: that instant is where the parent stops contributing.
        let mut segments = Vec::new();
        let mut lineage_id = agent.current_lineage;
        let mut end_seq = u32::MAX;
        let parents = read.open_table(LINEAGE_PARENTS);
        loop {
            segments.push((lineage_id, end_seq));
            let Some(parent) = parents.get(&lineage_id) else {
                break;
            };
            let parent = parent.value();
            lineage_id = parent.lineage_id;
            end_seq = parent.seq;
        }
        drop(parents);

        // ...then forwards, oldest branch first.
        let mut events = Vec::new();
        let mut next = EventPos::root(agent.current_lineage);
        let timeline = read.open_table(EVENTS);
        for (lineage_id, end_seq) in segments.into_iter().rev() {
            let is_current_lineage = lineage_id == agent.current_lineage;
            for (key, value) in timeline.range(
                EventPos::root(lineage_id)..=EventPos {
                    lineage_id,
                    seq: end_seq,
                },
            ) {
                let key = key.value();
                if key.seq == end_seq && end_seq != u32::MAX {
                    break;
                }
                if is_current_lineage {
                    next = key.next();
                }
                events.push(value.value().into_owned());
            }
        }
        Some((agent, next, events))
    }

    /// The agent owns the position: it is the sole writer of its own log, so
    /// nothing here has to be read before it can be written.
    pub(crate) async fn append(&self, at: EventPos, event: &AgentEvent<'_>) -> EventPos {
        let mut write = self.0.write().await;
        write
            .open_table(EVENTS)
            .insert(&at, SenValue::borrowed(event));
        write.commit();
        at.next()
    }

    /// Branch this agent's history at `parent`, and put it on the new branch.
    ///
    /// Everything up to `parent` is inherited; everything after stays where it
    /// is, still readable by anything that remembers the lineage it was on.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the tool that rewinds a transcript is not built yet"
        )
    )]
    pub(crate) async fn fork(&self, id: AgentId, parent: EventPos) -> EventPos {
        let mut write = self.0.write().await;
        let lineage_id = LineageId(next_counter(&mut write, CounterKey::LAST_LINEAGE_ID));
        write
            .open_table(LINEAGE_PARENTS)
            .insert(&lineage_id, &parent);
        let mut agents = write.open_table(AGENTS);
        let mut agent = agents
            .get(&id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.current_lineage = lineage_id;
        agents.insert(&id, SenValue::borrowed(&agent));
        drop(agents);
        write.commit();
        EventPos::root(lineage_id)
    }
}

fn next_counter(write: &mut WriteTxn, key: CounterKey) -> u64 {
    let mut counters = write.open_table(COUNTERS);
    let next = counters.get(&key).map(|value| value.value()).unwrap_or(0) + 1;
    counters.insert(&key, &next);
    next
}

/// What an agent is, as opposed to what has happened to it. Only
/// `current_lineage` ever changes, and only when it is forked.
///
/// Instructions are deliberately absent: `DECISION-instructions-are-code`.
#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct AgentRecord {
    pub profile: InferenceProfile,
    pub model: PersistedModel,
    pub prompt_cache_key: PromptCacheKey,
    pub current_lineage: LineageId,
}

#[derive(Clone, Copy, Debug, Encode, Decode)]
pub(crate) enum PersistedModel {
    Gpt55,
    Gpt56Sol,
    Gpt56Luna,
    Gpt56Terra,
}

impl From<InferenceModel> for PersistedModel {
    fn from(value: InferenceModel) -> Self {
        match value {
            InferenceModel::Gpt55 => Self::Gpt55,
            InferenceModel::Gpt56Sol => Self::Gpt56Sol,
            InferenceModel::Gpt56Luna => Self::Gpt56Luna,
            InferenceModel::Gpt56Terra => Self::Gpt56Terra,
        }
    }
}

impl From<PersistedModel> for InferenceModel {
    fn from(value: PersistedModel) -> Self {
        match value {
            PersistedModel::Gpt55 => Self::Gpt55,
            PersistedModel::Gpt56Sol => Self::Gpt56Sol,
            PersistedModel::Gpt56Luna => Self::Gpt56Luna,
            PersistedModel::Gpt56Terra => Self::Gpt56Terra,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub(crate) enum AgentEvent<'a> {
    /// An input was accepted into a queue.
    Queued(QueuedInput),
    /// The queues were thrown away without being sent. A cancel.
    QueueCleared,
    /// A boundary: every source was drained into `blocks`, they were appended
    /// to history, and a request went out carrying all of it.
    ///
    /// One event rather than an append and a start, because it was always one
    /// thing — and because the drain, the append and the send cannot come apart
    /// even in a crash. `blocks` can be empty: a retry or a resume sends with
    /// nothing pending, which is a fact worth being able to write down.
    Sent { blocks: Cow<'a, [ContextBlock]> },
    /// The model answered, and the request is over.
    Replied {
        blocks: Cow<'a, [ContextBlock]>,
        context_used: Option<u64>,
    },
}
