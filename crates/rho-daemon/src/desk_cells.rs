//! Cells-V2 persistence.

#![allow(dead_code)]

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue, WriteTxn};
use rho_desk::cells::{
    BodySnapshot, Cell, CellMutation, DeviceId, Id, Property, PropertyKey, Snapshot, Stamp, Store,
    VerdictEvent, Version,
};
use senax_encoder::{Decode, Encode};

const CELLS: TableDefinition<Sen<CellAddress>, Sen<Cell>> =
    TableDefinition::new("rho_desk_facts_v1");
const VERDICTS: TableDefinition<Sen<VerdictKey>, Sen<VerdictEvent>> =
    TableDefinition::new("rho_desk_fact_verdicts_v1");
const BODIES: TableDefinition<Sen<Id>, Sen<BodySnapshot>> =
    TableDefinition::new("rho_desk_note_body_v1");
const META: TableDefinition<(), Sen<CellMeta>> = TableDefinition::new("rho_desk_cell_meta_v2");
const MUTATIONS: TableDefinition<Sen<Stamp>, Sen<CellMutation>> =
    TableDefinition::new("rho_desk_fact_mutations_v1");

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct CellAddress {
    id: Id,
    key: PropertyKey,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct VerdictKey {
    id: Id,
    stamp: Stamp,
}

#[derive(Clone, Debug, Encode, Decode)]
struct CellMeta {
    daemon_device: DeviceId,
    frontier: Version,
    device_node_namespaces: Vec<(DeviceId, u16)>,
    next_node_namespace: u16,
}

#[derive(Clone)]
pub(crate) struct DeskCellStore {
    db: RhoDb,
}

impl DeskCellStore {
    pub(crate) async fn new(db: RhoDb, machine_seed: u64) -> Result<Self, String> {
        let mut write = db.write().await;
        initialize(&mut write, machine_seed)?;
        write.open_table(MUTATIONS);
        write.commit();
        Ok(Self { db })
    }

    pub(crate) fn sync_since(&self, known: &Version) -> Result<Snapshot, String> {
        let read = self.db.read();
        let meta = read
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        Store::from_snapshot(meta.daemon_device, read_snapshot(&read)?)
            .map(|store| store.since(known))
    }

    pub(crate) fn frontier(&self) -> Result<Version, String> {
        self.db
            .read()
            .open_table(META)
            .get(&())
            .map(|meta| meta.value().as_ref().frontier.clone())
            .ok_or_else(|| "Desk cells V2 metadata is missing".into())
    }

    pub(crate) fn bodies(&self) -> Vec<BodySnapshot> {
        self.db
            .read()
            .open_table(BODIES)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    /// The words of a note. Only a note has a body, which is the one thing
    /// left to check here: the store no longer holds a machine-owned row
    /// for anything, so there is nothing else to keep a client out of.
    pub(crate) async fn apply_body(
        &self,
        session_namespace: u16,
        id: Id,
        operation: rho_desk::TextOperation,
        transaction: Option<rho_desk::TextTransaction>,
    ) -> Result<bool, String> {
        if operation.timestamp().replica_id != session_namespace {
            return Err("Desk text operation does not belong to this connection".into());
        }
        if !matches!(id, Id::Note(_)) {
            return Err("only a Desk note has a body".into());
        }
        let mut write = self.db.write().await;
        let mut text = write
            .open_table(BODIES)
            .get(SenValue::borrowed(&id))
            .map(|value| value.value().into_owned())
            .unwrap_or(BodySnapshot {
                id: id.clone(),
                operations: Vec::new(),
                transactions: Vec::new(),
            });
        if let Some(old) = text
            .operations
            .iter()
            .find(|old| old.timestamp() == operation.timestamp())
        {
            let old_transaction = text
                .transactions
                .iter()
                .find(|candidate| candidate.edit_ids.contains(&operation.timestamp()));
            return if old == &operation && old_transaction == transaction.as_ref() {
                Ok(false)
            } else {
                Err("Desk text operation timestamp was reused with different content".into())
            };
        }
        if let Some(transaction) = &transaction
            && (transaction.id.replica_id != operation.timestamp().replica_id
                || !transaction.edit_ids.contains(&operation.timestamp())
                || transaction.edit_ids.len() > 1024)
        {
            return Err("invalid Desk node text transaction".into());
        }
        let buffer_id = text::BufferId::new(1).map_err(|error| error.to_string())?;
        let validation_buffer = text.buffer(text::ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?;
        validate_text_operation(&text, &validation_buffer, &operation)?;
        text.operations.push(operation);
        if let Some(transaction) = transaction {
            text.transactions.push(transaction);
        }
        let buffer = text.buffer(text::ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?;
        if buffer.len() > 4 * 1024 * 1024 {
            return Err("Desk node text exceeds 4194304 bytes".into());
        }
        write
            .open_table(BODIES)
            .insert(SenValue::owned(id), SenValue::owned(text));
        write.commit();
        Ok(true)
    }

    pub(crate) async fn node_namespace(&self, device: DeviceId) -> Result<u16, String> {
        let mut write = self.db.write().await;
        let mut meta = write
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        if let Some((_, namespace)) = meta
            .device_node_namespaces
            .iter()
            .find(|(candidate, _)| *candidate == device)
        {
            return Ok(*namespace);
        }
        let namespace = meta.next_node_namespace;
        meta.next_node_namespace = namespace
            .checked_add(1)
            .ok_or("Desk node namespace exhausted")?;
        meta.device_node_namespaces.push((device, namespace));
        write.open_table(META).insert(&(), SenValue::owned(meta));
        write.commit();
        Ok(namespace)
    }

    pub(crate) async fn apply_mutation(
        &self,
        session_device: DeviceId,
        session_namespace: u16,
        mutation: CellMutation,
    ) -> Result<(), String> {
        validate_mutation_bounds(&mutation)?;
        if mutation.stamp.device != session_device {
            return Err("Desk mutation stamp does not belong to this connection".into());
        }
        let mut write = self.db.write().await;
        let mut meta = write
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        let accepted = meta.frontier.get(&session_device).copied().unwrap_or(0);
        if mutation.stamp.version <= accepted {
            return match write
                .open_table(MUTATIONS)
                .get(SenValue::borrowed(&mutation.stamp))
            {
                Some(old) if old.value().as_ref() == &mutation => Ok(()),
                _ => Err("Desk mutation version is not newer than its device frontier".into()),
            };
        }
        let observed = meta.frontier.values().copied().max().unwrap_or(0);
        if mutation.stamp.version > observed.saturating_add(1) || observed == u64::MAX {
            return Err("Desk mutation version advances beyond the observable frontier".into());
        }
        let snapshot = read_snapshot_from_write(&mut write)?;
        let mut store = Store::from_snapshot(meta.daemon_device, snapshot)?;
        validate_user_mutation(&store, &mutation)?;
        store.apply_mutation(&mutation)?;
        persist_cells_and_verdicts(&mut write, &store.snapshot())?;
        meta.frontier = store.version().clone();
        write
            .open_table(META)
            .insert(&(), SenValue::borrowed(&meta));
        write
            .open_table(MUTATIONS)
            .insert(SenValue::owned(mutation.stamp), SenValue::owned(mutation));
        write.commit();
        Ok(())
    }
}

fn validate_text_operation(
    text: &BodySnapshot,
    buffer: &text::Buffer,
    operation: &rho_desk::TextOperation,
) -> Result<(), String> {
    use rho_desk::TextOperation;

    let native = operation.to_text()?;
    let timestamp = operation.timestamp();
    if timestamp.value == 0 || timestamp.replica_id == 0 {
        return Err("Desk text operation timestamp is invalid".into());
    }
    let version = match operation {
        TextOperation::Edit { version, .. } | TextOperation::Undo { version, .. } => version,
    };
    if version.len() > 4096
        || version.iter().any(|clock| clock.value == 0)
        || version
            .windows(2)
            .any(|pair| pair[0].replica_id >= pair[1].replica_id)
    {
        return Err("Desk text source version is not canonical or bounded".into());
    }
    let known = text
        .operations
        .iter()
        .map(rho_desk::TextOperation::timestamp)
        .fold(
            std::collections::BTreeMap::<u16, u32>::new(),
            |mut known, clock| {
                known
                    .entry(clock.replica_id)
                    .and_modify(|value| *value = (*value).max(clock.value))
                    .or_insert(clock.value);
                known
            },
        );
    if timestamp.value
        <= known
            .get(&timestamp.replica_id)
            .copied()
            .unwrap_or_default()
    {
        return Err("Desk text operation timestamp does not advance its replica".into());
    }
    if version
        .iter()
        .any(|clock| clock.value > known.get(&clock.replica_id).copied().unwrap_or_default())
    {
        return Err("Desk text source version has not been observed".into());
    }
    let observed = |clock: rho_desk::TreeClock| {
        version
            .binary_search_by_key(&clock.replica_id, |candidate| candidate.replica_id)
            .ok()
            .is_some_and(|index| version[index].value >= clock.value)
    };
    match operation {
        TextOperation::Edit { ranges, .. } => {
            let full_len = text
                .operations
                .iter()
                .filter(|old| observed(old.timestamp()))
                .filter_map(|old| match old {
                    TextOperation::Edit { new_text, .. } => {
                        Some(new_text.iter().map(String::len).sum::<usize>())
                    }
                    TextOperation::Undo { .. } => None,
                })
                .try_fold(0usize, |total, len| total.checked_add(len))
                .ok_or("Desk text history length overflow")? as u64;
            if ranges
                .iter()
                .any(|(start, end)| start > end || *end > full_len)
                || ranges.windows(2).any(|pair| pair[0].1 > pair[1].0)
            {
                return Err("Desk text edit ranges are invalid for its source version".into());
            }
            let text::Operation::Edit(native) = &native else {
                unreachable!("converted edit operation changed variants")
            };
            if !buffer.snapshot().are_valid_full_offsets_for_version(
                native
                    .ranges
                    .iter()
                    .flat_map(|range| [range.start, range.end]),
                &native.version,
            ) {
                return Err("Desk text edit range splits a UTF-8 character".into());
            }
        }
        TextOperation::Undo { counts, .. } => {
            if counts.len() > 65_536
                || counts.iter().any(|(clock, count)| {
                    clock.value == 0
                        || *count == 0
                        || !observed(*clock)
                        || !text.operations.iter().any(|old| old.timestamp() == *clock)
                })
                || counts.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            {
                return Err("Desk text undo counts are invalid or unbounded".into());
            }
        }
    }
    Ok(())
}

fn load_meta_from_write(write: &mut WriteTxn) -> Result<CellMeta, String> {
    write
        .open_table(META)
        .get(&())
        .map(|meta| meta.value().into_owned())
        .ok_or_else(|| "Desk cells V2 metadata is missing".into())
}

fn daemon_node_namespace(meta: &CellMeta) -> Result<u16, String> {
    meta.device_node_namespaces
        .iter()
        .find_map(|(device, namespace)| (*device == meta.daemon_device).then_some(*namespace))
        .ok_or_else(|| "Desk daemon node namespace is missing".into())
}

fn next_daemon_version(meta: &CellMeta) -> Result<u64, String> {
    meta.frontier
        .values()
        .copied()
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "Desk daemon version exhausted".into())
}

fn persist_accepted_mutation(
    write: &mut WriteTxn,
    meta: &mut CellMeta,
    store: &Store,
    mutation: CellMutation,
) {
    persist_cells_and_verdicts(write, &store.snapshot())
        .expect("validated Desk store snapshot persists");
    meta.frontier = store.version().clone();
    write.open_table(META).insert(&(), SenValue::borrowed(meta));
    write
        .open_table(MUTATIONS)
        .insert(SenValue::owned(mutation.stamp), SenValue::owned(mutation));
}

fn validate_mutation_bounds(mutation: &CellMutation) -> Result<(), String> {
    for write in &mutation.writes {
        subject_bounds(&write.id)?;
        match &write.property {
            Property::Parent(Some(parent)) => subject_bounds(parent)?,
            Property::Labeled { label, .. } => subject_bounds(label)?,
            Property::Name(name) if name.len() > 64 * 1024 => {
                return Err("Desk name exceeds 65536 bytes".into());
            }
            Property::SlackHandledThrough(ts) if ts.0.len() > 64 => {
                return Err("Desk Slack timestamp exceeds 64 bytes".into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// A subject is only as big as the identity its source gave it. Slack's own
/// strings and a path are the only unbounded parts, so they are the only
/// ones that need a ceiling.
fn subject_bounds(id: &Id) -> Result<(), String> {
    match id {
        Id::Slack(unit) => {
            let thread = unit.thread.as_deref().unwrap_or_default();
            if unit.workspace.len() > 64 || unit.channel.len() > 64 || thread.len() > 64 {
                return Err("Desk Slack unit identity exceeds 64 bytes".into());
            }
        }
        Id::PullRequest { repo, .. } if repo.len() > 1024 => {
            return Err("Desk repository name exceeds 1024 bytes".into());
        }
        Id::File { path, .. } if path.as_str().len() > 4096 => {
            return Err("Desk path exceeds 4096 bytes".into());
        }
        _ => {}
    }
    Ok(())
}

/// What the daemon will accept from a client.
///
/// The store holds the user's facts and only those, so there is no
/// machine-owned row to protect and no kind to police: every property is
/// the user's to write about any subject. What is left to check is that a
/// verdict entry says the truth, because undo is built on it — the facts it
/// claims to have changed must be the facts the mutation actually writes,
/// and the values it claims stood there before must be the ones that do.
fn validate_user_mutation(store: &Store, mutation: &CellMutation) -> Result<(), String> {
    let Some((verdict_id, event)) = &mutation.verdict else {
        return Ok(());
    };
    let (verdict, changes) = match event {
        VerdictEvent::Applied {
            verdict, changes, ..
        } => (verdict, changes),
        VerdictEvent::Undone { of } => match store.verdict_event(verdict_id, *of) {
            Some(VerdictEvent::Applied {
                verdict, changes, ..
            }) => (verdict, changes),
            _ => return Err("Desk verdict undo does not reference an applied verdict".into()),
        },
    };
    let applied = matches!(event, VerdictEvent::Applied { .. });
    validate_verdict_shape(verdict_id, verdict, changes, mutation, applied)?;
    let mut seen = std::collections::BTreeSet::new();
    for change in changes {
        if !seen.insert((change.id.clone(), change.key.clone())) {
            return Err("Desk verdict contains duplicate fact changes".into());
        }
        let (expected_current, expected_write) = match applied {
            true => (&change.before, &change.after),
            false => (&change.after, &change.before),
        };
        // A todo's note does not exist until the verdict writes it, so its
        // entry states what a thing nobody has said anything about reads as
        // holding rather than what the store returns.
        let unwritten_note = applied
            && matches!(verdict, rho_desk::cells::Verdict::Todo { note } if *note == change.id)
            && !store.facts(&change.id).any()
            && matches!(
                expected_current,
                Some(Property::Deleted(true))
                    | Some(Property::DeferUntil(None))
                    | Some(Property::PaceDays(0))
            );
        // A fact nobody has written reads as its unwritten claim, and that
        // is what the entry states, so the store's missing cell answers to
        // the same reading rather than to `None`.
        let current = store
            .property(&change.id, &change.key)
            .cloned()
            .or_else(|| change.key.unwritten());
        if !unwritten_note && current.as_ref() != expected_current.as_ref() {
            return Err("Desk verdict source value does not match current state".into());
        }
        let Some(expected_write) = expected_write else {
            return Err("Desk verdict changes cannot remove a fact".into());
        };
        if !mutation
            .writes
            .iter()
            .any(|write| write.id == change.id && &write.property == expected_write)
        {
            return Err("Desk verdict change is not applied by its mutation".into());
        }
    }
    Ok(())
}

fn validate_verdict_shape(
    verdict_id: &Id,
    verdict: &rho_desk::cells::Verdict,
    changes: &[rho_desk::cells::FactChange],
    mutation: &CellMutation,
    applied: bool,
) -> Result<(), String> {
    use rho_desk::cells::Verdict;

    // The entry is checked against the shape rho-desk builds for this
    // verdict, so the writer and the checker share one definition. `before`
    // is the writer's to state; everything else has to match.
    let expected = rho_desk::cells::verdict_changes(
        verdict_id,
        verdict,
        &|key| {
            changes
                .iter()
                .find(|change| &change.id == verdict_id && &change.key == key)
                .and_then(|change| change.before.clone())
        },
        rho_desk::cells::todo_cadence(changes),
        rho_desk::cells::slack_verdict(verdict_id, changes),
    )?;
    let sorted = |mut changes: Vec<rho_desk::cells::FactChange>| {
        changes.sort_by(|left, right| (&left.id, &left.key).cmp(&(&right.id, &right.key)));
        changes
    };
    let valid = sorted(changes.to_vec()) == sorted(expected)
        && match verdict {
            // The note a todo creates is filed under the thing the verdict
            // was dealt on by the same mutation, or it lands nowhere.
            Verdict::Todo { note } => {
                !applied
                    || mutation.writes.iter().any(|write| {
                        write.id == *note
                            && write.property == Property::Parent(Some(verdict_id.clone()))
                    })
            }
            _ => true,
        };
    if !valid {
        return Err("Desk verdict changes do not match the verdict semantics".into());
    }
    Ok(())
}

/// Opens the cell tables, making the empty state on a database that has
/// none, and converting node cells to fact cells the one time there are
/// any. The native-tree V1 conversion that used to run here is gone: it
/// ran once on every daemon it was ever going to run on.
pub(crate) fn initialize(write: &mut WriteTxn, machine_seed: u64) -> Result<(), String> {
    let mut meta = match write.open_table(META).get(&()) {
        Some(meta) => meta.value().into_owned(),
        None => {
            let daemon_device = DeviceId(*uuid::Uuid::new_v4().as_bytes());
            CellMeta {
                daemon_device,
                frontier: Version::new(),
                device_node_namespaces: vec![(daemon_device, 1)],
                next_node_namespace: 2,
            }
        }
    };
    write.open_table(CELLS);
    write.open_table(VERDICTS);
    write.open_table(BODIES);
    if let Some((cells, verdicts, bodies, report)) =
        crate::desk_migration::migrate(write, meta.daemon_device, machine_seed)?
    {
        // The converted cells keep the stamps they were written with, so
        // the frontier already covers them and peers still sync from the
        // version they know.
        for (device, version) in crate::desk_migration::frontier(&cells, &verdicts) {
            let entry = meta.frontier.entry(device).or_insert(0);
            *entry = (*entry).max(version);
        }
        let snapshot = Snapshot {
            cells,
            verdicts,
            version: meta.frontier.clone(),
        };
        persist_snapshot(write, &snapshot, bodies)?;
        tracing::info!("{}", report.line());
    }
    write.open_table(META).insert(&(), SenValue::owned(meta));
    Ok(())
}

fn read_snapshot(read: &rho_db::ReadTxn) -> Result<Snapshot, String> {
    let version = read
        .open_table(META)
        .get(&())
        .ok_or("Desk cells V2 metadata is missing")?
        .value()
        .as_ref()
        .frontier
        .clone();
    Ok(Snapshot {
        cells: read
            .open_table(CELLS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect(),
        verdicts: read
            .open_table(VERDICTS)
            .iter()
            .map(|(key, value)| {
                let key = key.value().into_owned();
                (key.id, key.stamp, value.value().into_owned())
            })
            .collect(),
        version,
    })
}

fn read_snapshot_from_write(write: &mut WriteTxn) -> Result<Snapshot, String> {
    let version = write
        .open_table(META)
        .get(&())
        .ok_or("Desk cells V2 metadata is missing")?
        .value()
        .as_ref()
        .frontier
        .clone();
    let cells = write
        .open_table(CELLS)
        .iter()
        .map(|(_, value)| value.value().into_owned())
        .collect();
    let verdicts = write
        .open_table(VERDICTS)
        .iter()
        .map(|(key, value)| {
            let key = key.value().into_owned();
            (key.id, key.stamp, value.value().into_owned())
        })
        .collect();
    Ok(Snapshot {
        cells,
        verdicts,
        version,
    })
}

fn persist_cells_and_verdicts(write: &mut WriteTxn, snapshot: &Snapshot) -> Result<(), String> {
    let mut cells = write.open_table(CELLS);
    for cell in &snapshot.cells {
        cells.insert(
            SenValue::owned(CellAddress {
                id: cell.id.clone(),
                key: cell.property.key(),
            }),
            SenValue::borrowed(cell),
        );
    }
    drop(cells);
    let mut verdicts = write.open_table(VERDICTS);
    for (id, stamp, event) in &snapshot.verdicts {
        verdicts.insert(
            SenValue::owned(VerdictKey {
                id: id.clone(),
                stamp: *stamp,
            }),
            SenValue::borrowed(event),
        );
    }
    Ok(())
}

fn persist_snapshot(
    write: &mut WriteTxn,
    snapshot: &Snapshot,
    bodies: Vec<BodySnapshot>,
) -> Result<(), String> {
    let mut cells = write.open_table(CELLS);
    for cell in &snapshot.cells {
        cells.insert(
            SenValue::owned(CellAddress {
                id: cell.id.clone(),
                key: cell.property.key(),
            }),
            SenValue::borrowed(cell),
        );
    }
    drop(cells);
    let mut verdicts = write.open_table(VERDICTS);
    for (id, stamp, event) in &snapshot.verdicts {
        verdicts.insert(
            SenValue::owned(VerdictKey {
                id: id.clone(),
                stamp: *stamp,
            }),
            SenValue::borrowed(event),
        );
    }
    drop(verdicts);
    let mut table = write.open_table(BODIES);
    for body in bodies {
        table.insert(SenValue::owned(body.id.clone()), SenValue::owned(body));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;
    use rho_desk::cells::{
        CellWrite, FactChange, State, Timestamp, TimestampPrecision, Uuid, Verdict, VerdictEvent,
    };

    use super::*;

    async fn fixture_store() -> DeskCellStore {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        let db = RhoDb::open(path);
        // RhoDb owns the open file after the temporary directory handle drops.
        DeskCellStore::new(db, 42).await.unwrap()
    }

    fn at(unix_ms: i64) -> Timestamp {
        Timestamp {
            unix_ms,
            precision: TimestampPrecision::Millisecond,
        }
    }

    fn next_version(store: &DeskCellStore) -> u64 {
        store
            .frontier()
            .unwrap()
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Writes one note at the root the way a client does. A note is
    /// whatever the user has said about it, so this is the whole of one.
    async fn seed_note(store: &DeskCellStore, device: DeviceId) -> Id {
        let namespace = store.node_namespace(device).await.unwrap();
        let id = Id::Note(Uuid::random());
        let version = next_version(store);
        store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp: Stamp { device, version },
                    writes: vec![
                        CellWrite {
                            id: id.clone(),
                            property: Property::Parent(None),
                        },
                        CellWrite {
                            id: id.clone(),
                            property: Property::CreatedAt(at(1)),
                        },
                    ],
                    verdict: None,
                },
            )
            .await
            .unwrap();
        // A note without a body has no text row, and the text rules are
        // tested against a real one.
        let mut buffer = text::Buffer::new(
            text::ReplicaId::new(namespace),
            text::BufferId::new(1).unwrap(),
            "",
        );
        let operation = rho_desk::TextOperation::from_text(&buffer.edit([(0..0, "seeded note")]));
        store
            .apply_body(namespace, id.clone(), operation, None)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn mutations_are_connection_bound_idempotent_and_sync_since_frontiers() {
        let store = fixture_store().await;
        let device = DeviceId([9; 16]);
        let id = seed_note(&store, device).await;
        let initial = store.sync_since(&Version::new()).unwrap();
        let namespace = store.node_namespace(device).await.unwrap();
        let label = Id::Label(Uuid::random());
        let version = next_version(&store);
        let mutation = CellMutation {
            stamp: Stamp { device, version },
            writes: vec![CellWrite {
                id: id.clone(),
                property: Property::Labeled {
                    label: label.clone(),
                    present: true,
                },
            }],
            verdict: None,
        };
        store
            .apply_mutation(device, namespace, mutation.clone())
            .await
            .unwrap();
        store
            .apply_mutation(device, namespace, mutation.clone())
            .await
            .unwrap();
        let delta = store.sync_since(&initial.version).unwrap();
        assert_eq!(delta.cells.len(), 1);
        assert_eq!(delta.version.get(&device), Some(&version));

        let mut conflict = mutation.clone();
        conflict.writes[0].property = Property::Labeled {
            label,
            present: false,
        };
        assert!(
            store
                .apply_mutation(device, namespace, conflict)
                .await
                .is_err()
        );
        assert!(
            store
                .apply_mutation(DeviceId([8; 16]), namespace, mutation)
                .await
                .is_err()
        );

        // A device that was away writes from its own version, but never
        // from one the frontier cannot explain.
        let offline = DeviceId([7; 16]);
        let offline_namespace = store.node_namespace(offline).await.unwrap();
        let name = |id: &Id, text: &str| CellWrite {
            id: id.clone(),
            property: Property::Name(text.into()),
        };
        store
            .apply_mutation(
                offline,
                offline_namespace,
                CellMutation {
                    stamp: Stamp {
                        device: offline,
                        version: 1,
                    },
                    writes: vec![name(&id, "offline")],
                    verdict: None,
                },
            )
            .await
            .unwrap();
        let global = store.frontier().unwrap().values().copied().max().unwrap();
        assert!(
            store
                .apply_mutation(
                    offline,
                    offline_namespace,
                    CellMutation {
                        stamp: Stamp {
                            device: offline,
                            version: global + 2,
                        },
                        writes: vec![name(&id, "jump")],
                        verdict: None,
                    },
                )
                .await
                .is_err()
        );
    }

    /// A Slack unit is addressable without anyone creating it, so a todo
    /// on one is the first thing the store ever hears about it.
    #[tokio::test]
    async fn a_todo_verdict_on_a_slack_unit_files_its_new_note_under_it() {
        let store = fixture_store().await;
        let device = DeviceId([12; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let unit = Id::Slack(rho_desk::cells::SlackUnit {
            workspace: "rho".into(),
            channel: "C1".into(),
            thread: Some("1.0".into()),
        });
        let note = Id::Note(Uuid::random());
        let wake = at(9);
        let changes = vec![
            FactChange {
                id: unit.clone(),
                key: PropertyKey::SlackHandledThrough,
                before: Some(Property::SlackHandledThrough(rho_desk::cells::SlackTs(
                    String::new(),
                ))),
                after: Some(Property::SlackHandledThrough(rho_desk::cells::SlackTs(
                    "2.0".into(),
                ))),
            },
            FactChange {
                id: note.clone(),
                key: PropertyKey::Deleted,
                before: Some(Property::Deleted(true)),
                after: Some(Property::Deleted(false)),
            },
            FactChange {
                id: note.clone(),
                key: PropertyKey::DeferUntil,
                before: Some(Property::DeferUntil(None)),
                after: Some(Property::DeferUntil(Some(wake))),
            },
            FactChange {
                id: note.clone(),
                key: PropertyKey::PaceDays,
                before: Some(Property::PaceDays(0)),
                after: Some(Property::PaceDays(7)),
            },
        ];
        let stamp = Stamp {
            device,
            version: next_version(&store),
        };
        let writes = |parent: Option<Id>| {
            let mut writes = vec![
                CellWrite {
                    id: unit.clone(),
                    property: Property::SlackHandledThrough(rho_desk::cells::SlackTs("2.0".into())),
                },
                CellWrite {
                    id: note.clone(),
                    property: Property::Deleted(false),
                },
                CellWrite {
                    id: note.clone(),
                    property: Property::DeferUntil(Some(wake)),
                },
                CellWrite {
                    id: note.clone(),
                    property: Property::PaceDays(7),
                },
            ];
            if let Some(parent) = parent {
                writes.push(CellWrite {
                    id: note.clone(),
                    property: Property::Parent(Some(parent)),
                });
            }
            writes
        };
        // The note has to land under the thing the verdict was dealt on.
        assert!(
            store
                .apply_mutation(
                    device,
                    namespace,
                    CellMutation {
                        stamp,
                        writes: writes(None),
                        verdict: Some((
                            unit.clone(),
                            VerdictEvent::Applied {
                                verdict: Verdict::Todo { note: note.clone() },
                                at: stamp,
                                changes: changes.clone(),
                            },
                        )),
                    },
                )
                .await
                .is_err()
        );
        store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp,
                    writes: writes(Some(unit.clone())),
                    verdict: Some((
                        unit.clone(),
                        VerdictEvent::Applied {
                            verdict: Verdict::Todo { note: note.clone() },
                            at: stamp,
                            changes,
                        },
                    )),
                },
            )
            .await
            .unwrap();
        let store = Store::from_snapshot(
            DeviceId([0; 16]),
            store.sync_since(&Version::new()).unwrap(),
        )
        .unwrap();
        // The unit is closed by its cursor, never by a state: a history page
        // replaying an older message cannot make it open again.
        assert_eq!(store.facts(&unit).state, State::Open);
        assert_eq!(
            store.facts(&unit).slack_handled_through,
            Some(rho_desk::cells::SlackTs("2.0".into()))
        );
        assert_eq!(store.facts(&note).parent, Some(unit));
    }

    #[tokio::test]
    async fn a_defer_verdict_zeroes_the_pace() {
        let store = fixture_store().await;
        let device = DeviceId([13; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let id = seed_note(&store, device).await;

        // The note is a todo first, so the pace has something to zero.
        store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp: Stamp {
                        device,
                        version: next_version(&store),
                    },
                    writes: vec![CellWrite {
                        id: id.clone(),
                        property: Property::PaceDays(7),
                    }],
                    verdict: None,
                },
            )
            .await
            .unwrap();

        let wake = at(9);
        let wakes = FactChange {
            id: id.clone(),
            key: PropertyKey::DeferUntil,
            before: Some(Property::DeferUntil(None)),
            after: Some(Property::DeferUntil(Some(wake))),
        };
        let paced_to_zero = FactChange {
            id: id.clone(),
            key: PropertyKey::PaceDays,
            before: Some(Property::PaceDays(7)),
            after: Some(Property::PaceDays(0)),
        };
        let defer = |version: u64, changes: Vec<FactChange>| {
            let stamp = Stamp { device, version };
            CellMutation {
                stamp,
                writes: changes
                    .iter()
                    .map(|change| CellWrite {
                        id: change.id.clone(),
                        property: change.after.clone().unwrap(),
                    })
                    .collect(),
                verdict: Some((
                    id.clone(),
                    VerdictEvent::Applied {
                        verdict: Verdict::Defer { until: wake },
                        at: stamp,
                        changes,
                    },
                )),
            }
        };

        let only_the_wake_time = defer(next_version(&store), vec![wakes.clone()]);
        assert!(
            store
                .apply_mutation(device, namespace, only_the_wake_time)
                .await
                .is_err()
        );
        let whole = defer(next_version(&store), vec![wakes, paced_to_zero]);
        store
            .apply_mutation(device, namespace, whole)
            .await
            .unwrap();

        let facts = Store::from_snapshot(
            DeviceId([0; 16]),
            store.sync_since(&Version::new()).unwrap(),
        )
        .unwrap()
        .facts(&id);
        assert_eq!(facts.defer_until, Some(wake));
        assert_eq!(facts.pace_days, 0);
    }

    #[tokio::test]
    async fn note_bodies_are_namespace_bound_and_exact_retries_are_idempotent() {
        let store = fixture_store().await;
        let device = DeviceId([11; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let id = seed_note(&store, device).await;
        let body = store.bodies().into_iter().next().unwrap();
        let before = store.bodies();
        for ranges in [vec![(0, 1)], vec![(1, 0)]] {
            let malformed = rho_desk::TextOperation::Edit {
                timestamp: rho_desk::TreeClock {
                    value: 1,
                    replica_id: namespace,
                },
                version: Vec::new(),
                ranges,
                new_text: vec!["x".into()],
            };
            assert!(
                store
                    .apply_body(namespace, id.clone(), malformed, None)
                    .await
                    .is_err()
            );
            assert_eq!(store.bodies(), before);
        }
        let mut buffer = body
            .buffer(namespace, text::BufferId::new(1).unwrap())
            .unwrap();
        let end = buffer.len();
        let operation = rho_desk::TextOperation::from_text(&buffer.edit([(end..end, "!")]));
        assert!(
            store
                .apply_body(namespace, id.clone(), operation.clone(), None)
                .await
                .unwrap()
        );
        assert!(
            !store
                .apply_body(namespace, id.clone(), operation.clone(), None)
                .await
                .unwrap()
        );

        // An edit that lands inside a character is refused, and leaves the
        // stored body exactly as it was.
        let stored = store.bodies().into_iter().next().unwrap();
        let mut utf_buffer = stored
            .buffer(namespace, text::BufferId::new(2).unwrap())
            .unwrap();
        let utf_end = utf_buffer.len();
        let utf_operation =
            rho_desk::TextOperation::from_text(&utf_buffer.edit([(utf_end..utf_end, "é")]));
        store
            .apply_body(namespace, id.clone(), utf_operation, None)
            .await
            .unwrap();
        let stored = store.bodies().into_iter().next().unwrap();
        let mut utf_buffer = stored
            .buffer(namespace, text::BufferId::new(3).unwrap())
            .unwrap();
        let utf_end = utf_buffer.len();
        let mut split_character =
            rho_desk::TextOperation::from_text(&utf_buffer.edit([(utf_end..utf_end, "x")]));
        if let rho_desk::TextOperation::Edit { ranges, .. } = &mut split_character {
            let end = ranges[0].0;
            ranges[0] = (end - 1, end - 1);
        }
        let before_split = store.bodies();
        assert!(
            store
                .apply_body(namespace, id.clone(), split_character, None)
                .await
                .is_err()
        );
        assert_eq!(store.bodies(), before_split);

        let mut stale = operation.clone();
        match &mut stale {
            rho_desk::TextOperation::Edit { timestamp, .. }
            | rho_desk::TextOperation::Undo { timestamp, .. } => {
                timestamp.value = timestamp.value.saturating_sub(1);
            }
        }
        assert!(
            store
                .apply_body(namespace, id.clone(), stale, None)
                .await
                .is_err()
        );
        assert!(
            store
                .apply_body(namespace + 1, id.clone(), operation.clone(), None)
                .await
                .is_err()
        );
        // Only a note has words; a Slack unit's are Slack's.
        assert!(
            store
                .apply_body(
                    namespace,
                    Id::Slack(rho_desk::cells::SlackUnit {
                        workspace: "rho".into(),
                        channel: "C1".into(),
                        thread: None,
                    }),
                    operation,
                    None,
                )
                .await
                .is_err()
        );
    }
}
