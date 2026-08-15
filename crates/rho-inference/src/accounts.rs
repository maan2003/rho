use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use bytes::BytesMut;
use redb::{TableDefinition, TableHandle as _, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::UnixMs;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Decoder as _, Encode, Encoder as _};
use tokio::sync::{Mutex, watch};

use crate::auth_cli::{auth_namespaces, chatgpt_weekly_usage};
use crate::responses::{InferenceAuth, QuotaUpdate};

const FORMAT: TableDefinition<(), String> = TableDefinition::new("chatgpt_inference_format");
const INITIAL_FORMAT: &str = "8c93d1e4";
const CURRENT_FORMAT: &str = "75b4468b";
const SETTINGS: TableDefinition<(), Sen<SettingsRecord>> =
    TableDefinition::new("chatgpt_inference_settings");
const QUOTAS: TableDefinition<String, Sen<QuotaRecord>> =
    TableDefinition::new("chatgpt_quota_observations");
const LEGACY_QUOTAS: TableDefinition<QuotaObservationKey, LegacyQuotaValue> =
    TableDefinition::new("quota_observations_by_model_time");
const HISTORY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const RESET_SWITCH_THRESHOLD_SECONDS: i64 = 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceState {
    pub namespaces: Vec<String>,
    pub disabled_namespaces: Vec<String>,
    pub active_namespace: Option<String>,
    pub quotas: Vec<InferenceQuotaSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceQuotaSummary {
    pub auth_namespace: String,
    pub remaining_percent: u8,
    pub burn_10m: u16,
    pub burn_2h: u16,
    pub burn_1d: u16,
    pub burn_3d: u16,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceQuotaPoint {
    pub observed_at: UnixMs,
    pub remaining_percent: u8,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceQuotaSeries {
    pub auth_namespace: String,
    pub points: Vec<InferenceQuotaPoint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedAuth {
    pub(crate) auth: InferenceAuth,
    pub(crate) namespace: Option<String>,
    pub(crate) account_id: Option<String>,
}

#[derive(Clone, Debug, Default, Encode, Decode)]
struct SettingsRecord {
    current_namespace: Option<String>,
    disabled_namespaces: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
struct QuotaModel(u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct QuotaObservationKey {
    model: QuotaModel,
    observed_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum QuotaProvider {
    ChatGpt,
    Claude,
}

#[derive(Clone, Debug, Encode, Decode)]
struct QuotaObservationRecord {
    provider: QuotaProvider,
    model: QuotaModel,
    auth_namespace: Option<String>,
    observed_at: UnixMs,
    used_percent: u8,
    reset_at_unix: Option<i64>,
}

#[derive(Debug)]
struct LegacyQuotaValue;

impl redb::Value for LegacyQuotaValue {
    type SelfType<'a> = QuotaObservationRecord;
    type AsBytes<'a> = BytesMut;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(mut data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        QuotaObservationRecord::decode(&mut data).expect("decode legacy quota observation")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let mut bytes = BytesMut::new();
        value
            .encode(&mut bytes)
            .expect("encode legacy quota observation");
        bytes
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("rho-db::Sen<rho_agent::db::QuotaObservationRecord>")
    }
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct QuotaRecord {
    pub(crate) auth_namespace: String,
    pub(crate) observed_at_ms: u64,
    pub(crate) weekly_used_percent: u8,
    pub(crate) weekly_reset_at_unix: Option<i64>,
    pub(crate) routing_used_percent: u8,
    pub(crate) routing_reset_at_unix: Option<i64>,
}

#[derive(Debug, Default)]
struct Account {
    quota: Option<QuotaRecord>,
    account_id: Option<String>,
    rate_limited: bool,
}

#[derive(Debug)]
struct AccountState {
    current: Option<String>,
    disabled: BTreeSet<String>,
    configured: BTreeSet<String>,
    accounts: HashMap<String, Account>,
    history: Vec<QuotaRecord>,
    last_history_timestamp: u64,
}

#[derive(Debug)]
pub(crate) struct AccountManager {
    db: RhoDb,
    inner: Mutex<AccountState>,
    public: watch::Sender<InferenceState>,
}

impl AccountManager {
    pub(crate) async fn open(db: RhoDb) -> Self {
        let settings = load_settings(&db);
        let history = records(&db);
        let configured = auth_namespaces()
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut accounts = HashMap::<String, Account>::new();
        for namespace in &configured {
            accounts.entry(namespace.clone()).or_default();
        }
        for record in &history {
            accounts
                .entry(record.auth_namespace.clone())
                .or_default()
                .quota = Some(record.clone());
        }
        if let Some(current) = &settings.current_namespace {
            accounts.entry(current.clone()).or_default();
        }
        let mut inner = AccountState {
            current: settings.current_namespace,
            disabled: settings.disabled_namespaces.into_iter().collect(),
            configured,
            accounts,
            last_history_timestamp: history
                .iter()
                .map(|record| record.observed_at_ms)
                .max()
                .unwrap_or_default(),
            history,
        };
        inner.reconsider();
        let public = watch::Sender::new(inner.public_state());
        let mut write = db.write().await;
        store_settings(&mut write, &inner);
        write.commit();
        Self {
            db,
            inner: Mutex::new(inner),
            public,
        }
    }

    pub(crate) async fn select(&self) -> anyhow::Result<SelectedAuth> {
        let inner = self.inner.lock().await;
        let Some(namespace) = &inner.current else {
            let message = if inner.configured.is_empty() {
                "no inference authentication credentials are configured"
            } else if inner
                .configured
                .iter()
                .all(|namespace| inner.disabled.contains(namespace))
            {
                "all inference authentication accounts are disabled"
            } else {
                "rate_limit_exceeded: all enabled inference accounts are exhausted"
            };
            anyhow::bail!(message);
        };
        Ok(SelectedAuth {
            auth: InferenceAuth::named(namespace)?,
            namespace: Some(namespace.clone()),
            account_id: inner
                .accounts
                .get(namespace)
                .and_then(|account| account.account_id.clone()),
        })
    }

    pub(crate) async fn observe_quota(&self, selected: &SelectedAuth, quota: QuotaUpdate) {
        let Some(namespace) = &selected.namespace else {
            return;
        };
        let mut write = self.db.write().await;
        let mut inner = self.inner.lock().await;
        let observed_at_ms = UnixMs::now()
            .0
            .max(inner.last_history_timestamp.saturating_add(1));
        let record = QuotaRecord {
            auth_namespace: namespace.clone(),
            observed_at_ms,
            weekly_used_percent: quota.weekly_used_percent,
            weekly_reset_at_unix: quota.weekly_reset_at_unix,
            routing_used_percent: quota.routing_used_percent,
            routing_reset_at_unix: quota.routing_reset_at_unix,
        };
        let changed = inner
            .accounts
            .get(namespace)
            .and_then(|account| account.quota.as_ref())
            .is_none_or(|old| !old.same_quota(&record));
        let old_current = inner.current.clone();
        let account = inner.accounts.entry(namespace.clone()).or_default();
        account.quota = Some(record.clone());
        account.account_id = selected.account_id.clone();
        if record.effective_remaining(now_secs()) > 0.0 {
            account.rate_limited = false;
        }
        if changed {
            inner.last_history_timestamp = observed_at_ms;
            let cutoff = observed_at_ms.saturating_sub(HISTORY_RETENTION_MS);
            inner
                .history
                .retain(|sample| sample.observed_at_ms >= cutoff);
            inner.history.push(record.clone());
            store_quota(&mut write, &record);
        }
        inner.reconsider();
        let selection_changed = inner.current != old_current;
        if selection_changed {
            store_settings(&mut write, &inner);
        }
        if changed || selection_changed {
            write.commit();
        }
        self.publish(&inner);
    }

    pub(crate) async fn mark_rate_limited(&self, selected: &SelectedAuth) -> bool {
        let Some(namespace) = &selected.namespace else {
            return false;
        };
        let mut write = self.db.write().await;
        let mut inner = self.inner.lock().await;
        let old_current = inner.current.clone();
        for (name, account) in &mut inner.accounts {
            let same_namespace = name == namespace;
            let same_account = selected
                .account_id
                .as_ref()
                .is_some_and(|id| account.account_id.as_ref() == Some(id));
            if same_namespace || same_account {
                account.rate_limited = true;
            }
        }
        inner.reconsider();
        if inner.current != old_current {
            store_settings(&mut write, &inner);
            write.commit();
        }
        self.publish(&inner);
        inner.current.is_some()
    }

    pub(crate) async fn set_enabled(&self, namespace: &str, enabled: bool) {
        let namespace = namespace.trim();
        if namespace.is_empty() {
            return;
        }
        let mut write = self.db.write().await;
        let mut inner = self.inner.lock().await;
        if enabled {
            if !inner.disabled.remove(namespace) {
                return;
            }
        } else if !(inner.configured.contains(namespace)
            || inner.current.as_deref() == Some(namespace))
            || !inner.disabled.insert(namespace.to_owned())
        {
            return;
        }
        inner.reconsider();
        store_settings(&mut write, &inner);
        write.commit();
        self.publish(&inner);
    }

    pub(crate) async fn set_configured(&self, namespaces: Vec<String>) -> Vec<String> {
        let mut write = self.db.write().await;
        let mut inner = self.inner.lock().await;
        let old_current = inner.current.clone();
        inner.configured = namespaces.into_iter().collect();
        for namespace in inner.configured.clone() {
            inner.accounts.entry(namespace).or_default();
        }
        inner.reconsider();
        if inner.current != old_current {
            store_settings(&mut write, &inner);
            write.commit();
        }
        self.publish(&inner);
        inner
            .configured
            .iter()
            .filter(|namespace| !inner.disabled.contains(*namespace))
            .cloned()
            .collect()
    }

    pub(crate) fn state(&self) -> InferenceState {
        self.public.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<InferenceState> {
        self.public.subscribe()
    }

    pub(crate) fn history(&self, since: UnixMs) -> Vec<InferenceQuotaSeries> {
        history(&self.db, since)
    }

    pub(crate) fn spawn_poller(self: &std::sync::Arc<Self>) {
        let weak = std::sync::Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(accounts) = weak.upgrade() else {
                    return;
                };
                accounts.poll_once().await;
                drop(accounts);
                tokio::time::sleep(std::time::Duration::from_secs(10 * 60)).await;
            }
        });
    }

    async fn poll_once(&self) {
        let namespaces = auth_namespaces().unwrap_or_default();
        let namespaces = self.set_configured(namespaces).await;
        for namespace in namespaces {
            let name = namespace.clone();
            let usage = tokio::task::spawn_blocking(move || chatgpt_weekly_usage(name)).await;
            let Ok(Ok(Some(usage))) = usage else {
                continue;
            };
            let Ok(auth) = InferenceAuth::named(&namespace) else {
                continue;
            };
            self.observe_quota(
                &SelectedAuth {
                    auth,
                    namespace: Some(namespace),
                    account_id: usage.account_id,
                },
                QuotaUpdate {
                    weekly_used_percent: usage.used_percent.clamp(0.0, 100.0).round() as u8,
                    weekly_reset_at_unix: Some(usage.reset_at_unix),
                    routing_used_percent: usage.routing_used_percent.clamp(0.0, 100.0).round()
                        as u8,
                    routing_reset_at_unix: Some(usage.routing_reset_at_unix),
                },
            )
            .await;
        }
    }

    fn publish(&self, inner: &AccountState) {
        let state = inner.public_state();
        if *self.public.borrow() != state {
            self.public.send_replace(state);
        }
    }
}

impl AccountState {
    fn reconsider(&mut self) {
        let now = now_secs();
        let current_usable = self.current.as_ref().is_some_and(|namespace| {
            !self.disabled.contains(namespace)
                && self
                    .accounts
                    .get(namespace)
                    .is_some_and(|account| account.usable(now))
        });
        let best = self.best_candidate(now);

        if !current_usable {
            self.current = best;
            return;
        }

        let Some(current) = self.current.as_ref() else {
            return;
        };
        let Some(best) = best else { return };
        if &best == current {
            return;
        }
        let current_reset = self.accounts[current]
            .quota
            .as_ref()
            .and_then(|quota| quota.upcoming_weekly_reset(now));
        let best_reset = self.accounts[&best]
            .quota
            .as_ref()
            .and_then(|quota| quota.upcoming_weekly_reset(now));
        if matches!(
            (current_reset, best_reset),
            (Some(current), Some(best))
                if current.saturating_sub(best) > RESET_SWITCH_THRESHOLD_SECONDS
        ) {
            self.current = Some(best);
        }
    }

    fn best_candidate(&self, now: i64) -> Option<String> {
        self.configured
            .iter()
            .filter(|namespace| !self.disabled.contains(*namespace))
            .filter(|namespace| {
                self.accounts
                    .get(*namespace)
                    .is_some_and(|account| account.usable(now))
            })
            .min_by(|left, right| {
                let left_reset = self.accounts[*left]
                    .quota
                    .as_ref()
                    .and_then(|quota| quota.upcoming_weekly_reset(now));
                let right_reset = self.accounts[*right]
                    .quota
                    .as_ref()
                    .and_then(|quota| quota.upcoming_weekly_reset(now));
                match (left_reset, right_reset) {
                    (Some(left_reset), Some(right_reset)) => {
                        left_reset.cmp(&right_reset).then_with(|| left.cmp(right))
                    }
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => left.cmp(right),
                }
            })
            .cloned()
    }

    fn public_state(&self) -> InferenceState {
        let mut namespaces = self.configured.iter().cloned().collect::<BTreeSet<_>>();
        namespaces.extend(self.disabled.iter().cloned());
        if let Some(current) = &self.current {
            namespaces.insert(current.clone());
        }
        InferenceState {
            namespaces: namespaces.into_iter().collect(),
            disabled_namespaces: self.disabled.iter().cloned().collect(),
            active_namespace: self.current.clone(),
            quotas: summaries(&self.history),
        }
    }
}

impl Account {
    fn usable(&self, now: i64) -> bool {
        !self.rate_limited
            && self
                .quota
                .as_ref()
                .is_none_or(|quota| quota.effective_remaining(now) > 0.0)
    }
}

impl QuotaRecord {
    fn same_quota(&self, other: &Self) -> bool {
        self.weekly_used_percent == other.weekly_used_percent
            && self.weekly_reset_at_unix == other.weekly_reset_at_unix
            && self.routing_used_percent == other.routing_used_percent
            && self.routing_reset_at_unix == other.routing_reset_at_unix
    }

    fn effective_remaining(&self, now: i64) -> f64 {
        let routing = if self.routing_reset_at_unix.is_some_and(|reset| reset <= now) {
            100.0
        } else {
            100.0 - f64::from(self.routing_used_percent)
        };
        let weekly = if self.weekly_reset_at_unix.is_some_and(|reset| reset <= now) {
            100.0
        } else {
            100.0 - f64::from(self.weekly_used_percent)
        };
        routing.min(weekly)
    }

    fn upcoming_weekly_reset(&self, now: i64) -> Option<i64> {
        self.weekly_reset_at_unix.filter(|reset| *reset > now)
    }
}

pub(crate) async fn init(db: &RhoDb) -> anyhow::Result<()> {
    let (current, has_legacy_quota) = {
        let read = db.read();
        (
            read.has_table(FORMAT.name())
                .then(|| read.open_table(FORMAT).get(&()).map(|value| value.value()))
                .flatten(),
            read.has_table(LEGACY_QUOTAS.name()),
        )
    };
    if current.as_deref() == Some(CURRENT_FORMAT) {
        return Ok(());
    }
    let mut write = db.write().await;
    write.open_table(SETTINGS);
    write.open_table(QUOTAS);
    let previous = current.unwrap_or_else(|| INITIAL_FORMAT.to_owned());
    anyhow::ensure!(
        previous == INITIAL_FORMAT,
        "unsupported ChatGPT inference database format {previous}; expected {CURRENT_FORMAT}"
    );
    if has_legacy_quota {
        migrate_legacy_chatgpt_quota(&mut write);
    }
    write
        .open_table(FORMAT)
        .insert(&(), CURRENT_FORMAT.to_owned());
    write.commit();
    Ok(())
}

fn migrate_legacy_chatgpt_quota(write: &mut rho_db::WriteTxn) {
    let legacy = write
        .open_table(LEGACY_QUOTAS)
        .iter()
        .filter_map(|(_, value)| {
            let record = value.value();
            (record.provider == QuotaProvider::ChatGpt && record.model == QuotaModel(1))
                .then_some(record)
        })
        .filter_map(|record| {
            Some(QuotaRecord {
                auth_namespace: record.auth_namespace?,
                observed_at_ms: record.observed_at.0,
                weekly_used_percent: record.used_percent,
                weekly_reset_at_unix: record.reset_at_unix,
                routing_used_percent: record.used_percent,
                routing_reset_at_unix: record.reset_at_unix,
            })
        })
        .collect::<Vec<_>>();
    let mut quotas = write.open_table(QUOTAS);
    for record in legacy {
        let key = format!("{:020}:{}", record.observed_at_ms, record.auth_namespace);
        quotas.insert(&key, SenValue::borrowed(&record));
    }
}

fn load_settings(db: &RhoDb) -> SettingsRecord {
    let read = db.read();
    if !read.has_table(SETTINGS.name()) {
        return SettingsRecord::default();
    }
    read.open_table(SETTINGS)
        .get(&())
        .map(|record| record.value().into_owned())
        .unwrap_or_default()
}

fn store_settings(write: &mut rho_db::WriteTxn, state: &AccountState) {
    let record = SettingsRecord {
        current_namespace: state.current.clone(),
        disabled_namespaces: state.disabled.iter().cloned().collect(),
    };
    write
        .open_table(SETTINGS)
        .insert(&(), SenValue::borrowed(&record));
}

#[cfg(test)]
async fn save_settings(db: &RhoDb, state: &AccountState) {
    let mut write = db.write().await;
    store_settings(&mut write, state);
    write.commit();
}

fn records(db: &RhoDb) -> Vec<QuotaRecord> {
    let read = db.read();
    if !read.has_table(QUOTAS.name()) {
        return Vec::new();
    }
    read.open_table(QUOTAS)
        .iter()
        .map(|(_, record)| record.value().into_owned())
        .collect()
}

fn store_quota(write: &mut rho_db::WriteTxn, record: &QuotaRecord) {
    let cutoff = record.observed_at_ms.saturating_sub(HISTORY_RETENTION_MS);
    let key = format!("{:020}:{}", record.observed_at_ms, record.auth_namespace);
    let mut table = write.open_table(QUOTAS);
    let expired = table
        .iter()
        .filter_map(|(key, value)| {
            (value.value().as_ref().observed_at_ms < cutoff).then(|| key.value())
        })
        .collect::<Vec<_>>();
    for key in expired {
        table.remove(&key);
    }
    table.insert(&key, SenValue::borrowed(record));
}

#[cfg(test)]
async fn append_quota(db: &RhoDb, record: &QuotaRecord) {
    let mut write = db.write().await;
    store_quota(&mut write, record);
    write.commit();
}

fn history(db: &RhoDb, since: UnixMs) -> Vec<InferenceQuotaSeries> {
    let mut groups = BTreeMap::<String, Vec<InferenceQuotaPoint>>::new();
    for record in records(db) {
        if record.observed_at_ms < since.0 {
            continue;
        }
        groups
            .entry(record.auth_namespace)
            .or_default()
            .push(InferenceQuotaPoint {
                observed_at: UnixMs(record.observed_at_ms),
                remaining_percent: 100u8.saturating_sub(record.weekly_used_percent),
                reset_at_unix: record.weekly_reset_at_unix,
            });
    }
    groups
        .into_iter()
        .map(|(auth_namespace, points)| InferenceQuotaSeries {
            auth_namespace,
            points,
        })
        .collect()
}

fn summaries(records: &[QuotaRecord]) -> Vec<InferenceQuotaSummary> {
    let now = UnixMs::now().0;
    let mut groups = BTreeMap::<String, Vec<&QuotaRecord>>::new();
    for record in records {
        groups
            .entry(record.auth_namespace.clone())
            .or_default()
            .push(record);
    }
    groups
        .into_iter()
        .filter_map(|(auth_namespace, samples)| {
            let latest = *samples.last()?;
            let reset_expired = latest
                .weekly_reset_at_unix
                .is_some_and(|reset| reset <= (now / 1_000) as i64);
            let burn = |duration| {
                if reset_expired {
                    0
                } else {
                    quota_burn(&samples, now, duration)
                }
            };
            Some(InferenceQuotaSummary {
                auth_namespace,
                remaining_percent: if reset_expired {
                    100
                } else {
                    100u8.saturating_sub(latest.weekly_used_percent)
                },
                burn_10m: burn(10 * 60 * 1_000),
                burn_2h: burn(2 * 60 * 60 * 1_000),
                burn_1d: burn(24 * 60 * 60 * 1_000),
                burn_3d: burn(3 * 24 * 60 * 60 * 1_000),
                reset_at_unix: if reset_expired {
                    None
                } else {
                    latest.weekly_reset_at_unix
                },
            })
        })
        .collect()
}

fn quota_burn(samples: &[&QuotaRecord], now: u64, duration_ms: u64) -> u16 {
    let cutoff = now.saturating_sub(duration_ms);
    let start = samples
        .partition_point(|sample| sample.observed_at_ms < cutoff)
        .saturating_sub(1);
    let Some((first, rest)) = samples.get(start..).and_then(|slice| slice.split_first()) else {
        return 0;
    };
    let mut epoch_start = *first;
    let mut epoch_end = *first;
    let mut burn = 0u16;
    for sample in rest {
        let same_epoch = match (epoch_end.weekly_reset_at_unix, sample.weekly_reset_at_unix) {
            (Some(left), Some(right)) => left.abs_diff(right) <= 60,
            (None, None) => true,
            _ => false,
        };
        if !same_epoch {
            burn += epoch_end
                .weekly_used_percent
                .saturating_sub(epoch_start.weekly_used_percent) as u16;
            epoch_start = sample;
        }
        epoch_end = sample;
    }
    burn + epoch_end
        .weekly_used_percent
        .saturating_sub(epoch_start.weekly_used_percent) as u16
}

fn now_secs() -> i64 {
    (UnixMs::now().0 / 1_000).try_into().unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(namespace: &str, used: u8, reset: i64) -> QuotaRecord {
        QuotaRecord {
            auth_namespace: namespace.to_owned(),
            observed_at_ms: 1,
            weekly_used_percent: used,
            weekly_reset_at_unix: Some(reset),
            routing_used_percent: used,
            routing_reset_at_unix: Some(reset),
        }
    }

    #[test]
    fn selection_is_sticky_with_one_hour_reset_hysteresis() {
        let now = now_secs();
        let mut state = AccountState {
            current: Some("current".to_owned()),
            disabled: BTreeSet::new(),
            configured: ["current".to_owned(), "earlier".to_owned()].into(),
            accounts: HashMap::from([
                (
                    "current".to_owned(),
                    Account {
                        quota: Some(quota("current", 10, now + 3 * 60 * 60)),
                        ..Account::default()
                    },
                ),
                (
                    "earlier".to_owned(),
                    Account {
                        quota: Some(quota("earlier", 10, now + 2 * 60 * 60 + 30 * 60)),
                        ..Account::default()
                    },
                ),
            ]),
            history: Vec::new(),
            last_history_timestamp: 0,
        };
        state.reconsider();
        assert_eq!(state.current.as_deref(), Some("current"));

        state.accounts.get_mut("earlier").unwrap().quota =
            Some(quota("earlier", 10, now + 60 * 60));
        state.reconsider();
        assert_eq!(state.current.as_deref(), Some("earlier"));
    }

    #[test]
    fn disabled_or_rate_limited_current_selects_replacement() {
        let mut state = AccountState {
            current: Some("current".to_owned()),
            disabled: ["current".to_owned()].into(),
            configured: ["current".to_owned(), "next".to_owned()].into(),
            accounts: HashMap::from([
                ("current".to_owned(), Account::default()),
                ("next".to_owned(), Account::default()),
            ]),
            history: Vec::new(),
            last_history_timestamp: 0,
        };
        state.reconsider();
        assert_eq!(state.current.as_deref(), Some("next"));

        state.accounts.get_mut("next").unwrap().rate_limited = true;
        state.reconsider();
        assert_eq!(state.current, None);
    }

    #[tokio::test]
    async fn settings_and_provider_quota_are_owned_by_inference_tables() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        init(&db).await.unwrap();
        let mut state = AccountState {
            current: Some("work".to_owned()),
            disabled: ["personal".to_owned()].into(),
            configured: ["work".to_owned()].into(),
            accounts: HashMap::new(),
            history: Vec::new(),
            last_history_timestamp: 0,
        };
        save_settings(&db, &state).await;
        let loaded = load_settings(&db);
        assert_eq!(loaded.current_namespace.as_deref(), Some("work"));
        assert_eq!(loaded.disabled_namespaces, ["personal"]);

        let record = quota("work", 25, i64::MAX);
        append_quota(&db, &record).await;
        state.history.push(record);
        assert_eq!(
            state.public_state().active_namespace.as_deref(),
            Some("work")
        );
        assert_eq!(state.public_state().quotas[0].remaining_percent, 75);
        assert_eq!(records(&db)[0].routing_used_percent, 25);
        assert_eq!(history(&db, UnixMs(0))[0].points[0].remaining_percent, 75);
    }

    #[tokio::test]
    async fn manager_persists_selection_and_deduplicates_stream_quota() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        init(&db).await.unwrap();
        save_settings(
            &db,
            &AccountState {
                current: Some("work".to_owned()),
                disabled: BTreeSet::new(),
                configured: BTreeSet::new(),
                accounts: HashMap::new(),
                history: Vec::new(),
                last_history_timestamp: 0,
            },
        )
        .await;
        let manager = AccountManager::open(db.clone()).await;
        let selected = manager.select().await.unwrap();
        let quota = QuotaUpdate {
            weekly_used_percent: 25,
            weekly_reset_at_unix: Some(i64::MAX),
            routing_used_percent: 30,
            routing_reset_at_unix: Some(i64::MAX),
        };

        manager.observe_quota(&selected, quota).await;
        manager.observe_quota(&selected, quota).await;

        assert_eq!(records(&db).len(), 1);
        drop(manager);
        let manager = AccountManager::open(db).await;
        assert_eq!(
            manager.select().await.unwrap().namespace.as_deref(),
            Some("work")
        );
    }

    #[tokio::test]
    async fn manager_only_disables_configured_namespaces() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        init(&db).await.unwrap();
        let manager = AccountManager::open(db).await;
        manager.set_configured(vec!["work".to_owned()]).await;

        manager.set_enabled("invented", false).await;
        assert!(manager.state().disabled_namespaces.is_empty());

        manager.set_enabled("work", false).await;
        assert_eq!(manager.state().disabled_namespaces, ["work"]);

        manager.set_configured(Vec::new()).await;
        manager.set_enabled("work", true).await;
        assert!(manager.state().disabled_namespaces.is_empty());
    }

    #[tokio::test]
    async fn missing_persisted_current_can_be_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        init(&db).await.unwrap();
        let inner = AccountState {
            current: Some("missing".to_owned()),
            disabled: BTreeSet::new(),
            configured: ["available".to_owned()].into(),
            accounts: HashMap::from([
                ("missing".to_owned(), Account::default()),
                ("available".to_owned(), Account::default()),
            ]),
            history: Vec::new(),
            last_history_timestamp: 0,
        };
        let manager = AccountManager {
            db,
            public: watch::Sender::new(inner.public_state()),
            inner: Mutex::new(inner),
        };

        manager.set_enabled("missing", false).await;

        assert_eq!(
            manager.select().await.unwrap().namespace.as_deref(),
            Some("available")
        );
        assert_eq!(manager.state().disabled_namespaces, ["missing"]);
    }

    #[tokio::test]
    async fn migrates_scoped_legacy_chatgpt_quota_once() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write
            .open_table(FORMAT)
            .insert(&(), INITIAL_FORMAT.to_owned());
        let record = QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel(1),
            auth_namespace: Some("work".to_owned()),
            observed_at: UnixMs(123),
            used_percent: 42,
            reset_at_unix: Some(456),
        };
        write.open_table(LEGACY_QUOTAS).insert(
            &QuotaObservationKey {
                model: QuotaModel(1),
                observed_at: 123,
            },
            &record,
        );
        write.commit();
        init(&db).await.unwrap();

        let migrated = records(&db);
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated[0].auth_namespace, "work");
        assert_eq!(migrated[0].weekly_used_percent, 42);
        assert_eq!(migrated[0].routing_used_percent, 42);
        assert_eq!(load_settings(&db).current_namespace, None);
    }

    #[test]
    fn quota_burn_ignores_usage_and_reset_target_jitter() {
        let samples = [
            (0, 17, 1_000),
            (100, 15, 1_001),
            (200, 17, 999),
            (300, 16, 1_000),
            (400, 17, 1_002),
        ]
        .map(
            |(observed_at_ms, weekly_used_percent, weekly_reset_at_unix)| QuotaRecord {
                auth_namespace: "work".to_owned(),
                observed_at_ms,
                weekly_used_percent,
                weekly_reset_at_unix: Some(weekly_reset_at_unix),
                routing_used_percent: weekly_used_percent,
                routing_reset_at_unix: Some(weekly_reset_at_unix),
            },
        );
        let samples = samples.iter().collect::<Vec<_>>();

        assert_eq!(quota_burn(&samples, 400, 1_000), 0);
    }
}
