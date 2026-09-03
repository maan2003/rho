//! Durable daemon ownership of the structured Desk document.

use std::collections::{BTreeMap, BTreeSet};

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_desk::{
    BatchOpRecord, BatchOperation, Binding, Document, NodeId, NodeKind, NodeOwner, OperationBatch,
    OrderKey, Replica, ReplicaAuthor, Snapshot, TextOpRecord, TextOperation, TextTransaction,
    TreeClock, TreeOpRecord, TreeOperation,
};
use senax_encoder::{Decode, Encode};
use text::{BufferId, ReplicaId};

const STATE: TableDefinition<(), Sen<PersistentState>> =
    TableDefinition::new("rho_desk_tree_state_v1");
const TREE_OPS: TableDefinition<u64, Sen<TreeOpRecord>> =
    TableDefinition::new("rho_desk_tree_ops_v1");
const TEXT_OPS: TableDefinition<u64, Sen<TextOpRecord>> =
    TableDefinition::new("rho_desk_node_text_ops_v1");
const BATCH_OPS: TableDefinition<u64, Sen<BatchOpRecord>> =
    TableDefinition::new("rho_desk_batch_ops_v1");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentState {
    snapshot: Snapshot,
    next_sequence: u64,
    next_replica_id: u16,
    #[senax(default)]
    next_machine_counter: u64,
    #[senax(default)]
    next_machine_clock: u32,
}

#[derive(Clone)]
pub struct DeskTreeStore {
    db: RhoDb,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BatchApplyError {
    Conflict(String),
    Invalid(String),
    Unauthorized(String),
}

impl BatchApplyError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }
}

impl std::fmt::Display for BatchApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Invalid(message) | Self::Unauthorized(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl From<String> for BatchApplyError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl DeskTreeStore {
    pub fn validate_machine_parent(&self, parent: NodeId) -> Result<(), String> {
        let document = Document::from_snapshot(self.snapshot())?;
        let Some(node) = document
            .materialize()
            .into_iter()
            .find(|node| node.id == parent)
        else {
            return Err("Desk binding parent no longer exists".into());
        };
        if node.kind != NodeKind::Heading || node.owner != NodeOwner::User {
            return Err("Desk machine rows must be filed under a user heading".into());
        }
        Ok(())
    }

    pub async fn new(
        db: RhoDb,
        old_org_for_test: Option<&str>,
        resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>,
    ) -> Result<Self, String> {
        let mut write = db.write().await;
        write.open_table(TREE_OPS);
        write.open_table(TEXT_OPS);
        write.open_table(BATCH_OPS);
        let imported = crate::desk_org_migration::import(&mut write, &resolve_agent)?;
        let native_logs_exist = write.open_table(TREE_OPS).iter().next().is_some()
            || write.open_table(TEXT_OPS).iter().next().is_some()
            || write.open_table(BATCH_OPS).iter().next().is_some();
        let migration_completed = crate::desk_org_migration::already_completed(&mut write);
        let state_exists = write.open_table(STATE).get(&()).is_some();
        if migration_completed && !state_exists {
            return Err("Desk migration is marked complete but native state is missing".into());
        }
        if !migration_completed && native_logs_exist {
            return Err(
                "Desk org migration marker is missing after native tree edits; refusing to overwrite native state"
                    .into(),
            );
        }
        let imported = imported.or_else(|| {
            old_org_for_test.map(|text| crate::desk_org_migration::import_org(text, &resolve_agent))
        });
        if !state_exists {
            let imported = imported.clone().unwrap_or_default();
            write.open_table(STATE).insert(
                &(),
                SenValue::owned(PersistentState {
                    snapshot: imported.clone(),
                    next_sequence: 1,
                    next_replica_id: ReplicaId::FIRST_COLLAB_ID.as_u16(),
                    next_machine_counter: imported
                        .nodes
                        .iter()
                        .filter(|node| node.id.replica_id == ReplicaId::REMOTE_SERVER.as_u16())
                        .map(|node| node.id.counter)
                        .max()
                        .unwrap_or(0),
                    next_machine_clock: imported
                        .version
                        .iter()
                        .filter(|clock| clock.replica_id == ReplicaId::REMOTE_SERVER.as_u16())
                        .map(|clock| clock.value)
                        .max()
                        .unwrap_or(0),
                }),
            );
        } else if !crate::desk_org_migration::already_completed(&mut write)
            && let Some(imported) = imported
        {
            let mut state = load_state(&mut write);
            let replicas = std::mem::take(&mut state.snapshot.replicas);
            state.snapshot = imported;
            merge_replicas(&mut state.snapshot.replicas, replicas);
            state.next_machine_counter = state
                .snapshot
                .nodes
                .iter()
                .filter(|node| node.id.replica_id == ReplicaId::REMOTE_SERVER.as_u16())
                .map(|node| node.id.counter)
                .max()
                .unwrap_or(0);
            state.next_machine_clock = state
                .snapshot
                .version
                .iter()
                .filter(|clock| clock.replica_id == ReplicaId::REMOTE_SERVER.as_u16())
                .map(|clock| clock.value)
                .max()
                .unwrap_or(0);
            save_state(&mut write, &state);
        }
        if !crate::desk_org_migration::already_completed(&mut write) {
            crate::desk_org_migration::finish(&mut write);
        }
        write.commit();
        Ok(Self { db })
    }

    pub fn snapshot(&self) -> Snapshot {
        let read = self.db.read();
        let state = read
            .open_table(STATE)
            .get(&())
            .expect("Desk tree state initialized")
            .value()
            .into_owned();
        let mut document =
            Document::from_snapshot(state.snapshot).expect("stored Desk tree snapshot");
        let tree = read
            .open_table(TREE_OPS)
            .iter()
            .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
            .collect::<Vec<_>>();
        let text = read
            .open_table(TEXT_OPS)
            .iter()
            .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
            .collect::<Vec<_>>();
        let batches = read
            .open_table(BATCH_OPS)
            .iter()
            .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
            .collect::<Vec<_>>();
        replay(&mut document, tree, text, batches).expect("stored Desk operations");
        let mut snapshot = document.snapshot();
        snapshot.sequence = state.next_sequence.saturating_sub(1);
        snapshot
    }

    pub async fn allocate_replica(&self, author: ReplicaAuthor) -> Result<u16, String> {
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let replica_id = state.next_replica_id;
        state.next_replica_id = replica_id
            .checked_add(1)
            .ok_or("Desk replica id space exhausted")?;
        state.snapshot.replicas.push(Replica { replica_id, author });
        save_state(&mut write, &state);
        write.commit();
        Ok(replica_id)
    }

    pub async fn apply_tree(&self, operation: TreeOperation) -> Result<TreeOpRecord, String> {
        let replica = operation.timestamp().replica_id;
        if !matches!(self.replica_author(replica), Some(ReplicaAuthor::User)) {
            return Err("Desk tree operation has an unassigned user replica id".into());
        }
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let mut document = materialize(&mut write, &state)?;
        authorize_user_tree_operation(&document, &operation)?;
        if !document.apply(operation.clone())? {
            return Err("duplicate Desk tree operation timestamp".into());
        }
        let sequence = take_sequence(&mut state)?;
        let record = TreeOpRecord {
            sequence,
            timestamp_ms: rho_core::UnixMs::now().0,
            operation,
        };
        write
            .open_table(TREE_OPS)
            .insert(&sequence, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    /// Creates the machine-owned row for a runtime binding under a user
    /// heading. Repeating the same request is idempotent.
    pub async fn bind_machine(
        &self,
        parent: NodeId,
        binding: Binding,
    ) -> Result<Option<BatchOpRecord>, String> {
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let mut document = materialize(&mut write, &state)?;
        let nodes = document.materialize();
        let parent_node = nodes
            .iter()
            .find(|node| node.id == parent)
            .ok_or("Desk binding parent no longer exists")?;
        if parent_node.kind != NodeKind::Heading || parent_node.owner != NodeOwner::User {
            return Err("Desk machine rows must be filed under a user heading".into());
        }
        if nodes.iter().any(|node| {
            node.parent == Some(parent)
                && node.owner == NodeOwner::Machine
                && node.bindings.values().any(|value| value == &binding)
        }) {
            return Ok(None);
        }
        let replica_id = ReplicaId::REMOTE_SERVER.as_u16();
        state.next_machine_counter = state
            .next_machine_counter
            .checked_add(1)
            .ok_or("Desk machine node id exhausted")?;
        let node_id = NodeId {
            replica_id,
            counter: state.next_machine_counter,
        };
        let last = nodes
            .iter()
            .filter(|node| node.parent == Some(parent))
            .max_by_key(|node| &node.order);
        let order = OrderKey::between(last.map(|node| &node.order), None);
        let kind = match &binding {
            Binding::Agent(_) => NodeKind::Agent,
            Binding::Page(_) => NodeKind::Page,
            Binding::File(_) => NodeKind::File,
        };
        let id = take_machine_clock(&mut state)?;
        let operations = vec![
            BatchOperation::Tree(TreeOperation::Create {
                timestamp: take_machine_clock(&mut state)?,
                node_id,
                kind,
                owner: NodeOwner::Machine,
                parent: Some(parent),
                order,
            }),
            BatchOperation::Tree(TreeOperation::SetBinding {
                timestamp: take_machine_clock(&mut state)?,
                node_id,
                kind: binding.kind(),
                value: Some(binding),
            }),
        ];
        let record = persist_machine_batch(&mut write, &mut state, &mut document, id, operations)?;
        write.commit();
        Ok(Some(record))
    }

    /// Removes every machine row carrying this runtime binding.
    pub async fn unbind_machine(&self, binding: Binding) -> Result<Option<BatchOpRecord>, String> {
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let mut document = materialize(&mut write, &state)?;
        let node_ids = document
            .materialize()
            .into_iter()
            .filter(|node| {
                node.owner == NodeOwner::Machine
                    && node.bindings.values().any(|value| value == &binding)
            })
            .map(|node| node.id)
            .collect::<Vec<_>>();
        if node_ids.is_empty() {
            return Ok(None);
        }
        let id = take_machine_clock(&mut state)?;
        let operation = BatchOperation::Tree(TreeOperation::Delete {
            timestamp: take_machine_clock(&mut state)?,
            node_ids,
        });
        let record =
            persist_machine_batch(&mut write, &mut state, &mut document, id, vec![operation])?;
        write.commit();
        Ok(Some(record))
    }

    /// Removes one specific machine-owned row. Page clients use the row id
    /// rather than the page binding so closing one filing cannot remove a
    /// second filing of the same runtime page.
    pub async fn unbind_machine_node(
        &self,
        node_id: NodeId,
    ) -> Result<Option<BatchOpRecord>, String> {
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let mut document = materialize(&mut write, &state)?;
        let nodes = document.materialize();
        let Some(node) = nodes.iter().find(|node| node.id == node_id) else {
            return Ok(None);
        };
        if node.owner != NodeOwner::Machine || node.kind != NodeKind::Page {
            return Err("Desk page unbind must target a machine-owned page row".into());
        }
        let id = take_machine_clock(&mut state)?;
        let operation = BatchOperation::Tree(TreeOperation::Delete {
            timestamp: take_machine_clock(&mut state)?,
            node_ids: vec![node_id],
        });
        let record =
            persist_machine_batch(&mut write, &mut state, &mut document, id, vec![operation])?;
        write.commit();
        Ok(Some(record))
    }

    pub async fn apply_text(
        &self,
        node_id: NodeId,
        operation: TextOperation,
        transaction: Option<TextTransaction>,
    ) -> Result<TextOpRecord, String> {
        let replica = operation.timestamp().replica_id;
        if !matches!(
            self.replica_author(replica),
            Some(ReplicaAuthor::User | ReplicaAuthor::Agent(_))
        ) {
            return Err("Desk node text operation has an unassigned user replica id".into());
        }
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let mut document = materialize(&mut write, &state)?;
        let node = document
            .materialize()
            .into_iter()
            .find(|node| node.id == node_id)
            .ok_or("Desk node text operation references a deleted node")?;
        if node.owner != NodeOwner::User {
            return Err("machine-owned Desk nodes are read-only".into());
        }
        if !document.apply_text(node_id, operation.clone(), transaction.clone())? {
            return Err("duplicate Desk node text operation timestamp".into());
        }
        let buffer_id = BufferId::new(1).unwrap();
        if document
            .text(node_id, ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?
            .len()
            > 4 * 1024 * 1024
        {
            return Err("Desk node text exceeds 4194304 bytes".into());
        }
        let sequence = take_sequence(&mut state)?;
        let record = TextOpRecord {
            sequence,
            timestamp_ms: rho_core::UnixMs::now().0,
            node_id,
            operation,
            transaction,
        };
        write
            .open_table(TEXT_OPS)
            .insert(&sequence, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    pub async fn apply_batch(
        &self,
        batch: OperationBatch,
    ) -> Result<BatchOpRecord, BatchApplyError> {
        if batch.operations.is_empty()
            || batch.operations.len() > 4096
            || batch.expected.len() > 4096
        {
            return Err(BatchApplyError::Invalid(
                "Desk batch operation count is invalid".into(),
            ));
        }
        if !matches!(
            self.replica_author(batch.id.replica_id),
            Some(ReplicaAuthor::User)
        ) {
            return Err(BatchApplyError::Unauthorized(
                "Desk batch id has an unassigned user replica id".into(),
            ));
        }
        for operation in &batch.operations {
            let replica = match operation {
                BatchOperation::Tree(operation) => operation.timestamp().replica_id,
                BatchOperation::Text { operation, .. } => operation.timestamp().replica_id,
            };
            if replica != batch.id.replica_id {
                return Err(BatchApplyError::Unauthorized(
                    "Desk batch operations must use the batch replica".into(),
                ));
            }
            if !matches!(self.replica_author(replica), Some(ReplicaAuthor::User)) {
                return Err(BatchApplyError::Unauthorized(
                    "Desk batch has an unassigned user replica id".into(),
                ));
            }
        }
        let mut write = self.db.write().await;
        if let Some(old) = write
            .open_table(BATCH_OPS)
            .iter()
            .map(|(_, record)| record.value().into_owned())
            .find(|record| record.batch.id == batch.id)
        {
            return if old.batch == batch {
                Ok(old)
            } else {
                Err(BatchApplyError::Invalid(
                    "Desk batch id was reused with different content".into(),
                ))
            };
        }
        let mut state = load_state(&mut write);
        let mut document = materialize(&mut write, &state)?;
        let expected_nodes = batch
            .expected
            .iter()
            .map(|expected| expected.node_id)
            .collect::<BTreeSet<_>>();
        if expected_nodes.len() != batch.expected.len() {
            return Err(BatchApplyError::Invalid(
                "Desk batch has duplicate text preconditions".into(),
            ));
        }
        let materialized = document
            .materialize()
            .into_iter()
            .map(|node| (node.id, node))
            .collect::<BTreeMap<_, _>>();
        let existing = materialized.keys().copied().collect::<BTreeSet<_>>();
        for operation in &batch.operations {
            let targets: Vec<NodeId> = match operation {
                BatchOperation::Text { node_id, .. } => vec![*node_id],
                BatchOperation::Tree(TreeOperation::Create { .. }) => Vec::new(),
                BatchOperation::Tree(TreeOperation::Delete { node_ids, .. }) => node_ids.clone(),
                BatchOperation::Tree(
                    TreeOperation::Move { node_id, .. }
                    | TreeOperation::SetTemporal { node_id, .. }
                    | TreeOperation::SetBinding { node_id, .. }
                    | TreeOperation::SetTag { node_id, .. },
                ) => vec![*node_id],
            };
            if targets
                .iter()
                .any(|node_id| existing.contains(node_id) && !expected_nodes.contains(node_id))
            {
                return Err(BatchApplyError::Invalid(
                    "Desk batch is missing a source text version".into(),
                ));
            }
        }
        for expected in &batch.expected {
            let Some(node) = materialized.get(&expected.node_id) else {
                return Err(BatchApplyError::Conflict(
                    "Desk batch source node changed".into(),
                ));
            };
            let canonical = expected.text_version.iter().all(|clock| clock.value != 0)
                && expected
                    .text_version
                    .windows(2)
                    .all(|pair| pair[0].replica_id < pair[1].replica_id);
            if !canonical {
                return Err(BatchApplyError::Invalid(
                    "Desk batch text version is not canonical".into(),
                ));
            }
            if node.kind != expected.kind
                || node.owner != expected.owner
                || node.parent != expected.parent
                || node.order != expected.order
                || document.text_version(expected.node_id)? != expected.text_version
            {
                return Err(BatchApplyError::Conflict(
                    "Desk batch source text version changed".into(),
                ));
            }
        }
        let deleted = batch
            .operations
            .iter()
            .filter_map(|operation| match operation {
                BatchOperation::Tree(TreeOperation::Delete { node_ids, .. }) => Some(node_ids),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut effective_nodes = materialized
            .values()
            .map(|node| (node.id, (node.owner, node.parent)))
            .collect::<BTreeMap<_, _>>();
        for operation in &batch.operations {
            match operation {
                BatchOperation::Tree(TreeOperation::Create {
                    node_id,
                    owner,
                    parent,
                    ..
                }) => {
                    effective_nodes.insert(*node_id, (*owner, *parent));
                }
                BatchOperation::Tree(TreeOperation::Move {
                    node_id, parent, ..
                }) => {
                    if let Some((_, effective_parent)) = effective_nodes.get_mut(node_id) {
                        *effective_parent = *parent;
                    }
                }
                _ => {}
            }
        }
        if effective_nodes.iter().any(|(node_id, (owner, parent))| {
            if *owner != NodeOwner::User || deleted.contains(node_id) {
                return false;
            }
            let mut parent = *parent;
            let mut visited = BTreeSet::new();
            while let Some(parent_id) = parent {
                if deleted.contains(&parent_id) {
                    return true;
                }
                if !visited.insert(parent_id) {
                    break;
                }
                parent = effective_nodes
                    .get(&parent_id)
                    .and_then(|(_, parent)| *parent);
            }
            false
        }) {
            return Err(BatchApplyError::Conflict(
                "Desk batch delete omitted a user descendant".into(),
            ));
        }
        let machine_moves = derive_machine_relocations(&mut write, &materialized, &batch)?;
        for operation in &batch.operations {
            match operation {
                BatchOperation::Tree(operation) => {
                    authorize_user_tree_operation(&document, operation)
                        .map_err(BatchApplyError::Unauthorized)?;
                    if !document.apply(operation.clone())? {
                        return Err(BatchApplyError::Invalid(
                            "duplicate Desk batch tree timestamp".into(),
                        ));
                    }
                }
                BatchOperation::Text {
                    node_id,
                    operation,
                    transaction,
                } => {
                    let node = document
                        .materialize()
                        .into_iter()
                        .find(|node| node.id == *node_id)
                        .ok_or_else(|| {
                            BatchApplyError::Invalid(
                                "Desk batch text references a deleted node".into(),
                            )
                        })?;
                    if node.owner != NodeOwner::User {
                        return Err(BatchApplyError::Unauthorized(
                            "machine-owned Desk nodes are read-only".into(),
                        ));
                    }
                    if !document.apply_text(*node_id, operation.clone(), transaction.clone())? {
                        return Err(BatchApplyError::Invalid(
                            "duplicate Desk batch text timestamp".into(),
                        ));
                    }
                    if document
                        .text(
                            *node_id,
                            ReplicaId::REMOTE_SERVER.as_u16(),
                            BufferId::new(1).unwrap(),
                        )?
                        .len()
                        > 4 * 1024 * 1024
                    {
                        return Err(BatchApplyError::Invalid(
                            "Desk node text exceeds 4194304 bytes".into(),
                        ));
                    }
                }
            }
        }
        let mut daemon_tree_operations = Vec::with_capacity(machine_moves.len());
        for (node_id, parent, order) in machine_moves {
            let machine_timestamp = take_machine_clock(&mut state)?;
            let operation = TreeOperation::Move {
                timestamp: machine_timestamp,
                node_id,
                parent,
                order,
            };
            if !document.apply(operation.clone())? {
                return Err(BatchApplyError::Invalid(
                    "duplicate Desk machine relocation timestamp".into(),
                ));
            }
            daemon_tree_operations.push(operation);
        }
        let sequence = take_sequence(&mut state)?;
        let record = BatchOpRecord {
            sequence,
            timestamp_ms: rho_core::UnixMs::now().0,
            batch,
            daemon_tree_operations,
        };
        write
            .open_table(BATCH_OPS)
            .insert(&sequence, SenValue::borrowed(&record));
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    fn replica_author(&self, replica_id: u16) -> Option<ReplicaAuthor> {
        self.db
            .read()
            .open_table(STATE)
            .get(&())
            .expect("Desk tree state initialized")
            .value()
            .as_ref()
            .snapshot
            .replicas
            .iter()
            .find(|replica| replica.replica_id == replica_id)
            .map(|replica| replica.author.clone())
    }
}

fn merge_replicas(target: &mut Vec<Replica>, replicas: impl IntoIterator<Item = Replica>) {
    for replica in replicas {
        if !target
            .iter()
            .any(|existing| existing.replica_id == replica.replica_id)
        {
            target.push(replica);
        }
    }
}

fn take_machine_clock(state: &mut PersistentState) -> Result<TreeClock, String> {
    state.next_machine_clock = state
        .next_machine_clock
        .checked_add(1)
        .ok_or("Desk machine clock exhausted")?;
    Ok(TreeClock {
        value: state.next_machine_clock,
        replica_id: ReplicaId::REMOTE_SERVER.as_u16(),
    })
}

fn persist_machine_batch(
    write: &mut rho_db::WriteTxn,
    state: &mut PersistentState,
    document: &mut Document,
    id: TreeClock,
    operations: Vec<BatchOperation>,
) -> Result<BatchOpRecord, String> {
    for operation in &operations {
        let BatchOperation::Tree(operation) = operation else {
            return Err("Desk machine batch cannot edit user text".into());
        };
        if operation.timestamp().replica_id != ReplicaId::REMOTE_SERVER.as_u16() {
            return Err("Desk machine operation used a non-machine replica".into());
        }
        if !document.apply(operation.clone())? {
            return Err("duplicate Desk machine operation timestamp".into());
        }
    }
    let sequence = take_sequence(state)?;
    let record = BatchOpRecord {
        sequence,
        timestamp_ms: rho_core::UnixMs::now().0,
        batch: OperationBatch {
            id,
            expected: Vec::new(),
            operations,
            machine_relocation: None,
        },
        daemon_tree_operations: Vec::new(),
    };
    write
        .open_table(BATCH_OPS)
        .insert(&sequence, SenValue::borrowed(&record));
    save_state(write, state);
    Ok(record)
}

fn authorize_user_tree_operation(
    document: &Document,
    operation: &TreeOperation,
) -> Result<(), String> {
    if let TreeOperation::Create { owner, .. } = operation {
        return if *owner == NodeOwner::User {
            Ok(())
        } else {
            Err("clients cannot create machine-owned Desk nodes".into())
        };
    }
    let targets: Vec<NodeId> = match operation {
        TreeOperation::Move { node_id, .. }
        | TreeOperation::SetTemporal { node_id, .. }
        | TreeOperation::SetBinding { node_id, .. }
        | TreeOperation::SetTag { node_id, .. } => vec![*node_id],
        TreeOperation::Delete { node_ids, .. } => node_ids.clone(),
        TreeOperation::Create { .. } => unreachable!(),
    };
    if targets
        .iter()
        .any(|node_id| document.owner(*node_id) != Some(NodeOwner::User))
    {
        Err("clients cannot structurally edit machine-owned Desk nodes".into())
    } else {
        Ok(())
    }
}

fn derive_machine_relocations(
    write: &mut rho_db::WriteTxn,
    materialized: &BTreeMap<NodeId, rho_desk::MaterializedNode>,
    batch: &OperationBatch,
) -> Result<Vec<(NodeId, Option<NodeId>, OrderKey)>, BatchApplyError> {
    let deleted = batch
        .operations
        .iter()
        .filter_map(|operation| match operation {
            BatchOperation::Tree(TreeOperation::Delete { node_ids, .. }) => Some(node_ids),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<BTreeSet<_>>();
    let evacuated = materialized
        .values()
        .filter(|node| {
            node.owner == NodeOwner::Machine
                && node.parent.is_some_and(|parent| deleted.contains(&parent))
        })
        .collect::<Vec<_>>();
    match &batch.machine_relocation {
        None if evacuated.is_empty() => Ok(Vec::new()),
        None => Err(BatchApplyError::Unauthorized(
            "deleting this heading requires machine-row evacuation".into(),
        )),
        Some(rho_desk::MachineRelocationIntent::EvacuateDeletedChildren) => {
            let expected = batch
                .expected
                .iter()
                .map(|expected| expected.node_id)
                .collect::<BTreeSet<_>>();
            if evacuated.iter().any(|node| !expected.contains(&node.id)) {
                return Err(BatchApplyError::Invalid(
                    "Desk evacuation is missing a machine-row precondition".into(),
                ));
            }
            Ok(evacuated
                .into_iter()
                .map(|node| {
                    let mut destination = node.parent;
                    while destination.is_some_and(|parent| deleted.contains(&parent)) {
                        destination = destination.and_then(|parent| {
                            materialized.get(&parent).and_then(|node| node.parent)
                        });
                    }
                    (node.id, destination, node.order.clone())
                })
                .collect())
        }
        Some(rho_desk::MachineRelocationIntent::Restore {
            delete_batch_id,
            replacements,
        }) => {
            derive_machine_restoration(write, materialized, batch, *delete_batch_id, replacements)
        }
    }
}

fn derive_machine_restoration(
    write: &mut rho_db::WriteTxn,
    materialized: &BTreeMap<NodeId, rho_desk::MaterializedNode>,
    batch: &OperationBatch,
    delete_batch_id: TreeClock,
    replacements: &[rho_desk::NodeReplacement],
) -> Result<Vec<(NodeId, Option<NodeId>, OrderKey)>, BatchApplyError> {
    let records = write
        .open_table(BATCH_OPS)
        .iter()
        .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
        .collect::<Vec<_>>();
    let (delete_sequence, source) = records
        .iter()
        .find(|(_, record)| record.batch.id == delete_batch_id)
        .ok_or_else(|| {
            BatchApplyError::Unauthorized(
                "Desk machine restore references an unknown delete".into(),
            )
        })?;
    if source.batch.machine_relocation
        != Some(rho_desk::MachineRelocationIntent::EvacuateDeletedChildren)
    {
        return Err(BatchApplyError::Unauthorized(
            "Desk machine restore source was not an evacuation".into(),
        ));
    }
    let deleted = source
        .batch
        .operations
        .iter()
        .filter_map(|operation| match operation {
            BatchOperation::Tree(TreeOperation::Delete { node_ids, .. }) => Some(node_ids),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<BTreeSet<_>>();
    let replacement_map = replacements
        .iter()
        .map(|replacement| (replacement.deleted, replacement.replacement))
        .collect::<BTreeMap<_, _>>();
    let replacement_ids = replacements
        .iter()
        .map(|replacement| replacement.replacement)
        .collect::<BTreeSet<_>>();
    if replacements.len() > 4096
        || replacement_map.len() != replacements.len()
        || replacement_ids.len() != replacements.len()
        || replacement_map.len() != deleted.len()
        || replacement_map.keys().copied().collect::<BTreeSet<_>>() != deleted
    {
        return Err(BatchApplyError::Unauthorized(
            "Desk machine restore replacements do not cover the deleted subtree".into(),
        ));
    }
    let creates = batch
        .operations
        .iter()
        .filter_map(|operation| match operation {
            BatchOperation::Tree(TreeOperation::Create {
                node_id,
                kind,
                owner,
                parent,
                order,
                ..
            }) => Some((*node_id, (*kind, *owner, *parent, order))),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for expected in source
        .batch
        .expected
        .iter()
        .filter(|expected| deleted.contains(&expected.node_id))
    {
        let replacement = replacement_map[&expected.node_id];
        let translated_parent = expected
            .parent
            .map(|parent| replacement_map.get(&parent).copied().unwrap_or(parent));
        if !matches!(creates.get(&replacement),
            Some((kind, NodeOwner::User, parent, order))
                if *kind == expected.kind
                    && *parent == translated_parent
                    && **order == expected.order)
        {
            return Err(BatchApplyError::Unauthorized(
                "Desk machine restore replacement does not match the deleted node".into(),
            ));
        }
    }
    let mut moves = Vec::new();
    let expected = batch
        .expected
        .iter()
        .map(|expected| expected.node_id)
        .collect::<BTreeSet<_>>();
    for operation in &source.daemon_tree_operations {
        let TreeOperation::Move {
            node_id,
            parent: evacuated_parent,
            order,
            ..
        } = operation
        else {
            continue;
        };
        let original_parent = source
            .batch
            .expected
            .iter()
            .find(|expected| expected.node_id == *node_id)
            .and_then(|expected| expected.parent)
            .ok_or_else(|| BatchApplyError::Invalid("evacuation lost original parent".into()))?;
        let current = materialized.get(node_id).ok_or_else(|| {
            BatchApplyError::Conflict("evacuated machine row no longer exists".into())
        })?;
        if !expected.contains(node_id) {
            return Err(BatchApplyError::Invalid(
                "Desk restoration is missing a machine-row precondition".into(),
            ));
        }
        let moved_later = records.iter().any(|(sequence, record)| {
            sequence > delete_sequence
                && record.daemon_tree_operations.iter().any(|operation| {
                    matches!(operation, TreeOperation::Move { node_id: later, .. } if later == node_id)
                })
        });
        if current.owner != NodeOwner::Machine
            || current.parent != *evacuated_parent
            || current.order != *order
            || moved_later
        {
            return Err(BatchApplyError::Conflict(
                "evacuated machine row changed after deletion".into(),
            ));
        }
        moves.push((
            *node_id,
            Some(replacement_map[&original_parent]),
            order.clone(),
        ));
    }
    Ok(moves)
}

fn load_state(write: &mut rho_db::WriteTxn) -> PersistentState {
    write
        .open_table(STATE)
        .get(&())
        .expect("Desk tree state initialized")
        .value()
        .into_owned()
}

fn save_state(write: &mut rho_db::WriteTxn, state: &PersistentState) {
    write
        .open_table(STATE)
        .insert(&(), SenValue::borrowed(state));
}

fn take_sequence(state: &mut PersistentState) -> Result<u64, String> {
    let sequence = state.next_sequence;
    state.next_sequence = sequence
        .checked_add(1)
        .ok_or("Desk operation sequence exhausted")?;
    Ok(sequence)
}

fn materialize(write: &mut rho_db::WriteTxn, state: &PersistentState) -> Result<Document, String> {
    let mut document = Document::from_snapshot(state.snapshot.clone())?;
    let tree = write
        .open_table(TREE_OPS)
        .iter()
        .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
        .collect::<Vec<_>>();
    let text = write
        .open_table(TEXT_OPS)
        .iter()
        .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
        .collect::<Vec<_>>();
    let batches = write
        .open_table(BATCH_OPS)
        .iter()
        .map(|(sequence, record)| (sequence.value(), record.value().into_owned()))
        .collect::<Vec<_>>();
    replay(&mut document, tree, text, batches)?;
    Ok(document)
}

enum StoredOperation {
    Tree(TreeOpRecord),
    Text(TextOpRecord),
    Batch(BatchOpRecord),
}

fn replay(
    document: &mut Document,
    tree: impl IntoIterator<Item = (u64, TreeOpRecord)>,
    text: impl IntoIterator<Item = (u64, TextOpRecord)>,
    batches: impl IntoIterator<Item = (u64, BatchOpRecord)>,
) -> Result<(), String> {
    let mut operations = tree
        .into_iter()
        .map(|(sequence, record)| (sequence, StoredOperation::Tree(record)))
        .chain(
            text.into_iter()
                .map(|(sequence, record)| (sequence, StoredOperation::Text(record))),
        )
        .chain(
            batches
                .into_iter()
                .map(|(sequence, record)| (sequence, StoredOperation::Batch(record))),
        )
        .collect::<Vec<_>>();
    operations.sort_by_key(|(sequence, _)| *sequence);
    for (_, operation) in operations {
        match operation {
            StoredOperation::Tree(record) => {
                document.apply(record.operation)?;
            }
            StoredOperation::Text(record) => {
                document.apply_text(record.node_id, record.operation, record.transaction)?;
            }
            StoredOperation::Batch(record) => {
                for operation in record.batch.operations {
                    match operation {
                        BatchOperation::Tree(operation) => {
                            document.apply(operation)?;
                        }
                        BatchOperation::Text {
                            node_id,
                            operation,
                            transaction,
                        } => {
                            document.apply_text(node_id, operation, transaction)?;
                        }
                    }
                }
                for operation in record.daemon_tree_operations {
                    document.apply(operation)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rho_desk::{BindingKind, TemporalKind};

    use super::*;

    fn legacy_snapshot(text: &str) -> String {
        text.to_owned()
    }

    fn expectation(document: &Document, node_id: NodeId) -> rho_desk::NodeExpectation {
        let node = document
            .materialize()
            .into_iter()
            .find(|node| node.id == node_id)
            .unwrap();
        rho_desk::NodeExpectation {
            node_id,
            kind: node.kind,
            owner: node.owner,
            parent: node.parent,
            order: node.order,
            text_version: document.text_version(node_id).unwrap(),
        }
    }

    #[test]
    fn imports_headings_prose_marks_and_tags() {
        let snapshot = crate::desk_org_migration::import_org(
            "preamble\n* TODO Work :keep:\n:todo: 2026-03-01 2d\nbody\n** Child\nchild body\n",
            |_| None,
        );
        let document = Document::from_snapshot(snapshot).unwrap();
        let nodes = document.materialize();
        assert_eq!(
            nodes.iter().map(|node| node.kind).collect::<Vec<_>>(),
            vec![
                NodeKind::Prose,
                NodeKind::Heading,
                NodeKind::Prose,
                NodeKind::Heading,
                NodeKind::Prose
            ]
        );
        assert!(nodes[1].tags.contains("keep"));
        assert!(nodes[1].temporal.contains_key(&TemporalKind::Todo));
        assert_eq!(nodes[3].parent, Some(nodes[1].id));
        let child_body = document
            .text(nodes[4].id, 9, BufferId::new(9).unwrap())
            .unwrap();
        assert_eq!(child_body, "child body\n");
    }

    #[test]
    fn import_preserves_multiple_machine_children_and_malformed_properties() {
        let first = rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(7)).unwrap();
        let second = rho_core::AgentId::from_counter(2, &rho_core::AgentIdDomain(7)).unwrap();
        let page = uuid::Uuid::new_v4();
        let text = format!(
            "* Work :eng-one:web-{page}:eng-two:web-not-a-uuid:\n:agent: broken\n:todo: not-a-date\n"
        );
        let snapshot = crate::desk_org_migration::import_org(&text, |tag| match tag {
            "eng-one" => Some(first),
            "eng-two" => Some(second),
            _ => None,
        });
        let document = Document::from_snapshot(snapshot).unwrap();
        let nodes = document.materialize();
        assert_eq!(
            nodes.iter().map(|node| node.kind).collect::<Vec<_>>(),
            vec![
                NodeKind::Heading,
                NodeKind::Agent,
                NodeKind::Page,
                NodeKind::Agent,
                NodeKind::Prose,
            ]
        );
        assert_eq!(
            nodes[1].bindings.get(&BindingKind::Agent),
            Some(&Binding::Agent(first))
        );
        assert_eq!(
            nodes[3].bindings.get(&BindingKind::Agent),
            Some(&Binding::Agent(second))
        );
        let prose = document
            .text(nodes[4].id, 9, BufferId::new(9).unwrap())
            .unwrap();
        assert!(prose.contains(":agent: broken"));
        assert!(prose.contains(":todo: not-a-date"));
    }

    #[tokio::test]
    async fn persists_tree_and_per_node_text_operations() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let old = String::new();
        let store = DeskTreeStore::new(db.clone(), Some(&old), |_| None)
            .await
            .unwrap();
        let replica = store.allocate_replica(ReplicaAuthor::User).await.unwrap();
        let node_id = NodeId {
            replica_id: replica,
            counter: 1,
        };
        store
            .apply_tree(TreeOperation::Create {
                timestamp: TreeClock {
                    value: 1,
                    replica_id: replica,
                },
                node_id,
                kind: NodeKind::Prose,
                owner: NodeOwner::User,
                parent: None,
                order: OrderKey(vec![100]),
            })
            .await
            .unwrap();
        let mut buffer = text::Buffer::new(ReplicaId::new(replica), BufferId::new(1).unwrap(), "");
        let operation = TextOperation::from_text(&buffer.edit([(0..0, "hello\n")]));
        store.apply_text(node_id, operation, None).await.unwrap();
        assert_eq!(store.snapshot().sequence, 2);

        store
            .apply_tree(TreeOperation::Delete {
                timestamp: TreeClock {
                    value: 3,
                    replica_id: replica,
                },
                node_ids: vec![node_id],
            })
            .await
            .unwrap();

        let reopened = DeskTreeStore::new(db, Some(&old), |_| None).await.unwrap();
        let document = Document::from_snapshot(reopened.snapshot()).unwrap();
        assert_eq!(document.materialize().len(), 0);
        assert_eq!(
            document
                .text(node_id, 33, BufferId::new(33).unwrap())
                .unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn user_delete_relocates_machine_row_and_undo_restores_parent() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-relocation.redb"));
        let store = DeskTreeStore::new(db, Some(&String::new()), |_| None)
            .await
            .unwrap();
        let replica = store.allocate_replica(ReplicaAuthor::User).await.unwrap();
        let heading = NodeId {
            replica_id: replica,
            counter: 1,
        };
        store
            .apply_tree(TreeOperation::Create {
                timestamp: TreeClock {
                    value: 1,
                    replica_id: replica,
                },
                node_id: heading,
                kind: NodeKind::Heading,
                owner: NodeOwner::User,
                parent: None,
                order: OrderKey(vec![100]),
            })
            .await
            .unwrap();
        let second_heading = NodeId {
            replica_id: replica,
            counter: 2,
        };
        store
            .apply_tree(TreeOperation::Create {
                timestamp: TreeClock {
                    value: 2,
                    replica_id: replica,
                },
                node_id: second_heading,
                kind: NodeKind::Heading,
                owner: NodeOwner::User,
                parent: None,
                order: OrderKey(vec![100]),
            })
            .await
            .unwrap();
        let agent = rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(7)).unwrap();
        let machine_record = store
            .bind_machine(heading, Binding::Agent(agent))
            .await
            .unwrap()
            .unwrap();
        let machine = machine_record
            .batch
            .operations
            .iter()
            .find_map(|operation| match operation {
                BatchOperation::Tree(TreeOperation::Create { node_id, .. }) => Some(*node_id),
                _ => None,
            })
            .unwrap();
        let before = Document::from_snapshot(store.snapshot()).unwrap();
        let delete_id = TreeClock {
            value: 3,
            replica_id: replica,
        };
        let without_intent = store
            .apply_batch(OperationBatch {
                id: delete_id,
                expected: vec![
                    expectation(&before, heading),
                    expectation(&before, second_heading),
                    expectation(&before, machine),
                ],
                operations: vec![BatchOperation::Tree(TreeOperation::Delete {
                    timestamp: TreeClock {
                        value: 4,
                        replica_id: replica,
                    },
                    node_ids: vec![heading, second_heading],
                })],
                machine_relocation: None,
            })
            .await;
        assert!(matches!(
            without_intent,
            Err(BatchApplyError::Unauthorized(_))
        ));
        let delete = store
            .apply_batch(OperationBatch {
                id: delete_id,
                expected: vec![
                    expectation(&before, heading),
                    expectation(&before, second_heading),
                    expectation(&before, machine),
                ],
                operations: vec![BatchOperation::Tree(TreeOperation::Delete {
                    timestamp: TreeClock {
                        value: 4,
                        replica_id: replica,
                    },
                    node_ids: vec![heading, second_heading],
                })],
                machine_relocation: Some(
                    rho_desk::MachineRelocationIntent::EvacuateDeletedChildren,
                ),
            })
            .await
            .unwrap();
        assert!(matches!(
            delete.daemon_tree_operations[0],
            TreeOperation::Move { .. }
        ));
        let moved = Document::from_snapshot(store.snapshot()).unwrap();
        assert_eq!(
            moved
                .materialize()
                .into_iter()
                .find(|node| node.id == machine)
                .unwrap()
                .parent,
            None
        );

        let restored = NodeId {
            replica_id: replica,
            counter: 3,
        };
        let duplicate_restore = store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 5,
                    replica_id: replica,
                },
                expected: vec![expectation(&moved, machine)],
                operations: vec![BatchOperation::Tree(TreeOperation::Create {
                    timestamp: TreeClock {
                        value: 6,
                        replica_id: replica,
                    },
                    node_id: restored,
                    kind: NodeKind::Heading,
                    owner: NodeOwner::User,
                    parent: None,
                    order: OrderKey(vec![100]),
                })],
                machine_relocation: Some(rho_desk::MachineRelocationIntent::Restore {
                    delete_batch_id: delete_id,
                    replacements: vec![
                        rho_desk::NodeReplacement {
                            deleted: heading,
                            replacement: restored,
                        },
                        rho_desk::NodeReplacement {
                            deleted: second_heading,
                            replacement: restored,
                        },
                    ],
                }),
            })
            .await;
        assert!(matches!(
            duplicate_restore,
            Err(BatchApplyError::Unauthorized(_))
        ));
        let second_restored = NodeId {
            replica_id: replica,
            counter: 4,
        };
        store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 7,
                    replica_id: replica,
                },
                expected: vec![expectation(&moved, machine)],
                operations: vec![
                    BatchOperation::Tree(TreeOperation::Create {
                        timestamp: TreeClock {
                            value: 8,
                            replica_id: replica,
                        },
                        node_id: restored,
                        kind: NodeKind::Heading,
                        owner: NodeOwner::User,
                        parent: None,
                        order: OrderKey(vec![100]),
                    }),
                    BatchOperation::Tree(TreeOperation::Create {
                        timestamp: TreeClock {
                            value: 9,
                            replica_id: replica,
                        },
                        node_id: second_restored,
                        kind: NodeKind::Heading,
                        owner: NodeOwner::User,
                        parent: None,
                        order: OrderKey(vec![100]),
                    }),
                ],
                machine_relocation: Some(rho_desk::MachineRelocationIntent::Restore {
                    delete_batch_id: delete_id,
                    replacements: vec![
                        rho_desk::NodeReplacement {
                            deleted: heading,
                            replacement: restored,
                        },
                        rho_desk::NodeReplacement {
                            deleted: second_heading,
                            replacement: second_restored,
                        },
                    ],
                }),
            })
            .await
            .unwrap();
        let final_document = Document::from_snapshot(store.snapshot()).unwrap();
        assert_eq!(
            final_document
                .materialize()
                .into_iter()
                .find(|node| node.id == machine)
                .unwrap()
                .parent,
            Some(restored)
        );
    }

    #[tokio::test]
    async fn batch_rejects_stale_text_versions_without_partial_tree_changes() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, Some(""), |_| None).await.unwrap();
        let replica = store.allocate_replica(ReplicaAuthor::User).await.unwrap();
        let node_id = NodeId {
            replica_id: replica,
            counter: 1,
        };
        store
            .apply_tree(TreeOperation::Create {
                timestamp: TreeClock {
                    value: 1,
                    replica_id: replica,
                },
                node_id,
                kind: NodeKind::Prose,
                owner: NodeOwner::User,
                parent: None,
                order: OrderKey(vec![100]),
            })
            .await
            .unwrap();
        let mut buffer = text::Buffer::new(ReplicaId::new(replica), BufferId::new(1).unwrap(), "");
        let first = TextOperation::from_text(&buffer.edit([(0..0, "before")]));
        store.apply_text(node_id, first, None).await.unwrap();
        let before = Document::from_snapshot(store.snapshot()).unwrap();
        let expected = expectation(&before, node_id);

        let concurrent = TextOperation::from_text(&buffer.edit([(6..6, " concurrent")]));
        store.apply_text(node_id, concurrent, None).await.unwrap();
        let result = store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 10,
                    replica_id: replica,
                },
                expected: vec![expected],
                operations: vec![BatchOperation::Tree(TreeOperation::Delete {
                    timestamp: TreeClock {
                        value: 4,
                        replica_id: replica,
                    },
                    node_ids: vec![node_id],
                })],
                machine_relocation: None,
            })
            .await;
        assert_eq!(
            result.unwrap_err(),
            BatchApplyError::Conflict("Desk batch source text version changed".into())
        );
        let after = Document::from_snapshot(store.snapshot()).unwrap();
        assert_eq!(after.materialize().len(), 1);
        assert_eq!(
            after.text(node_id, 9, BufferId::new(9).unwrap()).unwrap(),
            "before concurrent"
        );
        let current = expectation(&after, node_id);
        let created_id = NodeId {
            replica_id: replica,
            counter: 2,
        };
        let missing_id = NodeId {
            replica_id: replica,
            counter: 3,
        };
        let mut missing_buffer =
            text::Buffer::new(ReplicaId::new(replica), BufferId::new(3).unwrap(), "");
        let invalid_text = TextOperation::from_text(&missing_buffer.edit([(0..0, "lost")]));
        let late_failure = store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 12,
                    replica_id: replica,
                },
                expected: vec![current.clone()],
                operations: vec![
                    BatchOperation::Tree(TreeOperation::Create {
                        timestamp: TreeClock {
                            value: 6,
                            replica_id: replica,
                        },
                        node_id: created_id,
                        kind: NodeKind::Heading,
                        owner: NodeOwner::User,
                        parent: None,
                        order: OrderKey(vec![200]),
                    }),
                    BatchOperation::Text {
                        node_id: missing_id,
                        operation: invalid_text,
                        transaction: None,
                    },
                ],
                machine_relocation: None,
            })
            .await;
        assert!(matches!(late_failure, Err(BatchApplyError::Invalid(_))));
        let unchanged = store.snapshot();
        assert_eq!(unchanged.sequence, 3);
        assert_eq!(
            Document::from_snapshot(unchanged)
                .unwrap()
                .materialize()
                .len(),
            1
        );
        let batch = OperationBatch {
            id: TreeClock {
                value: 11,
                replica_id: replica,
            },
            expected: vec![current],
            operations: vec![BatchOperation::Tree(TreeOperation::Delete {
                timestamp: TreeClock {
                    value: 5,
                    replica_id: replica,
                },
                node_ids: vec![node_id],
            })],
            machine_relocation: None,
        };
        let record = store.apply_batch(batch.clone()).await.unwrap();
        assert_eq!(record.sequence, 4);
        assert_eq!(store.apply_batch(batch).await.unwrap().sequence, 4);
        assert_eq!(
            store
                .apply_batch(OperationBatch {
                    id: TreeClock {
                        value: 11,
                        replica_id: replica,
                    },
                    expected: Vec::new(),
                    operations: vec![BatchOperation::Tree(TreeOperation::Delete {
                        timestamp: TreeClock {
                            value: 6,
                            replica_id: replica,
                        },
                        node_ids: vec![node_id],
                    })],
                    machine_relocation: None,
                })
                .await
                .unwrap_err(),
            BatchApplyError::Invalid("Desk batch id was reused with different content".into())
        );
        assert!(
            Document::from_snapshot(store.snapshot())
                .unwrap()
                .materialize()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn batch_delete_cannot_orphan_user_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-delete-child.redb"));
        let store = DeskTreeStore::new(db, Some(""), |_| None).await.unwrap();
        let replica = store.allocate_replica(ReplicaAuthor::User).await.unwrap();
        let parent = NodeId {
            replica_id: replica,
            counter: 1,
        };
        let child = NodeId {
            replica_id: replica,
            counter: 2,
        };
        let other = NodeId {
            replica_id: replica,
            counter: 3,
        };
        let created = NodeId {
            replica_id: replica,
            counter: 4,
        };
        for (timestamp, node_id, parent) in [
            (1, parent, None),
            (2, child, Some(parent)),
            (3, other, None),
        ] {
            store
                .apply_tree(TreeOperation::Create {
                    timestamp: TreeClock {
                        value: timestamp,
                        replica_id: replica,
                    },
                    node_id,
                    kind: NodeKind::Heading,
                    owner: NodeOwner::User,
                    parent,
                    order: OrderKey(vec![100]),
                })
                .await
                .unwrap();
        }
        let before = Document::from_snapshot(store.snapshot()).unwrap();
        let result = store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 3,
                    replica_id: replica,
                },
                expected: vec![expectation(&before, parent)],
                operations: vec![BatchOperation::Tree(TreeOperation::Delete {
                    timestamp: TreeClock {
                        value: 4,
                        replica_id: replica,
                    },
                    node_ids: vec![parent],
                })],
                machine_relocation: None,
            })
            .await;
        assert!(matches!(result, Err(BatchApplyError::Conflict(_))));
        let created_below_deleted = store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 4,
                    replica_id: replica,
                },
                expected: vec![expectation(&before, parent), expectation(&before, child)],
                operations: vec![
                    BatchOperation::Tree(TreeOperation::Delete {
                        timestamp: TreeClock {
                            value: 5,
                            replica_id: replica,
                        },
                        node_ids: vec![parent, child],
                    }),
                    BatchOperation::Tree(TreeOperation::Create {
                        timestamp: TreeClock {
                            value: 6,
                            replica_id: replica,
                        },
                        node_id: created,
                        kind: NodeKind::Heading,
                        owner: NodeOwner::User,
                        parent: Some(child),
                        order: OrderKey(vec![100]),
                    }),
                ],
                machine_relocation: None,
            })
            .await;
        assert!(matches!(
            created_below_deleted,
            Err(BatchApplyError::Conflict(_))
        ));
        let moved_below_deleted = store
            .apply_batch(OperationBatch {
                id: TreeClock {
                    value: 4,
                    replica_id: replica,
                },
                expected: vec![
                    expectation(&before, parent),
                    expectation(&before, child),
                    expectation(&before, other),
                ],
                operations: vec![
                    BatchOperation::Tree(TreeOperation::Delete {
                        timestamp: TreeClock {
                            value: 5,
                            replica_id: replica,
                        },
                        node_ids: vec![parent, child],
                    }),
                    BatchOperation::Tree(TreeOperation::Move {
                        timestamp: TreeClock {
                            value: 6,
                            replica_id: replica,
                        },
                        node_id: other,
                        parent: Some(child),
                        order: OrderKey(vec![100]),
                    }),
                ],
                machine_relocation: None,
            })
            .await;
        assert!(matches!(
            moved_below_deleted,
            Err(BatchApplyError::Conflict(_))
        ));
        let nodes = Document::from_snapshot(store.snapshot())
            .unwrap()
            .materialize();
        assert!(nodes.iter().any(|node| node.id == parent));
        assert!(nodes.iter().any(|node| node.id == child));
        assert!(nodes.iter().any(|node| node.id == other));
        assert!(!nodes.iter().any(|node| node.id == created));
    }

    #[tokio::test]
    async fn overlapping_deletes_authorize_against_tombstoned_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, Some(""), |_| None).await.unwrap();
        let replica = store.allocate_replica(ReplicaAuthor::User).await.unwrap();
        let ids = [1, 2].map(|counter| NodeId {
            replica_id: replica,
            counter,
        });
        for (index, node_id) in ids.into_iter().enumerate() {
            store
                .apply_tree(TreeOperation::Create {
                    timestamp: TreeClock {
                        value: index as u32 + 1,
                        replica_id: replica,
                    },
                    node_id,
                    kind: NodeKind::Heading,
                    owner: NodeOwner::User,
                    parent: None,
                    order: OrderKey(vec![(index as u16 + 1) * 100]),
                })
                .await
                .unwrap();
        }
        store
            .apply_tree(TreeOperation::Delete {
                timestamp: TreeClock {
                    value: 3,
                    replica_id: replica,
                },
                node_ids: vec![ids[0]],
            })
            .await
            .unwrap();
        store
            .apply_tree(TreeOperation::Delete {
                timestamp: TreeClock {
                    value: 4,
                    replica_id: replica,
                },
                node_ids: ids.to_vec(),
            })
            .await
            .unwrap();
        assert!(
            store
                .snapshot()
                .nodes
                .iter()
                .all(|node| node.deleted_at.is_some())
        );
    }

    #[tokio::test]
    async fn machine_bindings_are_idempotent_and_removed_with_the_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, Some(&legacy_snapshot("* parent\n")), |_| None)
            .await
            .unwrap();
        let parent = Document::from_snapshot(store.snapshot())
            .unwrap()
            .materialize()[0]
            .id;
        let binding = Binding::Agent(
            rho_core::AgentId::from_counter(11, &rho_core::AgentIdDomain(7)).unwrap(),
        );

        let record = store
            .bind_machine(parent, binding.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.batch.operations.len(), 2);
        assert!(matches!(
            (&record.batch.operations[0], &record.batch.operations[1]),
            (
                BatchOperation::Tree(TreeOperation::Create { .. }),
                BatchOperation::Tree(TreeOperation::SetBinding { .. })
            )
        ));
        assert!(
            store
                .bind_machine(parent, binding.clone())
                .await
                .unwrap()
                .is_none()
        );
        let bound = Document::from_snapshot(store.snapshot()).unwrap();
        let rows = bound.materialize();
        assert!(rows.iter().any(|node| {
            node.parent == Some(parent)
                && node.owner == NodeOwner::Machine
                && node.bindings.get(&BindingKind::Agent) == Some(&binding)
        }));

        assert!(store.unbind_machine(binding).await.unwrap().is_some());
        let unbound = Document::from_snapshot(store.snapshot()).unwrap();
        assert_eq!(unbound.materialize().len(), 1);
    }

    #[tokio::test]
    async fn page_unbind_removes_only_the_addressed_machine_row() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, Some(&legacy_snapshot("* first\n* second\n")), |_| None)
            .await
            .unwrap();
        let headings = Document::from_snapshot(store.snapshot())
            .unwrap()
            .materialize()
            .into_iter()
            .map(|node| node.id)
            .collect::<Vec<_>>();
        let binding = Binding::Page(rho_desk::PageId([7; 16]));
        store
            .bind_machine(headings[0], binding.clone())
            .await
            .unwrap();
        store
            .bind_machine(headings[1], binding.clone())
            .await
            .unwrap();
        let rows = Document::from_snapshot(store.snapshot())
            .unwrap()
            .materialize();
        let first_page = rows
            .iter()
            .find(|node| node.parent == Some(headings[0]) && node.kind == NodeKind::Page)
            .unwrap()
            .id;

        store.unbind_machine_node(first_page).await.unwrap();
        let remaining = Document::from_snapshot(store.snapshot())
            .unwrap()
            .materialize();
        assert!(!remaining.iter().any(|node| node.id == first_page));
        assert!(remaining.iter().any(|node| {
            node.parent == Some(headings[1])
                && node.bindings.get(&BindingKind::Page) == Some(&binding)
        }));
    }
}
