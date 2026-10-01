//! Raw redb schema for persisted agents.
//!
//! One log per agent (`agent_log`), dense positions from zero; one journal
//! (`journal`) naming every append in the order it landed. The log remains
//! authoritative; `agent_heads` is its transactionally updated read projection.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use redb::TableDefinition;
use redb_derive::{Key, Value as RedbValue};
#[cfg(test)]
use rho_agent_types::{AdvisorIntelligence, EngineerIntelligence};
use rho_agent_types::{AgentId, AgentIdDomain, AgentRole, Place, Seq, UnixMs};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};
use uuid::Uuid;

use crate::AgentEvent;
use crate::inference::PromptCacheKey;
#[cfg(test)]
use crate::inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::journal::{Feed, Journal, LogAppended};
use crate::log::{
    AgentConfig, AgentEventPos, AgentHead, AgentOrigin, AgentRuntime, AgentSpawnedBy,
    AgentUsageBucket, AgentUsageModel, ClaudeRewind, ContextBoundary, NativeRecovery,
    SessionBinding, usage_model_of,
};

mod conversation_migration;
mod native;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
/// Singleton row holding this database's random machine seed (see
/// [`PrefixIdDomain::machine_seed`]), generated once at init.
const MACHINE: TableDefinition<u8, u64> = TableDefinition::new("machine");
const MACHINE_SEED_KEY: u8 = 0;
pub(crate) const FORMAT: TableDefinition<(), String> = TableDefinition::new("format");
/// The redb savepoint taken before a migration, by the hop it guards
/// (`from->to`), so `rho debug rollback` can put the store back.
const RECOVERY: TableDefinition<String, u64> = TableDefinition::new("recovery_savepoints");
/// Every agent's raw log: one row per event, keyed by agent then
/// position, so a range read gives one agent and nothing else. Never
/// rewritten; a rewind is a row like any other.
const AGENT_LOG: TableDefinition<(AgentId, u64), Sen<AgentEvent<'static>>> =
    TableDefinition::new("agent_log");
/// Current heads, derived from the log in the same transaction as every append.
const AGENT_HEADS: TableDefinition<AgentId, Sen<AgentHead>> = TableDefinition::new("agent_heads");
/// The order every append landed in, across agents: `seq -> (agent, pos)`,
/// written in the same transaction as the row it names. What a client
/// follows to stay current.
const JOURNAL: TableDefinition<u64, (AgentId, u64)> = TableDefinition::new("journal");
/// Where each Claude agent's transcript rows stop: the session file they
/// came from and one past the last line copied. Written with the rows.
const AGENT_RESPONSE_SUBSCRIPTIONS: TableDefinition<AgentResponseSubscription, ()> =
    TableDefinition::new("agent_response_subscriptions");
/// Model, namespace presence, namespace text, per-namespace sequence.
const QUOTA_OBSERVATIONS: TableDefinition<
    (QuotaModel, u8, String, u64),
    Sen<QuotaObservationRecord>,
> = TableDefinition::new("quota_observations_by_model_namespace_sequence");
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct AgentUsageKey {
    agent_id: AgentId,
    bucket_start_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct GlobalAgentUsageKey {
    bucket_start_ms: u64,
    model: AgentUsageModel,
}

fn usage_model(config: &AgentConfig) -> AgentUsageModel {
    usage_model_of(&config.runtime, config.binding)
}

const AGENT_USAGE_BUCKETS: TableDefinition<AgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_agent_time");
const AGENT_USAGE_TOTALS: TableDefinition<AgentId, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_totals");
const GLOBAL_AGENT_USAGE: TableDefinition<GlobalAgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_time_provider");
/// The Claude account every agent runs on. One row: the account is global,
/// and switching it moves every agent at its next turn.
const CLAUDE_ACCOUNT: TableDefinition<(), String> = TableDefinition::new("claude_account");
const CURRENT_AGENT_DB_FORMAT: &str = "b85e2d07";
/// The log before the dealer read the conversation alone: statuses, waits
/// and turn edges were rows of their own.
const TURNS_AGENT_DB_FORMAT: &str = "1f34dc6c";
const QUOTA_RESET_JITTER_SECONDS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
}

/// A persistent relationship that routes every future terminal response from
/// `target` into `subscriber` as ordinary agent mail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct AgentResponseSubscription {
    target: AgentId,
    subscriber: AgentId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct QuotaModel(u8);

impl QuotaModel {
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);
    pub const OPUS: Self = Self(3);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            _ => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum QuotaProvider {
    ChatGpt,
    Claude,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct QuotaObservationRecord {
    pub provider: QuotaProvider,
    pub model: QuotaModel,
    /// The host-local OAuth namespace for ChatGPT observations. Claude and
    /// legacy observations are unscoped.
    pub auth_namespace: Option<String>,
    pub observed_at: UnixMs,
    pub used_percent: u8,
    pub reset_at_unix: Option<i64>,
}

fn add_global_agent_usage(write: &mut WriteTxn, model: AgentUsageModel, bucket: &AgentUsageBucket) {
    let key = GlobalAgentUsageKey {
        bucket_start_ms: bucket.bucket_start_ms,
        model,
    };
    let mut table = write.open_table(GLOBAL_AGENT_USAGE);
    let mut merged = table
        .get(&key)
        .map(|value| value.value().into_owned())
        .unwrap_or_else(|| AgentUsageBucket {
            bucket_start_ms: bucket.bucket_start_ms,
            ..AgentUsageBucket::default()
        });
    merged.add(bucket);
    table.insert(&key, SenValue::borrowed(&merged));
}

fn quota_observation_unchanged(old: &QuotaObservationRecord, new: &QuotaObservationRecord) -> bool {
    old.provider == new.provider
        && old.model == new.model
        && old.used_percent == new.used_percent
        && match (old.reset_at_unix, new.reset_at_unix) {
            (Some(old), Some(new)) => old.abs_diff(new) <= QUOTA_RESET_JITTER_SECONDS,
            (None, None) => true,
            _ => false,
        }
}

pub trait AgentReadTxnExt {
    /// This database's random machine seed; present once
    /// [`AgentWriteTxnExt::init_agent_tables`] has run.
    fn machine_seed(&self) -> u64;
    fn last_agent_counter(&self) -> u64;
    /// The fold of one agent's log. Panics when there is no such agent.
    fn get_agent(&self, agent_id: AgentId) -> AgentHead;
    fn try_get_agent(&self, agent_id: AgentId) -> Option<AgentHead>;
    fn agent_exists(&self, agent_id: AgentId) -> bool;
    /// Every agent, by the first row of its log; touches one row per agent.
    fn list_agent_ids(&self) -> Vec<AgentId>;
    /// Every agent's fold: the whole store. For conversions and tools.
    fn list_agents(&self) -> Vec<(AgentId, AgentHead)>;
    /// Who spawned an agent, read from its creation alone.
    fn agent_parent(&self, agent_id: AgentId) -> Option<AgentId>;
    /// The agent that spawned this one: its parent, or the Engineer that
    /// started it for the user. Spawn limits follow this edge.
    fn agent_spawner(&self, agent_id: AgentId) -> Option<AgentId>;
    fn agent_response_subscribers(&self, target: AgentId) -> Vec<AgentId>;
    fn is_agent_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool;
    /// The agent's history as it stands: every row a later `Rewound` did
    /// not take back, oldest first, and where the next row goes.
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    /// Frozen native replay range for the log's current visible branch.
    fn agent_context_boundary(&self, agent_id: AgentId) -> ContextBoundary;
    fn agent_native_recovery(&self, agent_id: AgentId) -> NativeRecovery;
    /// Visible native entries in this fixed half-open range, independent of
    /// later appends.
    fn agent_context_records(
        &self,
        agent_id: AgentId,
        boundary: ContextBoundary,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)>;
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
    fn agent_pending_claude_output(&self, agent_id: AgentId) -> Option<crate::ClaudeOutputBatch>;
    /// One row, hidden or not.
    fn agent_event(&self, agent_id: AgentId, pos: AgentEventPos) -> Option<AgentEvent<'static>>;
    /// Last response on the visible branch before this input's position.
    fn agent_input_carry(
        &self,
        agent_id: AgentId,
        before: AgentEventPos,
    ) -> Option<crate::inference::Carry>;

    /// Newest text-bearing visible rows, read backward and bounded before
    /// decoding/building a Luna request.

    /// How far the journal runs; zero when nothing has been appended.
    fn journal_head(&self) -> Seq;
    /// Journal entries after `since`, at most `limit`, with the rows they
    /// name.
    fn journal_since(
        &self,
        since: Seq,
        limit: usize,
    ) -> Vec<(Seq, AgentId, AgentEventPos, AgentEvent<'static>)>;
    /// Samples for one model, bounded to the horizon plus its preceding
    /// baseline.
    fn quota_observations(&self, model: QuotaModel, since: UnixMs) -> Vec<QuotaObservationRecord>;
    fn agent_usage(&self, agent_id: AgentId, since: UnixMs) -> Vec<AgentUsageBucket>;
    fn agent_usage_total(&self, agent_id: AgentId) -> AgentUsageBucket;
    fn global_agent_usage(&self, since: UnixMs) -> Vec<(AgentUsageModel, AgentUsageBucket)>;
    /// The Claude account agents run on, which is the default account until
    /// someone switches it.
    fn claude_account(&self) -> String;
}

#[allow(clippy::too_many_arguments)]
pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);
    /// Capture the exact native replay boundary within the append transaction.
    fn agent_context_boundary(&mut self, agent_id: AgentId) -> ContextBoundary;

    /// Appends one event at the agent's tail and names it in the journal.
    /// The tail is read from the table, not from a runtime's cursor, so
    /// any writer may append at any time. Returns where it landed.
    fn append_agent_event(&mut self, agent_id: AgentId, event: &AgentEvent<'_>) -> AgentEventPos;

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole);
    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey);
    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>);
    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid);

    fn alloc_agent_id(&mut self) -> AgentId;
    /// Applies an update only when its source is still visible. The
    /// returned cache is the acknowledged source of truth for a sidecar
    /// session; `None` means its result was made stale by a rewind.

    /// Takes back history from `to` on: told at a new position, so what
    /// the agent walked away from stays in the log
    /// (`DECISION-history-only-branches`). Returns where it was told.
    fn rewind_agent(&mut self, now: UnixMs, agent_id: AgentId, to: AgentEventPos) -> AgentEventPos;

    fn set_agent_response_subscription(
        &mut self,
        subscriber: AgentId,
        target: AgentId,
        subscribed: bool,
    );
    /// Records a changed whole-percentage weekly quota sample.
    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool;
    /// Puts every agent on `account` from its next turn. Running processes
    /// keep the account their namespace has mounted until then.
    fn set_claude_account(&mut self, account: &str);
    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket);
    fn replace_agent_usage(
        &mut self,
        buckets: &std::collections::HashMap<(AgentId, u64), AgentUsageBucket>,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) trait AgentProfileWriteTxnExt {
    /// The agent's first event. The role is the caller's: a binding
    /// implies one, but a spawner may ask for a narrower role than the
    /// binding's default.
    fn create_agent(
        &mut self,
        now: UnixMs,
        agent_id: AgentId,
        spawn_name: Option<String>,
        place: Place,
        role: AgentRole,
        mode: SessionBinding,
        runtime: AgentRuntime,
        origin: AgentOrigin,
    );

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding);
}

impl AgentProfileWriteTxnExt for WriteTxn {
    fn create_agent(
        &mut self,
        now: UnixMs,
        agent_id: AgentId,
        spawn_name: Option<String>,
        place: Place,
        role: AgentRole,
        mode: SessionBinding,
        runtime: AgentRuntime,
        origin: AgentOrigin,
    ) {
        let spawner = match origin {
            AgentOrigin::User => None,
            AgentOrigin::Child { parent: spawner } | AgentOrigin::UserOwned { by: spawner } => {
                Some(spawner)
            }
        };
        if let Some(spawner) = spawner {
            match agent_head_write(self, spawner)
                .expect("spawning agent must exist")
                .config
                .role
            {
                AgentRole::Engineer { .. } => {}
                AgentRole::Advisor { .. } => panic!("Advisors cannot spawn agents"),
            }
        }
        let spawned_by = match origin {
            AgentOrigin::User => AgentSpawnedBy::Direct,
            AgentOrigin::Child { .. } => AgentSpawnedBy::Engineer,
            AgentOrigin::UserOwned { by } => AgentSpawnedBy::UserOwned { by },
        };
        let parent_agent = origin.parent();
        let created = AgentEvent::Created {
            role,
            binding: mode,
            runtime,
            place,
            spawned_by,
            spawn_name,
            created_at: now,
            parent: parent_agent,
        };
        let at = self.append_agent_event(agent_id, &created);
        assert_eq!(at, AgentEventPos::ZERO, "agent {agent_id:?} already exists");
    }

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RoleChanged {
                role,
                binding: Some(binding),
                at: UnixMs::now(),
            },
        );
    }
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

    fn get_agent(&self, agent_id: AgentId) -> AgentHead {
        self.try_get_agent(agent_id)
            .unwrap_or_else(|| panic!("agent id missing: {agent_id:?}"))
    }

    fn try_get_agent(&self, agent_id: AgentId) -> Option<AgentHead> {
        self.open_table(AGENT_HEADS)
            .get(&agent_id)
            .map(|value| value.value().into_owned())
    }

    fn agent_exists(&self, agent_id: AgentId) -> bool {
        self.open_table(AGENT_LOG).get(&(agent_id, 0)).is_some()
    }

    fn list_agent_ids(&self) -> Vec<AgentId> {
        self.open_table(AGENT_HEADS)
            .iter()
            .map(|(id, _)| id.value())
            .collect()
    }

    fn list_agents(&self) -> Vec<(AgentId, AgentHead)> {
        self.open_table(AGENT_HEADS)
            .iter()
            .map(|(id, head)| (id.value(), head.value().into_owned()))
            .collect()
    }

    fn agent_parent(&self, agent_id: AgentId) -> Option<AgentId> {
        match self.agent_event(agent_id, AgentEventPos::ZERO)? {
            AgentEvent::Created { parent, .. } => parent,
            _ => None,
        }
    }
    fn agent_spawner(&self, agent_id: AgentId) -> Option<AgentId> {
        let head = self.try_get_agent(agent_id)?;
        match head.config.spawned_by {
            AgentSpawnedBy::UserOwned { by } => Some(by),
            _ => head.parent,
        }
    }
    fn agent_response_subscribers(&self, target: AgentId) -> Vec<AgentId> {
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS)
            .range(
                AgentResponseSubscription {
                    target,
                    subscriber: AgentId::MIN,
                }..=AgentResponseSubscription {
                    target,
                    subscriber: AgentId::MAX,
                },
            )
            .map(|(key, _)| key.value().subscriber)
            .collect()
    }

    fn is_agent_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool {
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS)
            .get(&AgentResponseSubscription { target, subscriber })
            .is_some()
    }

    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>) {
        let (next, records) = self.agent_event_records(agent_id);
        (next, records.into_iter().map(|(_, event)| event).collect())
    }

    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>) {
        let log = self.open_table(AGENT_LOG);
        visible_rows(rows(log.range(agent_range(agent_id))))
    }

    fn agent_context_boundary(&self, agent_id: AgentId) -> ContextBoundary {
        let cursor = self
            .open_table(native::NATIVE_CURSORS)
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default();
        ContextBoundary {
            from: cursor.from,
            through: self.get_agent(agent_id).next,
        }
    }

    fn agent_native_recovery(&self, agent_id: AgentId) -> NativeRecovery {
        self.open_table(native::NATIVE_CURSORS)
            .get(&agent_id)
            .map(|value| value.value().into_owned().recovery)
            .unwrap_or_default()
    }

    fn agent_context_records(
        &self,
        agent_id: AgentId,
        boundary: ContextBoundary,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)> {
        if boundary.from >= boundary.through {
            return Vec::new();
        }
        let log = self.open_table(AGENT_LOG);
        let mut hidden = Hidden::default();
        let mut visible = Vec::new();
        for (key, value) in log
            .range((agent_id, boundary.from.pos)..(agent_id, boundary.through.pos))
            .rev()
        {
            let pos = AgentEventPos::new(key.value().1);
            if hidden.from.is_some_and(|from| pos.pos >= from) {
                continue;
            }
            let event = value.value().into_owned();
            if hidden.visible(pos, &event) && matches!(event, AgentEvent::Entry(_)) {
                visible.push((pos, event));
            }
        }
        visible.reverse();
        visible
    }

    fn agent_pending_claude_output(&self, agent_id: AgentId) -> Option<crate::ClaudeOutputBatch> {
        let log = self.open_table(AGENT_LOG);
        let mut handed_off = BTreeSet::new();
        // Only the newest batch can still be pending. Handoffs to older
        // batches do not retire it, including across a rewind.
        for (_, event) in rows(log.range(agent_range(agent_id)).rev()) {
            match event {
                AgentEvent::ClaudeOutputHandedOff { id, .. } => {
                    handed_off.insert(id);
                }
                AgentEvent::ClaudeOutput { batch } => {
                    return (!handed_off.contains(&batch.id)).then_some(batch);
                }
                _ => {}
            }
        }
        None
    }

    fn agent_input_carry(
        &self,
        agent_id: AgentId,
        before: AgentEventPos,
    ) -> Option<crate::inference::Carry> {
        let log = self.open_table(AGENT_LOG);
        let mut hidden = Hidden::default();
        for (pos, event) in rows(log.range((agent_id, 0)..(agent_id, before.pos)).rev()) {
            if !hidden.visible(pos, &event) {
                continue;
            }
            if let AgentEvent::Entry(crate::entry::Entry::Step { carry, .. }) = event {
                return Some(carry);
            }
        }
        None
    }

    fn agent_event(&self, agent_id: AgentId, pos: AgentEventPos) -> Option<AgentEvent<'static>> {
        self.open_table(AGENT_LOG)
            .get(&(agent_id, pos.pos))
            .map(|value| value.value().into_owned())
    }

    fn journal_head(&self) -> Seq {
        Seq(self
            .open_table(JOURNAL)
            .iter()
            .next_back()
            .map(|(key, _)| key.value())
            .unwrap_or(0))
    }

    fn journal_since(
        &self,
        since: Seq,
        limit: usize,
    ) -> Vec<(Seq, AgentId, AgentEventPos, AgentEvent<'static>)> {
        let journal = self.open_table(JOURNAL);
        let log = self.open_table(AGENT_LOG);
        journal
            .range(since.0.saturating_add(1)..)
            .take(limit)
            .map(|(seq, row)| {
                let (agent_id, pos) = row.value();
                let event = log
                    .get(&(agent_id, pos))
                    .expect("journal names a row that exists")
                    .value()
                    .into_owned();
                (Seq(seq.value()), agent_id, AgentEventPos::new(pos), event)
            })
            .collect()
    }

    fn quota_observations(&self, model: QuotaModel, since: UnixMs) -> Vec<QuotaObservationRecord> {
        let table = self.open_table(QUOTA_OBSERVATIONS);
        let mut before = BTreeMap::<Option<String>, QuotaObservationRecord>::new();
        let mut observations = Vec::new();
        for (_, value) in table
            .range((model, 0, String::new(), 0)..)
            .take_while(|(key, _)| key.value().0 == model)
        {
            let observation = value.value().into_owned();
            if observation.observed_at < since {
                let old = before
                    .entry(observation.auth_namespace.clone())
                    .or_insert_with(|| observation.clone());
                if observation.observed_at >= old.observed_at {
                    *old = observation;
                }
            } else {
                observations.push(observation);
            }
        }
        observations.extend(before.into_values());
        observations.sort_by_key(|observation| observation.observed_at);
        observations
    }

    fn agent_usage(&self, agent_id: AgentId, since: UnixMs) -> Vec<AgentUsageBucket> {
        self.open_table(AGENT_USAGE_BUCKETS)
            .range(
                AgentUsageKey {
                    agent_id,
                    bucket_start_ms: since.0,
                }..=AgentUsageKey {
                    agent_id,
                    bucket_start_ms: u64::MAX,
                },
            )
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    fn agent_usage_total(&self, agent_id: AgentId) -> AgentUsageBucket {
        self.open_table(AGENT_USAGE_TOTALS)
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default()
    }

    fn claude_account(&self) -> String {
        self.open_table(CLAUDE_ACCOUNT)
            .get(&())
            .map(|value| value.value())
            .unwrap_or_else(|| rho_claude::accounts::DEFAULT_ACCOUNT.to_owned())
    }

    fn global_agent_usage(&self, since: UnixMs) -> Vec<(AgentUsageModel, AgentUsageBucket)> {
        self.open_table(GLOBAL_AGENT_USAGE)
            .range(
                GlobalAgentUsageKey {
                    bucket_start_ms: since.0,
                    model: AgentUsageModel::GPT,
                }..=GlobalAgentUsageKey {
                    bucket_start_ms: u64::MAX,
                    model: AgentUsageModel::LUNA,
                },
            )
            .map(|(key, value)| (key.value().model, value.value().into_owned()))
            .collect()
    }
}

impl AgentWriteTxnExt for WriteTxn {
    fn init_agent_tables(&mut self) {
        assert_agent_db_format(self);
        self.open_table(COUNTERS);
        self.open_table(FORMAT);
        self.open_table(AGENT_LOG);
        self.open_table(AGENT_HEADS);
        self.open_table(native::NATIVE_CURSORS);
        self.open_table(JOURNAL);
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
        self.open_table(QUOTA_OBSERVATIONS);
        self.open_table(AGENT_USAGE_BUCKETS);
        self.open_table(AGENT_USAGE_TOTALS);
        self.open_table(GLOBAL_AGENT_USAGE);
        self.open_table(CLAUDE_ACCOUNT);
        let mut machine = self.open_table(MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
    }
    fn agent_context_boundary(&mut self, agent_id: AgentId) -> ContextBoundary {
        let from = self
            .open_table(native::NATIVE_CURSORS)
            .get(&agent_id)
            .map(|row| row.value().into_owned().from)
            .unwrap_or_default();
        let through = self
            .open_table(AGENT_LOG)
            .range(agent_range(agent_id))
            .next_back()
            .map(|(key, _)| AgentEventPos::new(key.value().1).next())
            .unwrap_or_default();
        ContextBoundary { from, through }
    }

    fn append_agent_event(&mut self, agent_id: AgentId, event: &AgentEvent<'_>) -> AgentEventPos {
        let pos = {
            let log = self.open_table(AGENT_LOG);
            log.range(agent_range(agent_id))
                .next_back()
                .map(|(key, _)| AgentEventPos::new(key.value().1).next())
                .unwrap_or(AgentEventPos::ZERO)
        };
        debug_assert!(
            (pos == AgentEventPos::ZERO) == matches!(event, AgentEvent::Created { .. }),
            "creation is the first row of a log and nothing else is"
        );
        self.open_table(AGENT_LOG)
            .insert(&(agent_id, pos.pos), SenValue::borrowed(event));
        native::append(self, agent_id, pos, event);
        let head = if pos == AgentEventPos::ZERO {
            created_head(event, pos)
        } else {
            let mut head = self
                .open_table(AGENT_HEADS)
                .get(&agent_id)
                .expect("agent head missing for existing log")
                .value()
                .into_owned();
            fold_agent_head(&mut head, event);
            head.next = pos.next();
            head
        };
        self.open_table(AGENT_HEADS)
            .insert(&agent_id, SenValue::borrowed(&head));
        let seq = {
            let mut journal = self.open_table(JOURNAL);
            let seq = journal
                .iter()
                .next_back()
                .map(|(key, _)| key.value() + 1)
                .unwrap_or(1);
            journal.insert(&seq, &(agent_id, pos.pos));
            seq
        };
        // Told after the commit, so a listener woken by it finds the row.
        if let Some(journal) = self.observer::<Journal>() {
            let appends = journal.sender();
            let appended = LogAppended {
                seq: Seq(seq),
                agent_id,
                pos: pos.into(),
            };
            self.after_commit(move || {
                let _ = appends.send(Feed::Appended(appended));
            });
        }
        pos
    }

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RoleChanged {
                role,
                binding: None,
                at: UnixMs::now(),
            },
        );
    }

    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::PromptCacheKey(key),
                at: UnixMs::now(),
            },
        );
    }

    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewindPending(rewind),
                at: UnixMs::now(),
            },
        );
    }

    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewound { session_id },
                at: UnixMs::now(),
            },
        );
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
    }

    fn rewind_agent(&mut self, now: UnixMs, agent_id: AgentId, to: AgentEventPos) -> AgentEventPos {
        assert!(
            to != AgentEventPos::ZERO,
            "an agent's creation cannot be rewound away"
        );
        assert!(
            agent_event_visible_write(self, agent_id, to),
            "rewind target {to:?} of {agent_id:?} is not in its history"
        );
        self.append_agent_event(agent_id, &AgentEvent::Rewound { to, at: now })
    }

    fn set_agent_response_subscription(
        &mut self,
        subscriber: AgentId,
        target: AgentId,
        subscribed: bool,
    ) {
        assert_ne!(subscriber, target, "an agent cannot subscribe to itself");
        let key = AgentResponseSubscription { target, subscriber };
        let mut subscriptions = self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
        if subscribed {
            subscriptions.insert(&key, &());
        } else {
            subscriptions.remove(&key);
        }
    }
    fn set_claude_account(&mut self, account: &str) {
        self.open_table(CLAUDE_ACCOUNT)
            .insert(&(), account.to_owned());
    }

    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool {
        let namespace = observation.auth_namespace.clone().unwrap_or_default();
        let present = u8::from(observation.auth_namespace.is_some());
        let start = (observation.model, present, namespace.clone(), 0);
        let end = (observation.model, present, namespace.clone(), u64::MAX);
        let mut table = self.open_table(QUOTA_OBSERVATIONS);
        let records = table.range(start..=end);
        let mut next_sequence = 0;
        let mut latest_by_time = None;
        for (key, value) in records {
            next_sequence = key.value().3 + 1;
            let sample: QuotaObservationRecord = value.value().into_owned();
            if latest_by_time
                .as_ref()
                .is_none_or(|old: &QuotaObservationRecord| sample.observed_at >= old.observed_at)
            {
                latest_by_time = Some(sample);
            }
        }
        if latest_by_time
            .as_ref()
            .is_some_and(|old| quota_observation_unchanged(old, &observation))
        {
            return false;
        }
        table.insert(
            &(observation.model, present, namespace, next_sequence),
            SenValue::borrowed(&observation),
        );
        true
    }

    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket) {
        let mut bucket = bucket.clone();
        if bucket.model == AgentUsageModel::UNKNOWN {
            let head = agent_head_write(self, agent_id).expect("usage agent missing");
            bucket.model = usage_model(&head.config);
        }
        let key = AgentUsageKey {
            agent_id,
            bucket_start_ms: bucket.bucket_start_ms,
        };
        let mut buckets = self.open_table(AGENT_USAGE_BUCKETS);
        let mut merged = buckets
            .get(&key)
            .map(|value| value.value().into_owned())
            .unwrap_or_else(|| AgentUsageBucket {
                bucket_start_ms: bucket.bucket_start_ms,
                ..AgentUsageBucket::default()
            });
        merged.add(&bucket);
        buckets.insert(&key, SenValue::borrowed(&merged));
        drop(buckets);

        let mut totals = self.open_table(AGENT_USAGE_TOTALS);
        let mut total = totals
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default();
        total.add(&bucket);
        total.bucket_start_ms = 0;
        totals.insert(&agent_id, SenValue::borrowed(&total));
        drop(totals);

        add_global_agent_usage(self, bucket.model, &bucket);
    }

    fn replace_agent_usage(
        &mut self,
        replacement: &std::collections::HashMap<(AgentId, u64), AgentUsageBucket>,
    ) {
        let mut buckets = self.open_table(AGENT_USAGE_BUCKETS);
        let old_keys = buckets
            .iter()
            .map(|(key, _)| key.value())
            .collect::<Vec<_>>();
        for key in old_keys {
            buckets.remove(&key);
        }
        for ((agent_id, bucket_start_ms), bucket) in replacement {
            buckets.insert(
                &AgentUsageKey {
                    agent_id: *agent_id,
                    bucket_start_ms: *bucket_start_ms,
                },
                SenValue::borrowed(bucket),
            );
        }
        drop(buckets);

        let mut by_agent = std::collections::HashMap::<AgentId, AgentUsageBucket>::new();
        for ((agent_id, _), bucket) in replacement {
            by_agent.entry(*agent_id).or_default().add(bucket);
        }
        let mut totals = self.open_table(AGENT_USAGE_TOTALS);
        let old_agents = totals
            .iter()
            .map(|(key, _)| key.value())
            .collect::<Vec<_>>();
        for agent_id in old_agents {
            totals.remove(&agent_id);
        }
        for (agent_id, mut total) in by_agent {
            total.bucket_start_ms = 0;
            totals.insert(&agent_id, SenValue::borrowed(&total));
        }
    }
}

/// Every key of one agent's log.
fn agent_range(agent_id: AgentId) -> std::ops::RangeInclusive<(AgentId, u64)> {
    (agent_id, 0)..=(agent_id, u64::MAX)
}

/// Rows as `(position, event)`, from either kind of table iterator.
fn rows<'a>(
    iter: impl Iterator<
        Item = (
            redb::AccessGuard<'a, (AgentId, u64)>,
            redb::AccessGuard<'a, Sen<AgentEvent<'static>>>,
        ),
    >,
) -> impl Iterator<Item = (AgentEventPos, AgentEvent<'static>)> {
    iter.map(|(key, value)| {
        (
            AgentEventPos::new(key.value().1),
            value.value().into_owned(),
        )
    })
}

/// The rows a reader sees, oldest first: every `Rewound` takes back what
/// stands from `to` on, itself included in what remains. Also where the
/// next row goes, hidden rows counted.
fn visible_rows(
    all: impl Iterator<Item = (AgentEventPos, AgentEvent<'static>)>,
) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>) {
    let mut visible = Vec::new();
    let mut next = AgentEventPos::ZERO;
    for (pos, event) in all {
        next = pos.next();
        if let AgentEvent::Rewound { to, .. } = &event {
            // Visible positions stay sorted even after earlier branches were
            // removed; a rewind cuts one suffix, not a scattered set.
            let keep = visible.partition_point(|(kept, _)| kept < to);
            visible.truncate(keep);
        }
        visible.push((pos, event));
    }
    (next, visible)
}

/// Walking a log backward: which positions a later `Rewound` hides.
#[derive(Default)]
struct Hidden {
    from: Option<u64>,
}

impl Hidden {
    /// Whether the row at `pos` is visible, noting the rewind it may be.
    /// Rows must come newest first.
    fn visible(&mut self, pos: AgentEventPos, event: &AgentEvent<'_>) -> bool {
        if self.from.is_some_and(|from| pos.pos >= from) {
            return false;
        }
        if let AgentEvent::Rewound { to, .. } = event {
            self.from = Some(self.from.map_or(to.pos, |from| from.min(to.pos)));
        }
        true
    }
}

/// A user message in the log: the row that carries a pending notice.
fn carries_notice(event: &AgentEvent<'_>) -> bool {
    matches!(
        event,
        AgentEvent::Transcript {
            line: crate::TranscriptLine::User { .. },
            ..
        } | AgentEvent::Entry(crate::entry::Entry::Received {
            from: crate::entry::Party::Human,
            ..
        })
    )
}

fn created_head(event: &AgentEvent<'_>, pos: AgentEventPos) -> AgentHead {
    let AgentEvent::Created { parent, .. } = event else {
        panic!("an agent head starts at creation");
    };
    AgentHead {
        config: created_config(event),
        title_attempted: false,
        generated_title: None,
        parent: *parent,
        user_interacted: false,
        pending_notice: None,
        next: pos.next(),
    }
}

pub(crate) fn agent_head_write(write: &mut WriteTxn, agent_id: AgentId) -> Option<AgentHead> {
    write
        .open_table(AGENT_HEADS)
        .get(&agent_id)
        .map(|value| value.value().into_owned())
}

/// Whether a rewind destination is still in the visible history.
fn agent_event_visible_write(
    write: &mut WriteTxn,
    agent_id: AgentId,
    position: AgentEventPos,
) -> bool {
    let log = write.open_table(AGENT_LOG);
    let mut hidden = Hidden::default();
    for (pos, event) in rows(log.range(agent_range(agent_id)).rev()) {
        let visible = hidden.visible(pos, &event);
        if pos <= position {
            return pos == position && visible;
        }
    }
    false
}

/// The config a `Created` event states. Panics on any other event: only
/// creation can begin a config.
fn created_config(event: &AgentEvent<'_>) -> AgentConfig {
    let AgentEvent::Created {
        role,
        binding,
        runtime,
        place,
        spawned_by,
        spawn_name,
        created_at,
        ..
    } = event
    else {
        panic!("config can only begin at a Created event");
    };
    AgentConfig {
        role: *role,
        binding: *binding,
        runtime: runtime.clone(),
        place: place.clone(),
        spawned_by: *spawned_by,
        spawn_name: spawn_name.clone(),
        created_at: *created_at,
        claude_rewind: None,
    }
}

/// One event's effect on the head.
fn fold_agent_head(head: &mut AgentHead, event: &AgentEvent<'_>) {
    match event {
        AgentEvent::Created { .. } => head.config = created_config(event),
        AgentEvent::RoleChanged { role, binding, .. } => {
            head.config.role = *role;
            if let Some(binding) = binding {
                head.config.binding = *binding;
            }
        }
        // An empty notice is nothing to say (what the old `WorkdirAdded`
        // rows became).
        AgentEvent::Notice { text, .. } => {
            if !text.is_empty() {
                head.pending_notice = Some(text.to_string());
            }
        }
        AgentEvent::RuntimeRebound { change, .. } => match change {
            crate::RuntimeChange::ClaudeRewindPending(rewind) => {
                head.config.claude_rewind = rewind.clone();
            }
            crate::RuntimeChange::ClaudeRewound { session_id } => {
                head.config.runtime = AgentRuntime::Claude {
                    session_id: *session_id,
                };
                head.config.claude_rewind = None;
            }
            crate::RuntimeChange::PromptCacheKey(key) => {
                head.config.runtime = AgentRuntime::Rho {
                    prompt_cache_key: *key,
                };
            }
        },
        AgentEvent::TitleAttempted { .. } => head.title_attempted = true,
        AgentEvent::Titled { title, .. } => {
            head.title_attempted = true;
            head.generated_title = title.clone();
        }
        event if carries_notice(event) => {
            head.user_interacted = true;
            head.pending_notice = None;
        }
        AgentEvent::Retired { .. }
        | AgentEvent::Turn { .. }
        | AgentEvent::Wants { .. }
        | AgentEvent::Rewound { .. }
        | AgentEvent::ClaudeOutput { .. }
        | AgentEvent::ClaudeOutputHandedOff { .. }
        | AgentEvent::ClaudeExecAdmitted { .. }
        | AgentEvent::ExecObserved { .. }
        | AgentEvent::Failed { .. }
        | AgentEvent::Entry(_)
        | AgentEvent::Transcript { .. } => {}
    }
}

/// Opens the current-format agent store. Existing recovery savepoints
/// remain available for explicit rollback or deletion.
pub async fn prepare(db: &rho_db::RhoDb) {
    let read = db.read();
    let stored = if read.has_table("format") {
        read.open_table(FORMAT).get(&()).map(|value| value.value())
    } else {
        None
    };
    let unstamped = stored.is_none();
    if unstamped
        && ["agent_log", "agent_heads", "counters"]
            .iter()
            .any(|table| read.has_table(table))
    {
        panic!(
            "this rho agent database has no format stamp but contains agent tables; \
             this build expects {CURRENT_AGENT_DB_FORMAT}. Restore a compatible rho build \
             or remove the local rho database if you do not need the saved agents."
        );
    }
    let from = stored.as_deref().unwrap_or_default();
    let hop = format!("{from}->{CURRENT_AGENT_DB_FORMAT}");
    let needs_savepoint = from == TURNS_AGENT_DB_FORMAT
        && (!read.has_table("recovery_savepoints")
            || read.open_table(RECOVERY).get(&hop).is_none());
    drop(read);
    if needs_savepoint {
        db.persistent_savepoint(|write, id| {
            write.open_table(RECOVERY).insert(&hop, &id);
        })
        .await;
    }
    let mut write = db.write().await;
    write.init_agent_tables();
    write.commit();
}

/// Every persistent savepoint in the store, with the migration hop it
/// was recorded for when `prepare` took it.
pub async fn savepoints(db: &rho_db::RhoDb) -> Vec<(u64, Option<String>)> {
    let mut write = db.write().await;
    let ids = write.persistent_savepoints();
    if ids.is_empty() {
        return Vec::new();
    }
    let recorded = write
        .open_table(RECOVERY)
        .iter()
        .map(|(key, value)| (value.value(), key.value()))
        .collect::<HashMap<_, _>>();
    ids.into_iter()
        .map(|id| (id, recorded.get(&id).cloned()))
        .collect()
}

/// Drops the savepoints `prepare` recorded, once the migration they guard
/// has been verified and nothing will roll back to them. Returns the ids
/// dropped. A dropped savepoint stops pinning the pages the migration
/// freed, so `rho debug compact` can give them back.
pub async fn forget_savepoints(db: &rho_db::RhoDb) -> Vec<u64> {
    let mut write = db.write().await;
    let recorded = write
        .open_table(RECOVERY)
        .iter()
        .map(|(key, value)| (key.value(), value.value()))
        .collect::<Vec<_>>();
    let mut dropped = Vec::new();
    for (key, id) in recorded {
        write.delete_persistent_savepoint(id);
        write.open_table(RECOVERY).remove(&key);
        dropped.push(id);
    }
    write.commit();
    dropped
}

/// Removes every row the named agents own: their log, their journal
/// entries, their subscriptions and their usage. Keys only; no value is
/// decoded, so an agent this build can no longer read goes as well as
/// any other. Returns how many log rows went, per agent.
pub async fn delete_agents(db: &rho_db::RhoDb, agents: &[AgentId]) -> Vec<(AgentId, usize)> {
    let doomed = agents.iter().copied().collect::<BTreeSet<_>>();
    let mut write = db.write().await;
    let mut deleted = Vec::new();
    for &agent_id in &doomed {
        let mut log = write.open_table(AGENT_LOG);
        let keys = log
            .range((agent_id, 0)..=(agent_id, u64::MAX))
            .map(|(key, _)| key.value())
            .collect::<Vec<_>>();
        for key in &keys {
            log.remove(key);
        }
        drop(log);
        write.open_table(AGENT_HEADS).remove(&agent_id);
        write.open_table(native::NATIVE_CURSORS).remove(&agent_id);
        deleted.push((agent_id, keys.len()));

        let mut usage = write.open_table(AGENT_USAGE_BUCKETS);
        let keys = usage
            .range(
                AgentUsageKey {
                    agent_id,
                    bucket_start_ms: 0,
                }..=AgentUsageKey {
                    agent_id,
                    bucket_start_ms: u64::MAX,
                },
            )
            .map(|(key, _)| key.value())
            .collect::<Vec<_>>();
        for key in &keys {
            usage.remove(key);
        }
        drop(usage);
        write.open_table(AGENT_USAGE_TOTALS).remove(&agent_id);
    }

    let mut journal = write.open_table(JOURNAL);
    let seqs = journal
        .iter()
        .filter(|(_, row)| doomed.contains(&row.value().0))
        .map(|(seq, _)| seq.value())
        .collect::<Vec<_>>();
    for seq in &seqs {
        journal.remove(seq);
    }
    drop(journal);

    let mut subscriptions = write.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
    let keys = subscriptions
        .iter()
        .map(|(key, _)| key.value())
        .filter(|key| doomed.contains(&key.target) || doomed.contains(&key.subscriber))
        .collect::<Vec<_>>();
    for key in &keys {
        subscriptions.remove(key);
    }
    drop(subscriptions);
    write.commit();
    deleted
}

/// Drops every persistent savepoint `prepare` did not record: leftovers
/// of older builds, each pinning every page freed since it was taken.
/// Returns the ids dropped.
pub async fn drop_stale_savepoints(db: &rho_db::RhoDb) -> Vec<u64> {
    let stale = savepoints(db)
        .await
        .into_iter()
        .filter_map(|(id, hop)| hop.is_none().then_some(id))
        .collect::<Vec<_>>();
    if stale.is_empty() {
        return stale;
    }
    let mut write = db.write().await;
    for id in &stale {
        write.delete_persistent_savepoint(*id);
    }
    write.commit();
    stale
}

/// Puts the store back as it was before its last migration, from the
/// savepoint `prepare` took, and drops that savepoint. Returns the hop
/// undone. Nothing else may have the store open.
pub async fn rollback(db: &rho_db::RhoDb) -> anyhow::Result<String> {
    let mut write = db.write().await;
    let recorded = if write.persistent_savepoints().is_empty() {
        Vec::new()
    } else {
        write
            .open_table(RECOVERY)
            .iter()
            .map(|(key, value)| (key.value(), value.value()))
            .collect::<Vec<_>>()
    };
    let Some((hop, id)) = recorded.into_iter().max_by_key(|(_, id)| *id) else {
        anyhow::bail!("no migration savepoint is recorded in this store");
    };
    anyhow::ensure!(
        write.restore_persistent_savepoint(id),
        "savepoint {id} for {hop} is no longer in the store"
    );
    write.delete_persistent_savepoint(id);
    write.commit();
    Ok(hop)
}

fn assert_agent_db_format(write: &mut WriteTxn) {
    let stored = write.open_table(FORMAT).get(&()).map(|value| value.value());
    match stored.as_deref() {
        None => {}
        Some(CURRENT_AGENT_DB_FORMAT) => return,
        Some(TURNS_AGENT_DB_FORMAT) => conversation_migration::migrate(write),
        Some(other) => panic!(
            "this rho agent database was written by an older or different rho version \
             (database format {other}, this build expects {CURRENT_AGENT_DB_FORMAT}). \
             Restore a compatible rho build, or remove \
             the local rho database if you do not need the saved agents."
        ),
    }
    write
        .open_table(FORMAT)
        .insert(&(), &CURRENT_AGENT_DB_FORMAT.to_owned());
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
pub(crate) mod tests;
