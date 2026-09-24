//! The daemon's dev shell cache: which shells were evaluated, and which of
//! their environments stay pinned.
//!
//! `rho-devshell-builder`, run in workset processes' namespaces, checks
//! entries against their own checkouts, evaluates, and pins (see
//! `rho-devshell`); the daemon only keeps the entries and
//! decides what stays rooted. An entry lives as long as its environment is
//! in the Nix store. The [`PIN_BUDGET`] most recently used environments keep
//! a GC root in the shared cache directory; older ones are unpinned and
//! remain usable until Nix collects them, when using one pins it again.
//! One process owns the entries, so pinning and unpinning never race.
//! Clients reach it over [`protocol::socket_path`] ([`Store::serve`]).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
pub use rho_devshell::Candidate;
use rho_devshell::protocol::{self, Reply, Request};
use rho_devshell::{activations_dir, gc_root, roots_dir};
use senax_encoder::{Decode, Encode};

/// How many environments stay pinned, most recently used first.
pub const PIN_BUDGET: usize = 50;

/// Unpinned entries beyond this many are dropped, least recently used
/// first. Entries are small; this only bounds a store never collected.
pub const MAX_ENTRIES: usize = 1000;

const SHELLS_TABLE: &str = "devshell_shells";
const SHELLS: TableDefinition<u64, Sen<Entry>> = TableDefinition::new(SHELLS_TABLE);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct Entry {
    key: String,
    env_store_path: String,
    /// Opaque to the daemon: what the workset needs to check the entry.
    data: Vec<u8>,
    /// Logical time of the last store or use.
    used: u64,
    /// Whether the daemon keeps the environment's GC root.
    pinned: bool,
}

pub struct Store {
    db: RhoDb,
    /// The shared cache directory, at the path views bind it at.
    dir: PathBuf,
    state: tokio::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<u64, Entry>,
    clock: u64,
}

impl Store {
    /// Open the entries in `db`. Before any workset can pin, this
    /// reconciles the roots in `dir` with them: after a crash, a root may
    /// exist that no entry pins, or the reverse.
    pub async fn open(db: RhoDb, dir: PathBuf) -> Self {
        let state = load(&db, &dir).await;
        Self {
            db,
            dir,
            state: tokio::sync::Mutex::new(state),
        }
    }

    /// The entries of `key` whose environment still exists, most recently
    /// used first. Entries whose environment is gone are dropped.
    pub async fn lookup(&self, key: &str) -> Result<Vec<Candidate>> {
        self.with(|state, changes| {
            let mut found: Vec<(u64, &Entry)> = Vec::new();
            let mut gone = HashSet::new();
            for (id, entry) in state.entries.iter().filter(|(_, entry)| entry.key == key) {
                if Path::new(&entry.env_store_path).exists() {
                    found.push((*id, entry));
                } else {
                    gone.insert(entry.env_store_path.clone());
                }
            }
            found.sort_by_key(|(id, entry)| std::cmp::Reverse((entry.used, *id)));
            let found = found
                .into_iter()
                .map(|(id, entry)| Candidate {
                    id,
                    env_store_path: entry.env_store_path.clone(),
                    data: entry.data.clone(),
                })
                .collect();
            for env in gone {
                state.drop_env(&self.dir, &env, changes);
            }
            found
        })
        .await
    }

    /// Mark entry `id` as in use and keep its environment pinned. Returns
    /// whether its GC root is in place; if not, the caller pins it.
    pub async fn used(&self, id: u64) -> Result<bool> {
        self.with(|state, changes| {
            let Some(entry) = state.entries.get(&id) else {
                anyhow::bail!("no dev shell entry {id}");
            };
            let env = entry.env_store_path.clone();
            let rooted = is_rooted(&self.dir, &env);
            state.touch(id, changes);
            state.pin(&env, changes);
            state.evict(&self.dir, changes);
            Ok(rooted)
        })
        .await?
    }

    /// Record an evaluated shell whose environment the caller has pinned,
    /// replacing an entry of `key` with the same `data`.
    pub async fn store(&self, key: String, env_store_path: String, data: Vec<u8>) -> Result<u64> {
        self.with(|state, changes| {
            let same: Vec<u64> = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.key == key && entry.data == data)
                .map(|(id, _)| *id)
                .collect();
            for id in same {
                state.entries.remove(&id);
                changes.insert(id);
            }
            let id = state.entries.keys().next_back().map_or(1, |id| id + 1);
            state.entries.insert(
                id,
                Entry {
                    key,
                    env_store_path: env_store_path.clone(),
                    data,
                    used: 0,
                    pinned: true,
                },
            );
            state.touch(id, changes);
            state.pin(&env_store_path, changes);
            state.evict(&self.dir, changes);
            id
        })
        .await
    }

    /// Entry `id`'s environment is gone: drop every entry sharing it.
    pub async fn forget(&self, id: u64) -> Result<()> {
        self.with(|state, changes| {
            if let Some(env) = state.entries.get(&id).map(|entry| entry.env_store_path.clone()) {
                state.drop_env(&self.dir, &env, changes);
            }
        })
        .await
    }

    /// Listen on the socket in the shared cache directory, replacing a
    /// previous daemon's, and return the loop answering its clients.
    pub fn serve(self: Arc<Self>) -> Result<impl Future<Output = ()> + Send + 'static> {
        let path = protocol::socket_path(&self.dir);
        std::fs::create_dir_all(&self.dir)?;
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        Ok(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                tokio::spawn(self.clone().answer(stream));
            }
        })
    }

    /// Answer one client's requests in order.
    async fn answer(self: Arc<Self>, mut stream: tokio::net::UnixStream) {
        while let Ok(Some(request)) = protocol::read::<Request>(&mut stream).await {
            let result = match request {
                Request::Lookup(key) => self.lookup(&key).await.map(Reply::Candidates),
                Request::Used(id) => self.used(id).await.map(Reply::Rooted),
                Request::Store {
                    key,
                    env_store_path,
                    data,
                } => self.store(key, env_store_path, data).await.map(Reply::Stored),
                Request::Forget(id) => self.forget(id).await.map(|()| Reply::Done),
            };
            let reply = result.unwrap_or_else(|error| Reply::Error(format!("{error:#}")));
            if protocol::write(&mut stream, &reply).await.is_err() {
                return;
            }
        }
    }

    /// Run `f` on the loaded state and persist the entries it changed.
    async fn with<T>(&self, f: impl FnOnce(&mut State, &mut HashSet<u64>) -> T) -> Result<T> {
        let mut state = self.state.lock().await;
        let mut changes = HashSet::new();
        let result = f(&mut state, &mut changes);
        if !changes.is_empty() {
            let mut write = self.db.write().await;
            {
                let mut table = write.open_table(SHELLS);
                for id in changes {
                    match state.entries.get(&id) {
                        Some(entry) => {
                            table.insert(id, SenValue::borrowed(entry));
                        }
                        None => {
                            table.remove(id);
                        }
                    }
                }
            }
            write.commit();
        }
        Ok(result)
    }
}

/// Read the entries and reconcile the roots directory with them.
async fn load(db: &RhoDb, dir: &Path) -> State {
    let mut state = State::default();
    {
        let read = db.read();
        let table = read
            .has_table(SHELLS_TABLE)
            .then(|| read.open_table(SHELLS));
        for (id, entry) in table.iter().flat_map(|table| table.iter()) {
            let entry = entry.value().into_owned();
            state.clock = state.clock.max(entry.used);
            state.entries.insert(id.value(), entry);
        }
    }
    let pinned: HashSet<PathBuf> = state
        .entries
        .values()
        .filter(|entry| entry.pinned)
        .map(|entry| root_of(dir, &entry.env_store_path))
        .collect();
    if let Ok(roots) = std::fs::read_dir(roots_dir(dir)) {
        for root in roots.flatten() {
            if !pinned.contains(&root.path()) {
                let _ = std::fs::remove_file(root.path());
            }
        }
    }
    let mut changes = HashSet::new();
    for (id, entry) in &mut state.entries {
        if entry.pinned && !is_rooted(dir, &entry.env_store_path) {
            entry.pinned = false;
            changes.insert(*id);
        }
    }
    if !changes.is_empty() {
        let mut write = db.write().await;
        {
            let mut table = write.open_table(SHELLS);
            for id in changes {
                table.insert(id, SenValue::borrowed(&state.entries[&id]));
            }
        }
        write.commit();
    }
    state
}

impl State {
    fn touch(&mut self, id: u64, changes: &mut HashSet<u64>) {
        self.clock += 1;
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.used = self.clock;
            changes.insert(id);
        }
    }

    /// Record that `env` is pinned, for every entry sharing it.
    fn pin(&mut self, env: &str, changes: &mut HashSet<u64>) {
        for (id, entry) in &mut self.entries {
            if entry.env_store_path == env && !entry.pinned {
                entry.pinned = true;
                changes.insert(*id);
            }
        }
    }

    /// Unpin environments beyond [`PIN_BUDGET`] and drop unpinned entries
    /// beyond [`MAX_ENTRIES`], least recently used first.
    fn evict(&mut self, dir: &Path, changes: &mut HashSet<u64>) {
        let mut pinned: BTreeMap<&str, u64> = BTreeMap::new();
        for entry in self.entries.values().filter(|entry| entry.pinned) {
            let used = pinned.entry(&entry.env_store_path).or_default();
            *used = (*used).max(entry.used);
        }
        let mut pinned: Vec<(&str, u64)> = pinned.into_iter().collect();
        pinned.sort_by_key(|(_, used)| std::cmp::Reverse(*used));
        let unpin: Vec<String> = pinned
            .into_iter()
            .skip(PIN_BUDGET)
            .map(|(env, _)| env.to_owned())
            .collect();
        for env in unpin {
            let _ = std::fs::remove_file(root_of(dir, &env));
            for (id, entry) in &mut self.entries {
                if entry.env_store_path == env {
                    entry.pinned = false;
                    changes.insert(*id);
                }
            }
        }
        if self.entries.len() > MAX_ENTRIES {
            let mut unpinned: Vec<(u64, u64)> = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.pinned)
                .map(|(id, entry)| (entry.used, *id))
                .collect();
            unpinned.sort();
            let excess = self.entries.len() - MAX_ENTRIES;
            for (_, id) in unpinned.into_iter().take(excess) {
                let entry = self.entries.remove(&id).unwrap();
                changes.insert(id);
                self.forget_activation(dir, &entry.env_store_path);
            }
        }
    }

    /// Drop every entry of `env`, its root and its activation script.
    fn drop_env(&mut self, dir: &Path, env: &str, changes: &mut HashSet<u64>) {
        self.entries.retain(|id, entry| {
            let keep = entry.env_store_path != env;
            if !keep {
                changes.insert(*id);
            }
            keep
        });
        let _ = std::fs::remove_file(root_of(dir, env));
        self.forget_activation(dir, env);
    }

    /// Remove `env`'s activation scripts once no entry uses it.
    fn forget_activation(&self, dir: &Path, env: &str) {
        if !self.entries.values().any(|entry| entry.env_store_path == env) {
            let _ = std::fs::remove_dir_all(activations_dir(dir, env));
        }
    }
}

fn root_of(dir: &Path, env: &str) -> PathBuf {
    gc_root(&roots_dir(dir), env)
}

/// Whether `env`'s GC root exists and points at it.
fn is_rooted(dir: &Path, env: &str) -> bool {
    std::fs::read_link(root_of(dir, env)).is_ok_and(|target| target == Path::new(env))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        temp: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let store = Self::open(&temp).await;
            Self { temp, store }
        }

        async fn open(temp: &tempfile::TempDir) -> Store {
            std::fs::create_dir_all(temp.path().join("cache/roots")).unwrap();
            Store::open(RhoDb::open(temp.path().join("db")), temp.path().join("cache")).await
        }

        /// A stand-in store path that exists, pinned as a worker would.
        fn env(&self, name: &str) -> String {
            let path = self.temp.path().join(name);
            std::fs::write(&path, "").unwrap();
            let env = path.to_str().unwrap().to_owned();
            self.pin(&env);
            env
        }

        fn pin(&self, env: &str) {
            let _ = std::fs::remove_file(self.root(env));
            std::os::unix::fs::symlink(env, self.root(env)).unwrap();
        }

        fn root(&self, env: &str) -> PathBuf {
            root_of(&self.temp.path().join("cache"), env)
        }

        async fn ids(&self, key: &str) -> Vec<u64> {
            self.store.lookup(key).await.unwrap().into_iter().map(|c| c.id).collect()
        }
    }

    #[tokio::test]
    async fn lookup_is_newest_used_first_and_same_data_replaces() {
        let f = Fixture::new().await;
        let (a, b) = (f.env("a"), f.env("b"));
        let one = f.store.store("k".into(), a.clone(), b"one".to_vec()).await.unwrap();
        let two = f.store.store("k".into(), b, b"two".to_vec()).await.unwrap();
        f.store.store("other".into(), a.clone(), b"one".to_vec()).await.unwrap();
        assert_eq!(f.ids("k").await, [two, one]);
        assert!(f.store.used(one).await.unwrap());
        assert_eq!(f.ids("k").await, [one, two]);
        let again = f.store.store("k".into(), a, b"one".to_vec()).await.unwrap();
        assert_eq!(f.ids("k").await, [again, two]);
    }

    #[tokio::test]
    async fn gone_environments_drop_their_entries_and_root() {
        let f = Fixture::new().await;
        let a = f.env("a");
        let id = f.store.store("k".into(), a.clone(), vec![]).await.unwrap();
        f.store.store("other".into(), a.clone(), vec![1]).await.unwrap();
        std::fs::remove_file(&a).unwrap();
        assert!(f.ids("k").await.is_empty());
        assert!(f.ids("other").await.is_empty());
        assert!(f.root(&a).symlink_metadata().is_err());
        assert!(f.store.used(id).await.is_err());
    }

    #[tokio::test]
    async fn only_the_budget_of_recently_used_environments_stays_pinned() {
        let f = Fixture::new().await;
        let mut ids = Vec::new();
        let mut envs = Vec::new();
        for n in 0..=PIN_BUDGET {
            let env = f.env(&format!("env{n}"));
            ids.push(f.store.store(format!("k{n}"), env.clone(), vec![]).await.unwrap());
            envs.push(env);
        }
        // The oldest lost its root but is still found; using it re-pins it
        // at the expense of the next oldest.
        assert!(f.root(&envs[0]).symlink_metadata().is_err());
        assert!(f.root(&envs[1]).symlink_metadata().is_ok());
        assert_eq!(f.ids("k0").await, [ids[0]]);
        assert!(!f.store.used(ids[0]).await.unwrap());
        f.pin(&envs[0]);
        assert!(f.root(&envs[1]).symlink_metadata().is_err());
        assert!(f.store.used(ids[0]).await.unwrap());
    }

    #[tokio::test]
    async fn loading_reconciles_roots_with_entries() {
        let f = Fixture::new().await;
        let (a, b) = (f.env("a"), f.env("b"));
        let id = f.store.store("k".into(), a.clone(), vec![]).await.unwrap();
        // A root no entry pins (a crash between pin and store), and a
        // pinned entry whose root is missing.
        std::fs::remove_file(f.root(&a)).unwrap();
        let (root_b, Fixture { temp, store }) = (f.root(&b), f);
        drop(store);
        let store = Fixture::open(&temp).await;
        store.lookup("k").await.unwrap();
        assert!(root_b.symlink_metadata().is_err());
        assert!(!store.used(id).await.unwrap());
    }
}
