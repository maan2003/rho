//! Durable daemon ownership for the Desk tree and per-node Zed buffers.

use std::collections::{BTreeMap, BTreeSet};

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_ui_proto::desk::{
    DeskBinding, DeskClock, DeskNode, DeskNodeId, DeskNodeText, DeskOperation, DeskOrderKey,
    DeskReplica, DeskReplicaAuthor, DeskSnapshot, DeskStructureAuthor, DeskStructureOp,
    DeskStructureOpId, DeskStructureOpRecord, DeskTextOpRecord, DeskTransaction,
};
use senax_encoder::{Decode, Encode};
use text::ReplicaId;

const STATE: TableDefinition<(), Sen<PersistentState>> = TableDefinition::new("rho_desk_state_v1");
const STRUCTURE_OPS: TableDefinition<u64, Sen<DeskStructureOpRecord>> =
    TableDefinition::new("rho_desk_structure_ops_v1");
const TEXT_OPS: TableDefinition<u64, Sen<DeskTextOpRecord>> =
    TableDefinition::new("rho_desk_text_ops_v1");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentState {
    snapshot: DeskSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            snapshot: DeskSnapshot::default(),
            next_text_sequence: 1,
            next_replica_id: ReplicaId::FIRST_COLLAB_ID.as_u16(),
        }
    }
}

#[derive(Clone)]
pub struct DeskStore {
    db: RhoDb,
}

impl DeskStore {
    pub async fn new(db: RhoDb) -> Self {
        let mut write = db.write().await;
        write.open_table(STRUCTURE_OPS);
        write.open_table(TEXT_OPS);
        if write.open_table(STATE).get(&()).is_none() {
            write
                .open_table(STATE)
                .insert(&(), SenValue::owned(PersistentState::default()));
        }
        write.commit();
        Self { db }
    }

    pub fn snapshot(&self) -> DeskSnapshot {
        let read = self.db.read();
        let mut snapshot = read
            .open_table(STATE)
            .get(&())
            .expect("Desk state initialized")
            .value()
            .into_owned()
            .snapshot;
        snapshot.texts.clear();
        let visible: BTreeSet<_> = snapshot.nodes.iter().map(|node| node.id).collect();
        let mut texts: BTreeMap<DeskNodeId, DeskNodeText> = BTreeMap::new();
        for (_, value) in read.open_table(TEXT_OPS).iter() {
            let record = value.value().into_owned();
            if !visible.contains(&record.node_id) {
                continue;
            }
            let text = texts.entry(record.node_id).or_insert_with(|| DeskNodeText {
                node_id: record.node_id,
                operations: Vec::new(),
                transactions: Vec::new(),
            });
            text.operations.push(record.operation);
            if let Some(transaction) = record.transaction {
                text.transactions.push(transaction);
            }
        }
        snapshot.texts = texts.into_values().collect();
        snapshot
    }

    pub async fn allocate_user_replica(&self) -> Result<u16, String> {
        let mut write = self.db.write().await;
        let mut state = load_state_for_write(&mut write);
        let replica_id = state.next_replica_id;
        state.next_replica_id = state
            .next_replica_id
            .checked_add(1)
            .ok_or_else(|| "Desk replica id space exhausted".to_owned())?;
        state.snapshot.replicas.push(DeskReplica {
            replica_id,
            author: DeskReplicaAuthor::User,
        });
        save_state(&mut write, &state);
        write.commit();
        Ok(replica_id)
    }

    pub async fn bind(
        &self,
        node_id: DeskNodeId,
        agent_id: rho_ui_proto::AgentId,
    ) -> Result<DeskBinding, String> {
        let mut write = self.db.write().await;
        let mut state = load_state_for_write(&mut write);
        let binding = state.snapshot.bind(node_id, agent_id)?;
        save_state(&mut write, &state);
        write.commit();
        Ok(binding)
    }

    pub async fn insert(
        &self,
        parent: Option<DeskNodeId>,
        order: DeskOrderKey,
    ) -> Result<DeskStructureOpRecord, String> {
        let mut write = self.db.write().await;
        let mut state = load_state_for_write(&mut write);
        let node = DeskNode {
            id: state.snapshot.allocate_node_id(),
            parent,
            order,
        };
        let record = apply_structure(
            &mut state.snapshot,
            DeskStructureOp::Insert { nodes: vec![node] },
            None,
        )?;
        write
            .open_table(STRUCTURE_OPS)
            .insert(&record.id.0, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    pub async fn apply_structure(
        &self,
        op: DeskStructureOp,
    ) -> Result<DeskStructureOpRecord, String> {
        if matches!(op, DeskStructureOp::Insert { .. }) {
            return Err("Desk node ids are allocated by DeskInsert".to_owned());
        }
        let mut write = self.db.write().await;
        let mut state = load_state_for_write(&mut write);
        let record = apply_structure(&mut state.snapshot, op, None)?;
        write
            .open_table(STRUCTURE_OPS)
            .insert(&record.id.0, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    pub async fn undo_structure(
        &self,
        op_id: DeskStructureOpId,
    ) -> Result<DeskStructureOpRecord, String> {
        let mut write = self.db.write().await;
        let mut state = load_state_for_write(&mut write);
        if state.snapshot.undone_structure_ops.contains(&op_id) {
            return Err(format!(
                "Desk structure operation {} is already undone",
                op_id.0
            ));
        }
        let original = write
            .open_table(STRUCTURE_OPS)
            .get(&op_id.0)
            .ok_or_else(|| format!("unknown Desk structure operation {}", op_id.0))?
            .value()
            .into_owned();
        let record = apply_structure(&mut state.snapshot, original.inverse, Some(op_id))?;
        state.snapshot.undone_structure_ops.push(op_id);
        write
            .open_table(STRUCTURE_OPS)
            .insert(&record.id.0, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    pub async fn apply_text(
        &self,
        node_id: DeskNodeId,
        operation: DeskOperation,
        transaction: Option<DeskTransaction>,
    ) -> Result<DeskTextOpRecord, String> {
        let replica_id = operation.replica_id();
        if !self.is_user_replica(replica_id) {
            return Err("Desk text operation has an unassigned user replica id".to_owned());
        }
        if !matches!(operation, DeskOperation::Edit { .. }) {
            return Err("Desk text undo must use DeskTextUndo".to_owned());
        }
        let transaction = transaction.ok_or_else(|| "Desk edit lacks transaction".to_owned())?;
        if transaction.id.replica_id != replica_id
            || !transaction.edit_ids.contains(&operation.timestamp())
            || transaction.edit_ids.len() > 1024
        {
            return Err("invalid Desk text transaction".to_owned());
        }
        self.append_text(node_id, operation, Some(transaction), None)
            .await
    }

    pub async fn undo_text(
        &self,
        node_id: DeskNodeId,
        transaction_id: DeskClock,
    ) -> Result<DeskTextOpRecord, String> {
        if !self.is_user_replica(transaction_id.replica_id) {
            return Err("Desk transaction has an unassigned user replica id".to_owned());
        }
        let snapshot = self.snapshot();
        let node_text = snapshot
            .texts
            .iter()
            .find(|text| text.node_id == node_id)
            .ok_or_else(|| "Desk node has no text history".to_owned())?;
        let transaction = node_text
            .transactions
            .iter()
            .find(|transaction| transaction.id == transaction_id)
            .ok_or_else(|| "unknown Desk text transaction".to_owned())?;
        let read = self.db.read();
        if read.open_table(TEXT_OPS).iter().any(|(_, value)| {
            let record = value.value();
            record.as_ref().node_id == node_id && record.as_ref().undo_of == Some(transaction_id)
        }) {
            return Err("Desk text transaction is already undone".to_owned());
        }
        drop(read);
        let mut buffer = node_text.buffer(transaction_id.replica_id)?;
        let operation = buffer.undo_edit_ids(transaction.edit_ids.iter().copied().map(Into::into));
        self.append_text(
            node_id,
            DeskOperation::from_text(&operation),
            None,
            Some(transaction_id),
        )
        .await
    }

    async fn append_text(
        &self,
        node_id: DeskNodeId,
        operation: DeskOperation,
        transaction: Option<DeskTransaction>,
        undo_of: Option<DeskClock>,
    ) -> Result<DeskTextOpRecord, String> {
        let mut write = self.db.write().await;
        let mut state = load_state_for_write(&mut write);
        if state.snapshot.node(node_id).is_none() {
            return Err(format!("unknown Desk node {}", node_id.0));
        }
        let mut node_text = self
            .snapshot()
            .texts
            .into_iter()
            .find(|text| text.node_id == node_id)
            .unwrap_or(DeskNodeText {
                node_id,
                operations: Vec::new(),
                transactions: Vec::new(),
            });
        if node_text
            .operations
            .iter()
            .any(|existing| existing.timestamp() == operation.timestamp())
        {
            return Err("duplicate Desk text operation timestamp".to_owned());
        }
        node_text.operations.push(operation.clone());
        let buffer = node_text.buffer(ReplicaId::REMOTE_SERVER.as_u16())?;
        if buffer.text().len() > 4 * 1024 * 1024 {
            return Err("Desk node text exceeds 4194304 bytes".to_owned());
        }
        let sequence = state.next_text_sequence;
        state.next_text_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| "Desk text sequence exhausted".to_owned())?;
        let record = DeskTextOpRecord {
            sequence,
            node_id,
            timestamp_ms: rho_core::UnixMs::now().0,
            operation,
            transaction,
            undo_of,
        };
        write
            .open_table(TEXT_OPS)
            .insert(&sequence, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    fn is_user_replica(&self, replica_id: u16) -> bool {
        self.db
            .read()
            .open_table(STATE)
            .get(&())
            .expect("Desk state initialized")
            .value()
            .as_ref()
            .snapshot
            .replicas
            .iter()
            .any(|replica| {
                replica.replica_id == replica_id
                    && matches!(replica.author, DeskReplicaAuthor::User)
            })
    }
}

fn load_state_for_write(write: &mut rho_db::WriteTxn) -> PersistentState {
    write
        .open_table(STATE)
        .get(&())
        .expect("Desk state initialized")
        .value()
        .into_owned()
}

fn save_state(write: &mut rho_db::WriteTxn, state: &PersistentState) {
    write
        .open_table(STATE)
        .insert(&(), SenValue::borrowed(state));
}

fn apply_structure(
    snapshot: &mut DeskSnapshot,
    op: DeskStructureOp,
    undo_of: Option<DeskStructureOpId>,
) -> Result<DeskStructureOpRecord, String> {
    let inverse = snapshot.apply_structure(&op)?;
    let id = DeskStructureOpId(
        snapshot
            .last_structure_op_id
            .checked_add(1)
            .ok_or_else(|| "Desk structure operation ids exhausted".to_owned())?,
    );
    snapshot.last_structure_op_id = id.0;
    Ok(DeskStructureOpRecord {
        id,
        author: DeskStructureAuthor::User,
        timestamp_ms: rho_core::UnixMs::now().0,
        op,
        inverse,
        undo_of,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persistence_round_trips_tree_text_and_native_undo() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        let store = DeskStore::new(db.clone()).await;
        let replica_id = store.allocate_user_replica().await.unwrap();
        let inserted = store.insert(None, DeskOrderKey::first()).await.unwrap();
        let node_id = match inserted.op {
            DeskStructureOp::Insert { ref nodes } => nodes[0].id,
            _ => unreachable!(),
        };
        let mut buffer = text::Buffer::new(
            ReplicaId::new(replica_id),
            text::BufferId::new(node_id.0).unwrap(),
            "",
        );
        let edit = buffer.edit([(0..0, "TODO plan\nremember this")]);
        let edit_id = edit.timestamp();
        let transaction = DeskTransaction {
            id: edit_id.into(),
            edit_ids: vec![edit_id.into()],
        };
        store
            .apply_text(
                node_id,
                DeskOperation::from_text(&edit),
                Some(transaction.clone()),
            )
            .await
            .unwrap();
        let agent_id = rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(7)).unwrap();
        store.bind(node_id, agent_id).await.unwrap();
        assert_eq!(
            store.snapshot().texts[0].buffer(9).unwrap().text(),
            "TODO plan\nremember this"
        );
        drop(store);

        let reopened = DeskStore::new(db).await;
        assert_eq!(reopened.snapshot().bindings[0].agent_id, agent_id);
        reopened.undo_text(node_id, transaction.id).await.unwrap();
        assert_eq!(reopened.snapshot().texts[0].buffer(10).unwrap().text(), "");
        reopened.undo_structure(inserted.id).await.unwrap();
        assert!(reopened.snapshot().nodes.is_empty());
        assert!(reopened.snapshot().bindings[0].orphaned);
        assert!(reopened.snapshot().next_node_id > node_id.0);
    }
}
