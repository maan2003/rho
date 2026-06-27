//! redb-backed key/value database boundary for rho.
//!
//! `RhoDb` owns transaction lifecycle and leaves domain records to higher
//! crates. Acquiring a write transaction is async because redb permits only one
//! writer; once acquired, table operations are synchronous.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use bytes::BytesMut;
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use tokio::fs;
use tokio::sync::{Mutex, OwnedMutexGuard};

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
    path: PathBuf,
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

impl RhoDb {
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let database_path = path.clone();
        let database = tokio::task::spawn_blocking(move || {
            let database = Database::create(database_path)?;
            Ok::<_, anyhow::Error>(database)
        })
        .await??;

        Ok(Self {
            path,
            database: Arc::new(database),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<ReadTxn> {
        Ok(ReadTxn {
            inner: self.database.begin_read()?,
        })
    }

    pub async fn write(&self) -> Result<WriteTxn> {
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        let mut inner = self.database.begin_write()?;
        inner.set_durability(Durability::Immediate)?;
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

    pub fn iter<K: Key>(&self) -> Result<Vec<(K, K::Value)>> {
        let table = self.inner.open_table(definition::<K>())?;
        let mut items = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            items.push((decode(key.value())?, decode(value.value())?));
        }
        Ok(items)
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

fn definition<K: Key>() -> TableDefinition<'static, &'static [u8], &'static [u8]> {
    TableDefinition::new(K::TABLE)
}

fn encode<T>(value: &T) -> Result<BytesMut>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder,
{
    let mut bytes = BytesMut::new();
    value.encode(&mut bytes)?;
    Ok(bytes)
}

fn decode<T>(bytes: &[u8]) -> Result<T>
where
    T: senax_encoder::Encoder + senax_encoder::Decoder,
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

        let db = RhoDb::open(&path).await.unwrap();
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

        let reopened = RhoDb::open(&path).await.unwrap();
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
        let db = RhoDb::open(temp.path().join("rho.redb")).await.unwrap();

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
        let items = read.iter::<TestKey>().unwrap();
        assert_eq!(
            items.iter().map(|(key, _)| key.0).collect::<Vec<_>>(),
            [1, 2]
        );
    }
}
