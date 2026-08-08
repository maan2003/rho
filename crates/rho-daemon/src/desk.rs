//! Durable daemon ownership for one org-like Desk CRDT document.

use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_agent::db::AgentReadTxnExt as _;
use rho_db::{RhoDb, Sen, SenValue};
use rho_ui_proto::desk::{
    DeskAnchor, DeskBinding, DeskOperation, DeskReplica, DeskReplicaAuthor, DeskSnapshot,
    DeskTextOpRecord, DeskTransaction, parse,
};
use senax_encoder::{Decode, Encode};
use text::ReplicaId;

// Persisted TypeNames include the Rust type path: `PersistentStateV4` is now a
// wire-stable name and must not be renamed while the v4 table exists.
const STATE: TableDefinition<(), Sen<PersistentStateV4>> =
    TableDefinition::new("rho_desk_state_v4");
const TEXT_OPS: TableDefinition<u64, Sen<DeskTextOpRecord>> =
    TableDefinition::new("rho_desk_text_ops_v2");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentStateV4 {
    snapshot: DeskSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
    /// Pre-tag anchor bindings. Drained into heading-line tags at startup;
    /// kept in the wire-stable struct shape, but always empty now.
    bindings: Vec<DeskBinding>,
}

impl Default for PersistentStateV4 {
    fn default() -> Self {
        Self {
            snapshot: DeskSnapshot::default(),
            next_text_sequence: 1,
            next_replica_id: ReplicaId::FIRST_COLLAB_ID.as_u16(),
            bindings: Vec::new(),
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
            let migrated = migrate_v3(&mut write)
                .or_else(|| migrate_v2(&mut write, &agent_handles))
                .or_else(|| migrate_v1(&mut write, &agent_handles));
            let mut state = migrated.unwrap_or_default();
            extract_bindings(
                |handle| resolve_agent_handle(&db, handle),
                &mut write,
                &mut state,
            );
            write.delete_table("rho_desk_structure_ops_v1");
            write.delete_table("rho_desk_text_ops_v1");
            write.delete_table("rho_desk_state_v1");
            write.delete_table("rho_desk_state_v2");
            write.delete_table("rho_desk_state_v3");
            write.open_table(STATE).insert(&(), SenValue::owned(state));
        }
        // Anchor bindings ride into the text as heading-line tags: the
        // document is now the binding source of truth, so the stored
        // table drains once and stays empty.
        let mut state = load_state(&mut write);
        if !state.bindings.is_empty() {
            for binding in std::mem::take(&mut state.bindings) {
                let Some(label) = agent_handles.get(&binding.agent_id) else {
                    continue;
                };
                if let Err(error) = retag_in_txn(
                    |handle| resolve_agent_handle(&db, handle),
                    &mut write,
                    &mut state,
                    binding.agent_id,
                    label,
                    Some(binding.anchor),
                ) {
                    tracing::warn!(%error, "converting a Desk anchor binding to a tag failed");
                }
            }
            save_state(&mut write, &state);
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

    /// Moves `agent_id`'s heading-line tag: removed everywhere it appears,
    /// inserted on the heading containing `anchor` (`None` just unfiles).
    /// The rewrite is a server-replica CRDT edit, so clients converge
    /// through the ordinary text-op stream. Returns the record to
    /// broadcast when the text changed at all.
    pub async fn retag_agent(
        &self,
        agent_id: rho_ui_proto::AgentId,
        anchor: Option<DeskAnchor>,
    ) -> Result<Option<DeskTextOpRecord>, String> {
        let label = agent_label(&self.db, agent_id)
            .ok_or_else(|| "Desk retag references an unknown agent".to_owned())?;
        let mut write = self.db.write().await;
        let mut state = load_state(&mut write);
        let record = retag_in_txn(
            |handle| resolve_agent_handle(&self.db, handle),
            &mut write,
            &mut state,
            agent_id,
            &label,
            anchor,
        )?;
        if record.is_some() {
            save_state(&mut write, &state);
            write.commit();
        }
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

fn agent_label(db: &RhoDb, agent_id: rho_ui_proto::AgentId) -> Option<String> {
    let read = db.read();
    let prefix_len = prefix_id::uniform_prefix_len(read.last_agent_counter(), 200).max(4);
    read.list_agents()
        .into_iter()
        .find(|(id, _)| *id == agent_id)
        .map(|(id, record)| {
            format!(
                "{}-{}",
                record.role.handle_prefix(),
                &id.encoded()[..prefix_len]
            )
        })
}

fn retag_in_txn(
    resolve: impl Fn(&str) -> Option<rho_ui_proto::AgentId>,
    write: &mut rho_db::WriteTxn,
    state: &mut PersistentStateV4,
    agent_id: rho_ui_proto::AgentId,
    label: &str,
    anchor: Option<DeskAnchor>,
) -> Result<Option<DeskTextOpRecord>, String> {
    let mut materialized = state.snapshot.clone();
    materialized.operations.extend(
        write
            .open_table(TEXT_OPS)
            .iter()
            .map(|(_, value)| value.value().as_ref().operation.clone()),
    );
    let text = materialized.document_text()?;
    let mut buffer = materialized.buffer(ReplicaId::REMOTE_SERVER.as_u16())?;
    let snapshot = buffer.snapshot();
    let target_offset = match anchor {
        Some(anchor) => {
            let anchor = anchor.to_text(snapshot.remote_id());
            if !snapshot.can_resolve(&anchor) {
                return Err("Desk retag anchor does not resolve".to_owned());
            }
            Some(text::ToOffset::to_offset(&anchor, &snapshot))
        }
        None => None,
    };
    let edits = retag_edits(&resolve, &text, agent_id, label, target_offset);
    if edits.is_empty() {
        return Ok(None);
    }
    let operation = DeskOperation::from_text(&buffer.edit(edits));
    append_text_in_txn(write, state, operation, None).map(Some)
}

/// The text edits that move `agent_id`'s tag to the heading containing
/// `target_offset`: every stale occurrence is stripped (the whole token
/// when it holds nothing else), and one tag is written onto the target
/// unless it already carries the agent.
fn retag_edits(
    resolve: &impl Fn(&str) -> Option<rho_ui_proto::AgentId>,
    text: &str,
    agent_id: rho_ui_proto::AgentId,
    label: &str,
    target_offset: Option<usize>,
) -> Vec<(std::ops::Range<usize>, String)> {
    let headings = parse(text);
    let target = target_offset.and_then(|offset| {
        headings
            .iter()
            .rev()
            .find(|heading| heading.heading_range.start <= offset)
            .map(|heading| heading.heading_range.start)
    });
    let mut edits = Vec::new();
    let mut already_tagged = false;
    for heading in &headings {
        let Some(tags_range) = heading.tags_range.clone() else {
            continue;
        };
        let mine = heading
            .tags
            .iter()
            .enumerate()
            .filter(|(_, tag)| resolve(tag) == Some(agent_id))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if mine.is_empty() {
            continue;
        }
        if Some(heading.heading_range.start) == target {
            already_tagged = true;
            continue;
        }
        if mine.len() == heading.tags.len() {
            // The token empties out, so the separating whitespace goes too.
            let start = text[..tags_range.start]
                .trim_end_matches([' ', '\t'])
                .len()
                .max(heading.stars_range.end);
            edits.push((start..tags_range.end, String::new()));
        } else {
            let mut segment_start = tags_range.start + 1;
            for (index, tag) in heading.tags.iter().enumerate() {
                let segment_end = segment_start + tag.len() + 1;
                if mine.contains(&index) {
                    edits.push((segment_start..segment_end, String::new()));
                }
                segment_start = segment_end;
            }
        }
    }
    if let Some(target_start) = target
        && !already_tagged
        && let Some(heading) = headings
            .iter()
            .find(|heading| heading.heading_range.start == target_start)
    {
        match &heading.tags_range {
            Some(range) => edits.push((range.end..range.end, format!("{label}:"))),
            None => edits.push((
                heading.heading_range.end..heading.heading_range.end,
                format!(" :{label}:"),
            )),
        }
    }
    edits.sort_by_key(|(range, _)| range.start);
    edits
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
    state: &mut PersistentStateV4,
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

fn load_state(write: &mut rho_db::WriteTxn) -> PersistentStateV4 {
    write
        .open_table(STATE)
        .get(&())
        .expect("Desk state initialized")
        .value()
        .into_owned()
}
fn save_state(write: &mut rho_db::WriteTxn, state: &PersistentStateV4) {
    write
        .open_table(STATE)
        .insert(&(), SenValue::borrowed(state));
}

/// Converts visible v3 `:agent:` property lines into heading-line tags and
/// strips the property lines from the text. Runs on every migration path (a
/// fresh v4 state simply has no headings to convert). Lines whose handle
/// does not resolve stay in the text for the user to deal with. The rewrite
/// is appended as a server-replica CRDT operation, so racing clients
/// converge through the ordinary text-op stream.
fn extract_bindings(
    resolve: impl Fn(&str) -> Option<rho_ui_proto::AgentId>,
    write: &mut rho_db::WriteTxn,
    state: &mut PersistentStateV4,
) {
    let mut materialized = state.snapshot.clone();
    materialized.operations.extend(
        write
            .open_table(TEXT_OPS)
            .iter()
            .map(|(_, value)| value.value().as_ref().operation.clone()),
    );
    let Ok(text) = materialized.document_text() else {
        return;
    };
    let Ok(mut buffer) = materialized.buffer(ReplicaId::REMOTE_SERVER.as_u16()) else {
        return;
    };
    let mut edits = Vec::new();
    for heading in parse(&text) {
        let mut labels = Vec::new();
        for property in heading
            .properties
            .iter()
            .filter(|property| property.key.eq_ignore_ascii_case("agent"))
        {
            if resolve(&property.value).is_none() {
                continue;
            }
            let end = property.line_range.end
                + usize::from(text.as_bytes().get(property.line_range.end) == Some(&b'\n'));
            edits.push((property.line_range.start..end, String::new()));
            labels.push(property.value.trim().to_owned());
        }
        if labels.is_empty() {
            continue;
        }
        // All of a heading's labels merge into one token; separate tokens
        // would not parse (only the line's last word can be tags).
        edits.push(match &heading.tags_range {
            Some(range) => (range.end..range.end, format!("{}:", labels.join(":"))),
            None => (
                heading.heading_range.end..heading.heading_range.end,
                format!(" :{}:", labels.join(":")),
            ),
        });
    }
    if edits.is_empty() {
        return;
    }
    edits.sort_by_key(|(range, _)| range.start);
    let operation = DeskOperation::from_text(&buffer.edit(edits));
    let _ = append_text_in_txn(write, state, operation, None);
}

// The redb value TypeName embeds this exact Rust path; the deployed v3 table
// was created as `Sen<rho_daemon::desk::PersistentStateV3>`, so the compat
// struct must keep that name and module.
#[derive(Clone, Debug, Encode, Decode)]
struct PersistentStateV3 {
    snapshot: DeskSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
}
const V3_STATE: TableDefinition<(), Sen<PersistentStateV3>> =
    TableDefinition::new("rho_desk_state_v3");

fn migrate_v3(write: &mut rho_db::WriteTxn) -> Option<PersistentStateV4> {
    let old = write.open_table(V3_STATE).get(&())?.value().into_owned();
    Some(PersistentStateV4 {
        snapshot: old.snapshot,
        next_text_sequence: old.next_text_sequence,
        next_replica_id: old.next_replica_id,
        bindings: Vec::new(),
    })
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
) -> Option<PersistentStateV4> {
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

    let mut state = PersistentStateV4 {
        snapshot: DeskSnapshot {
            text: old.snapshot.text,
            operations: old.snapshot.operations,
            transactions: old.snapshot.transactions,
            replicas: old.snapshot.replicas,
        },
        next_text_sequence: old.next_text_sequence,
        next_replica_id: old.next_replica_id,
        bindings: Vec::new(),
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
) -> Option<PersistentStateV4> {
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
    Some(PersistentStateV4 {
        snapshot: DeskSnapshot {
            text: String::new(),
            operations: Vec::new(),
            transactions: Vec::new(),
            replicas: old.snapshot.replicas,
        },
        next_text_sequence: 2,
        next_replica_id: old.next_replica_id,
        bindings: Vec::new(),
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

    #[tokio::test]
    async fn retagging_moves_the_heading_tag_and_unfiles_by_edit() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        init_agent_tables(&db).await;
        let store = DeskStore::new(db.clone()).await;
        let replica = store.allocate_user_replica().await.unwrap();
        let mut buffer =
            text::Buffer::new(ReplicaId::new(replica), text::BufferId::new(1).unwrap(), "");
        let edit = buffer.edit([(0..0, "* one\n* two\n")]);
        store
            .apply_text(DeskOperation::from_text(&edit), None)
            .await
            .unwrap();
        let text = store.snapshot().document_text().unwrap();
        let snapshot = buffer.snapshot();
        let one = DeskAnchor::from_text(snapshot.anchor_after(0));
        let two = DeskAnchor::from_text(snapshot.anchor_after(text.find("* two").unwrap()));
        let resolve = |handle: &str| (handle == "eng-aa").then(|| agent(2));

        let mut retag = |anchor: Option<DeskAnchor>| {
            let db = db.clone();
            async move {
                let mut write = db.write().await;
                let mut state = load_state(&mut write);
                let record =
                    retag_in_txn(resolve, &mut write, &mut state, agent(2), "eng-aa", anchor)
                        .unwrap();
                save_state(&mut write, &state);
                write.commit();
                record
            }
        };
        assert!(retag(Some(one)).await.is_some());
        assert_eq!(
            store.snapshot().document_text().unwrap(),
            "* one :eng-aa:\n* two\n"
        );
        // Moving edits the tag between lines; the binding travels as text.
        assert!(retag(Some(two)).await.is_some());
        assert_eq!(
            store.snapshot().document_text().unwrap(),
            "* one\n* two :eng-aa:\n"
        );
        // Retagging in place changes nothing.
        assert!(retag(Some(two)).await.is_none());
        // Unfiling strips the tag and its separating space.
        assert!(retag(None).await.is_some());
        assert_eq!(store.snapshot().document_text().unwrap(), text);
    }

    #[test]
    fn retag_edits_do_token_surgery_on_shared_tags() {
        let a = agent(1);
        let b = agent(2);
        let resolve = |handle: &str| match handle {
            "eng-a" => Some(a),
            "eng-b" => Some(b),
            _ => None,
        };
        // Removing one agent from a shared token keeps the others.
        let text = "* one :eng-a:eng-b:\n* two\n";
        let edits = retag_edits(&resolve, text, a, "eng-a", Some(text.find("* two").unwrap()));
        let mut patched = text.to_owned();
        for (range, replacement) in edits.iter().rev() {
            patched.replace_range(range.clone(), replacement);
        }
        assert_eq!(patched, "* one :eng-b:\n* two :eng-a:\n");
        // Joining an existing token extends it instead of adding a second
        // token (which would not parse).
        let text = "* one :eng-a:\n";
        let edits = retag_edits(&resolve, text, b, "eng-b", Some(0));
        let mut patched = text.to_owned();
        for (range, replacement) in edits.iter().rev() {
            patched.replace_range(range.clone(), replacement);
        }
        assert_eq!(patched, "* one :eng-a:eng-b:\n");
    }

    #[tokio::test]
    async fn v3_migration_extracts_agent_lines_into_heading_tags() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk.redb"));
        init_agent_tables(&db).await;
        let mut buffer = text::Buffer::new(ReplicaId::new(8), text::BufferId::new(1).unwrap(), "");
        let operation = DeskOperation::from_text(&buffer.edit([(
            0..0,
            "* one\nbody\n* two\n:agent: eng-good\n:agent: adv-gone\nnotes\n",
        )]));
        let mut write = db.write().await;
        write.open_table(V3_STATE).insert(
            &(),
            SenValue::owned(PersistentStateV3 {
                snapshot: DeskSnapshot::default(),
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
        let mut state = migrate_v3(&mut write).unwrap();
        extract_bindings(
            |handle| (handle == "eng-good").then(|| agent(5)),
            &mut write,
            &mut state,
        );
        write.open_table(STATE).insert(&(), SenValue::owned(state));
        write.commit();

        let store = DeskStore::new(db).await;
        let text = store.snapshot().document_text().unwrap();
        // The resolvable handle becomes a heading-line tag; the dead one
        // stays visible for the user to deal with.
        assert_eq!(text, "* one\nbody\n* two :eng-good:\n:agent: adv-gone\nnotes\n");
        assert_eq!(parse(&text)[1].tags, vec!["eng-good"]);
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
