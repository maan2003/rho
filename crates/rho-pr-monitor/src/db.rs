use redb::TableDefinition;
use rho_agent::db::AgentId;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

const PR_WATCHES: TableDefinition<String, Sen<PrWatch>> = TableDefinition::new("pr_watches");
const PR_FEEDBACK: TableDefinition<String, Sen<FeedbackRecord>> =
    TableDefinition::new("pr_feedback");

#[derive(Clone, Debug, Encode, Decode)]
pub struct PrWatch {
    pub generation: u64,
    pub repository_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub subscriber: AgentId,
    pub approved_review_bots: Vec<String>,
    pub seen_feedback: Vec<String>,
    pub ci_fingerprint: String,
    pub pr_state: String,
    pub ready: bool,
    pub last_error: Option<String>,
    pub consecutive_errors: u32,
    pub retry_after_ms: u64,
    pub active: bool,
}

impl PrWatch {
    pub fn key(&self) -> String {
        format!("{}:{}", self.repository_id, self.number)
    }
}

#[derive(Clone, Debug, Encode, Decode)]
pub struct FeedbackRecord {
    pub watch_key: String,
    pub subscriber: AgentId,
    pub generation: u64,
    pub surface: String,
    pub comment_id: u64,
    reply: Option<ReplyState>,
}

impl FeedbackRecord {
    pub fn new(
        watch_key: String,
        subscriber: AgentId,
        generation: u64,
        surface: String,
        comment_id: u64,
    ) -> Self {
        Self {
            watch_key,
            subscriber,
            generation,
            surface,
            comment_id,
            reply: None,
        }
    }
}

#[derive(Clone, Debug, Encode, Decode)]
#[allow(dead_code)] // Retained to decode feedback records written by the former subscription flow.
enum ReplyState {
    Reserved { marker: String, started_at_ms: u64 },
    Posted { marker: String, url: String },
}

pub trait PrMonitorReadTxnExt {
    fn list_pr_watches(&self) -> Vec<PrWatch>;
    fn get_pr_watch(&self, key: &str) -> Option<PrWatch>;
}

pub trait PrMonitorWriteTxnExt {
    fn register_pr_watch(
        &mut self,
        watch: &PrWatch,
        replay_existing: bool,
    ) -> anyhow::Result<PrWatch>;
    fn update_pr_watch_state(&mut self, watch: &PrWatch) -> bool;
    fn set_feedback_record(&mut self, event_id: &str, record: &FeedbackRecord) -> bool;
    fn remove_pr_watch(&mut self, key: &str, subscriber: AgentId, generation: u64) -> bool;
}

impl PrMonitorReadTxnExt for ReadTxn {
    fn list_pr_watches(&self) -> Vec<PrWatch> {
        self.open_table(PR_WATCHES)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    fn get_pr_watch(&self, key: &str) -> Option<PrWatch> {
        self.open_table(PR_WATCHES)
            .get(&key.to_owned())
            .map(|value| value.value().into_owned())
    }
}

impl PrMonitorWriteTxnExt for WriteTxn {
    fn register_pr_watch(
        &mut self,
        watch: &PrWatch,
        replay_existing: bool,
    ) -> anyhow::Result<PrWatch> {
        let mut watches = self.open_table(PR_WATCHES);
        let mut watch = watch.clone();
        if let Some(value) = watches.get(&watch.key()) {
            let current = value.value().into_owned();
            drop(value);
            anyhow::ensure!(
                current.subscriber == watch.subscriber,
                "pull request is already subscribed by another Engineer"
            );
            if !replay_existing {
                watch.seen_feedback = current.seen_feedback;
            }
        } else {
            let active_count = watches
                .iter()
                .filter(|(_, value)| value.value().into_owned().active)
                .count();
            anyhow::ensure!(active_count < 16, "active PR watch limit (16) reached");
        }
        watches.insert(&watch.key(), SenValue::borrowed(&watch));
        Ok(watch)
    }

    fn update_pr_watch_state(&mut self, watch: &PrWatch) -> bool {
        let mut watches = self.open_table(PR_WATCHES);
        let mut watch = watch.clone();
        let Some(value) = watches.get(&watch.key()) else {
            return false;
        };
        let current = value.value().into_owned();
        drop(value);
        if current.generation != watch.generation {
            return false;
        }
        watch.subscriber = current.subscriber;
        watch.approved_review_bots = current.approved_review_bots;
        watches.insert(&watch.key(), SenValue::borrowed(&watch));
        true
    }

    fn set_feedback_record(&mut self, event_id: &str, record: &FeedbackRecord) -> bool {
        let watches = self.open_table(PR_WATCHES);
        let current = watches
            .get(&record.watch_key)
            .map(|value| value.value().into_owned());
        if !current.is_some_and(|watch| {
            watch.subscriber == record.subscriber && watch.generation == record.generation
        }) {
            return false;
        }
        drop(watches);
        let mut feedback = self.open_table(PR_FEEDBACK);
        let mut record = record.clone();
        if let Some(existing) = feedback
            .get(&event_id.to_owned())
            .map(|value| value.value().into_owned())
            && existing.subscriber == record.subscriber
            && existing.generation == record.generation
        {
            record.reply = existing.reply;
        }
        feedback.insert(&event_id.to_owned(), SenValue::borrowed(&record));
        true
    }

    fn remove_pr_watch(&mut self, key: &str, subscriber: AgentId, generation: u64) -> bool {
        let mut watches = self.open_table(PR_WATCHES);
        let Some(value) = watches.get(&key.to_owned()) else {
            return false;
        };
        let watch = value.value().into_owned();
        drop(value);
        if watch.subscriber != subscriber || watch.generation != generation {
            return false;
        }
        watches.remove(&key.to_owned());
        true
    }
}

#[cfg(test)]
mod tests {
    use rho_agent::db::{AgentId, AgentIdDomain};
    use rho_db::RhoDb;

    use super::*;

    #[tokio::test]
    async fn watch_and_feedback_record_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let subscriber = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let watch = PrWatch {
            generation: 10,
            repository_id: 42,
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
            url: "https://github.com/acme/widgets/pull/7".into(),
            subscriber,
            approved_review_bots: vec!["reviewer[bot]".into()],
            seen_feedback: vec!["issue:1:v1".into()],
            ci_fingerprint: "ci".into(),
            pr_state: "open".into(),
            ready: false,
            last_error: None,
            consecutive_errors: 0,
            retry_after_ms: 0,
            active: true,
        };
        let record = FeedbackRecord::new(
            watch.key(),
            subscriber,
            watch.generation,
            "inline".into(),
            9,
        );
        let mut write = db.write().await;
        write.register_pr_watch(&watch, false).unwrap();
        assert!(write.set_feedback_record("inline:9:v1", &record));
        write.commit();

        let stored = db.read().get_pr_watch(&watch.key()).unwrap();
        assert_eq!(stored.subscriber, subscriber);
        assert_eq!(stored.approved_review_bots, vec!["reviewer[bot]"]);
        assert_eq!(stored.seen_feedback, vec!["issue:1:v1"]);
        let mut stale_poll = watch.clone();
        stale_poll.approved_review_bots.clear();
        stale_poll.ci_fingerprint = "new-ci".into();
        let mut write = db.write().await;
        assert!(write.update_pr_watch_state(&stale_poll));
        write.commit();
        let stored = db.read().get_pr_watch(&watch.key()).unwrap();
        assert_eq!(stored.approved_review_bots, vec!["reviewer[bot]"]);
        assert_eq!(stored.ci_fingerprint, "new-ci");

        let other = AgentId::from_counter(2, &AgentIdDomain(0)).unwrap();
        let mut conflicting = stored.clone();
        conflicting.subscriber = other;
        let mut write = db.write().await;
        assert!(write.register_pr_watch(&conflicting, false).is_err());
        write.commit();

        let mut renewed = stored;
        renewed.generation = 11;
        let mut write = db.write().await;
        write.register_pr_watch(&renewed, false).unwrap();
        assert!(!write.update_pr_watch_state(&stale_poll));
        assert!(!write.remove_pr_watch(&watch.key(), subscriber, watch.generation));
        assert!(write.remove_pr_watch(&watch.key(), subscriber, renewed.generation));
        assert!(!write.update_pr_watch_state(&stale_poll));
        write.commit();
        assert!(db.read().get_pr_watch(&watch.key()).is_none());
    }
}
