//! The agent host's redb tables beside the workset store ([`crate::store`]):
//! id counters, the machine seed, usage and quota, the Claude account, and
//! the format stamp with its migration savepoints.

use std::collections::{BTreeMap, HashMap};

use redb::TableDefinition;
use redb_derive::{Key, Value as RedbValue};
use rho_agent_types::{AgentId, AgentIdDomain, UnixMs};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use crate::log::{AgentUsageBucket, AgentUsageModel};
use crate::record::Runtime;
use crate::store::StoreWrite as _;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
/// Singleton row holding this database's random machine seed (see
/// [`PrefixIdDomain::machine_seed`]), generated once at init.
const MACHINE: TableDefinition<u8, u64> = TableDefinition::new("machine");
const MACHINE_SEED_KEY: u8 = 0;
pub(crate) const FORMAT: TableDefinition<(), String> = TableDefinition::new("format");
/// The redb savepoint taken before a migration, by the hop it guards
/// (`from->to`), so `rho debug rollback` can put the store back.
const RECOVERY: TableDefinition<String, u64> = TableDefinition::new("recovery_savepoints");
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

const AGENT_USAGE_BUCKETS: TableDefinition<AgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_agent_time");
const AGENT_USAGE_TOTALS: TableDefinition<AgentId, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_totals");
const GLOBAL_AGENT_USAGE: TableDefinition<GlobalAgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_time_provider");
/// The Claude account every agent runs on. One row: the account is global,
/// and switching it moves every agent at its next turn.
const CLAUDE_ACCOUNT: TableDefinition<(), String> = TableDefinition::new("claude_account");
/// The stamp of the workset store
/// ([ARCH-agent-host](../../specs/ARCH-agent-host.md)).
const CURRENT_AGENT_DB_FORMAT: &str = "7c2a5f9d";
/// One hop: from a format, to the next, by this rewrite.
type Migration = (&'static str, &'static str, fn(&mut WriteTxn));
/// The hops a store takes to the current format, one build at a time,
/// each from a savepoint `rho debug rollback` can put it back to. Oldest
/// first. Existing records retain their layout; seed the new projection and
/// gate downgrade before older hosts encounter new event variants.
const MIGRATIONS: &[Migration] = &[
    ("e41b7c2a", "a8f02c31", crate::store::migrate_mail),
    // Old postings retain their encoding; only the new Direct kind needs
    // the newer reader.
    ("a8f02c31", "b5082aa5", |_| {}),
    // Existing rows decode unchanged. An old host cannot decode MailWritten.
    ("b5082aa5", "d8e4c62f", |_| {}),
    // Existing mail rows decode unchanged. Older hosts cannot read a new
    // live revision or a stop with no agent.
    ("d8e4c62f", "9f7265ce", |_| {}),
    (
        "9f7265ce",
        CURRENT_AGENT_DB_FORMAT,
        crate::store::mail::migrate_pings,
    ),
];
const QUOTA_RESET_JITTER_SECONDS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct QuotaModel(u8);

impl QuotaModel {
    pub const GPT: Self = Self(1);
    /// Claude's weekly window for Fable alone.
    pub const FABLE: Self = Self(2);
    /// Claude's weekly window over every model; the name is from when Opus
    /// was the model that counted.
    pub const OPUS: Self = Self(3);
    /// Claude's five-hour session window.
    pub const SESSION: Self = Self(4);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::SESSION => "session",
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
        .get(key)
        .map(|value| value.value().into_owned())
        .unwrap_or_else(|| AgentUsageBucket {
            bucket_start_ms: bucket.bucket_start_ms,
            ..AgentUsageBucket::default()
        });
    merged.add(bucket);
    table.insert(key, SenValue::borrowed(&merged));
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

/// What a runtime's usage is billed as.
pub fn usage_model_of(runtime: &Runtime) -> AgentUsageModel {
    match runtime {
        Runtime::Native { model, .. } => match model {
            crate::inference::config::InferenceModel::Astra => AgentUsageModel::ASTRA,
            crate::inference::config::InferenceModel::Terra => AgentUsageModel::TERRA,
            crate::inference::config::InferenceModel::Sol => AgentUsageModel::GPT,
            crate::inference::config::InferenceModel::Hydrogen => AgentUsageModel::HYDROGEN,
        },
        Runtime::Claude { model, .. } => match model {
            rho_claude::Model::Opus => AgentUsageModel::OPUS,
            rho_claude::Model::Fable | rho_claude::Model::Sonnet | rho_claude::Model::Haiku => {
                AgentUsageModel::FABLE
            }
        },
    }
}

pub trait AgentReadTxnExt {
    /// This database's machine seed: the [`AgentIdDomain`] of its ids.
    fn machine_seed(&self) -> u64;
    fn last_agent_counter(&self) -> u64;
    fn quota_observations(&self, model: QuotaModel, since: UnixMs) -> Vec<QuotaObservationRecord>;
    fn agent_usage(&self, agent_id: AgentId, since: UnixMs) -> Vec<AgentUsageBucket>;
    fn agent_usage_total(&self, agent_id: AgentId) -> AgentUsageBucket;
    fn global_agent_usage(&self, since: UnixMs) -> Vec<(AgentUsageModel, AgentUsageBucket)>;
    /// The Claude account agents run on.
    fn claude_account(&self) -> String;
}

pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);
    fn alloc_agent_id(&mut self) -> AgentId;
    /// Whether the observation was new enough to keep.
    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool;
    fn set_claude_account(&mut self, account: &str);
    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket);
    /// Forget an agent's usage, with its workset. Global totals stay.
    fn delete_agent_usage(&mut self, agent_id: AgentId);
}

impl AgentReadTxnExt for ReadTxn {
    fn machine_seed(&self) -> u64 {
        self.open_table(MACHINE)
            .get(MACHINE_SEED_KEY)
            .expect("machine seed missing; init_agent_tables must run first")
            .value()
    }

    fn last_agent_counter(&self) -> u64 {
        self.open_table(COUNTERS)
            .get(CounterKey::LAST_AGENT_ID)
            .map(|counter| counter.value())
            .unwrap_or(0)
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
            .get(agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default()
    }

    fn claude_account(&self) -> String {
        self.open_table(CLAUDE_ACCOUNT)
            .get(())
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
        self.open_table(QUOTA_OBSERVATIONS);
        self.open_table(AGENT_USAGE_BUCKETS);
        self.open_table(AGENT_USAGE_TOTALS);
        self.open_table(GLOBAL_AGENT_USAGE);
        self.open_table(CLAUDE_ACCOUNT);
        self.init_store_tables();
        let mut machine = self.open_table(MACHINE);
        if machine.get(MACHINE_SEED_KEY).is_none() {
            machine.insert(MACHINE_SEED_KEY, rand::random::<u64>());
        }
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
    }

    fn set_claude_account(&mut self, account: &str) {
        self.open_table(CLAUDE_ACCOUNT)
            .insert((), account.to_owned());
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
            let head = self.agent_head(agent_id).expect("usage agent missing");
            bucket.model = usage_model_of(&head.spec.runtime);
        }
        let key = AgentUsageKey {
            agent_id,
            bucket_start_ms: bucket.bucket_start_ms,
        };
        let mut buckets = self.open_table(AGENT_USAGE_BUCKETS);
        let mut merged = buckets
            .get(key)
            .map(|value| value.value().into_owned())
            .unwrap_or_else(|| AgentUsageBucket {
                bucket_start_ms: bucket.bucket_start_ms,
                ..AgentUsageBucket::default()
            });
        merged.add(&bucket);
        buckets.insert(key, SenValue::borrowed(&merged));
        drop(buckets);

        let mut totals = self.open_table(AGENT_USAGE_TOTALS);
        let mut total = totals
            .get(agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default();
        total.add(&bucket);
        total.bucket_start_ms = 0;
        totals.insert(agent_id, SenValue::borrowed(&total));
        drop(totals);

        add_global_agent_usage(self, bucket.model, &bucket);
    }

    fn delete_agent_usage(&mut self, agent_id: AgentId) {
        let mut usage = self.open_table(AGENT_USAGE_BUCKETS);
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
        self.open_table(AGENT_USAGE_TOTALS).remove(agent_id);
    }
}

pub async fn prepare(db: &rho_db::RhoDb) {
    let read = db.read();
    let stored = if read.has_table("format") {
        read.open_table(FORMAT).get(()).map(|value| value.value())
    } else {
        None
    };
    let unstamped = stored.is_none();
    if unstamped
        && ["agent_log", "agent_heads", "counters", "workset_log"]
            .iter()
            .any(|table| read.has_table(table))
    {
        panic!(
            "this rho agent database has no format stamp but contains agent tables; \
             this build expects {CURRENT_AGENT_DB_FORMAT}. Restore a compatible rho build \
             or remove the local rho database if you do not need the saved agents."
        );
    }
    drop(read);
    let mut stored = stored;
    for (from, to, migrate) in MIGRATIONS {
        if stored.as_deref() != Some(from) {
            continue;
        }
        // `rho debug rollback` puts the store back from this savepoint.
        let hop = format!("{from}->{to}");
        db.persistent_savepoint(|write, id| {
            write.open_table(RECOVERY).insert(hop, id);
        })
        .await;
        let mut write = db.write().await;
        migrate(&mut write);
        write.open_table(FORMAT).insert((), to.to_string());
        write.commit();
        stored = Some(to.to_string());
    }
    // A savepoint for a hop this build no longer knows guards nothing:
    // rolling back to it would leave a format this build cannot open, and
    // while it stands it pins every page the migration freed.
    let known = MIGRATIONS
        .iter()
        .map(|(from, to, _)| format!("{from}->{to}"))
        .collect::<Vec<_>>();
    let mut write = db.write().await;
    let stale = write
        .open_table(RECOVERY)
        .iter()
        .map(|(key, value)| (key.value(), value.value()))
        .filter(|(hop, _)| !known.contains(hop))
        .collect::<Vec<_>>();
    for (hop, id) in stale {
        write.delete_persistent_savepoint(id);
        write.open_table(RECOVERY).remove(&hop);
    }
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
    let stored = write.open_table(FORMAT).get(()).map(|value| value.value());
    match stored.as_deref() {
        None => {}
        Some(CURRENT_AGENT_DB_FORMAT) => return,
        Some(other) => panic!(
            "this rho agent database was written by an older or different rho version \
             (database format {other}, this build expects {CURRENT_AGENT_DB_FORMAT}). \
             Restore a compatible rho build, or remove \
             the local rho database if you do not need the saved agents."
        ),
    }
    write
        .open_table(FORMAT)
        .insert((), CURRENT_AGENT_DB_FORMAT.to_owned());
}

fn next_counter(write: &mut WriteTxn, key: CounterKey) -> u64 {
    let mut counters = write.open_table(COUNTERS);
    let next = counters.get(key).map(|value| value.value()).unwrap_or(0) + 1;
    counters.insert(key, next);
    next
}

fn machine_seed(write: &mut WriteTxn) -> u64 {
    write
        .open_table(MACHINE)
        .get(MACHINE_SEED_KEY)
        .expect("machine seed missing; init_agent_tables must run first")
        .value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mail_upgrade_preserves_names_ids_and_projects_the_first_join() {
        use rho_agent_types::{Address, ContentPart};

        use crate::store::{MailRead as _, MailWrite as _, NewMessage};

        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("db"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        let first = write
            .post(
                UnixMs(4),
                None,
                NewMessage {
                    from: Address::User,
                    reply_to: None,
                    mentions: Vec::new(),
                    content: vec![ContentPart::Text {
                        text: "Legacy channel".into(),
                    }],
                },
                Some(77),
            )
            .unwrap();
        let later = write
            .post(
                UnixMs(5),
                Some(first),
                NewMessage {
                    from: Address::Agent(agent),
                    reply_to: None,
                    mentions: Vec::new(),
                    content: vec![ContentPart::Text {
                        text: "Legacy body".into(),
                    }],
                },
                None,
            )
            .unwrap();
        write.join(UnixMs(6), first, agent).unwrap();
        write.join(UnixMs(7), later, agent).unwrap();
        write
            .name(UnixMs(8), first, "Renamed in old build".into())
            .unwrap();
        write
            .open_table(redb::TableDefinition::<AgentId, u64>::new(
                "chat_first_thread",
            ))
            .remove(agent);
        write.open_table(FORMAT).insert((), "e41b7c2a".to_owned());
        write.commit();
        let original = db
            .read()
            .mail_since(rho_agent_types::ChatSeq::ZERO, usize::MAX);

        prepare(&db).await;
        let read = db.read();
        assert_eq!(read.first_thread(agent), Some(first));
        assert_eq!(read.message(later).unwrap().text(), "Legacy body");
        assert_eq!(read.project_named("Renamed in old build"), Some(first));
        assert_eq!(
            read.mail_since(rho_agent_types::ChatSeq::ZERO, usize::MAX),
            original
        );
        let mut hops: Vec<_> = savepoints(&db)
            .await
            .into_iter()
            .map(|(_, hop)| hop.unwrap())
            .collect();
        hops.sort();
        assert_eq!(
            hops,
            [
                "9f7265ce->7c2a5f9d",
                "a8f02c31->b5082aa5",
                "b5082aa5->d8e4c62f",
                "d8e4c62f->9f7265ce",
                "e41b7c2a->a8f02c31"
            ]
        );
    }

    /// A savepoint recorded for a hop this build does not know goes when
    /// the store is prepared: nothing could roll back to it.
    #[tokio::test]
    async fn a_savepoint_for_a_forgotten_hop_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("db"));
        let mut write = db.write().await;
        write
            .open_table(FORMAT)
            .insert((), CURRENT_AGENT_DB_FORMAT.to_owned());
        write.commit();
        db.persistent_savepoint(|write, id| {
            write
                .open_table(RECOVERY)
                .insert("00000000->e41b7c2a".to_owned(), id);
        })
        .await;
        assert_eq!(savepoints(&db).await.len(), 1);

        prepare(&db).await;
        assert!(savepoints(&db).await.is_empty());
        assert!(rollback(&db).await.is_err());
    }
}
