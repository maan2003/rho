//! Cell-based convergent storage for the Desk.
//!
//! The store holds the user's facts and only those. A fact is
//! `(subject: Id, relation, object)`: the subject names a thing in the
//! system that owns it, the relation says what is being claimed, and each
//! relation declares its object type, its cardinality, and how two writes
//! merge. Anything a source already knows — an agent's spawner, a thread's
//! channel, a page's title — is derived at read time and never lands here,
//! so the store can never disagree with a source it never repeats.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use rho_core::AgentId;
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::PageId;

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeviceId(pub [u8; 16]);

/// A device-local Lamport version. Ordering is version first; device only
/// breaks ties, regardless of the field declaration order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct Stamp {
    pub device: DeviceId,
    pub version: u64,
}

impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.version
            .cmp(&other.version)
            .then_with(|| self.device.cmp(&other.device))
    }
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The identity rho mints. Only a note and a label get one: everything else
/// is named by the system that owns it.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
        Self(bytes)
    }
}

/// What Slack calls a place a conversation happens: a direct or group
/// conversation, a channel, or a followed thread. Never a message, which is
/// what made a done thread come back the moment history loaded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct SlackUnit {
    pub workspace: String,
    pub channel: String,
    /// `None` is the conversation or the channel itself.
    pub thread: Option<String>,
}

/// What a fact is about. There is no `kind` cell because the kind is the
/// id: an agent is an `Agent` because the registry says that agent exists,
/// not because anything in rho created a row for it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub enum Id {
    Note(Uuid),
    Label(Uuid),
    Agent(AgentId),
    Host(u64),
    Page(PageId),
    Slack(SlackUnit),
    PullRequest { repo: String, number: u64 },
    File { host: u64, path: Utf8PathBuf },
}

impl Id {
    /// Whether rho is allowed to mint this id. Everything else exists
    /// because its source says so.
    pub fn is_minted(&self) -> bool {
        matches!(self, Id::Note(_) | Id::Label(_))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum State {
    #[default]
    Open,
    Done,
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub enum TimestampPrecision {
    Day,
    Minute,
    Millisecond,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct Timestamp {
    pub unix_ms: i64,
    pub precision: TimestampPrecision,
}

/// A Slack timestamp: its message ordering as well as its time.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct SlackTs(pub String);

/// What the user can claim about a thing. The claim is the variant and its
/// payload together; there is no separate value column, so a fact that
/// needs more detail than an id carries says so in its own payload rather
/// than in a string somewhere else.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Relation {
    /// Where the user filed it. Any id may be a parent: an agent under a
    /// Slack thread, a note under a page, a label under a label. `None` is
    /// the root, written rather than deleted, so un-filing is a fact like
    /// any other.
    Parent(Option<Id>),
    /// A set: the label is part of the key, so two devices tagging at once
    /// do not fight over one cell.
    Labeled {
        label: Id,
        present: bool,
    },
    /// A label's own name.
    Name(String),
    State(State),
    DeferUntil(Option<Timestamp>),
    Deadline(Option<Timestamp>),
    PaceDays(u32),
    /// The Slack verdict cursor: everything up to here has been dealt with.
    HandledThrough(SlackTs),
    Deleted(bool),
    CreatedAt(Timestamp),
}

/// What makes a fact the same fact. For one-per-subject relations that is
/// the variant alone, so a later write replaces an earlier one; for a set
/// it is the variant and its member, so each member settles on its own.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub enum RelationKey {
    Parent,
    Labeled(Id),
    Name,
    State,
    DeferUntil,
    Deadline,
    PaceDays,
    HandledThrough,
    Deleted,
    CreatedAt,
}

/// How many claims of one relation a subject may hold at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cardinality {
    One,
    Set,
}

/// How two writes to the same fact settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    LastWriterWins,
    /// Opposing writes at the same version choose the add, so a label put
    /// on from two devices at once stays on.
    AddWins,
}

impl Relation {
    pub fn key(&self) -> RelationKey {
        match self {
            Relation::Parent(_) => RelationKey::Parent,
            Relation::Labeled { label, .. } => RelationKey::Labeled(label.clone()),
            Relation::Name(_) => RelationKey::Name,
            Relation::State(_) => RelationKey::State,
            Relation::DeferUntil(_) => RelationKey::DeferUntil,
            Relation::Deadline(_) => RelationKey::Deadline,
            Relation::PaceDays(_) => RelationKey::PaceDays,
            Relation::HandledThrough(_) => RelationKey::HandledThrough,
            Relation::Deleted(_) => RelationKey::Deleted,
            Relation::CreatedAt(_) => RelationKey::CreatedAt,
        }
    }

    pub fn cardinality(&self) -> Cardinality {
        self.key().cardinality()
    }

    pub fn merge(&self) -> Merge {
        self.key().merge()
    }

    /// Whether the claim is one the store will take at all. A thing is
    /// labelled with labels and nothing else.
    pub fn is_valid(&self) -> bool {
        match self {
            Relation::Labeled { label, .. } => matches!(label, Id::Label(_)),
            _ => true,
        }
    }
}

impl RelationKey {
    pub fn cardinality(&self) -> Cardinality {
        match self {
            RelationKey::Labeled(_) => Cardinality::Set,
            _ => Cardinality::One,
        }
    }

    pub fn merge(&self) -> Merge {
        match self {
            RelationKey::Labeled(_) => Merge::AddWins,
            _ => Merge::LastWriterWins,
        }
    }

    /// The claim that holds when nobody has written this fact. Undo needs
    /// it: putting a cell back where there was none is writing the state
    /// the reader saw, and that state is this. The relations left out are
    /// the ones with no unwritten reading — a name, a Slack cursor, a
    /// creation time either exist or the thing is not that thing.
    pub fn unwritten(&self) -> Option<Relation> {
        match self {
            RelationKey::Parent => Some(Relation::Parent(None)),
            RelationKey::Labeled(label) => Some(Relation::Labeled {
                label: label.clone(),
                present: false,
            }),
            RelationKey::State => Some(Relation::State(State::Open)),
            RelationKey::DeferUntil => Some(Relation::DeferUntil(None)),
            RelationKey::Deadline => Some(Relation::Deadline(None)),
            RelationKey::PaceDays => Some(Relation::PaceDays(0)),
            RelationKey::Deleted => Some(Relation::Deleted(false)),
            RelationKey::Name | RelationKey::HandledThrough | RelationKey::CreatedAt => None,
        }
    }
}

/// One fact, at the stamp it was claimed. A note's body is not here: it is
/// the text CRDT under the same id, merged by its own operations.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Cell {
    pub id: Id,
    pub relation: Relation,
    pub stamp: Stamp,
}

impl Cell {
    pub fn new(id: Id, relation: Relation, stamp: Stamp) -> Result<Self, String> {
        if !relation.is_valid() {
            return Err("Desk relation payload is not one the store takes".into());
        }
        Ok(Self {
            id,
            relation,
            stamp,
        })
    }

    pub fn key(&self) -> (Id, RelationKey) {
        (self.id.clone(), self.relation.key())
    }
}

/// A note's body: the text CRDT under the id the note is known by. It is
/// not a cell, because the words are not a claim that can be last-writer
/// merged; the relation vocabulary names it so the store's fact list is
/// complete, and this is where it actually lives.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct BodySnapshot {
    pub id: Id,
    pub operations: Vec<crate::TextOperation>,
    pub transactions: Vec<crate::TextTransaction>,
}

impl BodySnapshot {
    pub fn buffer(
        &self,
        replica_id: u16,
        buffer_id: text::BufferId,
    ) -> Result<text::Buffer, String> {
        let mut buffer = text::Buffer::new(text::ReplicaId::new(replica_id), buffer_id, "");
        buffer.apply_ops(
            self.operations
                .iter()
                .map(crate::TextOperation::to_text)
                .collect::<Result<Vec<_>, _>>()?,
        );
        if buffer.has_deferred_ops() {
            return Err("Desk note body has causally incomplete operation history".into());
        }
        Ok(buffer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Verdict {
    Done,
    Dismiss,
    Defer { until: Timestamp },
    Todo { note: Id },
    File { parent: Id },
}

/// One fact a verdict changed, with what stood there before, which is the
/// only thing undo cannot work out for itself.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct FactChange {
    pub id: Id,
    pub key: RelationKey,
    pub before: Option<Relation>,
    pub after: Option<Relation>,
}

/// The cadence a `todo` verdict gives the note it creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TodoCadence {
    pub defer_until: Timestamp,
    pub pace_days: u32,
}

/// The facts a verdict records in its log entry.
///
/// One definition for both sides: the GUI logs exactly this, and the daemon
/// checks a submitted entry against it, so the two can never drift.
/// `before` is what the relation held, which only the writer knows;
/// `cadence` is required for `todo`, whose changes describe the note the
/// verdict creates rather than the thing it was dealt on.
pub fn verdict_changes(
    id: &Id,
    verdict: &Verdict,
    before: &dyn Fn(&RelationKey) -> Option<Relation>,
    cadence: Option<TodoCadence>,
) -> Result<Vec<FactChange>, String> {
    let change = |after: Relation| FactChange {
        id: id.clone(),
        key: after.key(),
        // A fact nobody has written still reads as something, and undo has
        // to put that reading back, so the unwritten claim stands in for a
        // missing cell here rather than at each caller.
        before: before(&after.key()).or_else(|| after.key().unwritten()),
        after: Some(after),
    };
    let one = |after: Relation| Ok(vec![change(after)]);
    match verdict {
        Verdict::Done => one(Relation::State(State::Done)),
        Verdict::Dismiss => one(Relation::State(State::Dismissed)),
        // A snooze puts the card back at zero: `elapsed - pace` is the todo
        // curve when a pace is set and the defer curve when it is not, so a
        // deferred todo that kept its old pace would come back climbing.
        Verdict::Defer { until } => Ok(vec![
            change(Relation::DeferUntil(Some(*until))),
            change(Relation::PaceDays(0)),
        ]),
        Verdict::File { parent } => one(Relation::Parent(Some(parent.clone()))),
        // A todo is the one verdict that writes a whole new note, so its
        // entry carries all three facts that make that note a live cadence,
        // against what a note that never existed reads as holding. The
        // thing it was dealt on is handled by the same act: without that the
        // dealer offers it again the moment the todo is written.
        Verdict::Todo { note } => {
            let cadence = cadence.ok_or("a todo verdict needs its cadence")?;
            let fresh = |before: Relation, after: Relation| FactChange {
                id: note.clone(),
                key: after.key(),
                before: Some(before),
                after: Some(after),
            };
            Ok(vec![
                FactChange {
                    id: id.clone(),
                    key: RelationKey::State,
                    before: before(&RelationKey::State).or_else(|| RelationKey::State.unwritten()),
                    after: Some(Relation::State(State::Done)),
                },
                fresh(Relation::Deleted(true), Relation::Deleted(false)),
                fresh(
                    Relation::DeferUntil(None),
                    Relation::DeferUntil(Some(cadence.defer_until)),
                ),
                fresh(Relation::PaceDays(0), Relation::PaceDays(cadence.pace_days)),
            ])
        }
    }
}

/// The cadence a submitted `todo` entry claims, read back out of its changes.
pub fn todo_cadence(changes: &[FactChange]) -> Option<TodoCadence> {
    let after = |key: RelationKey| {
        changes
            .iter()
            .find(|change| change.key == key)
            .and_then(|change| change.after.clone())
    };
    let Some(Relation::DeferUntil(Some(defer_until))) = after(RelationKey::DeferUntil) else {
        return None;
    };
    let Some(Relation::PaceDays(pace_days)) = after(RelationKey::PaceDays) else {
        return None;
    };
    Some(TodoCadence {
        defer_until,
        pace_days,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum VerdictEvent {
    Applied {
        verdict: Verdict,
        at: Stamp,
        changes: Vec<FactChange>,
    },
    Undone {
        of: Stamp,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CellWrite {
    pub id: Id,
    pub relation: Relation,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CellMutation {
    pub stamp: Stamp,
    pub writes: Vec<CellWrite>,
    pub verdict: Option<(Id, VerdictEvent)>,
}

pub type Version = BTreeMap<DeviceId, u64>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Snapshot {
    pub cells: Vec<Cell>,
    pub verdicts: Vec<(Id, Stamp, VerdictEvent)>,
    pub version: Version,
}

/// Every fact the store holds about one id. Where the thing is shown, what
/// it is called, and whether it is dealable are not here: those are view
/// rules over these facts joined with the sources, and live in the GUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The user's filing, and whether they ever did any: `None` filed is
    /// the root, and never filed leaves the place to the source.
    pub parent: Option<Id>,
    pub filed: bool,
    pub labels: BTreeSet<Id>,
    pub name: Option<String>,
    pub state: State,
    pub defer_until: Option<Timestamp>,
    pub deadline: Option<Timestamp>,
    pub pace_days: u32,
    pub handled_through: Option<SlackTs>,
    pub deleted: bool,
    pub created_at: Option<Timestamp>,
}

impl Facts {
    /// Whether the user has said anything at all about this thing. An id
    /// with no user facts is in the store only because a source mentioned
    /// it, and the map does not show it for its own sake.
    pub fn any(&self) -> bool {
        self.filed
            || !self.labels.is_empty()
            || self.name.is_some()
            || self.state != State::Open
            || self.defer_until.is_some()
            || self.deadline.is_some()
            || self.pace_days != 0
            || self.handled_through.is_some()
            || self.created_at.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    device: DeviceId,
    clock: u64,
    cells: BTreeMap<(Id, RelationKey), Cell>,
    verdicts: BTreeMap<(Id, Stamp), VerdictEvent>,
    version: Version,
}

impl Store {
    pub fn new(device: DeviceId) -> Self {
        Self {
            device,
            clock: 0,
            cells: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            version: Version::new(),
        }
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn from_snapshot(device: DeviceId, snapshot: Snapshot) -> Result<Self, String> {
        let mut store = Self::new(device);
        store.merge(snapshot)?;
        Ok(store)
    }

    pub fn apply_mutation(&mut self, mutation: &CellMutation) -> Result<(), String> {
        if mutation.writes.is_empty() || mutation.writes.len() > 4096 {
            return Err("Desk cell mutation write count is invalid".into());
        }
        if mutation.verdict.as_ref().is_some_and(|(_, event)| {
            matches!(event, VerdictEvent::Applied { changes, .. } if changes.len() > 4096)
        }) {
            return Err("Desk verdict change count is invalid".into());
        }
        if let Some((_, VerdictEvent::Applied { at, .. })) = &mutation.verdict
            && *at != mutation.stamp
        {
            return Err("Desk verdict event stamp does not match its mutation".into());
        }
        let delta = Snapshot {
            cells: mutation
                .writes
                .iter()
                .map(|write| Cell::new(write.id.clone(), write.relation.clone(), mutation.stamp))
                .collect::<Result<Vec<_>, _>>()?,
            verdicts: mutation
                .verdict
                .iter()
                .map(|(id, event)| (id.clone(), mutation.stamp, event.clone()))
                .collect(),
            version: Version::from([(mutation.stamp.device, mutation.stamp.version)]),
        };
        self.merge(delta)
    }

    pub fn write(&mut self, id: Id, relation: Relation) -> Result<Cell, String> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("Desk device version exhausted")?;
        let cell = Cell::new(
            id,
            relation,
            Stamp {
                device: self.device,
                version: self.clock,
            },
        )?;
        self.merge_cell(cell.clone())?;
        Ok(cell)
    }

    pub fn append_verdict(
        &mut self,
        id: Id,
        verdict: Verdict,
        changes: Vec<FactChange>,
    ) -> Result<Stamp, String> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("Desk device version exhausted")?;
        let stamp = Stamp {
            device: self.device,
            version: self.clock,
        };
        self.verdicts.insert(
            (id, stamp),
            VerdictEvent::Applied {
                verdict,
                at: stamp,
                changes,
            },
        );
        self.observe(stamp);
        Ok(stamp)
    }

    pub fn append_undone(&mut self, id: Id, of: Stamp) -> Result<Stamp, String> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("Desk device version exhausted")?;
        let stamp = Stamp {
            device: self.device,
            version: self.clock,
        };
        self.verdicts
            .insert((id, stamp), VerdictEvent::Undone { of });
        self.observe(stamp);
        Ok(stamp)
    }

    pub fn merge(&mut self, snapshot: Snapshot) -> Result<(), String> {
        let mut staged = self.clone();
        staged.merge_inner(snapshot)?;
        *self = staged;
        Ok(())
    }

    fn merge_inner(&mut self, snapshot: Snapshot) -> Result<(), String> {
        for cell in snapshot.cells {
            self.merge_cell(cell)?;
        }
        for (id, stamp, event) in snapshot.verdicts {
            if matches!(&event, VerdictEvent::Applied { at, .. } if *at != stamp) {
                return Err("Desk verdict event stamp does not match its key".into());
            }
            let key = (id, stamp);
            if let Some(existing) = self.verdicts.get(&key) {
                if existing != &event {
                    return Err("conflicting Desk verdict event at the same stamp".into());
                }
            } else {
                self.verdicts.insert(key, event);
            }
            self.observe(stamp);
        }
        for (device, version) in snapshot.version {
            self.version
                .entry(device)
                .and_modify(|v| *v = (*v).max(version))
                .or_insert(version);
            self.clock = self.clock.max(version);
        }
        Ok(())
    }

    pub fn since(&self, version: &Version) -> Snapshot {
        Snapshot {
            cells: self
                .cells
                .values()
                .filter(|cell| {
                    cell.stamp.version > version.get(&cell.stamp.device).copied().unwrap_or(0)
                })
                .cloned()
                .collect(),
            verdicts: self
                .verdicts
                .iter()
                .filter(|((_, stamp), _)| {
                    stamp.version > version.get(&stamp.device).copied().unwrap_or(0)
                })
                .map(|((id, stamp), event)| (id.clone(), *stamp, event.clone()))
                .collect(),
            version: self.version.clone(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.since(&Version::new())
    }

    fn merge_cell(&mut self, cell: Cell) -> Result<(), String> {
        if !cell.relation.is_valid() {
            return Err("Desk relation payload is not one the store takes".into());
        }
        let key = cell.key();
        if let Some(old) = self.cells.get(&key)
            && old.stamp == cell.stamp
        {
            return if old == &cell {
                Ok(())
            } else {
                Err("conflicting Desk cell values at the same stamp".into())
            };
        }
        let replace = self.cells.get(&key).is_none_or(|old| wins(&cell, old));
        self.observe(cell.stamp);
        if replace {
            self.cells.insert(key, cell);
        }
        Ok(())
    }

    fn observe(&mut self, stamp: Stamp) {
        self.clock = self.clock.max(stamp.version);
        self.version
            .entry(stamp.device)
            .and_modify(|v| *v = (*v).max(stamp.version))
            .or_insert(stamp.version);
    }

    /// Every id the store holds a fact about, in id order.
    pub fn subjects(&self) -> BTreeSet<Id> {
        self.cells.keys().map(|(id, _)| id.clone()).collect()
    }

    pub fn relation(&self, id: &Id, key: &RelationKey) -> Option<&Relation> {
        self.cells
            .get(&(id.clone(), key.clone()))
            .map(|cell| &cell.relation)
    }

    /// What the user has said about one thing. No place, no title, no
    /// dealability: those are rules over this joined with the sources.
    pub fn facts(&self, id: &Id) -> Facts {
        let mut facts = Facts::default();
        for ((subject, _), cell) in self.cells.range((id.clone(), RelationKey::Parent)..) {
            if subject != id {
                break;
            }
            match &cell.relation {
                Relation::Parent(parent) => {
                    facts.parent = parent.clone();
                    facts.filed = true;
                }
                Relation::Labeled {
                    label,
                    present: true,
                } => {
                    facts.labels.insert(label.clone());
                }
                Relation::Labeled { .. } => {}
                Relation::Name(name) => facts.name = Some(name.clone()),
                Relation::State(state) => facts.state = *state,
                Relation::DeferUntil(at) => facts.defer_until = *at,
                Relation::Deadline(at) => facts.deadline = *at,
                Relation::PaceDays(days) => facts.pace_days = *days,
                Relation::HandledThrough(ts) => facts.handled_through = Some(ts.clone()),
                Relation::Deleted(deleted) => facts.deleted = *deleted,
                Relation::CreatedAt(at) => facts.created_at = Some(*at),
            }
        }
        facts
    }

    /// Every subject with its facts, skipping the deleted, in id order.
    pub fn all_facts(&self) -> Vec<(Id, Facts)> {
        self.subjects()
            .into_iter()
            .map(|id| {
                let facts = self.facts(&id);
                (id, facts)
            })
            .filter(|(_, facts)| !facts.deleted)
            .collect()
    }

    pub fn verdict_event(&self, id: &Id, stamp: Stamp) -> Option<&VerdictEvent> {
        self.verdicts.get(&(id.clone(), stamp))
    }
}

fn wins(new: &Cell, old: &Cell) -> bool {
    if new.relation.merge() == Merge::AddWins && new.stamp.version == old.stamp.version {
        match (&new.relation, &old.relation) {
            (Relation::Labeled { present: true, .. }, Relation::Labeled { present: false, .. }) => {
                return true;
            }
            (Relation::Labeled { present: false, .. }, Relation::Labeled { present: true, .. }) => {
                return false;
            }
            _ => {}
        }
    }
    new.stamp > old.stamp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(byte: u8) -> DeviceId {
        DeviceId([byte; 16])
    }

    fn note(byte: u8) -> Id {
        Id::Note(Uuid([byte; 16]))
    }

    fn label(byte: u8) -> Id {
        Id::Label(Uuid([byte; 16]))
    }

    fn timestamp(unix_ms: i64) -> Timestamp {
        Timestamp {
            unix_ms,
            precision: TimestampPrecision::Millisecond,
        }
    }

    fn stamp(byte: u8, version: u64) -> Stamp {
        Stamp {
            device: device(byte),
            version,
        }
    }

    fn snapshot(cells: Vec<Cell>) -> Snapshot {
        Snapshot {
            cells,
            verdicts: vec![],
            version: Version::new(),
        }
    }

    /// One of every relation, with two claims that disagree.
    fn variants() -> Vec<(Relation, Relation)> {
        vec![
            (Relation::Parent(None), Relation::Parent(Some(note(9)))),
            (
                Relation::Labeled {
                    label: label(1),
                    present: false,
                },
                Relation::Labeled {
                    label: label(1),
                    present: true,
                },
            ),
            (Relation::Name("a".into()), Relation::Name("b".into())),
            (Relation::State(State::Open), Relation::State(State::Done)),
            (
                Relation::DeferUntil(None),
                Relation::DeferUntil(Some(timestamp(2))),
            ),
            (
                Relation::Deadline(None),
                Relation::Deadline(Some(timestamp(3))),
            ),
            (Relation::PaceDays(1), Relation::PaceDays(2)),
            (
                Relation::HandledThrough(SlackTs("1.0".into())),
                Relation::HandledThrough(SlackTs("2.0".into())),
            ),
            (Relation::Deleted(false), Relation::Deleted(true)),
            (
                Relation::CreatedAt(timestamp(1)),
                Relation::CreatedAt(timestamp(2)),
            ),
        ]
    }

    #[test]
    fn every_relation_merge_is_commutative_and_idempotent() {
        for (left_claim, right_claim) in variants() {
            let left = Cell::new(note(1), left_claim, stamp(1, 3)).unwrap();
            let right = Cell::new(note(1), right_claim, stamp(2, 4)).unwrap();
            let left_snapshot = snapshot(vec![left]);
            let right_snapshot = snapshot(vec![right]);
            let mut ab = Store::new(device(8));
            ab.merge(left_snapshot.clone()).unwrap();
            ab.merge(right_snapshot.clone()).unwrap();
            let once = ab.snapshot();
            ab.merge(right_snapshot.clone()).unwrap();
            assert_eq!(ab.snapshot(), once);
            let mut ba = Store::new(device(8));
            ba.merge(right_snapshot).unwrap();
            ba.merge(left_snapshot).unwrap();
            assert_eq!(ba.snapshot(), once);
        }
    }

    #[test]
    fn a_relation_key_is_the_variant_and_a_set_member_is_the_variant_and_its_member() {
        // Two labels are two facts; two parents are one fact written twice.
        assert_ne!(
            Relation::Labeled {
                label: label(1),
                present: true,
            }
            .key(),
            Relation::Labeled {
                label: label(2),
                present: true,
            }
            .key()
        );
        assert_eq!(
            Relation::Parent(None).key(),
            Relation::Parent(Some(note(1))).key()
        );
        let mut store = Store::new(device(1));
        for member in [label(1), label(2)] {
            store
                .write(
                    note(1),
                    Relation::Labeled {
                        label: member,
                        present: true,
                    },
                )
                .unwrap();
        }
        store.write(note(1), Relation::Parent(None)).unwrap();
        store
            .write(note(1), Relation::Parent(Some(note(2))))
            .unwrap();
        let facts = store.facts(&note(1));
        assert_eq!(facts.labels, BTreeSet::from([label(1), label(2)]));
        assert_eq!(facts.parent, Some(note(2)));
    }

    #[test]
    fn only_a_label_can_be_labelled_with() {
        let mut store = Store::new(device(1));
        assert!(
            store
                .write(
                    note(1),
                    Relation::Labeled {
                        label: note(2),
                        present: true,
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn concurrent_label_add_wins_independent_of_device_order() {
        let member = || Id::Label(Uuid([7; 16]));
        let off = |at: Stamp| {
            Cell::new(
                note(1),
                Relation::Labeled {
                    label: member(),
                    present: false,
                },
                at,
            )
            .unwrap()
        };
        let on = |at: Stamp| {
            Cell::new(
                note(1),
                Relation::Labeled {
                    label: member(),
                    present: true,
                },
                at,
            )
            .unwrap()
        };
        for cells in [
            vec![off(stamp(9, 4)), on(stamp(1, 4))],
            vec![on(stamp(1, 4)), off(stamp(9, 4))],
        ] {
            let mut store = Store::new(device(3));
            for cell in cells {
                store.merge(snapshot(vec![cell])).unwrap();
            }
            assert!(store.facts(&note(1)).labels.contains(&member()));
        }
        // A later remove is still a remove: add-wins settles a tie, not an
        // order.
        let mut store = Store::new(device(3));
        store.merge(snapshot(vec![on(stamp(1, 4))])).unwrap();
        store.merge(snapshot(vec![off(stamp(9, 5))])).unwrap();
        assert!(store.facts(&note(1)).labels.is_empty());
    }

    #[test]
    fn losing_writes_still_advance_the_frontier() {
        let mut store = Store::new(device(1));
        store
            .merge(snapshot(vec![
                Cell::new(note(1), Relation::State(State::Done), stamp(2, 8)).unwrap(),
                Cell::new(note(1), Relation::State(State::Open), stamp(3, 7)).unwrap(),
            ]))
            .unwrap();
        assert_eq!(store.version().get(&device(3)), Some(&7));
        let delta = store.since(&Version::from([(device(2), 8)]));
        assert!(delta.cells.is_empty());
        assert_eq!(delta.version.get(&device(3)), Some(&7));
    }

    #[test]
    fn verdict_and_undo_events_merge_as_a_grow_only_union() {
        let applied_stamp = stamp(2, 4);
        let undone_stamp = stamp(3, 5);
        let applied = VerdictEvent::Applied {
            verdict: Verdict::Done,
            at: applied_stamp,
            changes: vec![FactChange {
                id: note(1),
                key: RelationKey::State,
                before: Some(Relation::State(State::Open)),
                after: Some(Relation::State(State::Done)),
            }],
        };
        let undone = VerdictEvent::Undone { of: applied_stamp };
        let mut left = Store::new(device(1));
        let mut right = Store::new(device(1));
        for (store, order) in [
            (
                &mut left,
                [
                    (applied_stamp, applied.clone()),
                    (undone_stamp, undone.clone()),
                ],
            ),
            (
                &mut right,
                [(undone_stamp, undone), (applied_stamp, applied)],
            ),
        ] {
            for (at, event) in order {
                store
                    .merge(Snapshot {
                        cells: vec![],
                        verdicts: vec![(note(1), at, event)],
                        version: Version::new(),
                    })
                    .unwrap();
            }
        }
        assert_eq!(left.snapshot(), right.snapshot());
    }

    #[test]
    fn a_subject_with_no_user_facts_is_not_one_the_store_speaks_for() {
        let store = Store::new(device(1));
        assert!(!store.facts(&note(1)).any());
        let mut store = store;
        store.write(note(1), Relation::Parent(None)).unwrap();
        // Filing at the root is still the user saying something.
        assert!(store.facts(&note(1)).any());
    }

    #[test]
    fn deleted_subjects_are_left_out_of_the_facts_the_store_offers() {
        let mut store = Store::new(device(1));
        store.write(note(1), Relation::Deleted(true)).unwrap();
        store.write(note(2), Relation::State(State::Done)).unwrap();
        assert_eq!(
            store
                .all_facts()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec![note(2)]
        );
    }

    /// Undo puts back what stood there before, and for a fact nobody had
    /// written that is the unwritten claim rather than nothing at all.
    #[test]
    fn a_verdict_on_an_untouched_thing_records_what_it_read_as_holding() {
        let store = Store::new(device(1));
        let changes = verdict_changes(
            &note(1),
            &Verdict::Done,
            &|key| store.relation(&note(1), key).cloned(),
            None,
        )
        .unwrap();
        assert_eq!(changes[0].before, Some(Relation::State(State::Open)));
    }

    #[test]
    fn daemon_and_gui_round_trip_only_cells_since_the_peer_version() {
        let mut daemon = Store::new(device(1));
        let mut gui = Store::new(device(2));
        gui.write(note(1), Relation::CreatedAt(timestamp(10)))
            .unwrap();
        let daemon_version = daemon.version().clone();
        daemon.merge(gui.since(&daemon_version)).unwrap();
        let gui_version = gui.version().clone();
        daemon.write(note(1), Relation::State(State::Done)).unwrap();
        let delta = daemon.since(&gui_version);
        assert_eq!(delta.cells.len(), 1);
        gui.merge(delta).unwrap();
        assert_eq!(gui.snapshot(), daemon.snapshot());
    }

    #[test]
    fn two_guis_converge_through_one_daemon() {
        let mut daemon = Store::new(device(1));
        let mut first = Store::new(device(2));
        let mut second = Store::new(device(3));
        first
            .write(note(1), Relation::CreatedAt(timestamp(10)))
            .unwrap();
        daemon.merge(first.since(daemon.version())).unwrap();
        second.merge(daemon.since(second.version())).unwrap();
        first
            .write(note(1), Relation::Deadline(Some(timestamp(20))))
            .unwrap();
        second
            .write(note(1), Relation::DeferUntil(Some(timestamp(15))))
            .unwrap();
        daemon.merge(first.since(daemon.version())).unwrap();
        daemon.merge(second.since(daemon.version())).unwrap();
        first.merge(daemon.since(first.version())).unwrap();
        second.merge(daemon.since(second.version())).unwrap();
        assert_eq!(first.snapshot(), daemon.snapshot());
        assert_eq!(second.snapshot(), daemon.snapshot());
    }
}
