//! The one-shot conversion from node cells to fact cells.
//!
//! It runs once at daemon start and then this whole file goes (the
//! standing rule for migrations). The legacy types below are copies of
//! the old `rho_desk::cells` shapes, kept only so the old rows can be
//! decoded; senax keys variants by name, so the copies read what the old
//! code wrote.

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use redb::TableDefinition;
use rho_core::AgentId;
use rho_db::{RecordedTypeName, Sen, SenAs, SenValue, WriteTxn};
use rho_desk::cells::{
    Cell, DeviceId, Id, Relation, SlackUnit, Stamp, State, Timestamp, Uuid, Verdict, VerdictEvent,
    Version,
};
use rho_desk::{NodeId, PageId};
use senax_encoder::{Decode, Encode};

const OLD_CELLS: TableDefinition<SenAs<OldCellAddress, CellAddressName>, SenAs<OldCell, CellName>> =
    TableDefinition::new("rho_desk_cells_v2");
const OLD_VERDICTS: TableDefinition<
    SenAs<OldVerdictKey, VerdictKeyName>,
    SenAs<OldVerdictEvent, VerdictEventName>,
> = TableDefinition::new("rho_desk_verdicts_v1");
const OLD_TEXTS: TableDefinition<Sen<NodeId>, Sen<rho_desk::NodeTextSnapshot>> =
    TableDefinition::new("rho_desk_node_text_v2");
const OLD_MUTATIONS: &str = "rho_desk_mutations_v2";

/// redb records the type names a table was created with, so the old rows
/// answer to the names the old code had, not to these copies'.
#[derive(Debug)]
struct CellAddressName;
#[derive(Debug)]
struct CellName;
#[derive(Debug)]
struct VerdictKeyName;
#[derive(Debug)]
struct VerdictEventName;

impl RecordedTypeName for CellAddressName {
    const NAME: &'static str = "rho-db::Sen<rho_daemon::desk_cells::CellAddress>";
}

impl RecordedTypeName for CellName {
    const NAME: &'static str = "rho-db::Sen<rho_desk::cells::Cell>";
}

impl RecordedTypeName for VerdictKeyName {
    const NAME: &'static str = "rho-db::Sen<rho_daemon::desk_cells::VerdictKey>";
}

impl RecordedTypeName for VerdictEventName {
    const NAME: &'static str = "rho-db::Sen<rho_desk::cells::VerdictEvent>";
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct OldCellAddress {
    node: NodeId,
    field: OldField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct OldVerdictKey {
    node: NodeId,
    stamp: Stamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
enum OldNodeKind {
    Note,
    Agent,
    Page,
    Thread,
    PullRequest,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
enum OldField {
    Kind,
    Parent,
    Deleted,
    CreatedAt,
    State,
    DeferUntil,
    Deadline,
    PaceDays,
    Tag(String),
    AgentId,
    Host,
    PageRef,
    Url,
    Workspace,
    Channel,
    ThreadTs,
    Repo,
    PullRequestNumber,
    Path,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum OldValue {
    Kind(OldNodeKind),
    Parent(Option<NodeId>),
    Bool(bool),
    Timestamp(Timestamp),
    OptionalTimestamp(Option<Timestamp>),
    State(State),
    AgentId(AgentId),
    Host(u64),
    PageRef(PageId),
    Text(String),
    Number(u64),
    Days(u32),
    Path(Utf8PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct OldCell {
    node: NodeId,
    field: OldField,
    stamp: Stamp,
    value: OldValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum OldVerdict {
    Done,
    Dismiss,
    Defer { until: Timestamp },
    Todo { note: NodeId },
    File { parent: NodeId },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct OldFieldChange {
    node: NodeId,
    field: OldField,
    before: Option<OldValue>,
    after: Option<OldValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum OldVerdictEvent {
    Applied {
        verdict: OldVerdict,
        at: Stamp,
        changes: Vec<OldFieldChange>,
    },
    Undone {
        of: Stamp,
    },
}

/// What the conversion did, for the log line and for the report on a copy
/// of real state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MigrationReport {
    pub(crate) notes: usize,
    pub(crate) agents: usize,
    pub(crate) pages: usize,
    pub(crate) files: usize,
    pub(crate) pull_requests: usize,
    pub(crate) slack_units: usize,
    pub(crate) labels: usize,
    pub(crate) bodies: usize,
    pub(crate) facts: usize,
    pub(crate) verdicts: usize,
    /// Machine-made thread nodes: no filing, no notes, nothing to keep.
    pub(crate) dropped_threads: usize,
    /// Nodes with no kind cell, or a kind whose identity cells were
    /// missing, so there is no id to move them to.
    pub(crate) dropped_unidentifiable: usize,
    pub(crate) dropped_verdicts: usize,
}

impl MigrationReport {
    pub(crate) fn line(&self) -> String {
        format!(
            "desk store migrated: {} notes, {} agents, {} pages, {} files, {} pull requests, \
             {} slack units, {} labels, {} bodies, {} facts, {} verdicts; \
             dropped {} machine thread nodes, {} unidentifiable nodes, {} verdicts",
            self.notes,
            self.agents,
            self.pages,
            self.files,
            self.pull_requests,
            self.slack_units,
            self.labels,
            self.bodies,
            self.facts,
            self.verdicts,
            self.dropped_threads,
            self.dropped_unidentifiable,
            self.dropped_verdicts
        )
    }
}

/// A note keeps its identity through the conversion so that two devices
/// converting the same state land on the same ids and still merge.
fn note_uuid(node: NodeId) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[1..3].copy_from_slice(&node.replica_id.to_le_bytes());
    bytes[3..11].copy_from_slice(&node.counter.to_le_bytes());
    Uuid(bytes)
}

/// The same for a label minted from a tag name: derived from the name, so
/// the tag `rho` becomes one label everywhere it was written.
fn label_uuid(name: &str) -> Uuid {
    let mut bytes = [0u8; 16];
    for half in 0..2 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        hash ^= half as u64;
        for byte in name.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        bytes[half * 8..half * 8 + 8].copy_from_slice(&hash.to_le_bytes());
    }
    Uuid(bytes)
}

struct OldNode {
    kind: OldNodeKind,
    fields: BTreeMap<OldField, OldCell>,
}

impl OldNode {
    fn value(&self, field: &OldField) -> Option<&OldValue> {
        self.fields.get(field).map(|cell| &cell.value)
    }

    fn text(&self, field: &OldField) -> Option<String> {
        match self.value(field) {
            Some(OldValue::Text(text)) => Some(text.clone()),
            _ => None,
        }
    }

    fn parent(&self) -> Option<NodeId> {
        match self.value(&OldField::Parent) {
            Some(OldValue::Parent(parent)) => *parent,
            _ => None,
        }
    }
}

/// Converts the old tables into the new ones inside the caller's write
/// transaction, and drops the old tables. Answers `None` when there is
/// nothing to convert.
pub(crate) fn migrate(
    write: &mut WriteTxn,
    daemon_device: DeviceId,
    machine_seed: u64,
) -> Result<
    Option<(
        Vec<Cell>,
        Vec<(Id, Stamp, VerdictEvent)>,
        Vec<rho_desk::cells::BodySnapshot>,
        MigrationReport,
    )>,
    String,
> {
    let mut nodes: BTreeMap<NodeId, OldNode> = BTreeMap::new();
    let mut loose: BTreeMap<NodeId, Vec<OldCell>> = BTreeMap::new();
    {
        let table = write.open_table(OLD_CELLS);
        for (_, value) in table.iter() {
            let cell = value.value().into_owned();
            loose.entry(cell.node).or_default().push(cell);
        }
    }
    if loose.is_empty() {
        drop_old_tables(write);
        return Ok(None);
    }
    let mut report = MigrationReport::default();
    for (node, cells) in loose {
        let kind = cells
            .iter()
            .find_map(|cell| match (&cell.field, &cell.value) {
                (OldField::Kind, OldValue::Kind(kind)) => Some(*kind),
                _ => None,
            });
        let Some(kind) = kind else {
            report.dropped_unidentifiable += 1;
            continue;
        };
        nodes.insert(
            node,
            OldNode {
                kind,
                fields: cells
                    .into_iter()
                    .map(|cell| (cell.field.clone(), cell))
                    .collect(),
            },
        );
    }

    // A thread node the user never touched was made by the mirror, and its
    // done-ness was lost on every restart anyway, so it leaves nothing.
    let parents_with_notes = nodes
        .values()
        .filter(|node| node.kind == OldNodeKind::Note)
        .filter_map(|node| node.parent())
        .collect::<BTreeSet<_>>();

    let mut ids: BTreeMap<NodeId, Id> = BTreeMap::new();
    for (node, old) in &nodes {
        let id = match old.kind {
            OldNodeKind::Note => Some(Id::Note(note_uuid(*node))),
            OldNodeKind::Agent => match old.value(&OldField::AgentId) {
                Some(OldValue::AgentId(agent)) => Some(Id::Agent(*agent)),
                _ => None,
            },
            OldNodeKind::Page => match old.value(&OldField::PageRef) {
                Some(OldValue::PageRef(page)) => Some(Id::Page(*page)),
                _ => None,
            },
            // A file row written before the host cell existed is a file on
            // the daemon that stored it, which is this one.
            OldNodeKind::File => match old.value(&OldField::Path) {
                Some(OldValue::Path(path)) => Some(Id::File {
                    host: match old.value(&OldField::Host) {
                        Some(OldValue::Host(host)) => *host,
                        _ => machine_seed,
                    },
                    path: path.clone(),
                }),
                _ => None,
            },
            OldNodeKind::PullRequest => {
                match (
                    old.text(&OldField::Repo),
                    old.value(&OldField::PullRequestNumber),
                ) {
                    (Some(repo), Some(OldValue::Number(number))) => Some(Id::PullRequest {
                        repo,
                        number: *number,
                    }),
                    _ => None,
                }
            }
            OldNodeKind::Thread => {
                let filed = old.parent().is_some();
                if !filed && !parents_with_notes.contains(node) {
                    report.dropped_threads += 1;
                    continue;
                }
                match (
                    old.text(&OldField::Workspace),
                    old.text(&OldField::Channel),
                    old.text(&OldField::ThreadTs),
                ) {
                    (Some(workspace), Some(channel), Some(thread)) => Some(Id::Slack(SlackUnit {
                        workspace,
                        channel,
                        thread: Some(thread),
                    })),
                    _ => None,
                }
            }
        };
        match id {
            Some(id) => {
                ids.insert(*node, id);
            }
            None => report.dropped_unidentifiable += 1,
        }
    }

    let mut cells: BTreeMap<(Id, rho_desk::cells::RelationKey), Cell> = BTreeMap::new();
    let mut labels: BTreeMap<String, (Id, Stamp)> = BTreeMap::new();
    let mut put = |cells: &mut BTreeMap<_, _>, id: Id, relation: Relation, stamp: Stamp| {
        if let Ok(cell) = Cell::new(id, relation, stamp) {
            cells.insert(cell.key(), cell);
        }
    };
    for (node, old) in &nodes {
        let Some(id) = ids.get(node).cloned() else {
            continue;
        };
        match old.kind {
            OldNodeKind::Note => report.notes += 1,
            OldNodeKind::Agent => report.agents += 1,
            OldNodeKind::Page => report.pages += 1,
            OldNodeKind::File => report.files += 1,
            OldNodeKind::PullRequest => report.pull_requests += 1,
            OldNodeKind::Thread => report.slack_units += 1,
        }
        for (field, cell) in &old.fields {
            let stamp = cell.stamp;
            match (field, &cell.value) {
                (OldField::Parent, OldValue::Parent(parent)) => {
                    // The old machine parent was either the root or the
                    // spawner's own node; only a note parent was the user
                    // filing the thing, so only that survives.
                    let parent = parent.and_then(|parent| {
                        (nodes.get(&parent).map(|node| node.kind) == Some(OldNodeKind::Note))
                            .then(|| ids.get(&parent).cloned())
                            .flatten()
                    });
                    if old.kind == OldNodeKind::Note || parent.is_some() {
                        put(&mut cells, id.clone(), Relation::Parent(parent), stamp);
                    }
                }
                (OldField::Deleted, OldValue::Bool(deleted)) => {
                    put(&mut cells, id.clone(), Relation::Deleted(*deleted), stamp)
                }
                (OldField::CreatedAt, OldValue::Timestamp(at)) => {
                    put(&mut cells, id.clone(), Relation::CreatedAt(*at), stamp)
                }
                (OldField::State, OldValue::State(state)) => {
                    put(&mut cells, id.clone(), Relation::State(*state), stamp)
                }
                (OldField::DeferUntil, OldValue::OptionalTimestamp(at)) => {
                    put(&mut cells, id.clone(), Relation::DeferUntil(*at), stamp)
                }
                (OldField::Deadline, OldValue::OptionalTimestamp(at)) => {
                    put(&mut cells, id.clone(), Relation::Deadline(*at), stamp)
                }
                (OldField::PaceDays, OldValue::Days(days)) => {
                    put(&mut cells, id.clone(), Relation::PaceDays(*days), stamp)
                }
                (OldField::Tag(name), OldValue::Bool(present)) => {
                    let entry = labels
                        .entry(name.clone())
                        .or_insert_with(|| (Id::Label(label_uuid(name)), stamp));
                    if stamp < entry.1 {
                        entry.1 = stamp;
                    }
                    let label = entry.0.clone();
                    put(
                        &mut cells,
                        id.clone(),
                        Relation::Labeled {
                            label,
                            present: *present,
                        },
                        stamp,
                    );
                }
                _ => {}
            }
        }
    }
    for (name, (label, stamp)) in &labels {
        report.labels += 1;
        put(
            &mut cells,
            label.clone(),
            Relation::Name(name.clone()),
            *stamp,
        );
        put(&mut cells, label.clone(), Relation::Parent(None), *stamp);
    }

    let mut verdicts = Vec::new();
    {
        let table = write.open_table(OLD_VERDICTS);
        for (key, value) in table.iter() {
            let key = key.value().into_owned();
            let event = value.value().into_owned();
            let Some(id) = ids.get(&key.node).cloned() else {
                report.dropped_verdicts += 1;
                continue;
            };
            match convert_verdict(&ids, event) {
                Some(event) => {
                    report.verdicts += 1;
                    verdicts.push((id, key.stamp, event));
                }
                None => report.dropped_verdicts += 1,
            }
        }
    }

    let mut bodies = Vec::new();
    {
        let table = write.open_table(OLD_TEXTS);
        for (_, value) in table.iter() {
            let text = value.value().into_owned();
            let Some(id @ Id::Note(_)) = ids.get(&text.node_id).cloned() else {
                continue;
            };
            report.bodies += 1;
            bodies.push(rho_desk::cells::BodySnapshot {
                id,
                operations: text.operations,
                transactions: text.transactions,
            });
        }
    }

    let cells = cells.into_values().collect::<Vec<_>>();
    report.facts = cells.len();
    let _ = daemon_device;
    drop_old_tables(write);
    Ok(Some((cells, verdicts, bodies, report)))
}

/// The frontier the converted cells and verdicts already reach, so the
/// daemon does not hand a peer a version it cannot explain.
pub(crate) fn frontier(cells: &[Cell], verdicts: &[(Id, Stamp, VerdictEvent)]) -> Version {
    let mut version = Version::new();
    let mut see = |stamp: &Stamp| {
        let entry = version.entry(stamp.device).or_insert(0);
        *entry = (*entry).max(stamp.version);
    };
    for cell in cells {
        see(&cell.stamp);
    }
    for (_, stamp, _) in verdicts {
        see(stamp);
    }
    version
}

fn convert_verdict(ids: &BTreeMap<NodeId, Id>, event: OldVerdictEvent) -> Option<VerdictEvent> {
    let (verdict, at, changes) = match event {
        OldVerdictEvent::Undone { of } => return Some(VerdictEvent::Undone { of }),
        OldVerdictEvent::Applied {
            verdict,
            at,
            changes,
        } => (verdict, at, changes),
    };
    let verdict = match verdict {
        OldVerdict::Done => Verdict::Done,
        OldVerdict::Dismiss => Verdict::Dismiss,
        OldVerdict::Defer { until } => Verdict::Defer { until },
        OldVerdict::Todo { note } => Verdict::Todo {
            note: ids.get(&note).cloned()?,
        },
        OldVerdict::File { parent } => Verdict::File {
            parent: ids.get(&parent).cloned()?,
        },
    };
    let mut converted = Vec::new();
    for change in changes {
        let id = ids.get(&change.node).cloned()?;
        let before = change
            .before
            .and_then(|value| relation(&change.field, &value, ids));
        let after = change
            .after
            .and_then(|value| relation(&change.field, &value, ids));
        let key = match after.as_ref().or(before.as_ref()) {
            Some(relation) => relation.key(),
            None => continue,
        };
        converted.push(rho_desk::cells::FactChange {
            id,
            key,
            before,
            after,
        });
    }
    Some(VerdictEvent::Applied {
        verdict,
        at,
        changes: converted,
    })
}

fn relation(field: &OldField, value: &OldValue, ids: &BTreeMap<NodeId, Id>) -> Option<Relation> {
    Some(match (field, value) {
        (OldField::Parent, OldValue::Parent(parent)) => Relation::Parent(match parent {
            Some(parent) => Some(ids.get(parent).cloned()?),
            None => None,
        }),
        (OldField::Deleted, OldValue::Bool(deleted)) => Relation::Deleted(*deleted),
        (OldField::CreatedAt, OldValue::Timestamp(at)) => Relation::CreatedAt(*at),
        (OldField::State, OldValue::State(state)) => Relation::State(*state),
        (OldField::DeferUntil, OldValue::OptionalTimestamp(at)) => Relation::DeferUntil(*at),
        (OldField::Deadline, OldValue::OptionalTimestamp(at)) => Relation::Deadline(*at),
        (OldField::PaceDays, OldValue::Days(days)) => Relation::PaceDays(*days),
        (OldField::Tag(name), OldValue::Bool(present)) => Relation::Labeled {
            label: Id::Label(label_uuid(name)),
            present: *present,
        },
        _ => return None,
    })
}

fn drop_old_tables(write: &mut WriteTxn) {
    write.delete_table("rho_desk_cells_v2");
    write.delete_table("rho_desk_verdicts_v1");
    write.delete_table("rho_desk_node_text_v2");
    write.delete_table(OLD_MUTATIONS);
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;
    use rho_desk::cells::{Id, RelationKey, State, TimestampPrecision, Version};

    use super::*;
    use crate::desk_cells::DeskCellStore;

    /// The daemon that stored the rows, which is the one converting them.
    const MACHINE_SEED: u64 = 42;

    fn node(counter: u64) -> NodeId {
        NodeId {
            replica_id: 1,
            counter,
        }
    }

    fn cell(node: NodeId, field: OldField, value: OldValue, version: u64) -> OldCell {
        OldCell {
            node,
            field,
            stamp: Stamp {
                device: DeviceId([1; 16]),
                version,
            },
            value,
        }
    }

    /// The whole conversion in one state: a note with a tag, an agent the
    /// user filed under it, an agent filed nowhere, a thread the user
    /// filed, a thread the mirror made, and a file row from before the
    /// host cell existed.
    #[tokio::test]
    async fn converting_node_cells_keeps_what_the_user_said_and_drops_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        let db = RhoDb::open(path);
        let heading = node(1);
        let filed_agent = node(2);
        let loose_agent = node(3);
        let filed_thread = node(4);
        let mirror_thread = node(5);
        let hostless_file = node(6);
        let agent = |counter: u64| {
            rho_core::AgentId::from_counter(counter, &rho_core::AgentIdDomain(42)).unwrap()
        };
        let thread = |node: NodeId, ts: &str, parent: Option<NodeId>, version: u64| {
            vec![
                cell(
                    node,
                    OldField::Kind,
                    OldValue::Kind(OldNodeKind::Thread),
                    version,
                ),
                cell(node, OldField::Parent, OldValue::Parent(parent), version),
                cell(
                    node,
                    OldField::Workspace,
                    OldValue::Text("rho".into()),
                    version,
                ),
                cell(
                    node,
                    OldField::Channel,
                    OldValue::Text("C1".into()),
                    version,
                ),
                cell(node, OldField::ThreadTs, OldValue::Text(ts.into()), version),
            ]
        };
        let mut cells = vec![
            cell(
                heading,
                OldField::Kind,
                OldValue::Kind(OldNodeKind::Note),
                1,
            ),
            cell(heading, OldField::Parent, OldValue::Parent(None), 1),
            cell(heading, OldField::Deleted, OldValue::Bool(false), 1),
            cell(
                heading,
                OldField::CreatedAt,
                OldValue::Timestamp(Timestamp {
                    unix_ms: 5,
                    precision: TimestampPrecision::Millisecond,
                }),
                1,
            ),
            cell(heading, OldField::State, OldValue::State(State::Open), 1),
            cell(
                heading,
                OldField::Tag("rho".into()),
                OldValue::Bool(true),
                1,
            ),
            cell(
                filed_agent,
                OldField::Kind,
                OldValue::Kind(OldNodeKind::Agent),
                2,
            ),
            cell(
                filed_agent,
                OldField::Parent,
                OldValue::Parent(Some(heading)),
                2,
            ),
            cell(
                filed_agent,
                OldField::AgentId,
                OldValue::AgentId(agent(1)),
                2,
            ),
            cell(filed_agent, OldField::Host, OldValue::Host(42), 2),
            cell(
                filed_agent,
                OldField::State,
                OldValue::State(State::Done),
                2,
            ),
            cell(
                loose_agent,
                OldField::Kind,
                OldValue::Kind(OldNodeKind::Agent),
                3,
            ),
            // The spawner's node was the machine's default parent, so it is
            // not a filing and does not survive.
            cell(
                loose_agent,
                OldField::Parent,
                OldValue::Parent(Some(filed_agent)),
                3,
            ),
            cell(
                loose_agent,
                OldField::AgentId,
                OldValue::AgentId(agent(2)),
                3,
            ),
            cell(loose_agent, OldField::Host, OldValue::Host(42), 3),
        ];
        cells.extend([
            cell(
                hostless_file,
                OldField::Kind,
                OldValue::Kind(OldNodeKind::File),
                8,
            ),
            cell(
                hostless_file,
                OldField::Parent,
                OldValue::Parent(Some(heading)),
                8,
            ),
            cell(
                hostless_file,
                OldField::Path,
                OldValue::Path("/src/rho/README.md".into()),
                8,
            ),
        ]);
        cells.extend(thread(filed_thread, "1.0", Some(heading), 4));
        cells.extend(thread(mirror_thread, "2.0", None, 5));

        {
            let mut write = db.write().await;
            let mut table = write.open_table(OLD_CELLS);
            for cell in &cells {
                table.insert(
                    SenValue::owned(OldCellAddress {
                        node: cell.node,
                        field: cell.field.clone(),
                    }),
                    SenValue::borrowed(cell),
                );
            }
            drop(table);
            let mut verdicts = write.open_table(OLD_VERDICTS);
            let at = Stamp {
                device: DeviceId([1; 16]),
                version: 6,
            };
            for (node, version) in [(filed_thread, 6), (mirror_thread, 7)] {
                verdicts.insert(
                    SenValue::owned(OldVerdictKey {
                        node,
                        stamp: Stamp {
                            device: DeviceId([1; 16]),
                            version,
                        },
                    }),
                    SenValue::owned(OldVerdictEvent::Applied {
                        verdict: OldVerdict::Done,
                        at,
                        changes: vec![OldFieldChange {
                            node,
                            field: OldField::State,
                            before: Some(OldValue::State(State::Open)),
                            after: Some(OldValue::State(State::Done)),
                        }],
                    }),
                );
            }
            drop(verdicts);
            write.open_table(OLD_TEXTS).insert(
                SenValue::owned(heading),
                SenValue::owned(rho_desk::NodeTextSnapshot {
                    node_id: heading,
                    operations: Vec::new(),
                    transactions: Vec::new(),
                }),
            );
            write.commit();
        }

        let store = DeskCellStore::new(db.clone(), MACHINE_SEED).await.unwrap();
        let snapshot = store.sync_since(&Version::new()).unwrap();
        let facts =
            rho_desk::cells::Store::from_snapshot(DeviceId([0; 16]), snapshot.clone()).unwrap();
        let note = Id::Note(note_uuid(heading));
        let unit = |ts: &str| {
            Id::Slack(SlackUnit {
                workspace: "rho".into(),
                channel: "C1".into(),
                thread: Some(ts.into()),
            })
        };

        assert_eq!(facts.facts(&note).parent, None);
        assert!(
            facts
                .facts(&note)
                .labels
                .contains(&Id::Label(label_uuid("rho")))
        );
        assert_eq!(
            facts.relation(&Id::Label(label_uuid("rho")), &RelationKey::Name),
            Some(&Relation::Name("rho".into()))
        );
        // The agent the user filed keeps its heading and its state; the one
        // that only sat under its spawner keeps neither.
        assert_eq!(facts.facts(&Id::Agent(agent(1))).parent, Some(note.clone()));
        assert_eq!(facts.facts(&Id::Agent(agent(1))).state, State::Done);
        assert_eq!(facts.facts(&Id::Agent(agent(2))).parent, None);
        assert!(!facts.facts(&Id::Agent(agent(2))).any());
        // The filed thread becomes its unit; the mirror's leaves nothing,
        // verdicts included.
        assert_eq!(facts.facts(&unit("1.0")).parent, Some(note.clone()));
        assert!(!facts.facts(&unit("2.0")).any());
        assert_eq!(snapshot.verdicts.len(), 1);
        assert_eq!(snapshot.verdicts[0].0, unit("1.0"));
        assert_eq!(store.bodies().len(), 1);
        assert_eq!(store.bodies()[0].id, note);

        // A file row written before the host cell existed is kept, on the
        // daemon that stored it.
        let file = Id::File {
            host: MACHINE_SEED,
            path: "/src/rho/README.md".into(),
        };
        assert_eq!(facts.facts(&file).parent, Some(note.clone()));

        // The old tables are gone, so the conversion never runs twice.
        assert!(!db.read().has_table("rho_desk_cells_v2"));
    }
}
