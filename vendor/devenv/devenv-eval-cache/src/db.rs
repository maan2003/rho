//! SQLite store of evaluated development shells.
//!
//! A cache key names everything an evaluation depends on that is not a
//! recorded input: the shell attribute, the system, how the flake is fetched,
//! `flake.lock`, and the evaluator version. One key can hold several
//! candidates, one per distinct input state seen (for example two worktrees
//! on different branches); [`EnvCache::lookup`] returns the most recently
//! used candidate whose inputs all still match.

use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::inputs::{FlakeScheme, FlakeView, Input, InputId, Kind, Root};

/// Candidates kept per key; older ones are evicted on store.
const MAX_CANDIDATES: i64 = 8;

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE shell (
  id             INTEGER NOT NULL PRIMARY KEY,
  key_hash       TEXT NOT NULL,
  drv_path       TEXT NOT NULL,
  env_store_path TEXT NOT NULL,
  env_json       TEXT NOT NULL,
  created_at     INTEGER NOT NULL,
  used_at        INTEGER NOT NULL
);
CREATE INDEX shell_key ON shell(key_hash, used_at);

CREATE TABLE shell_input (
  shell_id INTEGER NOT NULL REFERENCES shell(id) ON DELETE CASCADE,
  root     TEXT NOT NULL,
  kind     TEXT NOT NULL,
  path     TEXT NOT NULL,
  state    TEXT NOT NULL,
  PRIMARY KEY (shell_id, root, kind, path)
) WITHOUT ROWID;
";

/// Identity of an evaluation apart from its recorded inputs.
#[derive(Clone, Debug)]
pub struct CacheKey {
    pub system: String,
    pub shell: String,
    pub scheme: FlakeScheme,
    /// Contents of `flake.lock`, or `None` if the flake has none.
    pub lock: Option<Vec<u8>>,
    /// Anything else that changes evaluation results, such as the evaluator
    /// and Nix versions.
    pub evaluator: String,
}

impl CacheKey {
    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        for part in [
            "rho-dev-shell-v1",
            &self.system,
            &self.shell,
            self.scheme.as_str(),
            &self.evaluator,
        ] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        match &self.lock {
            Some(lock) => h.update(b"lock").update(blake3::hash(lock).as_bytes()),
            None => h.update(b"nolock"),
        };
        h.finalize().to_hex().to_string()
    }
}

/// A cached shell as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedShell {
    pub drv_path: String,
    pub env_store_path: String,
    pub env_json: String,
}

pub struct EnvCache {
    conn: Connection,
}

impl EnvCache {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        // WAL lets concurrent evaluators and validators share the file.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut cache = Self { conn };
        cache.migrate()?;
        Ok(cache)
    }

    fn migrate(&mut self) -> rusqlite::Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            // A cache: anything unknown is dropped rather than migrated.
            tx.execute_batch("DROP TABLE IF EXISTS shell_input; DROP TABLE IF EXISTS shell;")?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit()
    }

    /// Find a candidate for `key` whose inputs match `flake_dir` now.
    pub fn lookup(
        &self,
        key: &CacheKey,
        flake_dir: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> rusqlite::Result<Option<CachedShell>> {
        let key_hash = key.hash();
        let mut candidates = self.conn.prepare_cached(
            "SELECT id FROM shell WHERE key_hash = ?1 ORDER BY used_at DESC, id DESC",
        )?;
        let ids: Vec<i64> = candidates
            .query_map([&key_hash], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if ids.is_empty() {
            return Ok(None);
        }

        let mut per_candidate = Vec::with_capacity(ids.len());
        for id in &ids {
            per_candidate.push((*id, self.inputs(*id)?));
        }
        let mut view = FlakeView::new(flake_dir, key.scheme);
        if view
            .prefetch(per_candidate.iter().flat_map(|(_, inputs)| inputs.iter().map(|i| &i.id)))
            .is_err()
        {
            return Ok(None);
        }
        // Inputs repeat across candidates; compute each current state once.
        let mut current: HashMap<InputId, Option<String>> = HashMap::new();
        for (id, inputs) in per_candidate {
            let matches = inputs.iter().all(|input| {
                let state = current
                    .entry(input.id.clone())
                    .or_insert_with(|| view.state(&input.id, env).ok());
                state.as_deref() == Some(input.state.as_str())
            });
            if matches {
                self.conn.execute(
                    "UPDATE shell SET used_at = ?2 WHERE id = ?1",
                    params![id, now()],
                )?;
                return self
                    .conn
                    .query_row(
                        "SELECT drv_path, env_store_path, env_json FROM shell WHERE id = ?1",
                        [id],
                        |r| {
                            Ok(CachedShell {
                                drv_path: r.get(0)?,
                                env_store_path: r.get(1)?,
                                env_json: r.get(2)?,
                            })
                        },
                    )
                    .optional();
            }
        }
        Ok(None)
    }

    fn inputs(&self, shell_id: i64) -> rusqlite::Result<Vec<Input>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT root, kind, path, state FROM shell_input WHERE shell_id = ?1")?;
        let rows = stmt.query_map([shell_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut inputs = Vec::new();
        for row in rows {
            let (root, kind, path, state) = row?;
            let (Some(root), Some(kind)) = (Root::parse(&root), Kind::parse(&kind)) else {
                // Unknown input kinds can never be validated; the candidate
                // must not match.
                inputs.push(Input {
                    id: InputId { root: Root::Env, kind: Kind::Env, path: String::new() },
                    state: "\0unknown".into(),
                });
                continue;
            };
            inputs.push(Input { id: InputId { root, kind, path }, state });
        }
        Ok(inputs)
    }

    /// Store a new candidate for `key`, evicting the least recently used beyond [`MAX_CANDIDATES`].
    pub fn store(
        &mut self,
        key: &CacheKey,
        inputs: &[Input],
        shell: &CachedShell,
    ) -> rusqlite::Result<()> {
        let key_hash = key.hash();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now();
        tx.execute(
            "INSERT INTO shell (key_hash, drv_path, env_store_path, env_json, created_at, used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![key_hash, shell.drv_path, shell.env_store_path, shell.env_json, now],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO shell_input (shell_id, root, kind, path, state)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for input in inputs {
                insert.execute(params![
                    id,
                    input.id.root.as_str(),
                    input.id.kind.as_str(),
                    input.id.path,
                    input.state
                ])?;
            }
        }
        tx.execute(
            "DELETE FROM shell WHERE key_hash = ?1 AND id NOT IN (
               SELECT id FROM shell WHERE key_hash = ?1 ORDER BY used_at DESC, id DESC LIMIT ?2)",
            params![key_hash, MAX_CANDIDATES],
        )?;
        tx.commit()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}


#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use devenv_core::eval_op::EvalOp;

    use super::*;
    use crate::inputs::record_inputs;

    fn ops(flake: &Path) -> Vec<EvalOp> {
        let mount = PathBuf::from("/nix/store/00000000000000000000000000000000-source");
        vec![
            EvalOp::MountedInput {
                store_path: mount.clone(),
                url: format!("path:{}?lastModified=1", flake.display()),
            },
            EvalOp::MountedInput {
                store_path: "/nix/store/11111111111111111111111111111111-source".into(),
                url: "github:NixOS/nixpkgs/abc".into(),
            },
            EvalOp::EvaluatedFile { source: flake.join("flake.nix"), cached: false },
            EvalOp::ReadFile { source: mount.join("nix/a.nix") },
            EvalOp::ReadFile { source: "/nix/store/11111111111111111111111111111111-source/lib.nix".into() },
            EvalOp::EvaluatedFile { source: "«nix-internal»/derivation-internal.nix".into(), cached: false },
            EvalOp::GetEnv { name: "RHO_TEST_UNSET_VAR".into() },
        ]
    }

    fn key() -> CacheKey {
        CacheKey {
            system: "x86_64-linux".into(),
            shell: "default".into(),
            scheme: FlakeScheme::Path,
            lock: Some(b"{}".to_vec()),
            evaluator: "test".into(),
        }
    }

    fn shell(n: &str) -> CachedShell {
        CachedShell { drv_path: n.into(), env_store_path: n.into(), env_json: n.into() }
    }

    fn checkout(dir: &Path, a: &str) {
        std::fs::create_dir_all(dir.join("nix")).unwrap();
        std::fs::write(dir.join("flake.nix"), "{ }").unwrap();
        std::fs::write(dir.join("nix/a.nix"), a).unwrap();
    }

    fn record(dir: &Path) -> Vec<Input> {
        let started = SystemTime::now() + Duration::from_secs(3600);
        record_inputs(&ops(dir), dir, started, &|_| None).unwrap().inputs
    }

    #[test]
    fn inputs_are_relative_and_skip_locked_and_builtin_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("one");
        checkout(&dir, "1");
        let ids: Vec<_> = record(&dir).into_iter().map(|i| (i.id.root, i.id.kind, i.id.path)).collect();
        assert_eq!(
            ids,
            vec![
                (Root::Flake, Kind::File, "flake.nix".into()),
                (Root::Flake, Kind::File, "nix/a.nix".into()),
                (Root::Env, Kind::Env, "RHO_TEST_UNSET_VAR".into()),
            ]
        );
    }

    #[test]
    fn candidates_are_shared_across_checkouts_and_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let (one, two) = (tmp.path().join("one"), tmp.path().join("two"));
        checkout(&one, "1");
        checkout(&two, "2");
        let mut cache = EnvCache::open(&tmp.path().join("cache.sqlite")).unwrap();
        let env = |_: &str| None;

        cache.store(&key(), &record(&one), &shell("one")).unwrap();
        assert_eq!(cache.lookup(&key(), &two, &env).unwrap(), None);
        cache.store(&key(), &record(&two), &shell("two")).unwrap();

        assert_eq!(cache.lookup(&key(), &one, &env).unwrap(), Some(shell("one")));
        assert_eq!(cache.lookup(&key(), &two, &env).unwrap(), Some(shell("two")));
        std::fs::write(two.join("nix/a.nix"), "1").unwrap();
        assert_eq!(cache.lookup(&key(), &two, &env).unwrap(), Some(shell("one")));

        let other_lock = CacheKey { lock: None, ..key() };
        assert_eq!(cache.lookup(&other_lock, &one, &env).unwrap(), None);
    }

    #[test]
    fn inputs_modified_during_eval_are_not_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("one");
        checkout(&dir, "1");
        let started = SystemTime::now() - Duration::from_secs(3600);
        let err = record_inputs(&ops(&dir), &dir, started, &|_| None).unwrap_err();
        assert!(matches!(err, crate::inputs::RecordError::ChangedDuringEval(_)));
    }
}
