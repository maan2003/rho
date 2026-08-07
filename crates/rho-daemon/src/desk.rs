//! Durable daemon ownership for one org-like Desk CRDT document.

use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_agent::db::AgentReadTxnExt as _;
use rho_db::{RhoDb, Sen, SenValue};
use rho_ui_proto::desk::{
    DeskBinding, DeskIdToken, DeskOperation, DeskReplica, DeskReplicaAuthor, DeskSnapshot,
    DeskTextOpRecord, DeskTransaction, parse,
};
use senax_encoder::{Decode, Encode};
use text::ReplicaId;

const STATE: TableDefinition<(), Sen<PersistentState>> = TableDefinition::new("rho_desk_state_v2");
const TEXT_OPS: TableDefinition<u64, Sen<DeskTextOpRecord>> =
    TableDefinition::new("rho_desk_text_ops_v2");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentState {
    snapshot: DeskSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            snapshot: DeskSnapshot {
                next_id: 1,
                ..DeskSnapshot::default()
            },
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
        write.open_table(TEXT_OPS);
        if write.open_table(STATE).get(&()).is_none() {
            let migrated = migrate_v1(&mut write);
            let state = migrated.unwrap_or_default();
            write.delete_table("rho_desk_structure_ops_v1");
            write.delete_table("rho_desk_text_ops_v1");
            write.delete_table("rho_desk_state_v1");
            write.open_table(STATE).insert(&(), SenValue::owned(state));
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
        snapshot.operations.clear();
        snapshot.transactions.clear();
        for (_, value) in read.open_table(TEXT_OPS).iter() {
            let record = value.value();
            snapshot.operations.push(record.as_ref().operation.clone());
            if let Some(transaction) = &record.as_ref().transaction {
                snapshot.transactions.push(transaction.clone());
            }
        }
        snapshot.text = snapshot.document_text().unwrap_or_default();
        snapshot
    }

    pub async fn allocate_user_replica(&self) -> Result<u16, String> {
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let replica_id = state.next_replica_id;
        state.next_replica_id = replica_id
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

    pub async fn apply_text(
        &self,
        operation: DeskOperation,
        transaction: Option<DeskTransaction>,
    ) -> Result<DeskTextOpRecord, String> {
        if !self.is_user_replica(operation.replica_id()) {
            return Err("Desk text operation has an unassigned user replica id".to_owned());
        }
        if let Some(transaction) = &transaction
            && (transaction.id.replica_id != operation.replica_id()
                || !transaction.edit_ids.contains(&operation.timestamp())
                || transaction.edit_ids.len() > 1024)
        {
            return Err("invalid Desk text transaction".to_owned());
        }
        self.append_text(operation, transaction).await
    }

    /// Applies one atomic model-authored edit using a stable replica associated
    /// with the agent. Every non-empty search string is resolved against the
    /// same current document before anything is written.
    pub async fn apply_agent_edits(
        &self,
        agent_id: rho_ui_proto::AgentId,
        edits: &[(String, String)],
    ) -> Result<DeskTextOpRecord, String> {
        if edits.is_empty() {
            return Err("Desk edit list must not be empty".to_owned());
        }

        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let replica_id = if let Some(replica) = state
            .snapshot
            .replicas
            .iter()
            .find(|replica| replica.author == DeskReplicaAuthor::Agent(agent_id))
        {
            replica.replica_id
        } else {
            let replica_id = state.next_replica_id;
            state.next_replica_id = replica_id
                .checked_add(1)
                .ok_or_else(|| "Desk replica id space exhausted".to_owned())?;
            state.snapshot.replicas.push(DeskReplica {
                replica_id,
                author: DeskReplicaAuthor::Agent(agent_id),
            });
            replica_id
        };

        let mut snapshot = state.snapshot.clone();
        snapshot.operations = write
            .open_table(TEXT_OPS)
            .iter()
            .map(|(_, value)| value.value().as_ref().operation.clone())
            .collect();
        let text = snapshot.document_text()?;
        let mut replacements = Vec::with_capacity(edits.len());
        for (index, (old_str, new_str)) in edits.iter().enumerate() {
            if old_str.is_empty() {
                replacements.push((text.len()..text.len(), new_str.clone()));
                continue;
            }
            let mut matches = text.match_indices(old_str);
            let Some((start, _)) = matches.next() else {
                return Err(format!(
                    "Desk edit {} failed: old_str was not found",
                    index + 1
                ));
            };
            if matches.next().is_some() {
                return Err(format!(
                    "Desk edit {} failed: old_str is ambiguous (more than one match)",
                    index + 1
                ));
            }
            replacements.push((start..start + old_str.len(), new_str.clone()));
        }
        for index in 0..replacements.len() {
            for previous in 0..index {
                let left = &replacements[previous].0;
                let right = &replacements[index].0;
                if left.start < right.end && right.start < left.end {
                    return Err(format!(
                        "Desk edit {} failed: replacement overlaps edit {}",
                        index + 1,
                        previous + 1
                    ));
                }
            }
        }

        let mut buffer = snapshot.buffer(replica_id)?;
        let operation = DeskOperation::from_text(&buffer.edit(replacements));
        let record = append_text_in_txn(&mut write, &mut state, operation, None)?;
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
    }

    pub async fn bind(
        &self,
        token: DeskIdToken,
        agent_id: rho_ui_proto::AgentId,
    ) -> Result<DeskBinding, String> {
        let text = self.snapshot().document_text()?;
        let owned = parse(&text)
            .into_iter()
            .any(|heading| heading.token.as_ref() == Some(&token) && !heading.duplicate_token);
        if !owned {
            return Err(format!("unknown Desk identity token {}", token.0));
        }
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        state
            .snapshot
            .bindings
            .retain(|binding| binding.token != token && binding.agent_id != agent_id);
        let binding = DeskBinding {
            token,
            agent_id,
            orphaned: false,
        };
        state.snapshot.bindings.push(binding.clone());
        save_state(&mut write, &state);
        write.commit();
        Ok(binding)
    }

    /// Atomically inserts identity text, rewrites the state, and binds an
    /// agent.
    pub async fn staff_heading(
        &self,
        heading_offset: usize,
        agent_id: rho_ui_proto::AgentId,
    ) -> Result<(DeskTextOpRecord, DeskBinding), String> {
        let snapshot = self.snapshot();
        let text = snapshot.document_text()?;
        let heading = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == heading_offset)
            .ok_or_else(|| "Desk heading moved before staffing completed".to_owned())?;
        let candidate = heading.token.as_ref().and_then(|token| {
            snapshot
                .bindings
                .iter()
                .find(|binding| binding.token == *token && !binding.orphaned)
        });
        let candidate_disposition = candidate.and_then(|candidate| {
            self.db
                .read()
                .list_agents()
                .into_iter()
                .find_map(|(agent_id, agent)| {
                    (agent_id == candidate.agent_id).then_some(agent.disposition)
                })
        });
        if candidate.is_some_and(|binding| binding_is_live(binding, candidate_disposition)) {
            return Err("Desk heading is already staffed by a live agent".to_owned());
        }

        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let had_token = heading.token.is_some();
        let token = if let Some(token) = heading.token {
            token
        } else {
            let token = DeskIdToken(format!("h-{:x}", state.snapshot.next_id));
            state.snapshot.next_id = state
                .snapshot
                .next_id
                .checked_add(1)
                .ok_or_else(|| "Desk identity space exhausted".to_owned())?;
            token
        };
        let mut buffer = snapshot.buffer(ReplicaId::REMOTE_SERVER.as_u16())?;
        let heading_text = format!("{} STAFFED {}", "*".repeat(heading.depth), heading.title);
        let operation = if had_token {
            buffer.edit([(heading.heading_range.clone(), heading_text)])
        } else {
            let insertion = heading.heading_range.end
                + usize::from(text.as_bytes().get(heading.heading_range.end) == Some(&b'\n'));
            buffer.edit([
                (heading.heading_range.clone(), heading_text),
                (insertion..insertion, format!(":id: {}\n", token.0)),
            ])
        };
        let operation = DeskOperation::from_text(&operation);
        let record = append_text_in_txn(&mut write, &mut state, operation, None)?;
        state
            .snapshot
            .bindings
            .retain(|binding| binding.token != token && binding.agent_id != agent_id);
        let binding = DeskBinding {
            token,
            agent_id,
            orphaned: false,
        };
        state.snapshot.bindings.push(binding.clone());
        save_state(&mut write, &state);
        write.commit();
        Ok((record, binding))
    }

    async fn append_text(
        &self,
        operation: DeskOperation,
        transaction: Option<DeskTransaction>,
    ) -> Result<DeskTextOpRecord, String> {
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let record = append_text_in_txn(&mut write, &mut state, operation, transaction)?;
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

fn binding_is_live(binding: &DeskBinding, disposition: Option<rho_core::AgentDisposition>) -> bool {
    !binding.orphaned
        && matches!(
            disposition,
            Some(rho_core::AgentDisposition::Pending | rho_core::AgentDisposition::Snoozed { .. })
        )
}

fn append_text_in_txn(
    write: &mut rho_db::WriteTxn,
    state: &mut PersistentState,
    operation: DeskOperation,
    transaction: Option<DeskTransaction>,
) -> Result<DeskTextOpRecord, String> {
    let mut snapshot = {
        let read_ops = write.open_table(TEXT_OPS);
        let mut snapshot = state.snapshot.clone();
        snapshot.operations = read_ops
            .iter()
            .map(|(_, value)| value.value().as_ref().operation.clone())
            .collect();
        snapshot
    };
    if snapshot
        .operations
        .iter()
        .any(|existing| existing.timestamp() == operation.timestamp())
    {
        return Err("duplicate Desk text operation timestamp".to_owned());
    }
    snapshot.operations.push(operation.clone());
    let text = snapshot.document_text()?;
    if text.len() > 4 * 1024 * 1024 {
        return Err("Desk text exceeds 4194304 bytes".to_owned());
    }
    state.snapshot.refresh_orphans(&text);
    let sequence = state.next_text_sequence;
    state.next_text_sequence = sequence
        .checked_add(1)
        .ok_or_else(|| "Desk text sequence exhausted".to_owned())?;
    let record = DeskTextOpRecord {
        sequence,
        timestamp_ms: rho_core::UnixMs::now().0,
        operation,
        transaction,
    };
    write
        .open_table(TEXT_OPS)
        .insert(&sequence, SenValue::borrowed(&record));
    Ok(record)
}

fn load_state(write: &mut rho_db::WriteTxn) -> PersistentState {
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

// v1 compatibility types are intentionally private and should be deleted after
// the one-release migration window.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct OldNodeId(u64);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct OldOrder(Vec<u8>);
#[derive(Clone, Debug, Encode, Decode)]
struct OldNode {
    id: OldNodeId,
    parent: Option<OldNodeId>,
    order: OldOrder,
}
#[derive(Clone, Debug, Encode, Decode)]
struct OldBinding {
    node_id: OldNodeId,
    agent_id: rho_ui_proto::AgentId,
    orphaned: bool,
}
#[derive(Clone, Debug, Encode, Decode)]
struct OldStructureId(u64);
#[derive(Clone, Debug, Encode, Decode)]
struct OldSnapshot {
    nodes: Vec<OldNode>,
    texts: Vec<OldNodeText>,
    replicas: Vec<DeskReplica>,
    bindings: Vec<OldBinding>,
    next_node_id: u64,
    last_structure_op_id: u64,
    undone_structure_ops: Vec<OldStructureId>,
}
#[derive(Clone, Debug, Encode, Decode)]
struct OldNodeText {
    node_id: OldNodeId,
    operations: Vec<DeskOperation>,
    transactions: Vec<DeskTransaction>,
}
#[derive(Clone, Debug, Encode, Decode)]
struct OldState {
    snapshot: OldSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
}
#[derive(Clone, Debug, Encode, Decode)]
struct OldTextRecord {
    sequence: u64,
    node_id: OldNodeId,
    timestamp_ms: u64,
    operation: DeskOperation,
    transaction: Option<DeskTransaction>,
    undo_of: Option<rho_ui_proto::desk::DeskClock>,
}
const OLD_STATE: TableDefinition<(), Sen<OldState>> = TableDefinition::new("rho_desk_state_v1");
const OLD_TEXT_OPS: TableDefinition<u64, Sen<OldTextRecord>> =
    TableDefinition::new("rho_desk_text_ops_v1");

fn migrate_v1(write: &mut rho_db::WriteTxn) -> Option<PersistentState> {
    let old = write.open_table(OLD_STATE).get(&())?.value().into_owned();
    let mut histories: BTreeMap<OldNodeId, Vec<DeskOperation>> = BTreeMap::new();
    for (_, record) in write.open_table(OLD_TEXT_OPS).iter() {
        let record = record.value();
        histories
            .entry(record.as_ref().node_id.clone())
            .or_default()
            .push(record.as_ref().operation.clone());
    }
    let bindings: BTreeMap<_, _> = old
        .snapshot
        .bindings
        .iter()
        .map(|binding| (binding.node_id.clone(), binding))
        .collect();
    let mut children: BTreeMap<Option<OldNodeId>, Vec<&OldNode>> = BTreeMap::new();
    for node in &old.snapshot.nodes {
        children.entry(node.parent.clone()).or_default().push(node);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| (&a.order, &a.id).cmp(&(&b.order, &b.id)));
    }
    fn render(
        parent: Option<OldNodeId>,
        depth: usize,
        children: &BTreeMap<Option<OldNodeId>, Vec<&OldNode>>,
        histories: &BTreeMap<OldNodeId, Vec<DeskOperation>>,
        bindings: &BTreeMap<OldNodeId, &OldBinding>,
        out: &mut String,
    ) {
        for node in children.get(&parent).into_iter().flatten() {
            let mut buffer = text::Buffer::new(
                ReplicaId::REMOTE_SERVER,
                text::BufferId::new(node.id.0).unwrap(),
                "",
            );
            if let Some(ops) = histories.get(&node.id) {
                buffer.apply_ops(ops.iter().filter_map(|op| op.to_text().ok()));
            }
            let node_text = buffer.text();
            let mut lines = node_text.lines();
            let heading = lines.next().unwrap_or_default();
            out.push_str(&"*".repeat(depth));
            out.push(' ');
            out.push_str(heading);
            out.push('\n');
            if bindings.contains_key(&node.id) {
                out.push_str(&format!(":id: h-migrated-{:x}\n", node.id.0));
            }
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
            render(
                Some(node.id.clone()),
                depth + 1,
                children,
                histories,
                bindings,
                out,
            );
        }
    }
    let mut text = String::new();
    render(None, 1, &children, &histories, &bindings, &mut text);
    let mut buffer = text::Buffer::new(
        ReplicaId::REMOTE_SERVER,
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&buffer.edit([(0..0, text)]));
    let record = DeskTextOpRecord {
        sequence: 1,
        timestamp_ms: rho_core::UnixMs::now().0,
        operation: operation.clone(),
        transaction: None,
    };
    write
        .open_table(TEXT_OPS)
        .insert(&1, SenValue::borrowed(&record));
    Some(PersistentState {
        snapshot: DeskSnapshot {
            text: String::new(),
            operations: Vec::new(),
            transactions: Vec::new(),
            replicas: old.snapshot.replicas,
            bindings: old
                .snapshot
                .bindings
                .into_iter()
                .map(|binding| DeskBinding {
                    token: DeskIdToken(format!("h-migrated-{:x}", binding.node_id.0)),
                    agent_id: binding.agent_id,
                    orphaned: binding.orphaned,
                })
                .collect(),
            next_id: old.snapshot.next_node_id.max(1),
        },
        next_text_sequence: 2,
        next_replica_id: old.next_replica_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(counter: u64) -> rho_core::AgentId {
        rho_core::AgentId::from_counter(counter, &rho_core::AgentIdDomain(7)).unwrap()
    }

    #[test]
    fn orphaned_and_done_bindings_are_not_live() {
        let token = DeskIdToken("h-a".into());
        let mut binding = DeskBinding {
            token,
            agent_id: agent(1),
            orphaned: true,
        };
        assert!(!binding_is_live(
            &binding,
            Some(rho_core::AgentDisposition::Pending)
        ));
        binding.orphaned = false;
        assert!(!binding_is_live(
            &binding,
            Some(rho_core::AgentDisposition::Done)
        ));
        assert!(binding_is_live(
            &binding,
            Some(rho_core::AgentDisposition::Pending)
        ));
    }

    #[tokio::test]
    async fn staffing_reuses_an_orphaned_heading_token() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        let store = DeskStore::new(db.clone()).await;
        let replica = store.allocate_user_replica().await.unwrap();
        let mut buffer =
            text::Buffer::new(ReplicaId::new(replica), text::BufferId::new(1).unwrap(), "");
        let edit = buffer.edit([(0..0, "* TODO plan\n:id: h-a\nnotes\n")]);
        store
            .apply_text(DeskOperation::from_text(&edit), None)
            .await
            .unwrap();
        store
            .bind(DeskIdToken("h-a".into()), agent(1))
            .await
            .unwrap();
        let mut write = db.write().await;
        let mut state = load_state(&mut write);
        state.snapshot.bindings[0].orphaned = true;
        save_state(&mut write, &state);
        write.commit();

        let (_, binding) = store.staff_heading(0, agent(2)).await.unwrap();

        assert_eq!(binding.token.0, "h-a");
        let snapshot = store.snapshot();
        assert_eq!(snapshot.bindings, vec![binding]);
        assert_eq!(snapshot.document_text().unwrap().matches(":id:").count(), 1);
    }

    #[tokio::test]
    async fn persistence_round_trips_single_buffer_and_orphan_revive() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        let store = DeskStore::new(db.clone()).await;
        let replica = store.allocate_user_replica().await.unwrap();
        let mut buffer =
            text::Buffer::new(ReplicaId::new(replica), text::BufferId::new(1).unwrap(), "");
        let edit = buffer.edit([(0..0, "* TODO plan\n:id: h-a\nnotes\n")]);
        store
            .apply_text(DeskOperation::from_text(&edit), None)
            .await
            .unwrap();
        let agent = rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(7)).unwrap();
        store.bind(DeskIdToken("h-a".into()), agent).await.unwrap();
        drop(store);
        let reopened = DeskStore::new(db).await;
        assert_eq!(
            reopened.snapshot().document_text().unwrap(),
            "* TODO plan\n:id: h-a\nnotes\n"
        );
        let mut buffer = reopened.snapshot().buffer(replica).unwrap();
        let remove = buffer.edit([(12..25, "")]);
        reopened
            .apply_text(DeskOperation::from_text(&remove), None)
            .await
            .unwrap();
        assert!(reopened.snapshot().bindings[0].orphaned);
        let undo = buffer.undo_edit_ids([remove.timestamp()]);
        reopened
            .apply_text(DeskOperation::from_text(&undo), None)
            .await
            .unwrap();
        assert!(!reopened.snapshot().bindings[0].orphaned);
    }

    #[tokio::test]
    async fn migration_renders_tree_to_text_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        let agent = rho_core::AgentId::from_counter(2, &rho_core::AgentIdDomain(7)).unwrap();
        let mut root = text::Buffer::new(ReplicaId::new(8), text::BufferId::new(1).unwrap(), "");
        let root_op = DeskOperation::from_text(&root.edit([(0..0, "TODO root\nroot body")]));
        let mut child = text::Buffer::new(ReplicaId::new(9), text::BufferId::new(2).unwrap(), "");
        let child_op = DeskOperation::from_text(&child.edit([(0..0, "STAFFED child\nchild body")]));
        let mut write = db.write().await;
        write.open_table(OLD_STATE).insert(
            &(),
            SenValue::owned(OldState {
                snapshot: OldSnapshot {
                    nodes: vec![
                        OldNode {
                            id: OldNodeId(1),
                            parent: None,
                            order: OldOrder(vec![128]),
                        },
                        OldNode {
                            id: OldNodeId(2),
                            parent: Some(OldNodeId(1)),
                            order: OldOrder(vec![128]),
                        },
                    ],
                    texts: Vec::new(),
                    replicas: Vec::new(),
                    bindings: vec![OldBinding {
                        node_id: OldNodeId(2),
                        agent_id: agent,
                        orphaned: false,
                    }],
                    next_node_id: 3,
                    last_structure_op_id: 2,
                    undone_structure_ops: Vec::new(),
                },
                next_text_sequence: 3,
                next_replica_id: 10,
            }),
        );
        for (sequence, node_id, operation) in
            [(1, OldNodeId(1), root_op), (2, OldNodeId(2), child_op)]
        {
            write.open_table(OLD_TEXT_OPS).insert(
                &sequence,
                SenValue::owned(OldTextRecord {
                    sequence,
                    node_id,
                    timestamp_ms: 1,
                    operation,
                    transaction: None,
                    undo_of: None,
                }),
            );
        }
        write.commit();

        let store = DeskStore::new(db.clone()).await;
        let expected = "* TODO root\nroot body\n** STAFFED child\n:id: h-migrated-2\nchild body\n";
        assert_eq!(store.snapshot().document_text().unwrap(), expected);
        let parsed = parse(expected);
        assert_eq!(parsed[1].parent, Some(0));
        assert_eq!(store.snapshot().bindings[0].token.0, "h-migrated-2");
        drop(store);
        assert_eq!(
            DeskStore::new(db).await.snapshot().document_text().unwrap(),
            expected
        );
    }
}
