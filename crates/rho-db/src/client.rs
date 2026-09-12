//! The client's one database.
//!
//! A rho client keeps several kinds of state: the agent mirror, the desk
//! mirror, the Slack mirror and its cursors, the action journal, the
//! inbox. Each was its own redb file, which is one file lock, one page
//! cache and one allocator rebuild per kind, and five chances for a
//! session to be half open. They are one file now, the way the daemon's
//! store is one file. Every crate keeps its own tables and its own types;
//! only the file is shared.
//!
//! It is opened once, by the process that owns it, and handed to each
//! crate. Nothing here resolves the state directory: that is `main`'s,
//! and a library that guessed it would be guessing at the user's data.
//!
//! Opening is not free — after an unclean stop redb rebuilds its
//! allocator from every page — so the open happens off the main thread,
//! and things that must be told about it register with [`on_open`]
//! before it happens rather than opening the file themselves.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::RhoDb;

pub const FILE_NAME: &str = "rho-client.redb";
/// Held for as long as the database is open. redb takes its own lock and
/// panics on a second opener; this one is taken first so a second rho
/// says what is wrong instead.
const LOCK_FILE_NAME: &str = "rho-client.lock";

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

static SHARED: OnceLock<RhoDb> = OnceLock::new();
static LOCK: OnceLock<File> = OnceLock::new();
#[expect(clippy::type_complexity)]
static HOOKS: Mutex<Vec<Box<dyn FnOnce(&RhoDb) + Send>>> = Mutex::new(Vec::new());

/// Takes the exclusive lock on the client's database, before anything has
/// opened it. `main` calls this early and on the main thread: it is a
/// `flock` on an empty file and costs nothing, and a second rho has to
/// say so and stop rather than run on holding none of its own state.
/// Opening the database afterwards reuses this lock.
pub fn lock(state_dir: &Path) -> std::io::Result<()> {
    if LOCK.get().is_some() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)?;
    let _ = LOCK.set(acquire_lock(state_dir)?);
    Ok(())
}

/// Opens the client's database at `state_dir`, exclusively. For a tool
/// that reads the file while no client holds it, and for tests.
pub fn open(state_dir: &Path) -> std::io::Result<RhoDb> {
    std::fs::create_dir_all(state_dir)?;
    let db = open_locked(state_dir)?;
    own_the_file(state_dir)?;
    Ok(db)
}

/// The database, holding the lock unless this process already holds it.
/// The lock is taken first: redb's own refusal is a panic, so the door has
/// to be the one that answers.
fn open_locked(state_dir: &Path) -> std::io::Result<RhoDb> {
    let lock = match LOCK.get() {
        Some(_) => None,
        None => Some(acquire_lock(state_dir)?),
    };
    let db = RhoDb::open(path(state_dir));
    Ok(match lock {
        Some(lock) => db.holding(lock),
        None => db,
    })
}

/// Opens it once for this process and remembers it, then tells everything
/// that asked to be told. A second call is the first one's answer: the
/// file has one opener and this is it.
pub fn open_shared(state_dir: &Path) -> std::io::Result<RhoDb> {
    if let Some(db) = SHARED.get() {
        return Ok(db.clone());
    }
    std::fs::create_dir_all(state_dir)?;
    let db = open_locked(state_dir)?;
    own_the_file(state_dir)?;
    let db = SHARED.get_or_init(|| db).clone();
    let hooks = std::mem::take(
        &mut *HOOKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for hook in hooks {
        hook(&db);
    }
    Ok(db)
}

/// The database this process opened, if it has. `None` is not an error:
/// a test, or a rho told of no state directory, has no client database
/// and every cache over it is simply absent.
pub fn shared() -> Option<RhoDb> {
    SHARED.get().cloned()
}

/// Runs `hook` when the database opens, or now if it already has. This is
/// how `main` hands the file to a crate whose own open would otherwise
/// have to happen on the main thread.
pub fn on_open(hook: impl FnOnce(&RhoDb) + Send + 'static) {
    if let Some(db) = SHARED.get() {
        hook(db);
        return;
    }
    HOOKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Box::new(hook));
}

/// The file holds the user's messages, their captures and every verdict
/// they have given. It is theirs alone to read.
#[cfg(unix)]
fn own_the_file(state_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path(state_dir), std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn own_the_file(_state_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

fn acquire_lock(state_dir: &Path) -> std::io::Result<File> {
    let path = state_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // The lock is the file's only purpose; nothing is ever written to
        // it, so there is nothing to truncate.
        .truncate(false)
        .open(&path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            "the client database is in use; exit the GUI before opening or dumping it",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock is the file's, and it lives as long as the handle does.
    /// A call that took it and dropped it before returning would say the
    /// file is free while a database is open on it, which is how two
    /// openers meet inside redb rather than at the door.
    #[test]
    fn a_second_opener_is_told_the_file_is_in_use_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let held = open(dir.path()).expect("the first opener takes the file");
        let error = open(dir.path()).expect_err("the second is refused");
        assert!(
            error.to_string().contains("exit the GUI"),
            "a second rho is told what to do about it: {error}"
        );
        drop(held);
        open(dir.path()).expect("the file is free once the handle is gone");
    }

    /// Everything the client keeps is in the one file, under its owner's
    /// own names. Two owners' tables are side by side and neither knows
    /// about the other.
    #[tokio::test]
    async fn two_owners_keep_their_own_tables_in_the_one_file() {
        const MINE: redb::TableDefinition<u64, u64> = redb::TableDefinition::new("mine_v1");
        const YOURS: redb::TableDefinition<u64, u64> = redb::TableDefinition::new("yours_v1");

        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).unwrap();
        {
            let mut write = db.write().await;
            write.open_table(MINE).insert(&1, &11);
            write.open_table(YOURS).insert(&1, &22);
            write.commit();
        }
        let read = db.read();
        assert_eq!(read.open_table(MINE).get(&1).map(|it| it.value()), Some(11));
        assert_eq!(
            read.open_table(YOURS).get(&1).map(|it| it.value()),
            Some(22)
        );
    }
}
