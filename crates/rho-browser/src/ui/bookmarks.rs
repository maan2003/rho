//! Bookmarks are named URLs, not retained tabs. The browser profile holds
//! a page only while its tab lives; a bookmark is something the user
//! said, so it is a fact in their ledger ([`Fact::Bookmarked`]) and
//! reaches every paired device the way verdicts and notes do. The frame
//! holds the facts ([`Marks::bookmarks`]); nothing here keeps a copy.
//!
//! A bookmark's id is its URL's: the same page bookmarked on two devices,
//! or converted from two devices' old local tables, is one bookmark.

use rho_dealer::facts::Fact;
use rho_dealer::{Addressed, Marks};
use rho_frame::attention::Write;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BookmarkId(Uuid);

impl BookmarkId {
    /// The one bookmark of `url`.
    pub fn of(url: &Url) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"rho-browser/bookmark/1");
        hash.update(url.as_str().as_bytes());
        let bytes: [u8; 16] = hash.finalize().as_bytes()[..16]
            .try_into()
            .expect("sixteen bytes");
        Self(Uuid::from_bytes(bytes))
    }
}

impl Addressed for BookmarkId {
    const KIND: &'static str = "bookmark";

    fn rest(&self) -> String {
        self.0.hyphenated().to_string()
    }

    fn from_rest(rest: &str) -> Option<Self> {
        let id = Self(Uuid::parse_str(rest).ok()?);
        (id.rest() == rest).then_some(id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bookmark {
    pub id: BookmarkId,
    pub title: String,
    pub url: Url,
}

/// Every bookmark the user keeps, by title.
pub(super) fn named(marks: &Marks) -> Vec<Bookmark> {
    let mut named: Vec<_> = marks
        .bookmarks()
        .filter_map(|(node, held)| {
            Some(Bookmark {
                id: BookmarkId::from_address(node)?,
                title: held.title.clone(),
                url: page_url(&held.url)?,
            })
        })
        .collect();
    named.sort_by(|a, b| (&a.title, a.url.as_str()).cmp(&(&b.title, b.url.as_str())));
    named
}

pub(super) fn get(marks: &Marks, id: BookmarkId) -> Option<Bookmark> {
    let held = marks.get(&id.address()).bookmark.as_ref()?;
    Some(Bookmark {
        id,
        title: held.title.clone(),
        url: page_url(&held.url)?,
    })
}

/// Keeps `url` under `title`; a page already kept is renamed.
pub(super) fn save(title: &str, url: &Url) -> Write {
    Write::Fact(Fact::Bookmarked {
        node: BookmarkId::of(url).address(),
        title: title.to_owned(),
        url: url.to_string(),
    })
}

pub(super) fn remove(id: BookmarkId) -> Write {
    Write::Fact(Fact::Unbookmarked { node: id.address() })
}

/// Every bookmark, the one of `current` first when it has one: what
/// `remove bookmark…` offers.
pub(super) fn removable(marks: &Marks, current: Option<&Url>) -> Vec<Bookmark> {
    let mut all = named(marks);
    if let Some(at) = current.and_then(|url| all.iter().position(|kept| kept.url == *url)) {
        let here = all.remove(at);
        all.insert(0, here);
    }
    all
}

pub(super) fn page_url(value: &str) -> Option<Url> {
    crate::store::validate_launch_url(value).ok()?;
    let url = Url::parse(value).ok()?;
    (url.username().is_empty() && url.password().is_none()).then_some(url)
}

/// What builds before the ledger kept on this device alone: its bookmarks
/// are said once into the ledger, then the table goes. Remove once every
/// device has opened a build with it.
pub(super) mod local {
    use redb::{TableDefinition, TableHandle as _};
    use rho_db::RhoDb;

    use super::*;

    pub(in super::super) const BOOKMARKS: TableDefinition<&str, (&str, &str)> =
        TableDefinition::new("gui_browser_bookmark_v1");

    /// What the old table holds that the ledger does not, as writes;
    /// `None` once the table is gone.
    pub fn writes(db: &RhoDb, marks: &Marks) -> Option<Vec<Write>> {
        let read = db.read();
        if !read.has_table(BOOKMARKS.name()) {
            return None;
        }
        let writes = read
            .open_table(BOOKMARKS)
            .iter()
            .filter_map(|(_, stored)| {
                let (title, url) = stored.value();
                let url = page_url(url)?;
                let held = marks.get(&BookmarkId::of(&url).address());
                held.bookmark.is_none().then(|| save(title, &url))
            })
            .collect();
        Some(writes)
    }

    /// Drops the table once what it held has been said.
    pub async fn retire(db: &RhoDb) {
        let mut write = db.write().await;
        write.delete_table(BOOKMARKS.name());
        write.commit();
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use rho_db::RhoDb;
    use rho_dealer::facts::{Device, Entry};
    use rho_ledger::{Channel, Ledger, Secret};

    use super::*;

    const LAPTOP: Device = Device([1; 16]);

    fn url(text: &str) -> Url {
        page_url(text).unwrap()
    }

    /// Entries as the frame's attention says them, one minute apart.
    fn entries(device: Device, from_minute: i64, writes: Vec<Write>) -> Vec<Entry> {
        writes
            .into_iter()
            .enumerate()
            .map(|(index, write)| {
                let Write::Fact(fact) = write else {
                    panic!("a bookmark is a fact")
                };
                Entry {
                    device,
                    at: Timestamp::from_second(1_800_000_000 + (from_minute + index as i64) * 60)
                        .unwrap()
                        .to_zoned(jiff::tz::TimeZone::UTC),
                    fact,
                }
            })
            .collect()
    }

    fn marks_of(entries: impl IntoIterator<Item = Entry>) -> Marks {
        let mut marks = Marks::default();
        marks.apply(entries);
        marks
    }

    #[test]
    fn saving_a_page_again_renames_its_one_bookmark() {
        let page = url("https://guide.example.invalid/clock#checks");
        let other = url("https://guide.example.invalid/clock");
        let marks = marks_of(entries(
            LAPTOP,
            0,
            vec![
                save("Old title", &page),
                save("Clock", &other),
                save("Acceptance guide", &page),
            ],
        ));
        let named = named(&marks);
        assert_eq!(
            named
                .iter()
                .map(|kept| (kept.title.as_str(), kept.url.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Acceptance guide", page.as_str()),
                ("Clock", other.as_str()),
            ]
        );
        assert_eq!(
            get(&marks, BookmarkId::of(&page)).unwrap().title,
            "Acceptance guide"
        );
        assert!(page_url("file:///tmp/page.html").is_none());
        assert!(page_url("https://person:credential@guide.example.invalid/").is_none());
    }

    #[test]
    fn the_current_pages_bookmark_is_offered_first_for_removal() {
        let (a, b, c) = (
            url("https://a.example.invalid/"),
            url("https://b.example.invalid/"),
            url("https://c.example.invalid/"),
        );
        let marks = marks_of(entries(
            LAPTOP,
            0,
            vec![save("alpha", &a), save("bravo", &b), save("charlie", &c)],
        ));
        let titles = |current: Option<&Url>| {
            removable(&marks, current)
                .into_iter()
                .map(|kept| kept.title)
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(Some(&c)), ["charlie", "alpha", "bravo"]);
        assert_eq!(titles(None), ["alpha", "bravo", "charlie"]);
        let unkept = url("https://d.example.invalid/");
        assert_eq!(titles(Some(&unkept)), ["alpha", "bravo", "charlie"]);
    }

    /// Two devices that hold the same phrase: a bookmark the laptop keeps
    /// reaches the phone through the sealed log, and so does its removal.
    #[test]
    fn a_bookmark_removed_on_one_device_is_gone_on_the_other() {
        futures_lite::future::block_on(async {
            let scratch = tempfile::tempdir().unwrap();
            let laptop = Ledger::open(RhoDb::open(scratch.path().join("laptop.redb"))).await;
            let phone = Ledger::open(RhoDb::open(scratch.path().join("phone.redb"))).await;
            let secret = Secret::generate();
            laptop.set_secret(secret).await.unwrap();
            phone.set_secret(secret).await.unwrap();
            let device = Device(laptop.device().0);
            let slack = url("https://example.slack.com/archives/C1");
            let sync = async |at: &mut u64| {
                let (log, length) = laptop.lengths().into_iter().next().unwrap();
                let bytes = laptop.bytes_after(log, *at, usize::MAX);
                phone.receive(log, *at, bytes).await;
                *at = length;
            };
            let phone_marks = || {
                marks_of(
                    phone
                        .items(Channel::Facts)
                        .iter()
                        .filter_map(|item| Entry::decode(&item.bytes)),
                )
            };
            let say = async |minute, write| {
                let payloads = entries(device, minute, vec![write])
                    .iter()
                    .map(Entry::encode)
                    .collect();
                laptop.append(Channel::Facts, payloads).await;
            };

            let mut synced = 0;
            say(0, save("slack", &slack)).await;
            sync(&mut synced).await;
            assert_eq!(
                named(&phone_marks())
                    .into_iter()
                    .map(|kept| kept.url)
                    .collect::<Vec<_>>(),
                std::slice::from_ref(&slack)
            );

            say(1, remove(BookmarkId::of(&slack))).await;
            sync(&mut synced).await;
            assert_eq!(named(&phone_marks()), []);
        });
    }

    /// The old table's bookmarks are said into the ledger once: a second
    /// conversion before the table goes says nothing new, and none after.
    #[test]
    fn a_local_bookmark_is_converted_exactly_once() {
        futures_lite::future::block_on(async {
            let scratch = tempfile::tempdir().unwrap();
            let db = RhoDb::open(scratch.path().join("gui.redb"));
            let kept = url("https://example.slack.com/");
            let already = url("https://already.example.invalid/");
            {
                let mut write = db.write().await;
                let mut table = write.open_table(local::BOOKMARKS);
                table.insert(
                    "00000000-0000-4000-8000-000000000001",
                    ("work slack", kept.as_str()),
                );
                table.insert(
                    "00000000-0000-4000-8000-000000000002",
                    ("synced", already.as_str()),
                );
                table.insert(
                    "00000000-0000-4000-8000-000000000003",
                    ("not a page", "file:///etc/passwd"),
                );
                drop(table);
                write.commit();
            }
            // Another device already said one of them.
            let mut said = entries(Device([2; 16]), 0, vec![save("synced", &already)]);
            let first = local::writes(&db, &marks_of(said.clone())).unwrap();
            assert_eq!(first.len(), 1, "only what the ledger lacks");
            said.extend(entries(LAPTOP, 1, first));
            let marks = marks_of(said.clone());
            assert_eq!(
                local::writes(&db, &marks).map(|writes| writes.len()),
                Some(0),
                "converted already"
            );
            local::retire(&db).await;
            assert!(
                local::writes(&db, &marks_of([])).is_none(),
                "the table is gone"
            );

            let named = named(&marks);
            assert_eq!(
                named
                    .iter()
                    .map(|kept| (kept.title.as_str(), kept.url.as_str()))
                    .collect::<Vec<_>>(),
                [("synced", already.as_str()), ("work slack", kept.as_str())]
            );
        });
    }
}
