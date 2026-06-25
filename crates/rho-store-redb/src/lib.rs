//! redb-backed transcript storage for rho.
//!
//! The lower layer is an append-only forest of transcript nodes. Nodes are
//! clustered into lineages so a linear conversation is stored under contiguous
//! keys, while forks create cheap cross-lineage links. The agent layer is a
//! small ref table over that forest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use rho_core::ContextBlock;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::fs;

mod node_id;

pub use node_id::NodeRef;
pub(crate) use node_id::{decode_node_key, encode_node_key, encode_ordvarint, prefix_end};

const LINEAGES: TableDefinition<u64, &[u8]> = TableDefinition::new("lineages");
const PAYLOADS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("payloads");
const AGENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("agents");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const NEXT_LINEAGE_ID: &str = "next_lineage_id";
const NEXT_AGENT_ID: &str = "next_agent_id";

/// A stable id for an agent ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentId(pub u64);

/// Persisted agent metadata and its current transcript head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: AgentId,
    pub head: Option<NodeRef>,
    pub display_name: Option<String>,
    pub prompt_preview: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub parent_agent: Option<AgentId>,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
}

/// A redb database containing one global transcript forest plus agent refs.
#[derive(Clone, Debug)]
pub struct RedbStore {
    path: PathBuf,
    database: Arc<Database>,
}

/// Compatibility name for callers that still treat the redb store as one log.
pub type RedbLog = RedbStore;

impl RedbStore {
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

    /// Append a transcript node under `parent`.
    ///
    /// If `parent` is the current tip of its lineage, the new node extends that
    /// lineage with `seq + 1`. Otherwise, this creates a new lineage at `seq 0`
    /// whose fork parent is `parent`.
    pub async fn append_child(
        &self,
        parent: Option<NodeRef>,
        block: &ContextBlock,
    ) -> Result<NodeRef> {
        let database = Arc::clone(&self.database);
        let block = block.clone();

        tokio::task::spawn_blocking(move || {
            let bytes = encode_value(&block)?;
            let mut write_txn = database.begin_write()?;
            write_txn.set_durability(Durability::Immediate)?;
            let node_ref = {
                let mut lineages = write_txn.open_table(LINEAGES)?;
                let mut payloads = write_txn.open_table(PAYLOADS)?;
                let mut meta = write_txn.open_table(META)?;

                let node_ref = allocate_node_ref(parent, &mut lineages, &payloads, &mut meta)?;
                let key = encode_node_key(node_ref);
                payloads.insert(key.as_slice(), bytes.as_slice())?;
                node_ref
            };
            write_txn.commit()?;
            Ok::<_, anyhow::Error>(node_ref)
        })
        .await?
    }

    /// Read a branch from root to `head`.
    pub async fn read_branch(&self, head: NodeRef) -> Result<Vec<(NodeRef, ContextBlock)>> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let read_txn = database.begin_read()?;
            let lineages = read_txn.open_table(LINEAGES)?;
            let payloads = read_txn.open_table(PAYLOADS)?;
            read_branch_inner(&lineages, &payloads, head)
        })
        .await?
    }

    /// Return the current tip of a lineage, if it has any payload nodes.
    pub async fn lineage_tip(&self, lineage_id: u64) -> Result<Option<NodeRef>> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let read_txn = database.begin_read()?;
            let payloads = read_txn.open_table(PAYLOADS)?;
            lineage_tip(&payloads, lineage_id)
        })
        .await?
    }

    pub async fn create_agent(
        &self,
        head: Option<NodeRef>,
        display_name: Option<String>,
    ) -> Result<AgentRecord> {
        self.create_agent_with_metadata(head, display_name, BTreeMap::new(), None)
            .await
    }

    pub async fn create_agent_with_metadata(
        &self,
        head: Option<NodeRef>,
        display_name: Option<String>,
        metadata: BTreeMap<String, String>,
        parent_agent: Option<AgentId>,
    ) -> Result<AgentRecord> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let mut write_txn = database.begin_write()?;
            write_txn.set_durability(Durability::Immediate)?;
            let record = {
                let lineages = write_txn.open_table(LINEAGES)?;
                let payloads = write_txn.open_table(PAYLOADS)?;
                let mut agents = write_txn.open_table(AGENTS)?;
                let mut meta = write_txn.open_table(META)?;

                if let Some(head) = head {
                    ensure_node_exists(&lineages, &payloads, head)?;
                }

                let id = AgentId(next_counter(&mut meta, NEXT_AGENT_ID)?);
                let now = now_millis()?;
                let record = AgentRecord {
                    id,
                    head,
                    display_name,
                    prompt_preview: None,
                    metadata,
                    parent_agent,
                    created_at_millis: now,
                    updated_at_millis: now,
                };
                let bytes = encode_value(&record)?;
                agents.insert(id.0, bytes.as_slice())?;
                record
            };
            write_txn.commit()?;
            Ok::<_, anyhow::Error>(record)
        })
        .await?
    }

    pub async fn get_agent(&self, id: AgentId) -> Result<Option<AgentRecord>> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let read_txn = database.begin_read()?;
            let agents = read_txn.open_table(AGENTS)?;
            decode_agent_record(agents.get(id.0)?)
        })
        .await?
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let read_txn = database.begin_read()?;
            let agents = read_txn.open_table(AGENTS)?;
            let mut records = Vec::new();
            for item in agents.iter()? {
                let (_id, bytes) = item?;
                records.push(decode_value(bytes.value())?);
            }
            Ok::<_, anyhow::Error>(records)
        })
        .await?
    }

    pub async fn update_agent_head(&self, id: AgentId, head: Option<NodeRef>) -> Result<()> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || {
            let mut write_txn = database.begin_write()?;
            write_txn.set_durability(Durability::Immediate)?;
            {
                let lineages = write_txn.open_table(LINEAGES)?;
                let payloads = write_txn.open_table(PAYLOADS)?;
                let mut agents = write_txn.open_table(AGENTS)?;

                if let Some(head) = head {
                    ensure_node_exists(&lineages, &payloads, head)?;
                }

                let mut record = decode_agent_record(agents.get(id.0)?)?
                    .with_context(|| format!("agent {} does not exist", id.0))?;
                record.head = head;
                record.updated_at_millis = now_millis()?;
                let bytes = encode_value(&record)?;
                agents.insert(id.0, bytes.as_slice())?;
            }
            write_txn.commit()?;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    /// Append a block to an agent branch and move that agent's head.
    pub async fn append_agent_block(&self, id: AgentId, block: &ContextBlock) -> Result<NodeRef> {
        let database = Arc::clone(&self.database);
        let block = block.clone();

        tokio::task::spawn_blocking(move || {
            let bytes = encode_value(&block)?;
            let mut write_txn = database.begin_write()?;
            write_txn.set_durability(Durability::Immediate)?;
            let node_ref = {
                let mut lineages = write_txn.open_table(LINEAGES)?;
                let mut payloads = write_txn.open_table(PAYLOADS)?;
                let mut agents = write_txn.open_table(AGENTS)?;
                let mut meta = write_txn.open_table(META)?;

                let mut record = decode_agent_record(agents.get(id.0)?)?
                    .with_context(|| format!("agent {} does not exist", id.0))?;
                let node_ref = allocate_node_ref(record.head, &mut lineages, &payloads, &mut meta)?;
                let key = encode_node_key(node_ref);
                payloads.insert(key.as_slice(), bytes.as_slice())?;

                record.head = Some(node_ref);
                record.updated_at_millis = now_millis()?;
                let record_bytes = encode_value(&record)?;
                agents.insert(id.0, record_bytes.as_slice())?;
                node_ref
            };
            write_txn.commit()?;
            Ok::<_, anyhow::Error>(node_ref)
        })
        .await?
    }

    pub async fn read_agent_branch(&self, id: AgentId) -> Result<Vec<(NodeRef, ContextBlock)>> {
        let Some(record) = self.get_agent(id).await? else {
            bail!("agent {} does not exist", id.0);
        };
        match record.head {
            Some(head) => self.read_branch(head).await,
            None => Ok(Vec::new()),
        }
    }

    /// Compatibility helper: append to a single default agent.
    pub async fn append_block(&self, block: &ContextBlock) -> Result<()> {
        let agent = self.ensure_default_agent().await?;
        self.append_agent_block(agent.id, block).await?;
        Ok(())
    }

    /// Compatibility helper: read the single default agent's branch.
    pub async fn read_blocks(&self) -> Result<Vec<ContextBlock>> {
        let agent = self.ensure_default_agent().await?;
        Ok(self
            .read_agent_branch(agent.id)
            .await?
            .into_iter()
            .map(|(_node_ref, block)| block)
            .collect())
    }

    async fn ensure_default_agent(&self) -> Result<AgentRecord> {
        if let Some(agent) = self.get_agent(AgentId(0)).await? {
            return Ok(agent);
        }
        self.create_agent(None, Some("default".to_owned())).await
    }
}

fn initialize_database(database: &Database) -> Result<()> {
    let mut write_txn = database.begin_write()?;
    write_txn.set_durability(Durability::Immediate)?;
    {
        write_txn.open_table(LINEAGES)?;
        write_txn.open_table(PAYLOADS)?;
        write_txn.open_table(AGENTS)?;
        let mut meta = write_txn.open_table(META)?;
        if meta.get(NEXT_LINEAGE_ID)?.is_none() {
            meta.insert(NEXT_LINEAGE_ID, 0)?;
        }
        if meta.get(NEXT_AGENT_ID)?.is_none() {
            meta.insert(NEXT_AGENT_ID, 0)?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

fn allocate_node_ref(
    parent: Option<NodeRef>,
    lineages: &mut redb::Table<'_, u64, &[u8]>,
    payloads: &redb::Table<'_, &[u8], &[u8]>,
    meta: &mut redb::Table<'_, &str, u64>,
) -> Result<NodeRef> {
    match parent {
        None => {
            let lineage_id = next_counter(meta, NEXT_LINEAGE_ID)?;
            lineages.insert(lineage_id, encode_fork_parent(None).as_slice())?;
            Ok(NodeRef::new(lineage_id, 0))
        }
        Some(parent) => {
            ensure_node_exists(lineages, payloads, parent)?;
            let tip = lineage_tip(payloads, parent.lineage_id)?
                .context("parent lineage exists but has no payload tip")?;
            if tip == parent && parent.seq < u64::MAX {
                Ok(NodeRef::new(parent.lineage_id, parent.seq + 1))
            } else {
                let lineage_id = next_counter(meta, NEXT_LINEAGE_ID)?;
                lineages.insert(lineage_id, encode_fork_parent(Some(parent)).as_slice())?;
                Ok(NodeRef::new(lineage_id, 0))
            }
        }
    }
}

fn next_counter(table: &mut redb::Table<'_, &str, u64>, key: &'static str) -> Result<u64> {
    let next = table.get(key)?.map(|value| value.value()).unwrap_or(0);
    let after = next
        .checked_add(1)
        .with_context(|| format!("counter {key} exhausted"))?;
    table.insert(key, after)?;
    Ok(next)
}

fn ensure_node_exists(
    lineages: &redb::Table<'_, u64, &[u8]>,
    payloads: &redb::Table<'_, &[u8], &[u8]>,
    node_ref: NodeRef,
) -> Result<()> {
    ensure!(
        lineages.get(node_ref.lineage_id)?.is_some(),
        "lineage {} does not exist",
        node_ref.lineage_id
    );
    let key = encode_node_key(node_ref);
    ensure!(
        payloads.get(key.as_slice())?.is_some(),
        "node ({}, {}) does not exist",
        node_ref.lineage_id,
        node_ref.seq
    );
    Ok(())
}

fn read_branch_inner(
    lineages: &redb::ReadOnlyTable<u64, &[u8]>,
    payloads: &redb::ReadOnlyTable<&[u8], &[u8]>,
    head: NodeRef,
) -> Result<Vec<(NodeRef, ContextBlock)>> {
    ensure_node_exists_readonly(lineages, payloads, head)?;

    let mut segments = Vec::new();
    let mut cur = head;
    loop {
        segments.push(cur);
        let fork_parent = lineages
            .get(cur.lineage_id)?
            .with_context(|| format!("lineage {} does not exist", cur.lineage_id))
            .and_then(|value| decode_fork_parent(value.value()))?;
        match fork_parent {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    segments.reverse();

    let mut output = Vec::new();
    for segment in segments {
        let start = encode_node_key(NodeRef::new(segment.lineage_id, 0));
        let end = encode_node_key(segment);
        for item in payloads.range(start.as_slice()..=end.as_slice())? {
            let (key, bytes) = item?;
            let node_ref = decode_node_key(key.value())?;
            output.push((node_ref, decode_value(bytes.value())?));
        }
    }
    Ok(output)
}

fn ensure_node_exists_readonly(
    lineages: &redb::ReadOnlyTable<u64, &[u8]>,
    payloads: &redb::ReadOnlyTable<&[u8], &[u8]>,
    node_ref: NodeRef,
) -> Result<()> {
    ensure!(
        lineages.get(node_ref.lineage_id)?.is_some(),
        "lineage {} does not exist",
        node_ref.lineage_id
    );
    let key = encode_node_key(node_ref);
    ensure!(
        payloads.get(key.as_slice())?.is_some(),
        "node ({}, {}) does not exist",
        node_ref.lineage_id,
        node_ref.seq
    );
    Ok(())
}

fn lineage_tip(
    payloads: &impl ReadableTable<&'static [u8], &'static [u8]>,
    lineage_id: u64,
) -> Result<Option<NodeRef>> {
    let prefix = encode_ordvarint(lineage_id);
    if let Some(end) = prefix_end(&prefix) {
        let Some(item) = payloads
            .range(prefix.as_slice()..end.as_slice())?
            .rev()
            .next()
        else {
            return Ok(None);
        };
        let (key, _bytes) = item?;
        return Ok(Some(decode_node_key(key.value())?));
    }

    for item in payloads.iter()?.rev() {
        let (key, _bytes) = item?;
        if key.value().starts_with(&prefix) {
            return Ok(Some(decode_node_key(key.value())?));
        }
    }
    Ok(None)
}

fn decode_agent_record(value: Option<redb::AccessGuard<'_, &[u8]>>) -> Result<Option<AgentRecord>> {
    value.map(|bytes| decode_value(bytes.value())).transpose()
}

fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_stdvec(value).map_err(anyhow::Error::from)
}

fn decode_value<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    postcard::from_bytes(bytes).map_err(anyhow::Error::from)
}

fn now_millis() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .context("system time exceeds u64 millis")?)
}

fn encode_fork_parent(parent: Option<NodeRef>) -> Vec<u8> {
    match parent {
        None => Vec::new(),
        Some(parent) => encode_node_key(parent),
    }
}

fn decode_fork_parent(bytes: &[u8]) -> Result<Option<NodeRef>> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(decode_node_key(bytes)?))
    }
}

#[cfg(test)]
mod tests;
