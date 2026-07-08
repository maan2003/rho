//! Raw redb schema for persisted agents.

use camino::Utf8PathBuf;
use prefix_id::{PrefixId, PrefixIdDomain};
use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::UnixMs;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use rho_inference::PromptCacheKey;
pub use rho_inference::config::{DeepConfig, DeepEffort};
use rho_workspaces::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use uuid::Uuid;

use crate::AgentEvent;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
/// Singleton row holding this database's random machine seed (see
/// [`PrefixIdDomain::machine_seed`]), generated once at init.
const MACHINE: TableDefinition<u8, u64> = TableDefinition::new("machine");
const MACHINE_SEED_KEY: u8 = 0;
const FORMAT: TableDefinition<(), String> = TableDefinition::new("format");
const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentEventPos> =
    TableDefinition::new("lineage_parents");
const AGENT_EVENTS: TableDefinition<AgentEventPos, Sen<AgentEvent<'static>>> =
    TableDefinition::new("agent_events");
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");
const TOPICS: TableDefinition<TopicId, Sen<TopicRecord>> = TableDefinition::new("topics");
const TOPIC_AGENTS: TableDefinition<TopicAgentKey, ()> = TableDefinition::new("topic_agents");
/// Keyed by the workdir's absolute path (UTF-8; paths are strings on disk
/// and on the wire), making paths unique by construction.
const WORKDIRS: TableDefinition<String, Sen<WorkdirRecord>> = TableDefinition::new("workdirs");
/// Attention bookkeeping per agent: when it last finished a turn, whether
/// that turn ended blocked on the user, and what the user did about it.
/// Rows appear on an agent's first turn end; absence means "never finished
/// a turn". Kept out of [`AgentRecord`] so the hot turn-end write never
/// rewrites agent metadata.
const CURRENT_AGENT_DB_FORMAT: &str = "b8c0d4e1";

struct AgentDbMigration {
    from: &'static str,
    to: &'static str,
    migrate: fn(&mut WriteTxn),
}

const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[AgentDbMigration {
    from: "e7f3a9c2",
    to: "b8c0d4e1",
    migrate: backfill_agent_last_user_message,
}];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
    pub const LAST_TOPIC_ID: Self = Self(3);
    pub const LAST_WORKSPACE_ID: Self = Self(4);
}

pub use rho_core::{AgentId, AgentIdDomain};

pub type TopicId = PrefixId<TopicIdDomain>;

/// Keys topic-id encoding with this database's persisted machine seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TopicIdDomain(pub u64);

impl PrefixIdDomain for TopicIdDomain {
    const KIND: &'static str = "topic-id";

    fn machine_seed(&self) -> u64 {
        self.0
    }
}

/// A registered directory agents can be started in, keyed by its absolute
/// path. Purely selection vocabulary for clients; agents record their own
/// working directory and the daemon never requires it to match a registered
/// workdir.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WorkdirRecord {
    pub name: String,
    pub created_at: UnixMillis,
}

/// Pin/archive state, shared by topics and agents. Pinned items sort first
/// in client rails; archived items are hidden (never deleted — the event
/// history stays loadable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Status {
    Normal,
    Pinned,
    Archived,
}

/// What the user did about an agent's last finished turn. Attention is
/// action-cleared (the email-triage model): a turn end always demands a
/// disposition, and merely looking at the agent never provides one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentDisposition {
    /// No disposition yet: the ball is in the user's court.
    Pending,
    /// Acknowledged; nothing more needed until the next turn end. The
    /// default so an agent that never finished a turn has nothing to act
    /// on (and so pre-disposition records decode that way).
    #[default]
    Done,
    /// Deferred: quiet until `until`, then pending again (the Slack-reminder
    /// move for "I'll get back to this").
    Snoozed { until: UnixMillis },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TopicRecord {
    pub name: String,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue)]
pub struct TopicAgentKey {
    topic_id: TopicId,
    agent_id: AgentId,
}

impl TopicAgentKey {
    pub fn new(topic_id: TopicId, agent_id: AgentId) -> Self {
        Self { topic_id, agent_id }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentLineageId(u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentEventPos {
    lineage_id: AgentLineageId,
    seq: u32,
}

impl AgentEventPos {
    fn root(lineage_id: AgentLineageId) -> Self {
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

pub type UnixMillis = UnixMs;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    /// Where this agent works. Fixed at creation: the accumulated model
    /// context assumes one root for the agent's life (the workspace's repo
    /// path). For pool workspaces the jj workspace name is this agent's own
    /// id (or the joined agent's, for agents sharing a workspace).
    pub workspace: WorkspaceInfo,
    pub status: Status,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub mode: AgentMode,
    pub runtime: AgentRuntime,
    /// When the user last sent this agent a message; rail recency seed.
    /// Turn ends reset the disposition but leave this alone — replying is
    /// the engagement signal, finishing is the agent's schedule.
    #[senax(default)]
    pub last_user_message: UnixMillis,
    #[senax(default)]
    pub disposition: AgentDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum AgentRuntime {
    Rho { prompt_cache_key: PromptCacheKey },
    Claude { session_id: Uuid },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentMode {
    Deep(DeepConfig),
    Fable { effort: FableEffort },
    Opus { effort: OpusEffort },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum FableEffort {
    Medium,
    Xhigh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum OpusEffort {
    Medium,
    Xhigh,
}

impl AgentMode {
    pub fn deep_default() -> Self {
        Self::Deep(DeepConfig::default())
    }

    pub fn deep_config(self) -> Option<DeepConfig> {
        match self {
            Self::Deep(config) => Some(config),
            Self::Fable { .. } | Self::Opus { .. } => None,
        }
    }

    pub fn claude_model(self) -> Option<rho_claude::Model> {
        match self {
            Self::Fable { .. } => Some(rho_claude::Model::Fable),
            Self::Opus { .. } => Some(rho_claude::Model::Opus),
            Self::Deep(_) => None,
        }
    }

    pub fn claude_effort(self) -> Option<rho_claude::Effort> {
        match self {
            Self::Fable { effort } => Some(effort.to_claude_effort()),
            Self::Opus { effort } => Some(effort.to_claude_effort()),
            Self::Deep(_) => None,
        }
    }

    pub fn is_claude(self) -> bool {
        matches!(self, Self::Fable { .. } | Self::Opus { .. })
    }
}

impl FableEffort {
    fn to_claude_effort(self) -> rho_claude::Effort {
        match self {
            Self::Medium => rho_claude::Effort::Medium,
            Self::Xhigh => rho_claude::Effort::Xhigh,
        }
    }
}

impl OpusEffort {
    fn to_claude_effort(self) -> rho_claude::Effort {
        match self {
            Self::Medium => rho_claude::Effort::Medium,
            Self::Xhigh => rho_claude::Effort::Xhigh,
        }
    }
}

pub trait AgentReadTxnExt {
    /// This database's random machine seed; present once
    /// [`AgentWriteTxnExt::init_agent_tables`] has run.
    fn machine_seed(&self) -> u64;
    fn last_agent_counter(&self) -> u64;
    fn last_workspace_counter(&self) -> u64;
    fn get_topic(&self, topic_id: TopicId) -> TopicRecord;
    fn list_topics(&self) -> Vec<(TopicId, TopicRecord)>;
    fn list_topic_agents(&self, topic_id: TopicId) -> Vec<AgentId>;
    /// The topic an agent currently belongs to.
    fn agent_topic(&self, agent_id: AgentId) -> Option<TopicId>;
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)>;
    fn list_workdirs(&self) -> Vec<(Utf8PathBuf, WorkdirRecord)>;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
}

#[allow(clippy::too_many_arguments)]
pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);

    fn create_topic(&mut self, now: UnixMillis, name: String, status: Status) -> TopicId;

    fn set_topic_name(&mut self, now: UnixMillis, topic_id: TopicId, name: String);

    fn set_topic_status(&mut self, now: UnixMillis, topic_id: TopicId, status: Status);

    fn set_agent_status(&mut self, now: UnixMillis, agent_id: AgentId, status: Status);

    fn set_agent_mode(&mut self, now: UnixMillis, agent_id: AgentId, mode: AgentMode);

    fn set_agent_display_name(&mut self, now: UnixMillis, agent_id: AgentId, name: String);

    fn alloc_agent_id(&mut self) -> AgentId;

    /// Reserves a fresh jj workspace name. Ids never repeat, so recreated
    /// workspaces can't collide with forgotten names in the repo view.
    fn alloc_workspace_id(&mut self) -> WorkspaceId;

    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        topic_id: TopicId,
        display_name: Option<String>,
        workspace: WorkspaceInfo,
        mode: AgentMode,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    ) -> AgentEventPos;

    /// Re-points the agent's topic membership. Topics are ad-hoc groupings
    /// agents move into after the fact; nothing else about the agent changes.
    fn move_agent_to_topic(&mut self, agent_id: AgentId, topic_id: TopicId);

    /// Registers `path` or renames it if already registered.
    fn upsert_workdir(&mut self, now: UnixMillis, path: &str, name: String);

    fn remove_workdir(&mut self, path: &str);

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos;

    fn fork_agent_lineage(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        parent: AgentEventPos,
    ) -> AgentEventPos;

    /// Records a turn end for attention purposes; resets the disposition to
    /// `Pending` — every finished turn demands a fresh disposition.
    fn record_agent_turn_end(&mut self, agent_id: AgentId);

    /// Stamps the user's engagement with an agent (rail recency) and clears
    /// its disposition: replying is as much a verdict as :done.
    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId);

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition);
}

impl AgentReadTxnExt for ReadTxn {
    fn machine_seed(&self) -> u64 {
        self.open_table(MACHINE)
            .get(&MACHINE_SEED_KEY)
            .expect("machine seed missing; init_agent_tables must run first")
            .value()
    }

    fn last_agent_counter(&self) -> u64 {
        self.open_table(COUNTERS)
            .get(&CounterKey::LAST_AGENT_ID)
            .map(|counter| counter.value())
            .unwrap_or(0)
    }

    fn last_workspace_counter(&self) -> u64 {
        self.open_table(COUNTERS)
            .get(&CounterKey::LAST_WORKSPACE_ID)
            .map(|counter| counter.value())
            .unwrap_or(0)
    }

    fn get_topic(&self, topic_id: TopicId) -> TopicRecord {
        self.open_table(TOPICS)
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned()
    }

    fn list_topics(&self) -> Vec<(TopicId, TopicRecord)> {
        self.open_table(TOPICS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn list_topic_agents(&self, topic_id: TopicId) -> Vec<AgentId> {
        self.open_table(TOPIC_AGENTS)
            .range(
                TopicAgentKey::new(topic_id, AgentId::MIN)
                    ..=TopicAgentKey::new(topic_id, AgentId::MAX),
            )
            .map(|(key, _)| key.value().agent_id)
            .collect()
    }

    fn agent_topic(&self, agent_id: AgentId) -> Option<TopicId> {
        self.open_table(TOPIC_AGENTS)
            .iter()
            .map(|(key, _)| key.value())
            .find(|key| key.agent_id == agent_id)
            .map(|key| key.topic_id)
    }

    fn get_agent(&self, agent_id: AgentId) -> AgentRecord {
        self.open_table(AGENTS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned()
    }

    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)> {
        self.open_table(AGENTS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn list_workdirs(&self) -> Vec<(Utf8PathBuf, WorkdirRecord)> {
        self.open_table(WORKDIRS)
            .iter()
            .map(|(key, value)| (Utf8PathBuf::from(key.value()), value.value().into_owned()))
            .collect()
    }

    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>) {
        let (next, records) = self.agent_event_records(agent_id);
        (next, records.into_iter().map(|(_, event)| event).collect())
    }

    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>) {
        let agent = self.get_agent(agent_id);
        let mut segments = Vec::new();
        let mut lineage_id = agent.current_lineage;
        let mut end_seq = u32::MAX;
        let lineage_parents = self.open_table(LINEAGE_PARENTS);
        loop {
            segments.push((lineage_id, end_seq));
            let Some(parent) = lineage_parents.get(&lineage_id) else {
                break;
            };
            let parent = parent.value();
            lineage_id = parent.lineage_id;
            end_seq = parent.seq;
        }
        drop(lineage_parents);

        let mut events = Vec::new();
        let mut next = AgentEventPos::root(agent.current_lineage);
        let timeline = self.open_table(AGENT_EVENTS);
        for (lineage_id, end_seq) in segments.into_iter().rev() {
            let is_current_lineage = lineage_id == agent.current_lineage;
            for (key, value) in timeline.range(
                AgentEventPos::root(lineage_id)..=AgentEventPos {
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
                events.push((key, value.value().into_owned()));
            }
        }
        (next, events)
    }
}

impl AgentWriteTxnExt for WriteTxn {
    fn init_agent_tables(&mut self) {
        // Migrations run before the typed opens below: a migration may need
        // to rewrite a table whose stored key/value types no longer match
        // the current definitions.
        migrate_agent_db_format(self);
        self.open_table(COUNTERS);
        self.open_table(FORMAT);
        self.open_table(LINEAGE_PARENTS);
        self.open_table(AGENT_EVENTS);
        self.open_table(AGENTS);
        self.open_table(TOPICS);
        self.open_table(TOPIC_AGENTS);
        self.open_table(WORKDIRS);
        let mut machine = self.open_table(MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
    }

    fn create_topic(&mut self, now: UnixMillis, name: String, status: Status) -> TopicId {
        let domain = TopicIdDomain(machine_seed(self));
        let topic_id =
            TopicId::from_counter(next_counter(self, CounterKey::LAST_TOPIC_ID), &domain)
                .expect("topic id counter exceeds prefix-id capacity");
        let topic = TopicRecord {
            name,
            created_at: now,
            updated_at: now,
            status,
        };
        self.open_table(TOPICS)
            .insert(&topic_id, SenValue::borrowed(&topic));
        topic_id
    }

    fn set_topic_name(&mut self, now: UnixMillis, topic_id: TopicId, name: String) {
        let mut topics = self.open_table(TOPICS);
        let mut topic = topics
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned();
        topic.name = name;
        topic.updated_at = now;
        topics.insert(&topic_id, SenValue::borrowed(&topic));
    }

    fn set_topic_status(&mut self, now: UnixMillis, topic_id: TopicId, status: Status) {
        let mut topics = self.open_table(TOPICS);
        let mut topic = topics
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned();
        topic.status = status;
        topic.updated_at = now;
        topics.insert(&topic_id, SenValue::borrowed(&topic));
    }

    fn set_agent_status(&mut self, now: UnixMillis, agent_id: AgentId, status: Status) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.status = status;
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_mode(&mut self, now: UnixMillis, agent_id: AgentId, mode: AgentMode) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.mode = mode;
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_display_name(&mut self, now: UnixMillis, agent_id: AgentId, name: String) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.display_name = Some(name);
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
    }

    fn alloc_workspace_id(&mut self) -> WorkspaceId {
        let domain = WorkspaceIdDomain(machine_seed(self));
        WorkspaceId::from_counter(next_counter(self, CounterKey::LAST_WORKSPACE_ID), &domain)
            .expect("workspace id counter exceeds prefix-id capacity")
    }

    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        topic_id: TopicId,
        display_name: Option<String>,
        workspace: WorkspaceInfo,
        mode: AgentMode,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    ) -> AgentEventPos {
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS);
        let agent = AgentRecord {
            display_name,
            workspace,
            status: Status::Normal,
            created_at: now,
            updated_at: now,
            current_lineage: lineage_id,
            parent_agent,
            mode,
            runtime,
            // Creating an agent is engagement; there is no turn to act on.
            last_user_message: now,
            disposition: AgentDisposition::Done,
        };
        self.open_table(AGENTS)
            .insert(&agent_id, SenValue::borrowed(&agent));
        self.open_table(TOPIC_AGENTS)
            .insert(&TopicAgentKey::new(topic_id, agent_id), &());
        AgentEventPos::root(lineage_id)
    }

    fn move_agent_to_topic(&mut self, agent_id: AgentId, topic_id: TopicId) {
        let mut topic_agents = self.open_table(TOPIC_AGENTS);
        let previous = topic_agents
            .iter()
            .map(|(key, _)| key.value())
            .filter(|key| key.agent_id == agent_id)
            .collect::<Vec<_>>();
        for key in previous {
            topic_agents.remove(&key);
        }
        topic_agents.insert(&TopicAgentKey::new(topic_id, agent_id), &());
    }

    fn upsert_workdir(&mut self, now: UnixMillis, path: &str, name: String) {
        let mut workdirs = self.open_table(WORKDIRS);
        let created_at = workdirs
            .get(&path.to_owned())
            .map(|record| record.value().into_owned().created_at)
            .unwrap_or(now);
        workdirs.insert(
            &path.to_owned(),
            SenValue::borrowed(&WorkdirRecord { name, created_at }),
        );
    }

    fn remove_workdir(&mut self, path: &str) {
        self.open_table(WORKDIRS).remove(&path.to_owned());
    }

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos {
        self.open_table(AGENT_EVENTS)
            .insert(&at, SenValue::borrowed(event));
        at.next()
    }

    fn record_agent_turn_end(&mut self, agent_id: AgentId) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        // A turn end puts the ball back in the user's court; it says
        // nothing about engagement, so `last_user_message` stays.
        agent.disposition = AgentDisposition::Pending;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.last_user_message = now;
        // Replying is a verdict like :done or :snooze — the ball moves to
        // the agent's court even if the turn hasn't started yet (queued
        // delivery), so a pending lamp must not linger.
        agent.disposition = AgentDisposition::Done;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.disposition = disposition;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn fork_agent_lineage(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        parent: AgentEventPos,
    ) -> AgentEventPos {
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS)
            .insert(&lineage_id, &parent);
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.current_lineage = lineage_id;
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
        AgentEventPos::root(lineage_id)
    }
}

fn migrate_agent_db_format(write: &mut WriteTxn) {
    let current = CURRENT_AGENT_DB_FORMAT;
    let mut format = {
        let table = write.open_table(FORMAT);
        table
            .get(&())
            .map(|value| value.value())
            .unwrap_or_else(|| current.to_owned())
    };

    while format != current {
        let Some(migration) = AGENT_DB_MIGRATIONS
            .iter()
            .find(|migration| migration.from == format)
        else {
            panic!(
                "this rho agent database was written by an older or different rho version \
                 (database format {format}, this build expects {current}). \
                 Update rho one version at a time so migrations can run, or remove \
                 the local rho database if you do not need the saved agents."
            );
        };
        (migration.migrate)(write);
        format = migration.to.to_owned();
    }

    write.open_table(FORMAT).insert(&(), &current.to_owned());
}

fn backfill_agent_last_user_message(write: &mut WriteTxn) {
    let mut agents = write.open_table(AGENTS);
    let records = agents
        .iter()
        .map(|(key, value)| (key.value(), value.value().into_owned()))
        .collect::<Vec<_>>();
    for (agent_id, mut agent) in records {
        if agent.last_user_message == UnixMs(0) {
            agent.last_user_message = agent.created_at;
            agents.insert(&agent_id, SenValue::borrowed(&agent));
        }
    }
}

fn next_counter(write: &mut WriteTxn, key: CounterKey) -> u64 {
    let mut counters = write.open_table(COUNTERS);
    let next = counters.get(&key).map(|value| value.value()).unwrap_or(0) + 1;
    counters.insert(&key, &next);
    next
}

fn machine_seed(write: &mut WriteTxn) -> u64 {
    write
        .open_table(MACHINE)
        .get(&MACHINE_SEED_KEY)
        .expect("machine seed missing; init_agent_tables must run first")
        .value()
}

#[cfg(test)]
mod tests;
