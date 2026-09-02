//! Durable daemon ownership of the structured Desk document.

use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_desk::{
    Binding, BindingKind, Document, NodeId, NodeKind, NodeOwner, OrderKey, PageId, Replica,
    ReplicaAuthor, Snapshot, TemporalKind, TemporalMark, TextOpRecord, TextOperation,
    TextTransaction, TreeClock, TreeOpRecord, TreeOperation,
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

impl DeskTreeStore {
    pub async fn new(
        db: RhoDb,
        old: &DeskSnapshot,
        resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>,
    ) -> Self {
        let mut write = db.write().await;
        write.open_table(TREE_OPS);
        write.open_table(TEXT_OPS);
        if write.open_table(STATE).get(&()).is_none() {
            let snapshot = old
                .document_text()
                .ok()
                .map(|text| import_org(&text, resolve_agent))
                .unwrap_or_default();
            write.open_table(STATE).insert(
                &(),
                SenValue::owned(PersistentState {
                    snapshot,
                    next_sequence: 1,
                    next_replica_id: ReplicaId::FIRST_COLLAB_ID.as_u16(),
                }),
            );
        }
        write.commit();
        Self { db }
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
        for (_, record) in read.open_table(TREE_OPS).iter() {
            document
                .apply(record.value().as_ref().operation.clone())
                .expect("stored Desk tree operation");
        }
        for (_, record) in read.open_table(TEXT_OPS).iter() {
            let record = record.value();
            document
                .apply_text(
                    record.as_ref().node_id,
                    record.as_ref().operation.clone(),
                    record.as_ref().transaction.clone(),
                )
                .expect("stored Desk node text operation");
        }
        document.snapshot()
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
    let live = document
        .materialize()
        .into_iter()
        .map(|node| (node.id, node.owner))
        .collect::<BTreeMap<_, _>>();
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
        .any(|node_id| live.get(node_id) != Some(&NodeOwner::User))
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
    for (_, record) in write.open_table(TREE_OPS).iter() {
        document.apply(record.value().as_ref().operation.clone())?;
    }
    for (_, record) in write.open_table(TEXT_OPS).iter() {
        let record = record.value();
        document.apply_text(
            record.as_ref().node_id,
            record.as_ref().operation.clone(),
            record.as_ref().transaction.clone(),
        )?;
    }
    Ok(document)
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
                owner: NodeOwner::User,
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
            None,
            text,
            &mut next_clock,
        );
    }

    for (index, heading) in headings.iter().enumerate() {
        let id = next_id();
        let parent = heading.parent.map(|parent| heading_ids[parent]);
        create(
            &mut document,
            id,
            NodeKind::Heading,
            parent,
            &heading.title,
            &mut next_clock,
        );
        heading_ids.push(id);
        import_heading_meta(&mut document, id, heading, &resolve_agent, &mut next_clock);
        let body_end = headings
            .get(index + 1)
            .map_or(text.len(), |next| next.heading_range.start);
        let body = strip_imported_properties(
            &text[heading.body_range.start..body_end],
            heading,
            heading.body_range.start,
        );
        if !body.is_empty() {
            let prose_id = next_id();
            create(
                &mut document,
                prose_id,
                NodeKind::Prose,
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
) {
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
            document
                .apply(TreeOperation::SetBinding {
                    timestamp: next_clock(),
                    node_id: id,
                    kind: BindingKind::Agent,
                    value: Some(Binding::Agent(agent)),
                })
                .unwrap();
        } else if let Some(page) = tag
            .strip_prefix("web-")
            .and_then(|uuid| uuid::Uuid::parse_str(uuid).ok())
        {
            document
                .apply(TreeOperation::SetBinding {
                    timestamp: next_clock(),
                    node_id: id,
                    kind: BindingKind::Page,
                    value: Some(Binding::Page(PageId(*page.as_bytes()))),
                })
                .unwrap();
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
        document
            .apply(TreeOperation::SetBinding {
                timestamp: next_clock(),
                node_id: id,
                kind: BindingKind::Agent,
                value: Some(Binding::Agent(agent)),
            })
            .unwrap();
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

fn strip_imported_properties(body: &str, heading: &DeskHeading, base: usize) -> String {
    let mut ranges = heading
        .properties
        .iter()
        .filter(|property| {
            rho_ui_proto::desk::TemporalMarkKind::from_property_key(&property.key).is_some()
                || property.key.eq_ignore_ascii_case("agent")
                || property.key.eq_ignore_ascii_case("project")
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

    #[tokio::test]
    async fn persists_tree_and_per_node_text_operations() {
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("desk-tree.redb"));
        let old = DeskSnapshot::default();
        let store = DeskTreeStore::new(db.clone(), &old, |_| None).await;
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

        let reopened = DeskTreeStore::new(db, &old, |_| None).await;
        let document = Document::from_snapshot(reopened.snapshot()).unwrap();
        assert_eq!(document.materialize().len(), 1);
        assert_eq!(
            document
                .text(node_id, 33, BufferId::new(33).unwrap())
                .unwrap(),
            "hello\n"
        );
    }
}
