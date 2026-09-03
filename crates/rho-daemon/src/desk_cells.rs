//! Cells-V2 persistence and the one-shot native-tree V1 conversion.

#![allow(dead_code)]

use redb::TableDefinition;
use rho_db::{Sen, SenValue, WriteTxn};
use rho_desk::NodeId;
use rho_desk::cells::{Cell, DeviceId, Field, Snapshot, Stamp, VerdictEvent, Version};
use senax_encoder::{Decode, Encode};

const CELLS: TableDefinition<Sen<CellAddress>, Sen<Cell>> =
    TableDefinition::new("rho_desk_cells_v2");
const VERDICTS: TableDefinition<Sen<VerdictKey>, Sen<VerdictEvent>> =
    TableDefinition::new("rho_desk_verdicts_v1");
const TEXTS: TableDefinition<Sen<NodeId>, Sen<rho_desk::NodeTextSnapshot>> =
    TableDefinition::new("rho_desk_node_text_v2");
const META: TableDefinition<(), Sen<CellMeta>> = TableDefinition::new("rho_desk_cell_meta_v2");
const MIGRATED: TableDefinition<(), Sen<MigrationReport>> =
    TableDefinition::new("rho_desk_tree_migrated_v2");

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct CellAddress {
    node: NodeId,
    field: Field,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct VerdictKey {
    node: NodeId,
    stamp: Stamp,
}

#[derive(Clone, Debug, Encode, Decode)]
struct CellMeta {
    daemon_device: DeviceId,
    frontier: Version,
    device_node_namespaces: Vec<(DeviceId, u16)>,
    next_node_namespace: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct MigrationReport {
    pub(crate) warnings: Vec<String>,
    pub(crate) page_urls_awaiting_backfill: Vec<NodeId>,
}

pub(crate) fn initialize(write: &mut WriteTxn) -> Result<MigrationReport, String> {
    let meta_exists = write.open_table(META).get(&()).is_some();
    let report = write
        .open_table(MIGRATED)
        .get(&())
        .map(|value| value.value().into_owned());
    match (meta_exists, report) {
        (true, Some(report)) => return Ok(report),
        (true, None) => {
            return Err("Desk cells V2 state exists without its migration marker".into());
        }
        (false, Some(_)) => {
            return Err("Desk cells V2 migration marker exists without state".into());
        }
        (false, None) => {}
    }

    let legacy = crate::desk_tree_v1::load_replayed(write)?
        .ok_or("Desk native-tree V1 state is missing during cells migration")?;
    let daemon_device = DeviceId(*uuid::Uuid::new_v4().as_bytes());
    let migrated = migrate_v1(legacy, daemon_device)?;
    let page_urls_awaiting_backfill =
        migrated
            .snapshot
            .cells
            .iter()
            .filter(|cell| {
                cell.field == Field::PageRef
                    && !migrated.snapshot.cells.iter().any(|candidate| {
                        candidate.node == cell.node && candidate.field == Field::Url
                    })
            })
            .map(|cell| cell.node)
            .collect();
    let report = MigrationReport {
        warnings: migrated.warnings,
        page_urls_awaiting_backfill,
    };
    persist_snapshot(write, &migrated.snapshot, migrated.texts)?;
    write.open_table(META).insert(
        &(),
        SenValue::owned(CellMeta {
            daemon_device,
            frontier: migrated.snapshot.version,
            device_node_namespaces: Vec::new(),
            next_node_namespace: migrated.next_node_namespace,
        }),
    );
    write
        .open_table(MIGRATED)
        .insert(&(), SenValue::borrowed(&report));
    Ok(report)
}

struct ConvertedV1 {
    snapshot: Snapshot,
    texts: Vec<rho_desk::NodeTextSnapshot>,
    warnings: Vec<String>,
    next_node_namespace: u16,
}

fn migrate_v1(
    legacy: crate::desk_tree_v1::Snapshot,
    device: DeviceId,
) -> Result<ConvertedV1, String> {
    use rho_desk::cells::{NodeKind, State, Store, Timestamp, TimestampPrecision, Value};

    use crate::desk_tree_v1 as v1;
    let next_node_namespace = legacy
        .nodes
        .iter()
        .map(|node| node.id.replica_id)
        .chain(legacy.replicas.iter().map(|replica| replica.replica_id))
        .max()
        .unwrap_or(text::ReplicaId::FIRST_COLLAB_ID.as_u16().saturating_sub(1))
        .checked_add(1)
        .ok_or("Desk node namespace exhausted during migration")?;
    let order = v1::Document::from_snapshot(legacy.clone())?
        .materialize()
        .into_iter()
        .enumerate()
        .map(|(index, node)| (node.id, index as i64))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut store = Store::new(device);
    let mut warnings = Vec::new();
    for (fallback_order, old) in legacy.nodes.iter().enumerate() {
        if old.kind == v1::NodeKind::Heading && !old.bindings.is_empty() {
            return Err(format!(
                "legacy heading {:?} has an unsupported binding",
                old.id
            ));
        }
        let id = current_id(old.id);
        let kind = match old.kind {
            v1::NodeKind::Agent => NodeKind::Agent,
            v1::NodeKind::Page => NodeKind::Page,
            v1::NodeKind::File => NodeKind::File,
            v1::NodeKind::Heading | v1::NodeKind::Prose | v1::NodeKind::Draft => NodeKind::Note,
        };
        let parent = old
            .placements
            .iter()
            .max_by_key(|placement| placement.timestamp)
            .and_then(|placement| placement.parent)
            .map(current_id);
        store.write(id, Field::Kind, Value::Kind(kind))?;
        store.write(id, Field::Parent, Value::Parent(parent))?;
        store.write(
            id,
            Field::CreatedAt,
            Value::Timestamp(Timestamp {
                unix_ms: order
                    .get(&old.id)
                    .copied()
                    .unwrap_or_else(|| order.len() as i64 + fallback_order as i64),
                precision: TimestampPrecision::Millisecond,
            }),
        )?;
        store.write(id, Field::Deleted, Value::Bool(old.deleted_at.is_some()))?;
        let state = old
            .temporal
            .iter()
            .filter_map(|(kind, stamp, mark)| {
                mark.as_ref()?;
                let value = match kind {
                    v1::TemporalKind::Todo => State::Open,
                    v1::TemporalKind::Done => State::Done,
                    v1::TemporalKind::Discarded => State::Dismissed,
                    _ => return None,
                };
                Some((*stamp, value))
            })
            .max_by_key(|(stamp, _)| *stamp)
            .map_or(State::Open, |(_, state)| state);
        store.write(id, Field::State, Value::State(state))?;
        let pace = old
            .temporal
            .iter()
            .filter_map(|(_, stamp, mark)| mark.map(|mark| (*stamp, mark.pace_days)))
            .max_by_key(|(stamp, _)| *stamp)
            .map_or(0, |(_, pace)| pace);
        store.write(id, Field::PaceDays, Value::Days(pace))?;
        let defer = old
            .temporal
            .iter()
            .filter_map(|(kind, stamp, mark)| {
                matches!(kind, v1::TemporalKind::Defer | v1::TemporalKind::Reminder)
                    .then_some((*stamp, *kind, *mark))
            })
            .filter_map(|(stamp, kind, mark)| mark.map(|mark| (stamp, kind, mark)))
            .max_by_key(|(stamp, _, _)| *stamp);
        if let Some((_, source, mark)) = defer {
            store.write(
                id,
                Field::DeferUntil,
                Value::OptionalTimestamp(Some(timestamp(mark)?)),
            )?;
            if source == v1::TemporalKind::Reminder {
                warnings.push(format!(
                    "node {:?}: reminder converted to defer-until",
                    old.id
                ));
            }
        }
        if let Some((_, _, Some(mark))) = old
            .temporal
            .iter()
            .filter(|(kind, _, _)| *kind == v1::TemporalKind::Deadline)
            .max_by_key(|(_, stamp, _)| *stamp)
        {
            store.write(
                id,
                Field::Deadline,
                Value::OptionalTimestamp(Some(timestamp(*mark)?)),
            )?;
        }
        for (tag, _, present) in &old.tags {
            store.write(id, Field::Tag(tag.clone()), Value::Bool(*present))?;
        }
        for (_, _, binding) in &old.bindings {
            match binding {
                Some(v1::Binding::Agent(agent)) => {
                    store.write(id, Field::AgentId, Value::AgentId(agent.clone()))?;
                    store.write(id, Field::Host, Value::Host(0))?;
                }
                Some(v1::Binding::Page(page)) => {
                    store.write(id, Field::PageRef, Value::PageRef(rho_desk::PageId(page.0)))?;
                    warnings.push(format!(
                        "node {:?}: page URL awaits GUI registry backfill",
                        old.id
                    ));
                }
                Some(v1::Binding::File(path)) => {
                    store.write(id, Field::Path, Value::Path(path.clone()))?;
                }
                None => {}
            }
        }
    }
    let texts = legacy
        .texts
        .into_iter()
        .map(crate::desk_tree_v1::text_into_current)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConvertedV1 {
        snapshot: store.snapshot(),
        texts,
        warnings,
        next_node_namespace,
    })
}

fn current_id(id: crate::desk_tree_v1::NodeId) -> NodeId {
    NodeId {
        replica_id: id.replica_id,
        counter: id.counter,
    }
}

fn timestamp(
    mark: crate::desk_tree_v1::TemporalMark,
) -> Result<rho_desk::cells::Timestamp, String> {
    use rho_desk::cells::{Timestamp, TimestampPrecision};
    let at = mark.at().ok_or("invalid legacy Desk temporal mark")?;
    Ok(Timestamp {
        unix_ms: at.and_utc().timestamp_millis(),
        precision: if mark.minute_of_day.is_some() {
            TimestampPrecision::Minute
        } else {
            TimestampPrecision::Day
        },
    })
}

fn persist_snapshot(
    write: &mut WriteTxn,
    snapshot: &Snapshot,
    texts: Vec<rho_desk::NodeTextSnapshot>,
) -> Result<(), String> {
    let mut cells = write.open_table(CELLS);
    for cell in &snapshot.cells {
        cells.insert(
            SenValue::owned(CellAddress {
                node: cell.node,
                field: cell.field.clone(),
            }),
            SenValue::borrowed(cell),
        );
    }
    drop(cells);
    let mut verdicts = write.open_table(VERDICTS);
    for (node, stamp, event) in &snapshot.verdicts {
        verdicts.insert(
            SenValue::owned(VerdictKey {
                node: *node,
                stamp: *stamp,
            }),
            SenValue::borrowed(event),
        );
    }
    drop(verdicts);
    let mut table = write.open_table(TEXTS);
    for text in texts {
        table.insert(SenValue::owned(text.node_id), SenValue::owned(text));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;

    use super::*;

    #[tokio::test]
    async fn frozen_v1_fixture_migrates_once_into_v2_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        std::fs::write(&path, include_bytes!("../testdata/desk-tree-v1-real.redb")).unwrap();
        let db = RhoDb::open(path);
        let first = {
            let mut write = db.write().await;
            let report = initialize(&mut write).unwrap();
            write.commit();
            report
        };
        let read = db.read();
        assert!(read.open_table(META).get(&()).is_some());
        assert!(read.open_table(MIGRATED).get(&()).is_some());
        let cells = read
            .open_table(CELLS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect::<Vec<_>>();
        assert!(cells.iter().any(|cell| {
            cell.field == Field::Kind
                && cell.value == rho_desk::cells::Value::Kind(rho_desk::cells::NodeKind::Note)
        }));
        assert!(cells.iter().any(|cell| {
            cell.field == Field::PaceDays && cell.value == rho_desk::cells::Value::Days(3)
        }));
        assert_eq!(
            cells
                .iter()
                .map(|cell| cell.node)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );
        assert!(cells.iter().all(|cell| {
            cell.field != Field::Deleted || cell.value == rho_desk::cells::Value::Bool(false)
        }));
        assert!(cells.iter().any(|cell| {
            cell.field == Field::Parent
                && matches!(cell.value, rho_desk::cells::Value::Parent(Some(_)))
        }));
        assert_eq!(read.open_table(TEXTS).iter().count(), 2);
        drop(read);
        let second = {
            let mut write = db.write().await;
            let report = initialize(&mut write).unwrap();
            write.commit();
            report
        };
        assert_eq!(second, first);
    }
}
