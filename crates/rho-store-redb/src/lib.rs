//! Append-only redb persistence for rho transcript blocks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use rho_core::ItemBlock;
use tokio::fs;

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const NEXT_BLOCK_ID: &str = "next_block_id";

#[derive(Clone, Debug)]
pub struct RedbLog {
    path: PathBuf,
    database: Arc<Database>,
}

impl RedbLog {
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let database_path = path.clone();
        let database = tokio::task::spawn_blocking(move || {
            let database = Database::create(database_path)?;
            initialize_database(&database)?;
            Ok::<_, anyhow::Error>(database)
        })
        .await??;

        Ok(Self {
            path,
            database: Arc::new(database),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append_block(&self, block: &ItemBlock) -> Result<()> {
        let database = Arc::clone(&self.database);
        let block = block.clone();

        tokio::task::spawn_blocking(move || {
            let bytes = serde_cbor::to_vec(&block)?;
            let write_txn = database.begin_write()?;
            {
                let mut blocks = write_txn.open_table(BLOCKS)?;
                let mut meta = write_txn.open_table(META)?;
                let next_id = meta
                    .get(NEXT_BLOCK_ID)?
                    .map(|value| value.value())
                    .unwrap_or(0);

                blocks.insert(next_id, bytes.as_slice())?;
                meta.insert(NEXT_BLOCK_ID, next_id + 1)?;
            }
            write_txn.commit()?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn read_blocks(&self) -> Result<Vec<ItemBlock>> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let read_txn = database.begin_read()?;
            let blocks = read_txn.open_table(BLOCKS)?;
            let mut output = Vec::new();

            for item in blocks.iter()? {
                let (_id, bytes) = item?;
                output.push(serde_cbor::from_slice(bytes.value())?);
            }

            Ok::<_, anyhow::Error>(output)
        })
        .await?
    }
}

fn initialize_database(database: &Database) -> Result<()> {
    let write_txn = database.begin_write()?;
    {
        write_txn.open_table(BLOCKS)?;
        let mut meta = write_txn.open_table(META)?;
        if meta.get(NEXT_BLOCK_ID)?.is_none() {
            meta.insert(NEXT_BLOCK_ID, 0)?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;
