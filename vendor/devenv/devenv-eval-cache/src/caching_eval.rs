//! The cache service: look up a valid candidate or store a new evaluation.

use std::collections::HashMap;
use std::path::PathBuf;

use devenv_cache_core::{CacheResult, Database};
use tracing::{debug, trace};

use crate::db::{self, StatCache};
use crate::eval_inputs::{Checkout, Input, InputIdentity};
use crate::ffi_cache::EvalCacheKey;

/// A valid cached result.
#[derive(Clone, Debug)]
pub struct CachedEvalResult {
    pub json_output: String,
    pub eval_id: i64,
    /// The inputs it was validated against.
    pub inputs: Vec<Input>,
}

pub struct CachingEvalService {
    db: Database,
}

impl CachingEvalService {
    /// Open (creating or recreating as needed) the cache database at `path`.
    pub fn open(path: PathBuf) -> CacheResult<Self> {
        Ok(Self {
            db: Database::new(path, db::MIGRATIONS)?,
        })
    }

    /// File hashes backed by this cache.
    pub fn hashes(&self) -> StatCache<'_> {
        StatCache::new(self.db.conn())
    }

    /// Check for a candidate of `key` whose inputs all match `checkout` now.
    ///
    /// Returns `None` if the cache is empty or every candidate is stale.
    pub fn get_cached(
        &self,
        key: &EvalCacheKey,
        checkout: &Checkout,
    ) -> CacheResult<Option<CachedEvalResult>> {
        let conn = self.db.conn();
        let evals = db::get_evals_by_key_hash(conn, &key.key_hash)?;
        if evals.is_empty() {
            debug!(key_hash = %key.key_hash, "eval not found in cache");
            return Ok(None);
        }

        // Candidates share most inputs; recapture each identity once.
        let mut hashes = StatCache::new(conn);
        let mut current: HashMap<InputIdentity, Option<Input>> = HashMap::new();
        for eval in evals {
            let inputs = db::get_inputs(conn, &eval)?;
            let valid = inputs.iter().all(|input| {
                let now = current.entry(input.identity()).or_insert_with(|| {
                    input
                        .recapture(checkout, &mut hashes)
                        .inspect_err(|e| trace!(error = %e, ?input, "error checking input"))
                        .ok()
                });
                let valid = now.as_ref() == Some(input);
                if !valid {
                    trace!(eval_id = eval.id, ?input, "input changed");
                }
                valid
            });
            if valid {
                db::update_eval_updated_at(conn, eval.id)?;
                debug!(key_hash = %key.key_hash, attr_name = %key.attr_name, eval_id = eval.id, "cache hit");
                return Ok(Some(CachedEvalResult {
                    json_output: eval.json_output,
                    eval_id: eval.id,
                    inputs,
                }));
            }
        }
        debug!(
            key_hash = %key.key_hash,
            attr_name = %key.attr_name,
            "cached evals invalidated due to input changes"
        );
        Ok(None)
    }

    /// Store a new eval result with its inputs.
    pub fn store(
        &mut self,
        key: &EvalCacheKey,
        json_output: &str,
        inputs: &[Input],
    ) -> CacheResult<i64> {
        let input_hash = Input::compute_input_hash(inputs);
        let tx = self
            .db
            .conn_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let eval_id = db::insert_eval_with_inputs(
            &tx,
            &key.key_hash,
            &key.attr_name,
            &input_hash,
            json_output,
            inputs,
        )?;
        tx.commit()?;
        debug!(
            key_hash = %key.key_hash,
            attr_name = %key.attr_name,
            num_inputs = inputs.len(),
            eval_id,
            "stored eval result in cache"
        );
        Ok(eval_id)
    }

    /// Remove one candidate, e.g. when its store paths were garbage
    /// collected.
    pub fn remove(&self, eval_id: i64) -> CacheResult<()> {
        db::delete_eval(self.db.conn(), eval_id)?;
        Ok(())
    }

    /// Ids of every stored candidate, for pruning per-candidate state kept
    /// outside the database such as GC roots.
    pub fn eval_ids(&self) -> CacheResult<Vec<i64>> {
        Ok(db::get_eval_ids(self.db.conn())?)
    }

    /// Remove every cached candidate of `key`, e.g. when a cached result is
    /// no longer usable because its store paths were garbage collected.
    pub fn invalidate(&self, key: &EvalCacheKey) -> CacheResult<()> {
        db::delete_evals(self.db.conn(), &key.key_hash)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval_inputs::{FlakeScheme, Uncached};
    use crate::ffi_cache::record_inputs;
    use devenv_core::eval_op::{EvalOp, ObservedKind};
    use std::path::Path;
    use tempfile::TempDir;

    const ROOT_MOUNT: &str = "/nix/store/00000000000000000000000000000000-source";

    /// What Nix would report evaluating a flake that imports `nix/a.nix` and
    /// copies `src`.
    fn ops(root: &Path) -> Vec<EvalOp> {
        let checkout = Checkout::new(root, FlakeScheme::Path).unwrap();
        let mut ops = vec![EvalOp::MountedInput {
            store_path: ROOT_MOUNT.into(),
            url: format!("path:{}?lastModified=1", root.display()),
        }];
        for (path, kind) in [
            ("flake.nix", ObservedKind::File),
            ("nix/a.nix", ObservedKind::File),
            ("src", ObservedKind::Dir),
            ("src/main.rs", ObservedKind::Stat),
            ("src/main.rs", ObservedKind::File),
        ] {
            ops.push(EvalOp::Observed {
                source: Path::new(ROOT_MOUNT).join(path),
                kind,
                value: checkout.observe(FlakeScheme::Path, Path::new(path), kind, &mut Uncached).unwrap(),
            });
        }
        ops
    }

    fn key() -> EvalCacheKey {
        EvalCacheKey::new("devShells.x86_64-linux.default", FlakeScheme::Path, &[b"{}"])
    }

    fn make_checkout(dir: &Path, a: &str) {
        std::fs::create_dir_all(dir.join("nix")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("flake.nix"), "{ }").unwrap();
        std::fs::write(dir.join("nix/a.nix"), a).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    }

    fn record(dir: &Path) -> Vec<Input> {
        record_inputs(&ops(dir), dir).unwrap().1.into_inputs()
    }

    fn lookup(cache: &CachingEvalService, dir: &Path) -> Option<String> {
        let checkout = Checkout::new(dir, FlakeScheme::Path).unwrap();
        cache.get_cached(&key(), &checkout).unwrap().map(|hit| hit.json_output)
    }

    #[test]
    fn candidates_are_shared_across_checkouts_and_validated() {
        let tmp = TempDir::new().unwrap();
        let (one, two) = (tmp.path().join("one"), tmp.path().join("two"));
        make_checkout(&one, "1");
        make_checkout(&two, "2");
        let mut cache = CachingEvalService::open(tmp.path().join("cache.sqlite")).unwrap();

        cache.store(&key(), "one", &record(&one)).unwrap();
        assert_eq!(lookup(&cache, &two), None);
        cache.store(&key(), "two", &record(&two)).unwrap();

        assert_eq!(lookup(&cache, &one).as_deref(), Some("one"));
        assert_eq!(lookup(&cache, &two).as_deref(), Some("two"));
        std::fs::write(two.join("nix/a.nix"), "1").unwrap();
        assert_eq!(lookup(&cache, &two).as_deref(), Some("one"));

        // Files the evaluator never read do not matter.
        std::fs::write(one.join("unread"), "x").unwrap();
        assert_eq!(lookup(&cache, &one).as_deref(), Some("one"));

        // Edits and new entries in a copied tree invalidate.
        std::fs::write(one.join("src/main.rs"), "fn main() { loop {} }").unwrap();
        assert_eq!(lookup(&cache, &one), None);
        std::fs::write(two.join("src/lib.rs"), "").unwrap();
        assert_eq!(lookup(&cache, &two), None);

        let other = EvalCacheKey::new("devShells.x86_64-linux.default", FlakeScheme::Path, &[b"{ }"]);
        let checkout = Checkout::new(&one, FlakeScheme::Path).unwrap();
        assert!(cache.get_cached(&other, &checkout).unwrap().is_none());
    }

    #[test]
    fn storing_identical_inputs_replaces_the_candidate() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("one");
        make_checkout(&dir, "1");
        let mut cache = CachingEvalService::open(tmp.path().join("cache.sqlite")).unwrap();
        cache.store(&key(), "old", &record(&dir)).unwrap();
        cache.store(&key(), "new", &record(&dir)).unwrap();
        assert_eq!(lookup(&cache, &dir).as_deref(), Some("new"));
        let rows: i64 = cache
            .db
            .conn()
            .query_row("SELECT count(*) FROM cached_eval", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn stat_cache_answers_for_unchanged_files_only() {
        use crate::eval_inputs::FileHashes;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, "a").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        let cache = CachingEvalService::open(tmp.path().join("cache.sqlite")).unwrap();

        let first = cache.hashes().content_hash(&file).unwrap();
        // The row is only trusted while the stat matches; ctime is recent
        // here (set_modified bumps it), so nothing was cached yet.
        let rows = |cache: &CachingEvalService| -> i64 {
            cache.db.conn().query_row("SELECT count(*) FROM file_input", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(rows(&cache), 0);
        std::fs::write(&file, "b").unwrap();
        assert_ne!(first, cache.hashes().content_hash(&file).unwrap());
    }
}
