//! Durable daemon ownership for one org-like Desk CRDT document.

use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_agent::db::AgentReadTxnExt as _;
use rho_db::{RhoDb, Sen, SenValue};
use rho_ui_proto::desk::{
    DeskOperation, DeskReplica, DeskReplicaAuthor, DeskSnapshot, DeskTextOpRecord, DeskTransaction,
    parse,
};
use senax_encoder::{Decode, Encode};
use text::ReplicaId;

// Persisted TypeNames include the Rust type path: `PersistentStateV3` is now a
// wire-stable name and must not be renamed while the v3 table exists.
const STATE: TableDefinition<(), Sen<PersistentStateV3>> =
    TableDefinition::new("rho_desk_state_v3");
const TEXT_OPS: TableDefinition<u64, Sen<DeskTextOpRecord>> =
    TableDefinition::new("rho_desk_text_ops_v2");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentStateV3 {
    snapshot: DeskSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
}

impl Default for PersistentStateV3 {
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
        let agent_handles = {
            let read = db.read();
            let prefix_len = prefix_id::uniform_prefix_len(read.last_agent_counter(), 200).max(4);
            read.list_agents()
                .into_iter()
                .map(|(agent_id, record)| {
                    (
                        agent_id,
                        format!(
                            "{}-{}",
                            record.role.handle_prefix(),
                            &agent_id.encoded()[..prefix_len]
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let mut write = db.write().await;
        write.open_table(TEXT_OPS);
        if write.open_table(STATE).get(&()).is_none() {
            let migrated = migrate_v2(&mut write, &agent_handles)
                .or_else(|| migrate_v1(&mut write, &agent_handles));
            let state = migrated.unwrap_or_default();
            write.delete_table("rho_desk_structure_ops_v1");
            write.delete_table("rho_desk_text_ops_v1");
            write.delete_table("rho_desk_state_v1");
            write.delete_table("rho_desk_state_v2");
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

    /// Atomically records the newly staffed agent in visible Desk text.
    pub async fn staff_heading(
        &self,
        heading_offset: usize,
        agent_id: rho_ui_proto::AgentId,
    ) -> Result<DeskTextOpRecord, String> {
        let snapshot = self.snapshot();
        let text = snapshot.document_text()?;
        let heading = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == heading_offset)
            .ok_or_else(|| "Desk heading moved before staffing completed".to_owned())?;
        let candidate = heading
            .agent_value
            .as_deref()
            .and_then(|handle| resolve_agent_handle(&self.db, handle));
        let candidate_disposition = candidate.and_then(|candidate| {
            self.db
                .read()
                .list_agents()
                .into_iter()
                .find_map(|(agent_id, agent)| (agent_id == candidate).then_some(agent.disposition))
        });
        if candidate.is_some() && binding_is_live(candidate_disposition) {
            return Err("Desk heading is already staffed by a live agent".to_owned());
        }

        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let mut buffer = snapshot.buffer(ReplicaId::REMOTE_SERVER.as_u16())?;
        let agent_line = format!(":agent: {}", agent_handle(&self.db, agent_id));
        let operation = if let Some(property) = heading
            .properties
            .iter()
            .find(|property| property.key.eq_ignore_ascii_case("agent"))
        {
            buffer.edit([(property.line_range.clone(), agent_line)])
        } else {
            let insertion = heading.heading_range.end
                + usize::from(text.as_bytes().get(heading.heading_range.end) == Some(&b'\n'));
            buffer.edit([(insertion..insertion, format!("{agent_line}\n"))])
        };
        let operation = DeskOperation::from_text(&operation);
        let record = append_text_in_txn(&mut write, &mut state, operation, None)?;
        save_state(&mut write, &state);
        write.commit();
        Ok(record)
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

pub(crate) fn binding_is_live(disposition: Option<rho_core::AgentDisposition>) -> bool {
    matches!(
        disposition,
        Some(rho_core::AgentDisposition::Pending | rho_core::AgentDisposition::Snoozed { .. })
    )
}

fn agent_handle(db: &RhoDb, agent_id: rho_ui_proto::AgentId) -> String {
    let read = db.read();
    let prefix_len = prefix_id::uniform_prefix_len(read.last_agent_counter(), 200).max(4);
    let role_prefix = read
        .list_agents()
        .into_iter()
        .find(|(candidate, _)| *candidate == agent_id)
        .map_or("eng", |(_, record)| record.role.handle_prefix());
    format!("{}-{}", role_prefix, &agent_id.encoded()[..prefix_len])
}

fn resolve_agent_handle(db: &RhoDb, handle: &str) -> Option<rho_ui_proto::AgentId> {
    let (role_prefix, encoded) = handle.trim().split_once('-')?;
    let read = db.read();
    let domain = rho_core::AgentIdDomain(read.machine_seed());
    let agent_id =
        match rho_ui_proto::AgentId::from_prefix(encoded, read.last_agent_counter() + 1, &domain)
            .ok()?
        {
            prefix_id::PrefixResolution::Unique(agent_id) => agent_id,
            prefix_id::PrefixResolution::Ambiguous { .. }
            | prefix_id::PrefixResolution::NotFound => return None,
        };
    read.list_agents()
        .into_iter()
        .find(|(candidate, record)| {
            *candidate == agent_id && record.role.handle_prefix() == role_prefix
        })
        .map(|(agent_id, _)| agent_id)
}

fn append_text_in_txn(
    write: &mut rho_db::WriteTxn,
    state: &mut PersistentStateV3,
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

fn load_state(write: &mut rho_db::WriteTxn) -> PersistentStateV3 {
    write
        .open_table(STATE)
        .get(&())
        .expect("Desk state initialized")
        .value()
        .into_owned()
}
fn save_state(write: &mut rho_db::WriteTxn, state: &PersistentStateV3) {
    write
        .open_table(STATE)
        .insert(&(), SenValue::borrowed(state));
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct V2DeskIdToken(String);
#[derive(Clone, Debug, Encode, Decode)]
struct V2DeskBinding {
    token: V2DeskIdToken,
    agent_id: rho_ui_proto::AgentId,
    orphaned: bool,
}
#[derive(Clone, Debug, Encode, Decode)]
struct V2DeskSnapshot {
    text: String,
    operations: Vec<DeskOperation>,
    transactions: Vec<DeskTransaction>,
    replicas: Vec<DeskReplica>,
    bindings: Vec<V2DeskBinding>,
    next_id: u64,
}
#[derive(Clone, Debug, Encode, Decode)]
// The redb value TypeName embeds this exact Rust path; the deployed v2 table
// was created as `Sen<rho_daemon::desk::PersistentState>`, so the compat
// struct must keep that name and module.
struct PersistentState {
    snapshot: V2DeskSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
}
const V2_STATE: TableDefinition<(), Sen<PersistentState>> =
    TableDefinition::new("rho_desk_state_v2");

/// Converts hidden v2 identity text into the visible binding contract. The
/// replacement is appended as a server-replica CRDT operation, so connected
/// clients that race startup converge through the ordinary text-op stream.
fn migrate_v2(
    write: &mut rho_db::WriteTxn,
    agent_handles: &BTreeMap<rho_ui_proto::AgentId, String>,
) -> Option<PersistentStateV3> {
    let old = write.open_table(V2_STATE).get(&())?.value().into_owned();
    let mut materialized = DeskSnapshot {
        text: old.snapshot.text.clone(),
        operations: old.snapshot.operations.clone(),
        transactions: old.snapshot.transactions.clone(),
        replicas: old.snapshot.replicas.clone(),
    };
    materialized.operations.extend(
        write
            .open_table(TEXT_OPS)
            .iter()
            .map(|(_, value)| value.value().as_ref().operation.clone()),
    );
    let text = materialized.document_text().ok()?;
    let bindings: BTreeMap<_, _> = old
        .snapshot
        .bindings
        .iter()
        .map(|binding| (binding.token.0.as_str(), binding.agent_id))
        .collect();
    let mut migrated = String::with_capacity(text.len());
    for part in text.split_inclusive('\n') {
        let (line, newline) = part
            .strip_suffix('\n')
            .map_or((part, ""), |line| (line, "\n"));
        let trimmed = line.trim();
        if let Some(token) = trimmed.strip_prefix(":id:").map(str::trim)
            && let Some(agent_id) = bindings.get(token)
        {
            let indent = &line[..line.len() - line.trim_start().len()];
            migrated.push_str(indent);
            migrated.push_str(":agent: ");
            migrated.push_str(
                agent_handles
                    .get(agent_id)
                    .map_or_else(|| format!("eng-{}", &agent_id.encoded()[..4]), Clone::clone)
                    .as_str(),
            );
            migrated.push_str(newline);
            continue;
        }
        let depth = line.bytes().take_while(|byte| *byte == b'*').count();
        if depth > 0 && line.as_bytes().get(depth) == Some(&b' ') {
            let content = &line[depth + 1..];
            if let Some(title) = content.strip_prefix("STAFFED")
                && (title.is_empty() || title.starts_with(char::is_whitespace))
            {
                migrated.push_str(&line[..depth + 1]);
                migrated.push_str(title.trim_start());
                migrated.push_str(newline);
                continue;
            }
        }
        migrated.push_str(line);
        migrated.push_str(newline);
    }

    let mut state = PersistentStateV3 {
        snapshot: DeskSnapshot {
            text: old.snapshot.text,
            operations: old.snapshot.operations,
            transactions: old.snapshot.transactions,
            replicas: old.snapshot.replicas,
        },
        next_text_sequence: old.next_text_sequence,
        next_replica_id: old.next_replica_id,
    };
    if migrated != text {
        let mut buffer = materialized
            .buffer(ReplicaId::REMOTE_SERVER.as_u16())
            .ok()?;
        let operation = DeskOperation::from_text(&buffer.edit([(0..text.len(), migrated)]));
        append_text_in_txn(write, &mut state, operation, None).ok()?;
    }
    Some(state)
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

fn migrate_v1(
    write: &mut rho_db::WriteTxn,
    agent_handles: &BTreeMap<rho_ui_proto::AgentId, String>,
) -> Option<PersistentStateV3> {
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
        agent_handles: &BTreeMap<rho_ui_proto::AgentId, String>,
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
            let heading = heading
                .strip_prefix("STAFFED")
                .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
                .map_or(heading, str::trim_start);
            out.push_str(&"*".repeat(depth));
            out.push(' ');
            out.push_str(heading);
            out.push('\n');
            if let Some(binding) = bindings.get(&node.id) {
                let handle = agent_handles
                    .get(&binding.agent_id)
                    .cloned()
                    .unwrap_or_else(|| format!("eng-{}", &binding.agent_id.encoded()[..4]));
                out.push_str(&format!(":agent: {handle}\n"));
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
                agent_handles,
                out,
            );
        }
    }
    let mut text = String::new();
    render(
        None,
        1,
        &children,
        &histories,
        &bindings,
        agent_handles,
        &mut text,
    );
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
    Some(PersistentStateV3 {
        snapshot: DeskSnapshot {
            text: String::new(),
            operations: Vec::new(),
            transactions: Vec::new(),
            replicas: old.snapshot.replicas,
        },
        next_text_sequence: 2,
        next_replica_id: old.next_replica_id,
    })
}

#[cfg(test)]
mod tests {
    use rho_agent::db::AgentWriteTxnExt as _;

    use super::*;

    fn agent(counter: u64) -> rho_core::AgentId {
        rho_core::AgentId::from_counter(counter, &rho_core::AgentIdDomain(7)).unwrap()
    }

    async fn init_agent_tables(db: &RhoDb) {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
    }

    #[test]
    fn done_and_missing_agents_are_not_live() {
        assert!(!binding_is_live(None));
        assert!(!binding_is_live(Some(rho_core::AgentDisposition::Done)));
        assert!(binding_is_live(Some(rho_core::AgentDisposition::Pending)));
    }

    #[tokio::test]
    async fn staffing_replaces_the_first_agent_property_without_rewriting_state() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        init_agent_tables(&db).await;
        let store = DeskStore::new(db.clone()).await;
        let replica = store.allocate_user_replica().await.unwrap();
        let mut buffer =
            text::Buffer::new(ReplicaId::new(replica), text::BufferId::new(1).unwrap(), "");
        let edit = buffer.edit([(0..0, "* TODO plan\n:agent: eng-dead\nnotes\n")]);
        store
            .apply_text(DeskOperation::from_text(&edit), None)
            .await
            .unwrap();
        store.staff_heading(0, agent(2)).await.unwrap();
        let text = store.snapshot().document_text().unwrap();
        assert_eq!(text.matches(":agent:").count(), 1);
        assert!(text.contains(&format!(":agent: eng-{}", &agent(2).encoded()[..4])));
        assert!(text.starts_with("* TODO plan\n"));
    }

    #[tokio::test]
    async fn bindings_are_derived_again_after_delete_and_undo() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        init_agent_tables(&db).await;
        let store = DeskStore::new(db.clone()).await;
        let replica = store.allocate_user_replica().await.unwrap();
        let mut buffer =
            text::Buffer::new(ReplicaId::new(replica), text::BufferId::new(1).unwrap(), "");
        let edit = buffer.edit([(0..0, "* TODO plan\n:agent: eng-test\nnotes\n")]);
        store
            .apply_text(DeskOperation::from_text(&edit), None)
            .await
            .unwrap();
        drop(store);
        let reopened = DeskStore::new(db).await;
        assert_eq!(
            reopened.snapshot().document_text().unwrap(),
            "* TODO plan\n:agent: eng-test\nnotes\n"
        );
        let mut buffer = reopened.snapshot().buffer(replica).unwrap();
        let line = parse(&reopened.snapshot().document_text().unwrap())[0].properties[0]
            .line_range
            .clone();
        let remove = buffer.edit([(line, "")]);
        reopened
            .apply_text(DeskOperation::from_text(&remove), None)
            .await
            .unwrap();
        assert!(
            parse(&reopened.snapshot().document_text().unwrap())[0]
                .agent_value
                .is_none()
        );
        let undo = buffer.undo_edit_ids([remove.timestamp()]);
        reopened
            .apply_text(DeskOperation::from_text(&undo), None)
            .await
            .unwrap();
        assert_eq!(
            parse(&reopened.snapshot().document_text().unwrap())[0]
                .agent_value
                .as_deref(),
            Some("eng-test")
        );
    }

    #[tokio::test]
    async fn migration_renders_tree_to_text_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        init_agent_tables(&db).await;
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
        let expected = format!(
            "* TODO root\nroot body\n** child\n:agent: eng-{}\nchild body\n",
            &agent.encoded()[..4]
        );
        assert_eq!(store.snapshot().document_text().unwrap(), expected);
        let parsed = parse(&expected);
        assert_eq!(parsed[1].parent, Some(0));
        assert_eq!(
            parsed[1].agent_value.as_deref(),
            Some(&format!("eng-{}", &agent.encoded()[..4])[..])
        );
        drop(store);
        assert_eq!(
            DeskStore::new(db).await.snapshot().document_text().unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn v2_migration_replaces_ids_and_strips_staffed_through_text_ops() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        init_agent_tables(&db).await;
        let agent_id = agent(3);
        let mut buffer = text::Buffer::new(ReplicaId::new(8), text::BufferId::new(1).unwrap(), "");
        let operation =
            DeskOperation::from_text(&buffer.edit([(0..0, "* STAFFED task\n:id: h-a\nbody\n")]));
        let mut write = db.write().await;
        write.open_table(V2_STATE).insert(
            &(),
            SenValue::owned(PersistentState {
                snapshot: V2DeskSnapshot {
                    text: String::new(),
                    operations: Vec::new(),
                    transactions: Vec::new(),
                    replicas: Vec::new(),
                    bindings: vec![V2DeskBinding {
                        token: V2DeskIdToken("h-a".into()),
                        agent_id,
                        orphaned: false,
                    }],
                    next_id: 2,
                },
                next_text_sequence: 2,
                next_replica_id: 9,
            }),
        );
        write.open_table(TEXT_OPS).insert(
            &1,
            SenValue::owned(DeskTextOpRecord {
                sequence: 1,
                timestamp_ms: 1,
                operation,
                transaction: None,
            }),
        );
        write.commit();

        let store = DeskStore::new(db.clone()).await;
        let expected = format!("* task\n:agent: eng-{}\nbody\n", &agent_id.encoded()[..4]);
        assert_eq!(store.snapshot().document_text().unwrap(), expected);
        assert_eq!(store.snapshot().operations.len(), 2);
        drop(store);
        assert_eq!(
            DeskStore::new(db).await.snapshot().document_text().unwrap(),
            expected
        );
    }
}
