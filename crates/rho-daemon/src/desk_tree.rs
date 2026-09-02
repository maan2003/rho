//! Durable daemon ownership of the structured Desk document.

use std::collections::{BTreeMap, BTreeSet};

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_desk::{
    BatchOpRecord, BatchOperation, Binding, BindingKind, Document, NodeId, NodeKind, NodeOwner,
    OperationBatch, OrderKey, PageId, Replica, ReplicaAuthor, Snapshot, TemporalKind, TemporalMark,
    TextOpRecord, TextOperation, TextTransaction, TreeClock, TreeOpRecord, TreeOperation,
};
use rho_ui_proto::desk::{DeskHeading, DeskSnapshot, TemporalMark as OldTemporalMark, parse};
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
    pub async fn new(
        db: RhoDb,
        old: &DeskSnapshot,
        resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>,
    ) -> Result<Self, String> {
        let mut write = db.write().await;
        write.open_table(TREE_OPS);
        write.open_table(TEXT_OPS);
        write.open_table(BATCH_OPS);
        let imported = import_org(&old.document_text()?, &resolve_agent);
        if write.open_table(STATE).get(&()).is_none() {
            write.open_table(STATE).insert(
                &(),
                SenValue::owned(PersistentState {
                    snapshot: imported,
                    next_sequence: 1,
                    next_replica_id: ReplicaId::FIRST_COLLAB_ID.as_u16(),
                }),
            );
        } else if write.open_table(TREE_OPS).iter().next().is_none()
            && write.open_table(TEXT_OPS).iter().next().is_none()
            && write.open_table(BATCH_OPS).iter().next().is_none()
        {
            // Phase 1 is a shadow behind the legacy editor. Re-import on
            // startup until the first native tree operation so cutover sees
            // every legacy edit and also repairs earlier importer revisions.
            let mut state = load_state(&mut write);
            let replicas = std::mem::take(&mut state.snapshot.replicas);
            state.snapshot = imported;
            merge_replicas(&mut state.snapshot.replicas, replicas);
            save_state(&mut write, &state);
        }
        write.commit();
        Ok(Self { db })
    }

    /// Keeps the shadow tree aligned with the visible legacy Desk until the
    /// tree receives its first native operation. Phase 2 then cuts over to
    /// this last complete import rather than the text present at Phase-1 boot.
    pub async fn refresh_legacy_import(
        &self,
        old: &DeskSnapshot,
        resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>,
    ) -> Result<Option<Snapshot>, String> {
        let text = old.document_text()?;
        let mut write = self.db.write().await;
        if write.open_table(TREE_OPS).iter().next().is_some()
            || write.open_table(TEXT_OPS).iter().next().is_some()
            || write.open_table(BATCH_OPS).iter().next().is_some()
        {
            return Ok(None);
        }
        let mut state = load_state(&mut write);
        let replicas = std::mem::take(&mut state.snapshot.replicas);
        state.snapshot = import_org(&text, resolve_agent);
        merge_replicas(&mut state.snapshot.replicas, replicas);
        state.snapshot.sequence = take_sequence(&mut state)?;
        let replacement = state.snapshot.clone();
        save_state(&mut write, &state);
        write.commit();
        Ok(Some(replacement))
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
        let sequence = take_sequence(&mut state)?;
        let record = BatchOpRecord {
            sequence,
            timestamp_ms: rho_core::UnixMs::now().0,
            batch,
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
            }
        }
    }
    Ok(())
}

fn import_org(text: &str, resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>) -> Snapshot {
    let headings = parse(text);
    let mut document = Document::default();
    let server = ReplicaId::REMOTE_SERVER.as_u16();
    document.add_replica(Replica {
        replica_id: server,
        author: ReplicaAuthor::Machine,
    });
    let mut counter = 0u64;
    let mut clock = 0u32;
    let mut next_id = || {
        counter += 1;
        NodeId {
            replica_id: server,
            counter,
        }
    };
    let mut next_clock = || {
        clock += 1;
        TreeClock {
            value: clock,
            replica_id: server,
        }
    };
    let mut heading_ids = Vec::new();
    let mut sibling_counts: BTreeMap<Option<NodeId>, u16> = BTreeMap::new();

    let mut create = |document: &mut Document,
                      id: NodeId,
                      kind: NodeKind,
                      owner: NodeOwner,
                      parent: Option<NodeId>,
                      text: &str,
                      next_clock: &mut dyn FnMut() -> TreeClock| {
        let sibling = sibling_counts.entry(parent).or_default();
        *sibling = sibling.saturating_add(1);
        document
            .apply(TreeOperation::Create {
                timestamp: next_clock(),
                node_id: id,
                kind,
                owner,
                parent,
                order: OrderKey(vec![sibling.saturating_mul(1024)]),
            })
            .unwrap();
        if !text.is_empty() {
            let mut buffer = text::Buffer::new(
                ReplicaId::new(server),
                BufferId::new(id.counter).unwrap(),
                "",
            );
            let operation = TextOperation::from_text(&buffer.edit([(0..0, text)]));
            document.apply_text(id, operation, None).unwrap();
        }
    };

    if let Some(first) = headings.first() {
        if first.heading_range.start > 0 {
            let id = next_id();
            create(
                &mut document,
                id,
                NodeKind::Prose,
                NodeOwner::User,
                None,
                &text[..first.heading_range.start],
                &mut next_clock,
            );
        }
    } else if !text.is_empty() {
        let id = next_id();
        create(
            &mut document,
            id,
            NodeKind::Prose,
            NodeOwner::User,
            None,
            text,
            &mut next_clock,
        );
    }

    for (index, heading) in headings.iter().enumerate() {
        let id = next_id();
        let parent = heading.parent.map(|parent| heading_ids[parent]);
        let title = if let Some(state) = heading.state_range.as_ref().and(heading.state) {
            format!("{} {}", state.keyword(), heading.title)
        } else {
            heading.title.clone()
        };
        create(
            &mut document,
            id,
            NodeKind::Heading,
            NodeOwner::User,
            parent,
            &title,
            &mut next_clock,
        );
        heading_ids.push(id);
        let machine_children =
            import_heading_meta(&mut document, id, heading, &resolve_agent, &mut next_clock);
        for (kind, binding) in machine_children {
            let child_id = next_id();
            create(
                &mut document,
                child_id,
                kind,
                NodeOwner::Machine,
                Some(id),
                "",
                &mut next_clock,
            );
            document
                .apply(TreeOperation::SetBinding {
                    timestamp: next_clock(),
                    node_id: child_id,
                    kind: binding.kind(),
                    value: Some(binding),
                })
                .unwrap();
        }
        let body_end = headings
            .get(index + 1)
            .map_or(text.len(), |next| next.heading_range.start);
        let body = strip_imported_properties(
            &text[heading.body_range.start..body_end],
            heading,
            heading.body_range.start,
            &resolve_agent,
        );
        if !body.is_empty() {
            let prose_id = next_id();
            create(
                &mut document,
                prose_id,
                NodeKind::Prose,
                NodeOwner::User,
                Some(id),
                &body,
                &mut next_clock,
            );
        }
    }
    document.snapshot()
}

fn import_heading_meta(
    document: &mut Document,
    id: NodeId,
    heading: &DeskHeading,
    resolve_agent: &impl Fn(&str) -> Option<rho_core::AgentId>,
    next_clock: &mut impl FnMut() -> TreeClock,
) -> Vec<(NodeKind, Binding)> {
    let mut machine_children = Vec::new();
    for mark in &heading.temporal_marks {
        let (kind, value) = convert_mark(mark);
        document
            .apply(TreeOperation::SetTemporal {
                timestamp: next_clock(),
                node_id: id,
                kind,
                value: Some(value),
            })
            .unwrap();
    }
    for tag in &heading.tags {
        if let Some(agent) = resolve_agent(tag) {
            machine_children.push((NodeKind::Agent, Binding::Agent(agent)));
        } else if let Some(page) = tag
            .strip_prefix("web-")
            .and_then(|uuid| uuid::Uuid::parse_str(uuid).ok())
        {
            machine_children.push((NodeKind::Page, Binding::Page(PageId(*page.as_bytes()))));
        } else {
            document
                .apply(TreeOperation::SetTag {
                    timestamp: next_clock(),
                    node_id: id,
                    tag: tag.clone(),
                    present: true,
                })
                .unwrap();
        }
    }
    if let Some(agent) = heading.agent_value.as_deref().and_then(resolve_agent) {
        machine_children.push((NodeKind::Agent, Binding::Agent(agent)));
    } else if let Some(value) = heading.agent_value.as_deref() {
        tracing::warn!(
            value,
            "dropping malformed Desk agent property during tree import"
        );
    }
    if let Some(project) = &heading.project {
        document
            .apply(TreeOperation::SetBinding {
                timestamp: next_clock(),
                node_id: id,
                kind: BindingKind::File,
                value: Some(Binding::File(project.into())),
            })
            .unwrap();
    }
    machine_children
}

fn convert_mark(mark: &OldTemporalMark) -> (TemporalKind, TemporalMark) {
    use rho_ui_proto::desk::TemporalMarkKind as Old;
    let kind = match mark.kind {
        Old::Todo => TemporalKind::Todo,
        Old::Deadline => TemporalKind::Deadline,
        Old::Defer => TemporalKind::Defer,
        Old::Reminder => TemporalKind::Reminder,
        Old::Done => TemporalKind::Done,
        Old::Discarded => TemporalKind::Discarded,
    };
    let date = mark.at.date();
    let time = mark.at.time();
    (
        kind,
        TemporalMark {
            year: chrono::Datelike::year(&date),
            month: chrono::Datelike::month(&date) as u8,
            day: chrono::Datelike::day(&date) as u8,
            minute_of_day: (!mark.date_only).then(|| {
                chrono::Timelike::hour(&time) as u16 * 60 + chrono::Timelike::minute(&time) as u16
            }),
            pace_days: mark.pace_days,
        },
    )
}

fn strip_imported_properties(
    body: &str,
    heading: &DeskHeading,
    base: usize,
    resolve_agent: &impl Fn(&str) -> Option<rho_core::AgentId>,
) -> String {
    let mut seen_temporal = std::collections::BTreeSet::new();
    let mut seen_agent = false;
    let mut seen_project = false;
    let mut ranges = heading
        .properties
        .iter()
        .filter(|property| {
            if let Some(kind) =
                rho_ui_proto::desk::TemporalMarkKind::from_property_key(&property.key)
            {
                let first = seen_temporal.insert(kind);
                return first
                    && rho_ui_proto::desk::TemporalMark::parse(kind, &property.value).is_some();
            }
            if property.key.eq_ignore_ascii_case("agent") {
                let first = !seen_agent;
                seen_agent = true;
                return first && resolve_agent(&property.value).is_some();
            }
            if property.key.eq_ignore_ascii_case("project") {
                let first = !seen_project;
                seen_project = true;
                return first;
            }
            false
        })
        .filter_map(|property| {
            property.line_range.start.checked_sub(base).map(|start| {
                let mut end = property.line_range.end.saturating_sub(base).min(body.len());
                if body.as_bytes().get(end) == Some(&b'\n') {
                    end += 1;
                }
                start.min(body.len())..end
            })
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);
    let mut out = String::new();
    let mut cursor = 0;
    for range in ranges {
        if range.start >= cursor {
            out.push_str(&body[cursor..range.start]);
            cursor = range.end.max(cursor);
        }
    }
    out.push_str(&body[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_snapshot(text: &str) -> DeskSnapshot {
        let replica = ReplicaId::REMOTE_SERVER;
        let mut buffer = text::Buffer::new(replica, BufferId::new(1).unwrap(), "");
        let operation = rho_ui_proto::desk::DeskOperation::from_text(&buffer.edit([(0..0, text)]));
        DeskSnapshot {
            text: String::new(),
            operations: vec![operation],
            transactions: Vec::new(),
            replicas: Vec::new(),
        }
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
        let snapshot = import_org(
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
        let snapshot = import_org(&text, |tag| match tag {
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
        let old = DeskSnapshot::default();
        let store = DeskTreeStore::new(db.clone(), &old, |_| None)
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

        let reopened = DeskTreeStore::new(db, &old, |_| None).await.unwrap();
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
    async fn batch_rejects_stale_text_versions_without_partial_tree_changes() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, &DeskSnapshot::default(), |_| None)
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
    async fn overlapping_deletes_authorize_against_tombstoned_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, &DeskSnapshot::default(), |_| None)
            .await
            .unwrap();
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
    async fn startup_failure_does_not_mark_import_complete() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let invalid = DeskSnapshot {
            operations: vec![rho_ui_proto::desk::DeskOperation::Edit {
                timestamp: rho_ui_proto::desk::DeskClock {
                    replica_id: 1,
                    value: 1,
                },
                version: Vec::new(),
                ranges: vec![(0, 0)],
                new_text: Vec::new(),
            }],
            ..DeskSnapshot::default()
        };
        assert!(
            DeskTreeStore::new(db.clone(), &invalid, |_| None)
                .await
                .is_err()
        );
        assert!(
            DeskTreeStore::new(db, &legacy_snapshot("* recovered\n"), |_| None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn legacy_shadow_reimports_until_first_native_operation() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let store = DeskTreeStore::new(db, &legacy_snapshot("* first\n"), |_| None)
            .await
            .unwrap();
        assert!(
            store
                .refresh_legacy_import(&legacy_snapshot("* second\n"), |_| None)
                .await
                .unwrap()
                .is_some()
        );
        let document = Document::from_snapshot(store.snapshot()).unwrap();
        let heading = document.materialize()[0].id;
        assert_eq!(
            document
                .text(heading, 9, BufferId::new(9).unwrap())
                .unwrap(),
            "second"
        );

        let replica = store.allocate_replica(ReplicaAuthor::User).await.unwrap();
        store
            .apply_tree(TreeOperation::Create {
                timestamp: TreeClock {
                    value: 1,
                    replica_id: replica,
                },
                node_id: NodeId {
                    replica_id: replica,
                    counter: 1,
                },
                kind: NodeKind::Prose,
                owner: NodeOwner::User,
                parent: None,
                order: OrderKey(vec![100]),
            })
            .await
            .unwrap();
        assert!(
            store
                .refresh_legacy_import(&legacy_snapshot("* third\n"), |_| None)
                .await
                .unwrap()
                .is_none()
        );
    }
}
