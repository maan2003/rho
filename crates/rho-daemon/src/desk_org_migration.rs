//! One-time import of the retired org-text Desk store.
//!
//! This is the only code allowed to read the legacy tables. The migration is
//! completed in the same transaction that installs the native tree snapshot;
//! the marker prevents legacy text from ever being consulted again.

use std::collections::BTreeMap;

use bytes::BytesMut;
use redb::{TableDefinition, TypeName, Value};
use rho_db::{SenValue, WriteTxn};
use rho_desk::{
    Binding, BindingKind, Document, NodeId, NodeKind, NodeOwner, OrderKey, PageId, Replica,
    ReplicaAuthor, Snapshot, TemporalKind, TemporalMark, TextOperation, TreeClock, TreeOperation,
};
use senax_encoder::{Decode, Decoder, Encode, Encoder};
use text::{BufferId, ReplicaId};

use crate::desk_org_migration_types::{
    DeskBinding, DeskTextOpRecord, ImportedHeading, ImportedOrgSnapshot,
    TemporalMark as OldTemporalMark, import_headings,
};

const LEGACY_STATE: TableDefinition<(), LegacyStateValue> =
    TableDefinition::new("rho_desk_state_v4");
const LEGACY_TEXT_OPS: TableDefinition<u64, LegacyTextOpValue> =
    TableDefinition::new("rho_desk_text_ops_v2");
const MIGRATED: TableDefinition<(), bool> = TableDefinition::new("rho_desk_org_migrated_v1");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentStateV4 {
    snapshot: ImportedOrgSnapshot,
    next_text_sequence: u64,
    next_replica_id: u16,
    bindings: Vec<DeskBinding>,
}

#[derive(Debug)]
struct LegacyStateValue;

/// redb persists Rust value type names as part of its table schema.  The
/// retired table was written while this record lived in rho-ui-proto, so the
/// decoder must continue to advertise that exact historical name even though
/// the wire-compatible migration type now lives here.
#[derive(Debug)]
struct LegacyTextOpValue;

impl Value for LegacyTextOpValue {
    type SelfType<'a> = SenValue<'a, DeskTextOpRecord>;
    type AsBytes<'a> = BytesMut;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut data = data;
        SenValue::owned(DeskTextOpRecord::decode(&mut data).expect("decode legacy Desk text op"))
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let mut bytes = BytesMut::new();
        value
            .as_ref()
            .encode(&mut bytes)
            .expect("encode legacy Desk text op");
        bytes
    }

    fn type_name() -> TypeName {
        TypeName::new("rho-db::Sen<rho_ui_proto::desk::DeskTextOpRecord>")
    }
}

impl Value for LegacyStateValue {
    type SelfType<'a> = SenValue<'a, PersistentStateV4>;
    type AsBytes<'a> = BytesMut;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut data = data;
        SenValue::owned(PersistentStateV4::decode(&mut data).expect("decode legacy Desk state"))
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let mut bytes = BytesMut::new();
        value
            .as_ref()
            .encode(&mut bytes)
            .expect("encode legacy Desk state");
        bytes
    }

    fn type_name() -> TypeName {
        TypeName::new("rho-db::Sen<rho_daemon::desk::PersistentStateV4>")
    }
}

pub(crate) fn already_completed(write: &mut WriteTxn) -> bool {
    write.open_table(MIGRATED).get(&()).is_some()
}

pub(crate) fn import(
    write: &mut WriteTxn,
    resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>,
) -> Result<Option<Snapshot>, String> {
    if already_completed(write) {
        return Ok(None);
    }
    let mut snapshot = write
        .open_table(LEGACY_STATE)
        .get(&())
        .map(|value| value.value().into_owned().snapshot);
    if let Some(snapshot) = &mut snapshot {
        snapshot.operations.clear();
        snapshot.transactions.clear();
        for (_, value) in write.open_table(LEGACY_TEXT_OPS).iter() {
            let record = value.value();
            snapshot.operations.push(record.as_ref().operation.clone());
            if let Some(transaction) = &record.as_ref().transaction {
                snapshot.transactions.push(transaction.clone());
            }
        }
        snapshot.text = snapshot.document_text()?;
    }
    snapshot
        .map(|snapshot| {
            snapshot
                .document_text()
                .map(|text| import_org(&text, &resolve_agent))
        })
        .transpose()
}

pub(crate) fn import_org(
    text: &str,
    resolve_agent: impl Fn(&str) -> Option<rho_core::AgentId>,
) -> Snapshot {
    let headings = import_headings(text);
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
    heading: &ImportedHeading,
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
    use crate::desk_org_migration_types::TemporalMarkKind as Old;
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
    heading: &ImportedHeading,
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
                crate::desk_org_migration_types::TemporalMarkKind::from_property_key(&property.key)
            {
                let first = seen_temporal.insert(kind);
                return first
                    && crate::desk_org_migration_types::TemporalMark::parse(kind, &property.value)
                        .is_some();
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

pub(crate) fn finish(write: &mut WriteTxn) {
    for table in [
        "rho_desk_structure_ops_v1",
        "rho_desk_text_ops_v1",
        "rho_desk_text_ops_v2",
        "rho_desk_state_v1",
        "rho_desk_state_v2",
        "rho_desk_state_v3",
        "rho_desk_state_v4",
    ] {
        write.delete_table(table);
    }
    write.open_table(MIGRATED).insert(&(), true);
}

pub(crate) fn resolve_agent_handle(db: &rho_db::RhoDb, handle: &str) -> Option<rho_core::AgentId> {
    use rho_agent::db::AgentReadTxnExt as _;

    let (role_prefix, encoded) = handle.trim().split_once('-')?;
    let read = db.read();
    let domain = rho_core::AgentIdDomain(read.machine_seed());
    let agent_id =
        match rho_core::AgentId::from_prefix(encoded, read.last_agent_counter() + 1, &domain)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn imports_database_written_by_retired_desk_store() {
        // Generated by the pre-native-tree rho-daemon DeskStore, not by the
        // compatibility decoder under test. This catches redb schema-name
        // drift as well as payload drift.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desk.redb");
        std::fs::write(&path, include_bytes!("../testdata/desk-v4-real.redb")).unwrap();
        let db = rho_db::RhoDb::open(path);
        let tree = crate::desk_tree::DeskTreeStore::new(db.clone(), None, |_| None)
            .await
            .unwrap();
        assert_eq!(
            rho_desk::render_org(tree.snapshot()).unwrap(),
            "* migrated :eng-old:\n"
        );
        let read = db.read();
        assert!(!read.has_table("rho_desk_state_v4"));
        assert!(!read.has_table("rho_desk_text_ops_v2"));
    }

    #[tokio::test]
    async fn imports_v4_once_and_removes_every_legacy_table() {
        assert_eq!(
            LegacyStateValue::type_name().name(),
            "rho-db::Sen<rho_daemon::desk::PersistentStateV4>"
        );
        assert_eq!(
            LegacyTextOpValue::type_name().name(),
            "rho-db::Sen<rho_ui_proto::desk::DeskTextOpRecord>"
        );
        let directory = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(directory.path().join("desk.redb"));
        let mut buffer = text::Buffer::new(
            clock::ReplicaId::REMOTE_SERVER,
            text::BufferId::new(1).unwrap(),
            "",
        );
        let operation = crate::desk_org_migration_types::ImportedTextOperation::from_text(
            &buffer.edit([(0..0, "* migrated\n")]),
        );
        let mut write = db.write().await;
        write.open_table(LEGACY_STATE).insert(
            &(),
            SenValue::owned(PersistentStateV4 {
                snapshot: ImportedOrgSnapshot {
                    ..ImportedOrgSnapshot::default()
                },
                next_text_sequence: 1,
                next_replica_id: 1,
                bindings: Vec::new(),
            }),
        );
        write.open_table(LEGACY_TEXT_OPS).insert(
            &1,
            SenValue::owned(DeskTextOpRecord {
                sequence: 1,
                timestamp_ms: 0,
                operation,
                transaction: None,
            }),
        );
        write.commit();

        let tree = crate::desk_tree::DeskTreeStore::new(db.clone(), None, |_| None)
            .await
            .unwrap();
        assert_eq!(
            rho_desk::render_org(tree.snapshot()).unwrap(),
            "* migrated\n"
        );
        let read = db.read();
        assert!(read.has_table("rho_desk_org_migrated_v1"));
        assert!(!read.has_table("rho_desk_state_v4"));
        assert!(!read.has_table("rho_desk_text_ops_v2"));
    }

    #[tokio::test]
    async fn missing_marker_never_overwrites_native_operation_history() {
        let directory = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(directory.path().join("desk.redb"));
        let tree = crate::desk_tree::DeskTreeStore::new(db.clone(), None, |_| None)
            .await
            .unwrap();
        let replica = tree
            .allocate_replica(rho_desk::ReplicaAuthor::User)
            .await
            .unwrap();
        tree.apply_tree(rho_desk::TreeOperation::Create {
            timestamp: rho_desk::TreeClock {
                value: 1,
                replica_id: replica,
            },
            node_id: rho_desk::NodeId {
                replica_id: replica,
                counter: 1,
            },
            kind: rho_desk::NodeKind::Heading,
            owner: rho_desk::NodeOwner::User,
            parent: None,
            order: rho_desk::OrderKey(vec![100]),
        })
        .await
        .unwrap();
        let mut write = db.write().await;
        write.delete_table("rho_desk_org_migrated_v1");
        write.commit();

        let error = crate::desk_tree::DeskTreeStore::new(db, None, |_| None)
            .await
            .err()
            .unwrap();
        assert!(error.contains("refusing to overwrite native state"));
    }
}
