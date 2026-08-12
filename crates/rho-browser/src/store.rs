use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use prefix_id::{PrefixId, PrefixIdDomain};
use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};
use url::Url;

const WEB_MACHINE: TableDefinition<u8, u64> = TableDefinition::new("rho_web_machine_v2");
const WEB_COUNTERS: TableDefinition<u8, u64> = TableDefinition::new("rho_web_counters_v2");
const WEB_WINDOWS: TableDefinition<Sen<WindowId>, Sen<WindowRecord>> =
    TableDefinition::new("rho_web_windows_v2");
const WEB_PAGES: TableDefinition<Sen<PageId>, Sen<PageRecord>> =
    TableDefinition::new("rho_web_pages_v2");
const MACHINE_SEED_KEY: u8 = 0;
const LAST_WINDOW_ID: u8 = 1;
const LAST_PAGE_ID: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowIdDomain(pub u64);
impl PrefixIdDomain for WindowIdDomain {
    const KIND: &'static str = "browser-window-id";
    fn machine_seed(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageIdDomain(pub u64);
impl PrefixIdDomain for PageIdDomain {
    const KIND: &'static str = "browser-page-id";
    fn machine_seed(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub PrefixId<WindowIdDomain>);

#[derive(Clone, Copy, Debug, Encode, Decode, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(pub PrefixId<PageIdDomain>);

impl fmt::Display for PageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "web-{}", self.0.encoded())
    }
}
impl FromStr for PageId {
    type Err = prefix_id::ParsePrefixIdError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(PrefixId::from_encoded(
            value.strip_prefix("web-").unwrap_or(value),
        )?))
    }
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct WindowRecord {
    pub id: WindowId,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub struct PageRecord {
    pub id: PageId,
    pub window_id: WindowId,
    /// The URL used when recreating the browser window. Without an observed
    /// navigation protocol this deliberately does not claim to be current.
    pub launch_url: String,
    pub created_at_ms: u64,
}

#[derive(Clone)]
pub struct WebStore {
    db: RhoDb,
}

impl WebStore {
    /// Opens the client-local browser schema. The v1 UUID tables existed only
    /// in the unshipped prototype; v2 starts with persisted prefix-id domains.
    pub async fn open(db: RhoDb) -> Self {
        let mut write = db.write().await;
        let mut machine = write.open_table(WEB_MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
        drop(machine);
        write.open_table(WEB_COUNTERS);
        write.open_table(WEB_WINDOWS);
        write.open_table(WEB_PAGES);
        write.commit();
        Self { db }
    }

    pub fn get_page(&self, id: PageId) -> Option<PageRecord> {
        self.db
            .read()
            .open_table(WEB_PAGES)
            .get(SenValue::borrowed(&id))
            .map(|value| value.value().into_owned())
    }

    pub fn page_handle(&self, id: PageId) -> String {
        let generated = self
            .db
            .read()
            .open_table(WEB_COUNTERS)
            .get(&LAST_PAGE_ID)
            .map(|value| value.value())
            .unwrap_or(0);
        let len = prefix_id::uniform_prefix_len(generated, 200).max(4);
        format!("web-{}", &id.0.encoded()[..len])
    }

    pub fn list_pages(&self) -> Vec<PageRecord> {
        let mut pages = self
            .db
            .read()
            .open_table(WEB_PAGES)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect::<Vec<_>>();
        pages.sort_unstable_by_key(|page| std::cmp::Reverse(page.created_at_ms));
        pages
    }

    /// Allocates and stores the window and page prefix IDs in one transaction.
    pub async fn create_page(&self, launch_url: String) -> Result<PageRecord> {
        validate_launch_url(&launch_url)?;
        let now = now_ms();
        let mut write = self.db.write().await;
        let seed = machine_seed(&mut write);
        let window = WindowRecord {
            id: WindowId(
                PrefixId::from_counter(
                    next_counter(&mut write, LAST_WINDOW_ID),
                    &WindowIdDomain(seed),
                )
                .expect("browser window id counter exceeds prefix-id capacity"),
            ),
            created_at_ms: now,
        };
        let page = PageRecord {
            id: PageId(
                PrefixId::from_counter(next_counter(&mut write, LAST_PAGE_ID), &PageIdDomain(seed))
                    .expect("browser page id counter exceeds prefix-id capacity"),
            ),
            window_id: window.id,
            launch_url,
            created_at_ms: now,
        };
        write
            .open_table(WEB_WINDOWS)
            .insert(SenValue::borrowed(&window.id), SenValue::borrowed(&window));
        write
            .open_table(WEB_PAGES)
            .insert(SenValue::borrowed(&page.id), SenValue::borrowed(&page));
        write.commit();
        Ok(page)
    }

    pub async fn delete_page(&self, id: PageId) -> bool {
        let mut write = self.db.write().await;
        let mut pages = write.open_table(WEB_PAGES);
        let Some(value) = pages.get(SenValue::borrowed(&id)) else {
            return false;
        };
        let page = value.value().into_owned();
        drop(value);
        pages.remove(SenValue::borrowed(&id));
        drop(pages);
        write
            .open_table(WEB_WINDOWS)
            .remove(SenValue::borrowed(&page.window_id));
        write.commit();
        true
    }
}

fn next_counter(write: &mut WriteTxn, key: u8) -> u64 {
    let mut counters = write.open_table(WEB_COUNTERS);
    let next = counters.get(&key).map(|value| value.value()).unwrap_or(0) + 1;
    counters.insert(&key, &next);
    next
}
fn machine_seed(write: &mut WriteTxn) -> u64 {
    write
        .open_table(WEB_MACHINE)
        .get(&MACHINE_SEED_KEY)
        .expect("browser machine seed missing")
        .value()
}

pub(crate) fn validate_launch_url(target: &str) -> Result<()> {
    if target.len() > 8192 {
        bail!("browser URL is too long")
    }
    let url = Url::parse(target).context("invalid browser URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("browser pages require an http or https URL")
    }
    Ok(())
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_ids_and_allocator_survive_reopen() {
        futures_lite::future::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("client.redb");
            let store = WebStore::open(RhoDb::open(&path)).await;
            let first = store
                .create_page("https://example.com".into())
                .await
                .unwrap();
            assert_eq!(store.get_page(first.id), Some(first.clone()));
            drop(store);
            let store = WebStore::open(RhoDb::open(&path)).await;
            let second = store
                .create_page("https://example.org".into())
                .await
                .unwrap();
            assert_ne!(first.id, second.id);
            assert_eq!(
                first.id.0.to_counter(&PageIdDomain(machine_seed(
                    &mut store.db.clone().write().await
                ))),
                1
            );
            assert!(store.delete_page(first.id).await);
            assert_eq!(store.list_pages(), vec![second]);
        });
    }
    #[test]
    fn page_id_text_round_trips() {
        let id = PageId(PrefixId::from_counter(42, &PageIdDomain(7)).unwrap());
        assert_eq!(id.to_string().parse(), Ok(id));
        assert!(id.to_string().starts_with("web-"));
        assert_eq!(id.to_string().len(), 16);
    }
}
