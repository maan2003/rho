//! Thin redb helpers for rho.
//!
//! Callers own table definitions and schema. `rho-db` only provides `Sen<T>`
//! for senax-backed redb keys/values and small transaction wrappers that treat
//! local database errors as fatal.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;

use bytes::BytesMut;
use redb::{AccessGuard, Database, ReadableDatabase, ReadableTable, TableDefinition, TypeName};
use tokio::sync::{Mutex, OwnedMutexGuard};

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

    pub fn as_ref(&self) -> &T {
        match self {
            Self::Owned(value) => value,
            Self::Borrowed(_) => panic!("borrowed sen value has no owned reference"),
        }
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
}

/// Read transaction wrapper. Methods panic on local database errors.
pub struct ReadTxn {
    inner: redb::ReadTransaction,
}

/// Write transaction wrapper. Methods panic on local database errors.
pub struct WriteTxn {
    inner: redb::WriteTransaction,
    _guard: OwnedMutexGuard<()>,
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
        }
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
        }
    }
}

impl ReadTxn {
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

    pub fn commit(self) {
        self.inner.commit().expect("commit rho-db write txn");
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
