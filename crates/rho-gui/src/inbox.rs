//! Client-local capture inbox.
//!
//! This store is deliberately independent of the Desk document. External
//! intake sources append an [`InboxDraft`]; consumers (including the dealer)
//! read [`InboxItem`] values and apply a [`Verdict`]. No source needs to know
//! how the GUI renders or files an item.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
#[cfg(feature = "native")]
use redb::TableDefinition;
#[cfg(feature = "native")]
use rho_db::{RhoDb, Sen, SenValue};
#[cfg(feature = "native")]
use senax_encoder::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
const ITEMS: TableDefinition<Sen<String>, Sen<InboxItem>> =
    TableDefinition::new("rho_gui_inbox_items_v2");

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "native", derive(Encode, Decode))]
#[serde(transparent)]
pub struct InboxId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "native", derive(Encode, Decode))]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Ping,
    Capture,
    Obligation,
    /// A Slack thread that owes the user an answer. Machine-owned: the Slack
    /// session appends, updates, and retires these without a user verdict.
    Slack,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "native", derive(Encode, Decode))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceReference {
    Page {
        id: String,
    },
    /// A Slack thread. `latest_ts` is the verdict key rather than part of the
    /// thread's identity: a newer reply changes it, which voids a skip and
    /// re-raises the card exactly as an agent's reply does.
    SlackThread {
        workspace: String,
        channel: String,
        thread_ts: String,
        latest_ts: String,
    },
    DeskNode {
        host: u32,
        node_id: NodeIdentity,
    },
    External {
        source: String,
        reference: String,
    },
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "native", derive(Encode, Decode))]
pub struct NodeIdentity {
    pub replica_id: u16,
    pub counter: u64,
}

impl From<rho_desk::NodeId> for NodeIdentity {
    fn from(id: rho_desk::NodeId) -> Self {
        Self {
            replica_id: id.replica_id,
            counter: id.counter,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "native", derive(Encode, Decode))]
pub struct CapturedContext {
    pub host: Option<String>,
    pub room: Option<String>,
    pub focused_surface: String,
}

/// Input accepted from keyboard, browser, or a future external intake source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxDraft {
    pub kind: InboxKind,
    pub text: String,
    pub source: SourceReference,
    pub context: CapturedContext,
    pub waiting_on: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "native", derive(Encode, Decode))]
pub struct InboxItem {
    pub id: InboxId,
    pub kind: InboxKind,
    pub text: String,
    pub source: SourceReference,
    pub context: CapturedContext,
    pub captured_at_ms: i64,
    #[serde(default)]
    #[cfg_attr(feature = "native", senax(default))]
    pub deferred_until_ms: Option<i64>,
    #[serde(default)]
    #[cfg_attr(feature = "native", senax(default))]
    pub resurfacing_count: u32,
    #[serde(default)]
    #[cfg_attr(feature = "native", senax(default))]
    pub waiting_on: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Filed,
    Discarded,
    Deferred { until_ms: i64 },
}

/// Small read/verdict API shared by the GUI and dealer.
pub struct InboxStore {
    #[cfg(feature = "native")]
    db: Option<RhoDb>,
    items: Vec<InboxItem>,
}

impl InboxStore {
    #[cfg(test)]
    pub(crate) fn set_captured_at_for_test(&mut self, id: &InboxId, captured_at_ms: i64) {
        self.items
            .iter_mut()
            .find(|item| &item.id == id)
            .expect("test inbox item")
            .captured_at_ms = captured_at_ms;
    }

    #[cfg(feature = "native")]
    pub fn open_default() -> anyhow::Result<Self> {
        let base = dirs::state_dir().context("state directory not available")?;
        Self::open(base.join("rho/inbox.redb"))
    }

    #[cfg(not(feature = "native"))]
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self { items: Vec::new() })
    }

    #[cfg(feature = "native")]
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let db = RhoDb::open(path.into());
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write.open_table(ITEMS);
            write.commit();
        });
        let mut items = {
            let read = db.read();
            let table = read.open_table(ITEMS);
            table
                .iter()
                .map(|(_, value)| value.value().into_owned())
                .collect::<Vec<_>>()
        };
        items.sort_by_key(|item| item.captured_at_ms);
        Ok(Self {
            db: Some(db),
            items,
        })
    }

    #[cfg(not(feature = "native"))]
    pub fn open(_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self::memory())
    }

    pub fn memory() -> Self {
        Self {
            #[cfg(feature = "native")]
            db: None,
            items: Vec::new(),
        }
    }

    pub fn items(&self) -> &[InboxItem] {
        &self.items
    }

    pub fn get(&self, id: &InboxId) -> Option<&InboxItem> {
        self.items.iter().find(|item| &item.id == id)
    }

    /// Generic, source-agnostic intake seam.
    pub fn append(&mut self, draft: InboxDraft) -> anyhow::Result<InboxId> {
        let captured_at_ms = now_ms();
        let id = InboxId(format!(
            "{captured_at_ms:x}-{:x}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        self.items.push(InboxItem {
            id: id.clone(),
            kind: draft.kind,
            text: draft.text,
            source: draft.source,
            context: draft.context,
            captured_at_ms,
            deferred_until_ms: None,
            resurfacing_count: 0,
            waiting_on: draft.waiting_on,
        });
        if let Err(error) = self.save() {
            self.items.pop();
            return Err(error);
        }
        Ok(id)
    }

    /// Replaces machine-owned item content without changing its identity or
    /// capture time (for example, when an external thread gains context).
    pub fn update(&mut self, id: &InboxId, draft: InboxDraft) -> anyhow::Result<bool> {
        let Some(index) = self.items.iter().position(|item| &item.id == id) else {
            return Ok(false);
        };
        let previous = self.items[index].clone();
        self.items[index].kind = draft.kind;
        self.items[index].text = draft.text;
        self.items[index].source = draft.source;
        self.items[index].context = draft.context;
        self.items[index].waiting_on = draft.waiting_on;
        if let Err(error) = self.save() {
            self.items[index] = previous;
            return Err(error);
        }
        Ok(true)
    }

    /// Machine-side retirement, separate from a user's semantic verdict.
    pub fn retire(&mut self, id: &InboxId) -> anyhow::Result<Option<InboxItem>> {
        self.remove(id)
    }

    /// Hides an item until `until_ms`. Call [`Self::refresh_deferred`] before
    /// projecting pending cards; that transition records the resurfacing.
    pub fn defer(&mut self, id: &InboxId, until_ms: i64) -> anyhow::Result<bool> {
        let Some(index) = self.items.iter().position(|item| &item.id == id) else {
            return Ok(false);
        };
        let previous = self.items[index].clone();
        self.items[index].deferred_until_ms = Some(until_ms);
        if let Err(error) = self.save() {
            self.items[index] = previous;
            return Err(error);
        }
        Ok(true)
    }

    /// Makes elapsed deferrals visible and counts the actual resurfacing,
    /// rather than counting a card that was dismissed before its wake time.
    pub fn refresh_deferred(&mut self, at_ms: i64) -> anyhow::Result<usize> {
        let previous = self.items.clone();
        let mut resurfaced = 0;
        for item in &mut self.items {
            if item.deferred_until_ms.is_some_and(|until| until <= at_ms) {
                item.deferred_until_ms = None;
                item.resurfacing_count = item.resurfacing_count.saturating_add(1);
                resurfaced += 1;
            }
        }
        if resurfaced > 0
            && let Err(error) = self.save()
        {
            self.items = previous;
            return Err(error);
        }
        Ok(resurfaced)
    }

    /// Retires an item after the user's explicit filing/discard decision.
    pub fn verdict(&mut self, id: &InboxId, verdict: Verdict) -> anyhow::Result<Option<InboxItem>> {
        match verdict {
            Verdict::Filed | Verdict::Discarded => self.remove(id),
            Verdict::Deferred { until_ms } => {
                self.defer(id, until_ms)?;
                Ok(self.get(id).cloned())
            }
        }
    }

    /// Restores the exact local state removed or changed by a user verdict.
    pub fn restore(&mut self, item: InboxItem) -> anyhow::Result<bool> {
        let previous = self
            .items
            .iter()
            .position(|candidate| candidate.id == item.id)
            .map(|index| self.items.remove(index));
        self.items.push(item.clone());
        self.items.sort_by_key(|item| item.captured_at_ms);
        if let Err(error) = self.save() {
            self.items.retain(|candidate| candidate.id != item.id);
            if let Some(previous) = previous {
                self.items.push(previous);
                self.items.sort_by_key(|item| item.captured_at_ms);
            }
            return Err(error);
        }
        Ok(true)
    }

    /// Pending projection input for the dealer: deferred items cost no card
    /// allocation until their wake time, while obligations remain immortal.
    pub fn pending_items(&self, at_ms: i64) -> impl Iterator<Item = &InboxItem> {
        self.items
            .iter()
            .filter(move |item| item.deferred_until_ms.is_none_or(|until| until <= at_ms))
    }

    fn remove(&mut self, id: &InboxId) -> anyhow::Result<Option<InboxItem>> {
        let Some(index) = self.items.iter().position(|item| &item.id == id) else {
            return Ok(None);
        };
        let item = self.items.remove(index);
        if let Err(error) = self.save() {
            self.items.insert(index, item);
            return Err(error);
        }
        Ok(Some(item))
    }

    fn save(&self) -> anyhow::Result<()> {
        #[cfg(not(feature = "native"))]
        {
            return Ok(());
        }
        #[cfg(feature = "native")]
        {
            #[cfg(feature = "native")]
            let Some(db) = &self.db else {
                return Ok(());
            };
            #[cfg(feature = "native")]
            futures::executor::block_on(async {
                let mut write = db.write().await;
                let mut table = write.open_table(ITEMS);
                let old_keys = table
                    .iter()
                    .map(|(key, _)| key.value().into_owned())
                    .collect::<Vec<_>>();
                for key in old_keys {
                    table.remove(SenValue::owned(key));
                }
                for item in &self.items {
                    table.insert(SenValue::borrowed(&item.id.0), SenValue::borrowed(item));
                }
                drop(table);
                write.commit();
            });
            Ok(())
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(kind: InboxKind, text: &str) -> InboxDraft {
        InboxDraft {
            kind,
            text: text.into(),
            source: SourceReference::None,
            context: CapturedContext::default(),
            waiting_on: None,
        }
    }

    #[test]
    fn persists_and_applies_verdicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.redb");
        let mut store = InboxStore::open(&path).unwrap();
        let id = store.append(draft(InboxKind::Obligation, "reply")).unwrap();
        drop(store);
        let mut store = InboxStore::open(&path).unwrap();
        assert_eq!(store.get(&id).unwrap().text, "reply");
        store.verdict(&id, Verdict::Discarded).unwrap();
        drop(store);
        assert!(InboxStore::open(path).unwrap().items().is_empty());
    }

    #[test]
    fn defer_survives_and_counts_when_it_resurfaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.redb");
        let mut store = InboxStore::open(&path).unwrap();
        let id = store.append(draft(InboxKind::Obligation, "later")).unwrap();
        store.defer(&id, 100).unwrap();
        drop(store);

        let mut reopened = InboxStore::open(path).unwrap();
        assert_eq!(reopened.get(&id).unwrap().deferred_until_ms, Some(100));
        assert_eq!(reopened.refresh_deferred(99).unwrap(), 0);
        assert_eq!(reopened.refresh_deferred(100).unwrap(), 1);
        let item = reopened.get(&id).unwrap();
        assert_eq!(item.deferred_until_ms, None);
        assert_eq!(item.resurfacing_count, 1);
    }
}
