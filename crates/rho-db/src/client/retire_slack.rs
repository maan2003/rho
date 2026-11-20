//! One-hop migration from the client that kept a personal Slack session.
//!
//! Remove after active devices have opened this build once. Renaming the
//! non-cache tables sets their original bytes aside without keeping the
//! old data model alive or copying potentially large attachment drafts.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context as _;
use redb::{ReadableDatabase as _, TableDefinition, TableError, TableHandle as _};

use crate::RhoDb;

const CLIENT_FORMAT: TableDefinition<(), &str> = TableDefinition::new("gui_client_format");
const CURRENT_CLIENT_FORMAT: &str = "fe38c0a7";

const DROP: &[&str] = &[
    "rho_slack_cache_generation_v1",
    "rho_slack_messages_v1",
    "rho_slack_gaps_v1",
    "rho_slack_users_v1",
    "rho_slack_conversations_v1",
    "rho_slack_units_v1",
];
const ARCHIVE: &[&str] = &[
    "rho_slack_favorites_v1",
    "rho_slack_cursors_v1",
    "rho_slack_reacted_with_v1",
    "rho_slack_draft_text_v1",
    "rho_slack_draft_files_v1",
    "rho_slack_pending_drafts_v1",
];

pub(super) fn migrate(state_dir: &Path, db: &RhoDb) -> anyhow::Result<()> {
    // Only owned paths, not an environment override that might name
    // something outside this device's store. Delete both the live file
    // and the temporary credential write if the previous build crashed.
    for name in ["slack-credentials.json", "slack-credentials.json.new"] {
        let path = state_dir.join(name);
        ignore_not_found(std::fs::remove_file(&path))
            .with_context(|| format!("remove retired credential store {}", path.display()))?;
    }
    ignore_not_found(std::fs::remove_dir_all(state_dir.join("slack-files")))
        .context("remove retired Slack download cache")?;

    // Builds from before the single client database used a separate
    // mirror. Retire its tables too, leaving its durable data in place.
    let standalone = state_dir.join("slack.redb");
    if standalone.try_exists()? {
        let db = redb::Database::open(&standalone).context("open retired Slack mirror")?;
        retire_tables(&db)?;
    }

    // The client still holds its exclusive state-directory lock and
    // hasn't handed out this handle, so no runtime or second writer
    // wrapper is needed to run the migration at open.
    retire_tables(&db.database.read())?;
    Ok(())
}

fn ignore_not_found(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn retire_tables(db: &redb::Database) -> anyhow::Result<()> {
    let read = db.begin_read()?;
    let names: BTreeSet<_> = read
        .list_tables()?
        .map(|table| table.name().to_owned())
        .collect();
    let migrated = match read.open_table(CLIENT_FORMAT) {
        Ok(table) => table
            .get(())?
            .is_some_and(|value| value.value() == CURRENT_CLIENT_FORMAT),
        Err(TableError::TableDoesNotExist(_)) => false,
        Err(error) => return Err(error.into()),
    };
    if migrated && !DROP.iter().chain(ARCHIVE).any(|name| names.contains(*name)) {
        return Ok(());
    }
    drop(read);

    let write = db.begin_write()?;
    for &name in DROP {
        write.delete_table(TableDefinition::<(), ()>::new(name))?;
    }
    for &name in ARCHIVE {
        if !names.contains(name) {
            continue;
        }
        // A downgrade followed by new drafts must not overwrite the
        // previous archive. Derive the next free name from the catalog.
        let archived = (1..)
            .map(|n| format!("retired_{name}_{n}"))
            .find(|name| !names.contains(name))
            .expect("a finite catalog has a free archive name");
        write.rename_table(
            TableDefinition::<(), ()>::new(name),
            TableDefinition::<(), ()>::new(&archived),
        )?;
    }
    write
        .open_table(CLIENT_FORMAT)?
        .insert((), CURRENT_CLIENT_FORMAT)?;
    write.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use redb::ReadableTable as _;

    use super::*;

    // Seed the tables before calling the public client opener, as an older
    // build would. Check stored values rather than just table existence.
    const DRAFT: TableDefinition<&str, &str> = TableDefinition::new("rho_slack_draft_text_v1");
    const MESSAGE: TableDefinition<&str, &str> = TableDefinition::new("rho_slack_messages_v1");
    const CLIENT: TableDefinition<u64, &str> = TableDefinition::new("chat_outbox_v1");
    const FIRST_ARCHIVE: TableDefinition<&str, &str> =
        TableDefinition::new("retired_rho_slack_draft_text_v1_1");
    const SECOND_ARCHIVE: TableDefinition<&str, &str> =
        TableDefinition::new("retired_rho_slack_draft_text_v1_2");

    const TARGET: &str = "acme\u{1f}C1\u{1f}";

    fn old_client(path: &Path, draft: &str) {
        let db = redb::Database::create(path).unwrap();
        let write = db.begin_write().unwrap();
        write
            .open_table(DRAFT)
            .unwrap()
            .insert(TARGET, draft)
            .unwrap();
        write
            .open_table(MESSAGE)
            .unwrap()
            .insert(TARGET, "server has this")
            .unwrap();
        write
            .open_table(CLIENT)
            .unwrap()
            .insert(12, "user's chat")
            .unwrap();
        write.commit().unwrap();
    }

    #[test]
    fn opening_an_old_device_forgets_credentials_and_cache_but_keeps_user_work() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path();
        let device = super::super::path(state);
        let standalone = state.join("slack.redb");
        old_client(&device, "unfinished reply");
        old_client(&standalone, "earlier unsent reply");
        for path in [
            "slack-credentials.json",
            "slack-credentials.json.new",
            "browser-cookie",
        ] {
            std::fs::write(state.join(path), "test data").unwrap();
        }
        let cache = state.join("slack-files");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("example"), "server has this too").unwrap();

        for _ in 0..2 {
            let db = super::super::open(state).unwrap();
            let read = db.read();
            assert!(!read.has_table(MESSAGE.name()));
            assert!(!read.has_table(DRAFT.name()));
            assert_eq!(
                read.open_table(CLIENT).get(12).unwrap().value(),
                "user's chat"
            );
            assert_eq!(
                read.open_table(FIRST_ARCHIVE).get(TARGET).unwrap().value(),
                "unfinished reply"
            );
            assert!(!read.has_table(SECOND_ARCHIVE.name()));
            assert_eq!(
                read.open_table(CLIENT_FORMAT).get(()).unwrap().value(),
                CURRENT_CLIENT_FORMAT
            );
        }
        for path in [
            "slack-credentials.json",
            "slack-credentials.json.new",
            "slack-files",
        ] {
            assert!(!state.join(path).try_exists().unwrap());
        }
        assert_eq!(
            std::fs::read(state.join("browser-cookie")).unwrap(),
            b"test data"
        );
        let old = redb::Database::open(&standalone).unwrap();
        let read = old.begin_read().unwrap();
        assert!(matches!(
            read.open_table(MESSAGE),
            Err(TableError::TableDoesNotExist(_))
        ));
        assert_eq!(
            read.open_table(FIRST_ARCHIVE)
                .unwrap()
                .get(TARGET)
                .unwrap()
                .unwrap()
                .value(),
            "earlier unsent reply"
        );
    }

    #[test]
    fn a_downgraded_client_cannot_read_or_overwrite_an_earlier_draft() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path();
        let device = super::super::path(state);
        old_client(&device, "before first upgrade");
        drop(super::super::open(state).unwrap());

        // An older version can open the DB but sees no old live draft.
        let old = redb::Database::open(&device).unwrap();
        let write = old.begin_write().unwrap();
        assert!(
            write
                .open_table(DRAFT)
                .unwrap()
                .get(TARGET)
                .unwrap()
                .is_none()
        );
        write.commit().unwrap();
        drop(old);
        old_client(&device, "after downgrade");
        std::fs::write(state.join("slack-credentials.json"), "registered again").unwrap();

        let db = super::super::open(state).unwrap();
        assert!(!state.join("slack-credentials.json").try_exists().unwrap());
        let read = db.read();
        assert!(!read.has_table(DRAFT.name()));
        assert_eq!(
            read.open_table(FIRST_ARCHIVE).get(TARGET).unwrap().value(),
            "before first upgrade"
        );
        assert_eq!(
            read.open_table(SECOND_ARCHIVE).get(TARGET).unwrap().value(),
            "after downgrade"
        );
    }
}
