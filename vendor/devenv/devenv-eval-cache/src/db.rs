//! Database schema and queries for the eval cache.

use crate::eval_inputs::{FileHashes, FlakeScheme, Input, PathInput, RevInputDesc};
use devenv_core::ObservedKind;
use devenv_cache_core::{CacheResult, compute_file_hash};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MIGRATIONS: &[&str] = &[r#"
-- One row per stored evaluation. A key can hold several candidates, one per
-- distinct input state (for example two worktrees on different branches).
CREATE TABLE cached_eval
(
  id               INTEGER NOT NULL PRIMARY KEY,
  key_hash         CHAR(64) NOT NULL,
  attr_name        TEXT NOT NULL,
  input_hash       CHAR(64) NOT NULL,
  json_output      TEXT NOT NULL,
  -- Whether the flake's source-info was forced, and its state then.
  flake_rev        BOOLEAN NOT NULL,
  flake_rev_hash   CHAR(64),
  -- Milliseconds; candidates are tried most recently used first.
  updated_at       INTEGER NOT NULL,
  UNIQUE(key_hash, input_hash)
);

CREATE INDEX idx_cached_eval_key ON cached_eval(key_hash, updated_at);

-- What the evaluator observed reading the flake's source tree; see
-- `PathInput`. Paths are relative to the source root.
CREATE TABLE eval_path_input
(
  cached_eval_id  INTEGER NOT NULL REFERENCES cached_eval(id) ON DELETE CASCADE,
  kind            TEXT NOT NULL,
  scheme          TEXT NOT NULL,
  path            BLOB NOT NULL,
  value           TEXT NOT NULL,
  PRIMARY KEY (cached_eval_id, kind, scheme, path)
) WITHOUT ROWID;

-- Content hashes of files on disk, valid while their stat is unchanged. This
-- is what lets validation skip hashing unchanged files.
CREATE TABLE file_input
(
  path          BLOB NOT NULL PRIMARY KEY,
  size          INTEGER NOT NULL,
  inode         INTEGER NOT NULL,
  mtime_ns      INTEGER NOT NULL,
  ctime_ns      INTEGER NOT NULL,
  content_hash  CHAR(64) NOT NULL,
  updated_at    INTEGER NOT NULL
);
"#];

/// Candidates kept per key; the least recently used are evicted on store.
const MAX_CANDIDATES: i64 = 8;

/// Stat cache rows unused this long are dropped on store.
const FILE_INPUT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A file changed this recently may change again without a visible stat
/// change (the "racily clean" problem), so its hash is not cached.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// The row type for the `cached_eval` table, without inputs.
#[derive(Clone, Debug)]
pub struct EvalRow {
    pub id: i64,
    pub json_output: String,
    /// `Some(state)` if the flake's source-info was an input.
    pub flake_rev: Option<Option<String>>,
}

/// Candidates for `key_hash`, most recently used first.
pub fn get_evals_by_key_hash(conn: &Connection, key_hash: &str) -> rusqlite::Result<Vec<EvalRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, json_output, flake_rev, flake_rev_hash FROM cached_eval
         WHERE key_hash = ?1 ORDER BY updated_at DESC, id DESC",
    )?;
    stmt.query_map([key_hash], |row| {
        let flake_rev: bool = row.get(2)?;
        Ok(EvalRow {
            id: row.get(0)?,
            json_output: row.get(1)?,
            flake_rev: flake_rev.then(|| row.get(3)).transpose()?,
        })
    })?
    .collect()
}

/// The inputs of a cached eval.
pub fn get_inputs(conn: &Connection, eval: &EvalRow) -> rusqlite::Result<Vec<Input>> {
    let mut inputs = Vec::new();
    let mut paths = conn.prepare_cached(
        "SELECT kind, scheme, path, value FROM eval_path_input WHERE cached_eval_id = ?1",
    )?;
    let rows = paths.query_map([eval.id], |row| {
        let invalid = |column, value: String| {
            rusqlite::Error::InvalidColumnType(column, value, rusqlite::types::Type::Text)
        };
        let kind: String = row.get(0)?;
        let scheme: String = row.get(1)?;
        let path: Vec<u8> = row.get(2)?;
        Ok(PathInput {
            kind: ObservedKind::parse(&kind).ok_or_else(|| invalid(0, kind))?,
            scheme: FlakeScheme::parse(&scheme).ok_or_else(|| invalid(1, scheme))?,
            path: PathBuf::from(OsStr::from_bytes(&path)),
            value: row.get(3)?,
        })
    })?;
    for row in rows {
        inputs.push(Input::Path(row?));
    }
    if let Some(content_hash) = &eval.flake_rev {
        inputs.push(Input::FlakeRev(RevInputDesc {
            content_hash: content_hash.clone(),
        }));
    }
    Ok(inputs)
}

/// Insert a cached eval with its inputs, replacing a candidate with the same
/// inputs and evicting the least recently used beyond [`MAX_CANDIDATES`].
pub fn insert_eval_with_inputs(
    tx: &Transaction<'_>,
    key_hash: &str,
    attr_name: &str,
    input_hash: &str,
    json_output: &str,
    inputs: &[Input],
) -> rusqlite::Result<i64> {
    tx.execute(
        "DELETE FROM cached_eval WHERE key_hash = ?1 AND input_hash = ?2",
        params![key_hash, input_hash],
    )?;
    let flake_rev = inputs.iter().find_map(|input| match input {
        Input::FlakeRev(rev) => Some(rev.content_hash.clone()),
        _ => None,
    });
    tx.execute(
        "INSERT INTO cached_eval
           (key_hash, attr_name, input_hash, json_output, flake_rev, flake_rev_hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            key_hash,
            attr_name,
            input_hash,
            json_output,
            flake_rev.is_some(),
            flake_rev.flatten(),
            now_millis()
        ],
    )?;
    let eval_id = tx.last_insert_rowid();

    let mut paths = tx.prepare_cached(
        "INSERT OR REPLACE INTO eval_path_input (cached_eval_id, kind, scheme, path, value)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for input in inputs {
        if let Input::Path(p) = input {
            paths.execute(params![
                eval_id,
                p.kind.as_str(),
                p.scheme.as_str(),
                p.path.as_os_str().as_bytes(),
                p.value
            ])?;
        }
    }

    tx.execute(
        "DELETE FROM cached_eval WHERE key_hash = ?1 AND id NOT IN (
           SELECT id FROM cached_eval WHERE key_hash = ?1
           ORDER BY updated_at DESC, id DESC LIMIT ?2)",
        params![key_hash, MAX_CANDIDATES],
    )?;
    let ttl = FILE_INPUT_TTL.as_millis() as i64;
    tx.execute(
        "DELETE FROM file_input WHERE updated_at < ?1",
        [now_millis() - ttl],
    )?;
    Ok(eval_id)
}

/// Remove every candidate of a key.
pub fn delete_evals(conn: &Connection, key_hash: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM cached_eval WHERE key_hash = ?1", [key_hash])?;
    Ok(())
}

/// Remove one candidate.
pub fn delete_eval(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM cached_eval WHERE id = ?1", [id])?;
    Ok(())
}

/// Ids of every candidate.
pub fn get_eval_ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM cached_eval")?;
    stmt.query_map([], |row| row.get(0))?.collect()
}

/// Mark a candidate as used now.
pub fn update_eval_updated_at(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE cached_eval SET updated_at = ?2 WHERE id = ?1",
        params![id, now_millis()],
    )?;
    Ok(())
}

/// [`FileHashes`] backed by the `file_input` table: a file whose size, inode,
/// mtime and ctime are unchanged keeps its recorded hash.
pub struct StatCache<'a> {
    conn: &'a Connection,
}

impl<'a> StatCache<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

impl FileHashes for StatCache<'_> {
    fn content_hash(&mut self, path: &Path) -> CacheResult<String> {
        let meta = std::fs::metadata(path)?;
        let mtime_ns = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
        let ctime_ns = meta.ctime() * 1_000_000_000 + meta.ctime_nsec();
        let path_bytes = path.as_os_str().as_bytes();
        let cached: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT content_hash FROM file_input
                 WHERE path = ?1 AND size = ?2 AND inode = ?3 AND mtime_ns = ?4 AND ctime_ns = ?5",
            )?
            .query_row(
                params![path_bytes, meta.size() as i64, meta.ino() as i64, mtime_ns, ctime_ns],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(hash) = cached {
            return Ok(hash);
        }

        let hash = compute_file_hash(path)?;
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        let racy = now_ns - RACY_WINDOW.as_nanos() as i64;
        if mtime_ns < racy && ctime_ns < racy {
            self.conn
                .prepare_cached(
                    "INSERT OR REPLACE INTO file_input
                       (path, size, inode, mtime_ns, ctime_ns, content_hash, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?
                .execute(params![
                    path_bytes,
                    meta.size() as i64,
                    meta.ino() as i64,
                    mtime_ns,
                    ctime_ns,
                    hash,
                    now_millis()
                ])?;
        }
        Ok(hash)
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
