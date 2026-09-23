use crate::error::{CacheError, CacheResult};
use rusqlite::{Connection, ErrorCode, ffi};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{error, trace, warn};

/// Database connection manager
#[derive(Debug)]
pub struct Database {
    conn: Connection,
    _path: PathBuf,
}

impl Database {
    /// Open the database at `path` and bring it to the latest schema.
    ///
    /// * `path` - Path to the SQLite database file
    /// * `migrations` - Schema steps in order; step `n` moves `user_version`
    ///   from `n` to `n + 1`. A database this code cannot migrate is a cache,
    ///   so it is recreated rather than repaired.
    pub fn new(path: PathBuf, migrations: &[&str]) -> CacheResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Serialize create + migrate across processes. Concurrent cold
        // opens race on this window: SQLite's busy timeout does not serialize
        // the whole migration sequence. A migration error used to delete the
        // database out from under the process that created it
        // (cachix/devenv#3133).
        let _init_lock = acquire_init_lock(&path)?;

        trace!("Running migrations");

        // Try WAL journal mode first, falling back to DELETE (the default) on
        // SQLITE_IOERR_SHMMAP. WAL is preferred for concurrency, but requires
        // shared-memory support the VFS doesn't always have (e.g. some
        // virtiofs/9p/network mounts). See cachix/devenv#2947.
        let mut conn = match open_connection(&path, JournalMode::Wal) {
            Ok(conn) => conn,
            Err(e) if is_shmmap_error(&e) => fall_back_to_delete_mode(&path, &e)?,
            Err(e) if is_busy_error(&e) => return Err(CacheError::Database(e)),
            // Not a database (or a corrupt one): recreate it like a failed
            // migration below.
            Err(e) => return Self::recreate(path, migrations, &e),
        };

        if let Err(err) = migrate(&mut conn, migrations) {
            // Same shm-mmap failure, just surfacing during migration instead of
            // at connect time (SQLite can defer opening the `-shm` file until
            // the first real transaction).
            if is_shmmap_error(&err) {
                drop(conn);
                let mut conn = fall_back_to_delete_mode(&path, &err)?;
                migrate(&mut conn, migrations)?;
                return Ok(Self { conn, _path: path });
            }

            // A lock/busy error means another connection is using the file.
            // Never delete it — that is how concurrent cold entry turned a
            // recoverable SQLITE_BUSY into SQLITE_IOERR_DELETE_NOENT
            // (cachix/devenv#3133).
            if is_busy_error(&err) {
                warn!(
                    error = %err,
                    path = %path.display(),
                    "database locked during migration, retrying without recreating"
                );
                migrate(&mut conn, migrations)?;
                return Ok(Self { conn, _path: path });
            }

            // Some other migration failure (corruption, a schema from a newer
            // version, etc).
            drop(conn);
            return Self::recreate(path, migrations, &err);
        }

        Ok(Self { conn, _path: path })
    }

    /// Delete and recreate the database once. No other process is in
    /// [`Database::new`] while we hold the init lock; this remains last-resort
    /// recovery if a process that already opened the database is still using
    /// the file.
    fn recreate(path: PathBuf, migrations: &[&str], err: &rusqlite::Error) -> CacheResult<Self> {
        error!(error = %err, "Failed to open the database. Attempting to recreate the database.");
        remove_sqlite_files(&path);

        let mut conn = open_connection(&path, JournalMode::Wal)?;
        if let Err(e) = migrate(&mut conn, migrations) {
            error!("Migration failed after recreating database: {}", e);
            return Err(CacheError::Database(e));
        }
        Ok(Self { conn, _path: path })
    }

    /// Get a reference to the connection
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Get a mutable reference to the connection, for transactions
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

#[derive(Clone, Copy)]
enum JournalMode {
    Wal,
    Delete,
}

fn open_connection(path: &Path, journal_mode: JournalMode) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let mode = match journal_mode {
        JournalMode::Wal => "WAL",
        JournalMode::Delete => "DELETE",
    };
    conn.pragma_update(None, "journal_mode", mode)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
    conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?; // 64 MB
    conn.pragma_update(None, "cache_size", 2000)?; // 2000 pages
    Ok(conn)
}

/// Apply the migrations `user_version` has not seen yet, all in one
/// transaction.
fn migrate(conn: &mut Connection, migrations: &[&str]) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version > migrations.len() {
        return Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_MISMATCH),
            Some(format!(
                "database schema version {version} is newer than this program's {}",
                migrations.len()
            )),
        ));
    }
    for migration in &migrations[version..] {
        tx.execute_batch(migration)?;
    }
    tx.pragma_update(None, "user_version", migrations.len() as i64)?;
    tx.commit()
}

/// Log the fallback and reopen in DELETE mode. Shared by the connect-time and
/// migrate-time failure sites, which hit the same class of error.
fn fall_back_to_delete_mode(
    path: &Path,
    error: impl std::fmt::Display,
) -> CacheResult<Connection> {
    warn!(
        %error,
        path = %path.display(),
        "got SQLITE_IOERR_SHMMAP, falling back to DELETE journal mode (reduced concurrency)"
    );
    remove_sqlite_files(path);
    open_connection(path, JournalMode::Delete).map_err(CacheError::Database)
}

/// Remove a SQLite database file and its associated WAL/SHM files.
fn remove_sqlite_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(Path::new(&file));
    }
}

fn sqlite_error_code(error: &rusqlite::Error) -> Option<ffi::Error> {
    match error {
        rusqlite::Error::SqliteFailure(e, _) => Some(*e),
        _ => None,
    }
}

/// True if this is exactly `SQLITE_IOERR_SHMMAP` -- WAL mode failing to
/// `mmap(MAP_SHARED)` its `-shm` coordination file. See cachix/devenv#2947.
fn is_shmmap_error(error: &rusqlite::Error) -> bool {
    sqlite_error_code(error).is_some_and(|e| e.extended_code == ffi::SQLITE_IOERR_SHMMAP)
}

/// SQLITE_BUSY / SQLITE_LOCKED, plus the I/O error SQLite emits when a
/// concurrent creator deletes the journal/WAL file out from under us.
fn is_busy_error(error: &rusqlite::Error) -> bool {
    sqlite_error_code(error).is_some_and(|e| {
        matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
            || e.extended_code == ffi::SQLITE_IOERR_DELETE_NOENT
    })
}

fn init_lock_path(db_path: &Path) -> PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(".init.lock");
    PathBuf::from(path)
}

/// Acquire an exclusive flock on a sibling of the SQLite file, released when
/// the returned file is dropped.
///
/// Must not lock the `.db` itself: SQLite uses POSIX locks on that file.
fn acquire_init_lock(db_path: &Path) -> CacheResult<File> {
    let lock_path = init_lock_path(db_path);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .and_then(|file| file.lock().map(|()| file))
        .map_err(|e| {
            CacheError::initialization(format!(
                "failed to lock {} for cache init: {e}",
                db_path.display()
            ))
        })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const MIGRATIONS: &[&str] = &["CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)"];

    #[test]
    fn test_database_with_percent_encoded_path() {
        let temp_dir = TempDir::new().unwrap();
        let dir_with_percent = temp_dir.path().join("test%2Fdir");
        std::fs::create_dir_all(&dir_with_percent).unwrap();
        let db_path = dir_with_percent.join("test.db");

        Database::new(db_path.clone(), MIGRATIONS).unwrap();

        assert!(db_path.exists());
    }

    #[test]
    fn test_database_creation() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let db = Database::new(db_path.clone(), MIGRATIONS).unwrap();

        // Test that the database file was created
        assert!(db_path.exists());

        // Test that we can execute queries
        db.conn()
            .execute("INSERT INTO test (name) VALUES (?1)", ["test_value"])
            .unwrap();
        let name: String = db
            .conn()
            .query_row("SELECT name FROM test WHERE name = ?1", ["test_value"], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "test_value");

        // Guard against the DELETE-mode fallback firing in the common case.
        let journal_mode: String = db
            .conn()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
    }

    #[test]
    fn migrations_apply_incrementally() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path.clone(), MIGRATIONS).unwrap();
        db.conn()
            .execute("INSERT INTO test (name) VALUES ('kept')", [])
            .unwrap();
        drop(db);

        let more = [MIGRATIONS[0], "ALTER TABLE test ADD COLUMN extra TEXT"];
        let db = Database::new(db_path, &more).unwrap();
        let (name, extra): (String, Option<String>) = db
            .conn()
            .query_row("SELECT name, extra FROM test", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((name.as_str(), extra), ("kept", None));
    }

    #[test]
    fn unknown_schema_is_recreated() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let newer = [MIGRATIONS[0], "CREATE TABLE later (id INTEGER)"];
        drop(Database::new(db_path.clone(), &newer).unwrap());

        let db = Database::new(db_path, MIGRATIONS).unwrap();
        let version: i64 = db
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn corrupt_database_is_recreated() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        std::fs::write(&db_path, b"this is not a sqlite database, just some bytes..").unwrap();

        let db = Database::new(db_path, MIGRATIONS).unwrap();
        db.conn().execute("INSERT INTO test (name) VALUES ('x')", []).unwrap();
    }

    #[test]
    fn concurrent_cold_open_succeeds() {
        // Several threads creating the same missing database must all
        // succeed (cachix/devenv#3133).
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = db_path.clone();
                std::thread::spawn(move || Database::new(path, MIGRATIONS))
            })
            .collect();

        let dbs: Vec<Database> = handles
            .into_iter()
            .map(|h| h.join().unwrap().expect("concurrent cold Database::new should succeed"))
            .collect();
        for db in &dbs {
            db.conn().execute("SELECT id, name FROM test", []).ok();
        }
        let journal_mode: String = dbs[0]
            .conn()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
    }
}
