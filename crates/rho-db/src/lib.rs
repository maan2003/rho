//! Thin redb helpers for rho.
//!
//! Callers own table definitions and schema. `rho-db` only provides `Sen<T>`
//! for senax-backed redb keys/values and small transaction wrappers that treat
//! local database errors as fatal.

use std::cmp::Ordering;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;

use bytes::BytesMut;
use redb::{Database, ReadableDatabase, TableDefinition, TypeName};
use tokio::sync::{Mutex, OwnedMutexGuard};

const CACHE_SIZE: usize = 10 * 1024 * 1024;

/// redb key/value wrapper using senax encoding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sen<T>(pub T);

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
        = Sen<T>
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
        Sen(T::decode(&mut data).expect("senax decode rho-db value"))
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let mut bytes = BytesMut::new();
        value
            .0
            .encode(&mut bytes)
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
    pub fn open_table<K, V>(&self, definition: TableDefinition<K, V>) -> redb::ReadOnlyTable<K, V>
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        self.inner
            .open_table(definition)
            .expect("open rho-db read table")
    }
}

impl WriteTxn {
    pub fn open_table<K, V>(&mut self, definition: TableDefinition<K, V>) -> redb::Table<'_, K, V>
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        self.inner
            .open_table(definition)
            .expect("open rho-db write table")
    }

    pub fn commit(self) {
        self.inner.commit().expect("commit rho-db write txn");
    }
}

#[cfg(test)]
mod tests {
    use redb::{ReadableTable, TableDefinition};
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
        write
            .open_table(ITEMS)
            .insert(
                &Sen(TestKey(42)),
                Sen(TestRecord {
                    name: "agent".to_owned(),
                    tags: vec!["main".to_owned()],
                }),
            )
            .unwrap();
        write.commit();
        drop(db);

        let reopened = RhoDb::open(&path);
        let read = reopened.read();
        let table = read.open_table(ITEMS);
        assert_eq!(
            table.get(&Sen(TestKey(42))).unwrap().unwrap().value().0,
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
            table
                .insert(
                    &Sen(TestKey(1)),
                    Sen(TestRecord {
                        name: "one".to_owned(),
                        tags: Vec::new(),
                    }),
                )
                .unwrap();
            table
                .insert(
                    &Sen(TestKey(2)),
                    Sen(TestRecord {
                        name: "two".to_owned(),
                        tags: Vec::new(),
                    }),
                )
                .unwrap();
        }
        write.commit();

        let read = db.read();
        let table = read.open_table(ITEMS);
        let items = table
            .iter()
            .unwrap()
            .map(|item| item.map(|(key, _)| key.value().0.0))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(items, [1, 2]);
    }
}
