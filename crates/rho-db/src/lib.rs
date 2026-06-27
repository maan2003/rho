//! redb-backed key/value database boundary for rho.
//!
//! `RhoDb` owns transaction lifecycle and leaves domain records to higher
//! crates. Opening the database and table operations are synchronous. Acquiring
//! a write transaction is async because redb permits only one writer.

use std::path::Path;
use std::sync::Arc;
use std::ops::{Bound, RangeBounds};

use anyhow::Result;
use bytes::BytesMut;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use tokio::sync::{Mutex, OwnedMutexGuard};

const CACHE_SIZE: usize = 10 * 1024 * 1024;

/// Key encoding for redb tables.
///
/// Implementations own ordering/prefix semantics. For ordered redb ranges, the
/// encoded bytes must preserve the intended sort order.
pub trait Key: senax_encoder::Encoder + senax_encoder::Decoder {
    const TABLE: &'static str;
    type Value: senax_encoder::Encoder + senax_encoder::Decoder;
}

/// redb-backed rho database handle.
#[derive(Clone, Debug)]
pub struct RhoDb {
    database: Arc<Database>,
    write_lock: Arc<Mutex<()>>,
}

/// Read transaction. All operations on an acquired transaction are synchronous.
pub struct ReadTxn {
    inner: redb::ReadTransaction,
}

/// Write transaction. All operations on an acquired transaction are
/// synchronous.
pub struct WriteTxn {
    inner: redb::WriteTransaction,
    _guard: OwnedMutexGuard<()>,
}

/// Decoded double-ended iterator over a rho-db table.
pub struct Iter<K: Key> {
    inner: redb::Range<'static, &'static [u8], &'static [u8]>,
    _marker: std::marker::PhantomData<fn() -> K>,
}

impl RhoDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let database = Database::builder()
            .set_cache_size(CACHE_SIZE)
            .create(path)?;

        Ok(Self {
            database: Arc::new(database),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn read(&self) -> Result<ReadTxn> {
        Ok(ReadTxn {
            inner: self.database.begin_read()?,
        })
    }

    pub async fn write(&self) -> Result<WriteTxn> {
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        let inner = self.database.begin_write()?;
        Ok(WriteTxn {
            inner,
            _guard: guard,
        })
    }
}

impl ReadTxn {
    pub fn get<K: Key>(&self, key: &K) -> Result<Option<K::Value>> {
        let table = self.inner.open_table(definition::<K>())?;
        table
            .get(encode(key)?.as_ref())?
            .map(|bytes| decode(bytes.value()))
            .transpose()
    }

    pub fn iter<K: Key>(&self) -> Result<Iter<K>> {
        let table = self.inner.open_table(definition::<K>())?;
        Ok(Iter::new(table.range::<&[u8]>(..)?))
    }

    pub fn range<K: Key>(&self, range: impl RangeBounds<K>) -> Result<Iter<K>> {
        let start = encode_bound(range.start_bound())?;
        let end = encode_bound(range.end_bound())?;
        let encoded_range = (bound_as_slice(&start), bound_as_slice(&end));
        let table = self.inner.open_table(definition::<K>())?;
        Ok(Iter::new(table.range::<&[u8]>(encoded_range)?))
    }
}

impl WriteTxn {
    pub fn get<K: Key>(&self, key: &K) -> Result<Option<K::Value>> {
        let table = self.inner.open_table(definition::<K>())?;
        table
            .get(encode(key)?.as_ref())?
            .map(|bytes| decode(bytes.value()))
            .transpose()
    }

    pub fn put<K: Key>(&mut self, key: &K, value: &K::Value) -> Result<()> {
        let key = encode(key)?;
        let value = encode(value)?;
        self.inner
            .open_table(definition::<K>())?
            .insert(key.as_ref(), value.as_ref())?;
        Ok(())
    }

    pub fn remove<K: Key>(&mut self, key: &K) -> Result<bool> {
        Ok(self
            .inner
            .open_table(definition::<K>())?
            .remove(encode(key)?.as_ref())?
            .is_some())
    }

    pub fn commit(self) -> Result<()> {
        self.inner.commit()?;
        Ok(())
    }
}

impl<K: Key> Iter<K> {
    fn new(inner: redb::Range<'static, &'static [u8], &'static [u8]>) -> Self {
        Self {
            inner,
            _marker: std::marker::PhantomData,
        }
    }

    fn decode_item(
        item: redb::Result<(
            redb::AccessGuard<'_, &'static [u8]>,
            redb::AccessGuard<'_, &'static [u8]>,
        )>,
    ) -> Result<(K, K::Value)> {
        let (key, value) = item?;
        Ok((decode(key.value())?, decode(value.value())?))
    }
}

impl<K: Key> Iterator for Iter<K> {
    type Item = Result<(K, K::Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(Self::decode_item)
    }
}

impl<K: Key> DoubleEndedIterator for Iter<K> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(Self::decode_item)
    }
}

fn definition<K: Key>() -> TableDefinition<'static, &'static [u8], &'static [u8]> {
    TableDefinition::new(K::TABLE)
}

fn encode<T>(value: &T) -> Result<BytesMut>
where
    T: senax_encoder::Encoder,
{
    let mut bytes = BytesMut::new();
    value.encode(&mut bytes)?;
    Ok(bytes)
}

fn encode_bound<T: senax_encoder::Encoder>(bound: Bound<&T>) -> Result<Bound<Vec<u8>>> {
    Ok(match bound {
        Bound::Included(value) => Bound::Included(encode(value)?.to_vec()),
        Bound::Excluded(value) => Bound::Excluded(encode(value)?.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    })
}

fn bound_as_slice(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(value) => Bound::Included(value.as_slice()),
        Bound::Excluded(value) => Bound::Excluded(value.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn decode<T>(bytes: &[u8]) -> Result<T>
where
    T: senax_encoder::Decoder,
{
    let mut bytes = bytes;
    T::decode(&mut bytes).map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use senax_encoder::{Decode, Encode};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestKey(u64);

    impl Key for TestKey {
        const TABLE: &'static str = "items";
        type Value = TestRecord;
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestRecord {
        name: String,
        #[senax(default)]
        tags: Vec<String>,
    }

    #[tokio::test]
    async fn typed_values_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rho.redb");

        let db = RhoDb::open(&path).unwrap();
        let mut write = db.write().await.unwrap();
        write
            .put(
                &TestKey(42),
                &TestRecord {
                    name: "agent".to_owned(),
                    tags: vec!["main".to_owned()],
                },
            )
            .unwrap();
        write.commit().unwrap();
        drop(db);

        let reopened = RhoDb::open(&path).unwrap();
        let read = reopened.read().unwrap();
        assert_eq!(
            read.get(&TestKey(42)).unwrap(),
            Some(TestRecord {
                name: "agent".to_owned(),
                tags: vec!["main".to_owned()],
            })
        );
    }

    #[tokio::test]
    async fn write_transaction_is_sync_after_acquire() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb")).unwrap();

        let mut write = db.write().await.unwrap();
        write
            .put(
                &TestKey(1),
                &TestRecord {
                    name: "one".to_owned(),
                    tags: Vec::new(),
                },
            )
            .unwrap();
        write
            .put(
                &TestKey(2),
                &TestRecord {
                    name: "two".to_owned(),
                    tags: Vec::new(),
                },
            )
            .unwrap();
        write.commit().unwrap();

        let read = db.read().unwrap();
        let items = read
            .iter::<TestKey>()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            items.iter().map(|(key, _)| key.0).collect::<Vec<_>>(),
            [1, 2]
        );

        let mut range = read.range(TestKey(1)..=TestKey(2)).unwrap();
        assert_eq!(range.next().unwrap().unwrap().0, TestKey(1));
        assert_eq!(range.next_back().unwrap().unwrap().0, TestKey(2));
        assert!(range.next().is_none());
    }
}
