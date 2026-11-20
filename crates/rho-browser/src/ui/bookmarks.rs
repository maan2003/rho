//! Device-local bookmarks are named URLs, not retained tabs. The browser
//! profile holds a page only while its tab lives; the shared GUI database
//! holds the user's explicit bookmarks across browser and GUI restarts.

use redb::{TableDefinition, TableHandle as _};
use rho_db::RhoDb;
use rho_dealer::Addressed;
use url::Url;
use uuid::Uuid;

const BOOKMARKS: TableDefinition<&str, (&str, &str)> =
    TableDefinition::new("gui_browser_bookmark_v1");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BookmarkId(Uuid);

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

pub(super) struct Bookmark {
    pub id: BookmarkId,
    pub title: String,
    pub url: Url,
}

pub(super) fn named(db: &RhoDb) -> Vec<Bookmark> {
    let read = db.read();
    if !read.has_table(BOOKMARKS.name()) {
        return Vec::new();
    }
    read.open_table(BOOKMARKS)
        .iter()
        .filter_map(|(key, stored)| {
            let (title, url) = stored.value();
            Some(Bookmark {
                id: BookmarkId::from_rest(key.value())?,
                title: title.to_owned(),
                url: page_url(url)?,
            })
        })
        .collect()
}

pub(super) fn get(db: &RhoDb, id: BookmarkId) -> Option<Bookmark> {
    let read = db.read();
    if !read.has_table(BOOKMARKS.name()) {
        return None;
    }
    let table = read.open_table(BOOKMARKS);
    let stored = table.get(id.rest().as_str())?;
    let (title, url) = stored.value();
    Some(Bookmark {
        id,
        title: title.to_owned(),
        url: page_url(url)?,
    })
}

pub(super) async fn save(db: &RhoDb, title: &str, url: &Url) {
    let mut write = db.write().await;
    {
        let mut bookmarks = write.open_table(BOOKMARKS);
        let matching = bookmarks.iter().find_map(|(id, record)| {
            (record.value().1 == url.as_str()).then(|| id.value().to_owned())
        });
        let id = matching.unwrap_or_else(|| BookmarkId(Uuid::new_v4()).rest());
        bookmarks.insert(id.as_str(), (title, url.as_str()));
    }
    write.commit();
}

pub(super) fn page_url(value: &str) -> Option<Url> {
    crate::store::validate_launch_url(value).ok()?;
    let url = Url::parse(value).ok()?;
    (url.username().is_empty() && url.password().is_none()).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_url_survives_reopen_and_renaming_keeps_its_identity() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("browser.redb");
        let url = page_url("https://guide.example.invalid/clock#checks").unwrap();
        let db = RhoDb::open(&path);
        futures_lite::future::block_on(save(&db, "Old title", &url));
        let id = named(&db).pop().unwrap().id;
        drop(db);

        let db = RhoDb::open(path);
        assert_eq!(get(&db, id).unwrap().url, url);
        futures_lite::future::block_on(save(&db, "Acceptance guide", &url));
        let result = named(&db);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, id);
        assert_eq!(result[0].title, "Acceptance guide");
        assert!(page_url("file:///tmp/page.html").is_none());
        assert!(page_url("https://person:credential@guide.example.invalid/").is_none());
    }
}
