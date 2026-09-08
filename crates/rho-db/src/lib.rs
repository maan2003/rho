//! Thin redb helpers for rho.
//!
//! Callers own table definitions and schema. `rho-db` only provides `Sen<T>`
//! for senax-backed redb keys/values and small transaction wrappers that treat
//! local database errors as fatal.

use std::any::Any;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use bytes::BytesMut;
use redb::{
    AccessGuard, Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle, TypeName,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub mod client;

const CACHE_SIZE: usize = 10 * 1024 * 1024;

/// redb key/value wrapper using senax encoding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sen<T>(pub T);

pub enum SenValue<'a, T> {
    Owned(T),
    Borrowed(&'a dyn senax_encoder::Encoder),
}

impl<T: Debug> Debug for SenValue<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(value) => f.debug_tuple("Owned").field(value).finish(),
            Self::Borrowed(_) => f.write_str("Borrowed(..)"),
        }
    }
}

impl<'a, T> SenValue<'a, T> {
    pub fn owned(value: T) -> Self {
        Self::Owned(value)
    }

    pub fn borrowed(value: &'a impl senax_encoder::Encoder) -> Self {
        Self::Borrowed(value)
    }

    fn owned_ref(&self) -> &T {
        match self {
            Self::Owned(value) => value,
            Self::Borrowed(_) => panic!("borrowed sen value has no owned reference"),
        }
    }
}

impl<T> AsRef<T> for SenValue<'_, T> {
    fn as_ref(&self) -> &T {
        self.owned_ref()
    }
}

impl<T: Clone> SenValue<'_, T> {
    pub fn into_owned(self) -> T {
        match self {
            Self::Owned(value) => value,
            Self::Borrowed(_) => panic!("borrowed sen value cannot be converted to owned"),
        }
    }
}

/// redb-backed rho database handle.
#[derive(Clone, Debug)]
pub struct RhoDb {
    database: Arc<Database>,
    write_lock: Arc<Mutex<()>>,
    /// One slot for the owning crate's observer of this database. Writers
    /// deep inside a transaction publish what they wrote through it, so no
    /// call site has to carry a channel down to the table.
    observer: Arc<OnceLock<Box<dyn Any + Send + Sync>>>,
    /// An exclusive lock on the file, for a database that was opened
    /// through one. Held here so it lives exactly as long as the handle
    /// does: the lock's whole purpose is to say the file is in use, and a
    /// lock dropped at the end of the call that took it says nothing.
    _lock: Option<Arc<std::fs::File>>,
}

/// Read transaction wrapper. Methods panic on local database errors.
pub struct ReadTxn {
    inner: redb::ReadTransaction,
}

/// Write transaction wrapper. Methods panic on local database errors.
pub struct WriteTxn {
    inner: redb::WriteTransaction,
    _guard: OwnedMutexGuard<()>,
    observer: Arc<OnceLock<Box<dyn Any + Send + Sync>>>,
    /// Run after the commit lands, never before: a reader woken by one of
    /// these must find the write already durable.
    after_commit: Vec<Box<dyn FnOnce() + Send>>,
}

/// Read-only table wrapper. Methods panic on local database errors.
pub struct ReadTable<K: redb::Key + 'static, V: redb::Value + 'static> {
    inner: redb::ReadOnlyTable<K, V>,
}

/// Mutable table wrapper. Methods panic on local database errors.
pub struct WriteTable<'txn, K: redb::Key + 'static, V: redb::Value + 'static> {
    inner: redb::Table<'txn, K, V>,
}

/// Double-ended table iterator wrapper. Iteration panics on local database
/// errors.
pub struct Iter<'a, K: redb::Key + 'static, V: redb::Value + 'static> {
    inner: redb::Range<'a, K, V>,
}

impl<T> Deref for Sen<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Sen<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> redb::Value for Sen<T>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder + Debug,
{
    type SelfType<'a>
        = SenValue<'a, T>
    where
        Self: 'a;

    type AsBytes<'a>
        = BytesMut
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut data = data;
        SenValue::Owned(T::decode(&mut data).expect("senax decode rho-db value"))
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let mut bytes = BytesMut::new();
        match value {
            SenValue::Owned(value) => value.encode(&mut bytes),
            SenValue::Borrowed(value) => value.encode(&mut bytes),
        }
        .expect("senax encode rho-db value");
        bytes
    }

    fn type_name() -> TypeName {
        TypeName::new(&format!("rho-db::Sen<{}>", std::any::type_name::<T>()))
    }
}

impl<T> redb::Key for Sen<T>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder + Debug,
{
    fn compare(data1: &[u8], data2: &[u8]) -> Ordering {
        data1.cmp(data2)
    }
}

/// The name redb recorded for a table's key or value type. A migration
/// reads rows the old code wrote, and redb checks the name the table was
/// created with, so the reader has to answer to a name its own types no
/// longer have.
pub trait RecordedTypeName {
    const NAME: &'static str;
}

/// A [`Sen`] that answers to a recorded name rather than to `T`'s own. It
/// encodes and decodes exactly as `Sen<T>` does; only the name differs.
#[derive(Debug)]
pub struct SenAs<T, N>(std::marker::PhantomData<(T, N)>);

impl<T, N> redb::Value for SenAs<T, N>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder + Debug,
    N: RecordedTypeName + Debug,
{
    type SelfType<'a>
        = SenValue<'a, T>
    where
        Self: 'a;

    type AsBytes<'a>
        = BytesMut
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        <Sen<T> as redb::Value>::from_bytes(data)
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        <Sen<T> as redb::Value>::as_bytes(value)
    }

    fn type_name() -> TypeName {
        TypeName::new(N::NAME)
    }
}

impl<T, N> redb::Key for SenAs<T, N>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder + Debug,
    N: RecordedTypeName + Debug,
{
    fn compare(data1: &[u8], data2: &[u8]) -> Ordering {
        data1.cmp(data2)
    }
}

/// A [`Sen`] read without trusting the bytes to decode. A row a newer
/// build wrote can name an enum variant this one has never heard of, and
/// decoding it through [`Sen`] aborts the process inside redb; read
/// through this and the row comes back as `None` for the caller to skip.
/// Read-only by construction: encoding is what the up-to-date type is for.
#[derive(Debug)]
pub struct Lenient<T>(std::marker::PhantomData<T>);

impl<T> redb::Value for Lenient<T>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder + Debug,
{
    type SelfType<'a>
        = Option<T>
    where
        Self: 'a;

    type AsBytes<'a>
        = BytesMut
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut data = data;
        T::decode(&mut data).ok()
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let mut bytes = BytesMut::new();
        value
            .as_ref()
            .expect("a lenient table is read through, never written to")
            .encode(&mut bytes)
            .expect("senax encode rho-db value");
        bytes
    }

    /// The name [`Sen<T>`] records, so the same table can be opened either
    /// way.
    fn type_name() -> TypeName {
        <Sen<T> as redb::Value>::type_name()
    }
}

impl<T> redb::Key for Lenient<T>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder + Debug,
{
    fn compare(data1: &[u8], data2: &[u8]) -> Ordering {
        data1.cmp(data2)
    }
}

impl RhoDb {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create rho-db parent directory");
        }

        let database = Database::builder()
            .set_cache_size(CACHE_SIZE)
            .create(path)
            .expect("open rho-db");

        Self {
            database: Arc::new(database),
            write_lock: Arc::new(Mutex::new(())),
            observer: Arc::new(OnceLock::new()),
            _lock: None,
        }
    }

    /// The same database, holding `lock` for as long as it lives.
    fn holding(mut self, lock: std::fs::File) -> Self {
        self._lock = Some(Arc::new(lock));
        self
    }

    /// Print what the file at `path` holds: bytes stored per table, and the
    /// pages the file allocates overall. Opens the file exclusively.
    pub fn print_stats(path: impl AsRef<Path>) -> anyhow::Result<()> {
        use redb::ReadableTableMetadata;
        let database = Database::builder()
            .set_cache_size(CACHE_SIZE)
            .open(path.as_ref())?;
        let read = database.begin_read()?;
        let write = database.begin_write()?;
        let mut rows = Vec::new();
        for handle in read.list_tables()? {
            let table = read.open_untyped_table(handle.clone())?;
            let stats = table.stats()?;
            rows.push((
                stats.stored_bytes(),
                format!(
                    "{:>14} stored {:>12} meta {:>12} fragmented {:>9} leaf {:>7} branch  {}",
                    stats.stored_bytes(),
                    stats.metadata_bytes(),
                    stats.fragmented_bytes(),
                    stats.leaf_pages(),
                    stats.branch_pages(),
                    handle.name()
                ),
            ));
        }
        rows.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (_, row) in rows {
            println!("{row}");
        }
        let stats = write.stats()?;
        println!(
            "database: {} allocated pages x {} bytes = {} bytes; {} stored, {} fragmented; savepoints {:?}",
            stats.allocated_pages(),
            stats.page_size(),
            stats.allocated_pages() * stats.page_size() as u64,
            stats.stored_bytes(),
            stats.fragmented_bytes(),
            write.list_persistent_savepoints()?.collect::<Vec<_>>()
        );
        write.abort()?;
        Ok(())
    }

    /// Compact the file at `path` in place and return `(bytes before, bytes
    /// after)`.
    ///
    /// Opens the file exclusively: nothing else may have it open. Fails while a
    /// persistent savepoint exists, because compaction moves the pages a
    /// savepoint would need to restore.
    pub fn compact(path: impl AsRef<Path>) -> anyhow::Result<(u64, u64)> {
        let path = path.as_ref();
        let before = std::fs::metadata(path)?.len();
        let mut database = Database::builder().set_cache_size(CACHE_SIZE).open(path)?;
        // redb shrinks the file by one region tail per commit, and a single
        // compact() commits only a few times, so a file with many free
        // regions at its end needs repeated calls.
        let mut after = before;
        loop {
            database.compact()?;
            let len = std::fs::metadata(path)?.len();
            if len >= after {
                break;
            }
            after = len;
        }
        drop(database);
        Ok((before, after))
    }

    pub fn read(&self) -> ReadTxn {
        ReadTxn {
            inner: self.database.begin_read().expect("begin rho-db read txn"),
        }
    }

    pub async fn write(&self) -> WriteTxn {
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        let inner = self.database.begin_write().expect("begin rho-db write txn");
        WriteTxn {
            inner,
            _guard: guard,
            observer: Arc::clone(&self.observer),
            after_commit: Vec::new(),
        }
    }

    /// This database's observer, made on first use. One type per database:
    /// asking for a second is a bug in the owning crate.
    pub fn observer<T: Any + Send + Sync>(&self, init: impl FnOnce() -> T) -> &T {
        self.observer
            .get_or_init(|| Box::new(init()))
            .downcast_ref()
            .expect("one observer type per database")
    }

    pub async fn persistent_savepoint(&self, record: impl FnOnce(&mut WriteTxn, u64)) -> u64 {
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        let inner = self.database.begin_write().expect("begin rho-db write txn");
        let id = inner
            .persistent_savepoint()
            .expect("create rho-db persistent savepoint");
        let mut write = WriteTxn {
            inner,
            _guard: guard,
            observer: Arc::clone(&self.observer),
            after_commit: Vec::new(),
        };
        record(&mut write, id);
        write.commit();
        id
    }
}

impl ReadTxn {
    /// Every table's name, for a migration proof looking at a store it
    /// did not write.
    pub fn table_names(&self) -> Vec<String> {
        self.inner
            .list_tables()
            .expect("list rho-db tables")
            .map(|table| table.name().to_owned())
            .collect()
    }

    pub fn has_table(&self, name: &str) -> bool {
        self.inner
            .list_tables()
            .expect("list rho-db tables")
            .any(|table| table.name() == name)
    }

    pub fn open_table<K, V>(&self, definition: TableDefinition<K, V>) -> ReadTable<K, V>
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        ReadTable {
            inner: self
                .inner
                .open_table(definition)
                .expect("open rho-db read table"),
        }
    }
}

impl WriteTxn {
    /// Puts the database back as it was when the savepoint was taken.
    /// Everything written since is gone once this commits, and savepoints
    /// taken after it are invalid. Returns whether the savepoint existed.
    pub fn restore_persistent_savepoint(&mut self, id: u64) -> bool {
        let savepoint = match self.inner.get_persistent_savepoint(id) {
            Ok(savepoint) => savepoint,
            Err(redb::SavepointError::InvalidSavepoint) => return false,
            Err(error) => panic!("get rho-db persistent savepoint {id}: {error}"),
        };
        self.inner
            .restore_savepoint(&savepoint)
            .expect("restore rho-db persistent savepoint");
        true
    }

    /// Deletes a persistent recovery savepoint, returning whether it existed.
    pub fn delete_persistent_savepoint(&mut self, id: u64) -> bool {
        self.inner
            .delete_persistent_savepoint(id)
            .expect("delete rho-db persistent savepoint")
    }

    /// Lists persistent recovery savepoints.
    pub fn persistent_savepoints(&self) -> Vec<u64> {
        self.inner
            .list_persistent_savepoints()
            .expect("list rho-db persistent savepoints")
            .collect()
    }

    pub fn open_table<K, V>(&mut self, definition: TableDefinition<K, V>) -> WriteTable<'_, K, V>
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        WriteTable {
            inner: self
                .inner
                .open_table(definition)
                .expect("open rho-db write table"),
        }
    }

    /// Opens a table, or `None` if the file records it under other
    /// key/value types. redb writes the Rust path of a value type into
    /// the table, so a type that moves between crates makes every
    /// database written before the move unopenable; a caller that can
    /// rebuild the table would rather be told than panicked at.
    pub fn try_open_table<K, V>(
        &mut self,
        definition: TableDefinition<K, V>,
    ) -> Option<WriteTable<'_, K, V>>
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        match self.inner.open_table(definition) {
            Ok(inner) => Some(WriteTable { inner }),
            Err(redb::TableError::TableTypeMismatch { .. }) => None,
            Err(error) => panic!("open rho-db write table: {error:?}"),
        }
    }

    /// Deletes a table by name (no type check), returning whether it
    /// existed. For migrations that change a table's key/value types.
    pub fn delete_table(&mut self, name: &str) -> bool {
        self.inner
            .delete_table(redb::TableDefinition::<(), ()>::new(name))
            .expect("delete rho-db table")
    }

    /// This database's observer, if one has been made.
    pub fn observer<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.observer
            .get()
            .and_then(|observer| observer.downcast_ref())
    }

    /// Queues work for after this transaction commits. Dropped unrun if it
    /// never does.
    pub fn after_commit(&mut self, effect: impl FnOnce() + Send + 'static) {
        self.after_commit.push(Box::new(effect));
    }

    pub fn commit(self) {
        let Self {
            inner,
            _guard,
            after_commit,
            ..
        } = self;
        inner.commit().expect("commit rho-db write txn");
        // The write lock is held until the commit lands, so an effect never
        // wakes a reader ahead of the next writer.
        drop(_guard);
        for effect in after_commit {
            effect();
        }
    }
}

impl<K, V> ReadTable<K, V>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    pub fn get<'a>(&self, key: impl Borrow<K::SelfType<'a>>) -> Option<AccessGuard<'_, V>> {
        self.inner.get(key).expect("get rho-db value")
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            inner: self.inner.iter().expect("iterate rho-db table"),
        }
    }

    pub fn range<'a, KR>(&self, range: impl std::ops::RangeBounds<KR> + 'a) -> Iter<'_, K, V>
    where
        KR: Borrow<K::SelfType<'a>> + 'a,
    {
        Iter {
            inner: self.inner.range(range).expect("range rho-db table"),
        }
    }
}

impl<'txn, K, V> WriteTable<'txn, K, V>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    pub fn get<'a>(&self, key: impl Borrow<K::SelfType<'a>>) -> Option<AccessGuard<'_, V>> {
        self.inner.get(key).expect("get rho-db value")
    }

    pub fn insert<'k, 'v>(
        &mut self,
        key: impl Borrow<K::SelfType<'k>>,
        value: impl Borrow<V::SelfType<'v>>,
    ) -> Option<AccessGuard<'_, V>> {
        self.inner.insert(key, value).expect("insert rho-db value")
    }

    pub fn remove<'a>(&mut self, key: impl Borrow<K::SelfType<'a>>) -> Option<AccessGuard<'_, V>> {
        self.inner.remove(key).expect("remove rho-db value")
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            inner: self.inner.iter().expect("iterate rho-db table"),
        }
    }

    pub fn range<'a, KR>(&self, range: impl std::ops::RangeBounds<KR> + 'a) -> Iter<'_, K, V>
    where
        KR: Borrow<K::SelfType<'a>> + 'a,
    {
        Iter {
            inner: self.inner.range(range).expect("range rho-db table"),
        }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    type Item = (AccessGuard<'a, K>, AccessGuard<'a, V>);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|item| item.expect("read rho-db iterator item"))
    }
}

impl<K, V> DoubleEndedIterator for Iter<'_, K, V>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner
            .next_back()
            .map(|item| item.expect("read rho-db iterator item"))
    }
}

#[cfg(test)]
mod tests {
    use redb::TableDefinition;
    use senax_encoder::{Decode, Encode};

    use super::*;

    const ITEMS: TableDefinition<Sen<TestKey>, Sen<TestRecord>> = TableDefinition::new("items");

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestKey(u64);

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestRecord {
        name: String,
        #[senax(default)]
        tags: Vec<String>,
    }

    #[tokio::test]
    async fn sen_values_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rho.redb");

        let db = RhoDb::open(&path);
        let mut write = db.write().await;
        write.open_table(ITEMS).insert(
            SenValue::owned(TestKey(42)),
            SenValue::owned(TestRecord {
                name: "agent".to_owned(),
                tags: vec!["main".to_owned()],
            }),
        );
        write.commit();
        drop(db);

        let reopened = RhoDb::open(&path);
        let read = reopened.read();
        let table = read.open_table(ITEMS);
        assert_eq!(
            table
                .get(SenValue::owned(TestKey(42)))
                .unwrap()
                .value()
                .into_owned(),
            TestRecord {
                name: "agent".to_owned(),
                tags: vec!["main".to_owned()],
            }
        );
    }

    #[tokio::test]
    async fn callers_use_redb_iterators_directly() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));

        let mut write = db.write().await;
        {
            let mut table = write.open_table(ITEMS);
            table.insert(
                SenValue::owned(TestKey(1)),
                SenValue::owned(TestRecord {
                    name: "one".to_owned(),
                    tags: Vec::new(),
                }),
            );
            table.insert(
                SenValue::owned(TestKey(2)),
                SenValue::owned(TestRecord {
                    name: "two".to_owned(),
                    tags: Vec::new(),
                }),
            );
        }
        write.commit();

        let read = db.read();
        let table = read.open_table(ITEMS);
        let items = table
            .iter()
            .map(|(key, _)| key.value().as_ref().0)
            .collect::<Vec<_>>();
        assert_eq!(items, [1, 2]);

        let mut range = table.range(SenValue::owned(TestKey(1))..=SenValue::owned(TestKey(2)));
        assert_eq!(range.next().unwrap().0.value().as_ref().0, 1);
        assert_eq!(range.next_back().unwrap().0.value().as_ref().0, 2);
        assert!(range.next().is_none());
    }
}
