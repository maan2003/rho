use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use gpui::{AppContext as _, Context, Entity};
use language::{Buffer, BufferEvent, Capability};
use rho_agents::{Attention, HostId};
use rho_desk::cells::{
    BodySnapshot, CellMutation, CellWrite, DeviceId, Facts, Id, Project, Property, PropertyKey,
    SlackTs, SlackUnit, Snapshot, Stamp, State, Store, StoryPos, Timestamp, TimestampPrecision,
    Uuid, Verdict, VerdictEvent, Version,
};
use rho_ui_proto::ClientMessage;
use text::{BufferId, ReplicaId};

use crate::workspace::Workspace;

struct HostDeskCells {
    /// What the daemon has told us. A rejected mutation falls back to this.
    confirmed: Store,
    /// What the reader sees: `confirmed` plus every mutation still in
    /// flight, so a keypress shows before the round trip finishes.
    view: Store,
    /// Mutations sent and not yet visible in `confirmed`, oldest first.
    pending: Vec<CellMutation>,
    buffers: BTreeMap<Id, Entity<Buffer>>,
    /// The map as it stands, in the order it is drawn, and where each id
    /// sits in it. Kept rather than made: a delta patches the rows it
    /// names, and only a change of shape walks the facts again.
    nodes: Vec<DeskNode>,
    node_at: HashMap<Id, Vec<usize>>,
    /// Every note's title, so that naming a row costs a read of this map
    /// rather than a read of the note's rope. A title only changes when
    /// its buffer does, and `refresh_titles` is what notices.
    titles: Rc<HashMap<Id, String>>,
    /// The buffer version each cached title was read at.
    title_versions: HashMap<Id, clock::Global>,
    _subscriptions: Vec<gpui::Subscription>,
    /// What the sources say right now, recomputed by the workspace rather
    /// than stored: the registry's agents and the Slack mirror's units.
    sources: Sources,
    /// The text replica namespace the daemon assigned this connection.
    namespace: u16,
    /// A `DeskSync` is in flight; the daemon answers exactly one.
    syncing: bool,
    /// The newest frontier poked while a sync was in flight. A poke that
    /// races its response must not be dropped, so it is answered after.
    poked: Option<Version>,
}

impl HostDeskCells {
    /// The stamp a new mutation carries: past everything this GUI has
    /// observed, so it beats its own earlier writes and cannot jump more
    /// than one past the daemon's global maximum.
    fn next_stamp(&self, device: DeviceId) -> Stamp {
        let version = self
            .view
            .version()
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Stamp { device, version }
    }

    /// Replays the unconfirmed writes over the confirmed cells. Used when a
    /// rejection drops one from the middle of the queue.
    fn rebuild_view(&mut self, device: DeviceId) {
        let mut view = Store::new(device);
        // The confirmed store is this GUI's own merge; it cannot fail to
        // merge into an empty store of the same device.
        let _ = view.merge(self.confirmed.snapshot());
        for mutation in &self.pending {
            if let Err(error) = view.apply_mutation(mutation) {
                tracing::warn!(%error, "dropping an unreplayable Desk mutation");
            }
        }
        self.view = view;
    }

    /// Forgets the mutations the daemon has now told us about, which is what
    /// keeps the replay queue from growing for the life of the process.
    fn prune_pending(&mut self) {
        let confirmed = self.confirmed.version().clone();
        self.pending.retain(|mutation| {
            confirmed.get(&mutation.stamp.device).copied().unwrap_or(0) < mutation.stamp.version
        });
    }
}

/// What an agent's own system says about it. The store never holds any of
/// this; it is read from the registry every time a view is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSource {
    pub agent: rho_core::AgentId,
    /// Who asked for this agent. The store's `Parent` is the user's
    /// filing and beats it; this is where the agent came from.
    pub spawned_by: Option<rho_core::AgentId>,
    pub workdir: Option<camino::Utf8PathBuf>,
    /// One past the newest story event this client holds: the cursor a
    /// verdict on this agent writes.
    pub newest: StoryPos,
    /// A turn is running.
    pub turn_running: bool,
    /// Where the last turn ended badly, if nothing has happened since.
    pub errored: Option<StoryPos>,
    /// What the last finished turn says it asks of the user, and where it
    /// said so.
    pub wants: Option<(rho_ui_proto::mirror::AgentWant, StoryPos)>,
}

impl AgentSource {
    /// The story has told something that asks for the user. Whether it is
    /// still owed is the cursor's business, in [`agent_card`]; this only
    /// decides whether the agent is on the map for its own sake.
    pub fn wants_user(&self) -> bool {
        self.wants.is_some() || self.errored.is_some()
    }
}

/// What the Slack mirror says about a conversation or a followed thread.
/// All of it only ever rises, so a history page or a reconnect can add to
/// what a card knows and never take it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackSource {
    pub unit: SlackUnit,
    pub title: String,
    /// The newest message in the unit, whoever wrote it: what a `d` writes
    /// as the cursor.
    pub newest: SlackTs,
    /// The newest message from someone else that concerns the user. The
    /// card is open exactly while this is past the cursor.
    pub newest_from_other: Option<SlackTs>,
    /// Why Slack is asking for the reader here, or `None` when it is not.
    /// Decided in rho-slack against Slack's own read state, and the first
    /// of the two conditions a card has to meet.
    pub reason: Option<rho_slack::model::Attention>,
}

/// What the browser says about one open tab. A tab is never created in
/// the store, so everything about where it sits comes from here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageSource {
    pub page: rho_desk::PageId,
    /// The page the reader opened this tab from: a ctrl-click, or a link
    /// that asked for a new tab. `None` for a tab opened for its own sake.
    pub opened_from: Option<rho_desk::PageId>,
}

/// The source facts a view joins the store with. Recomputed by the
/// workspace whenever a source changes; never written anywhere.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sources {
    pub host: u64,
    agents: Vec<AgentSource>,
    slack: Vec<SlackSource>,
    pages: Vec<PageSource>,
    /// Where each source sits in its own list. The walk asks for one
    /// agent, one unit or one page per node it builds, and asking by
    /// scanning made a walk cost the nodes times the sources. The lists
    /// are private so these cannot drift from them.
    by_agent: HashMap<rho_core::AgentId, usize>,
    by_unit: HashMap<rho_desk::cells::SlackUnit, usize>,
    by_page: HashMap<rho_desk::PageId, usize>,
}

// How many source entries the walk has looked at. A lookup that scans
// costs the length of what it scans, so this counts entries examined and
// not calls made: the difference between the two is the whole question.
//
// Thread-local because the suite runs tests concurrently in one process: a
// shared counter reads as one test's work plus whatever its neighbours were
// doing, which is a number that cannot be wrong in any way you can see.
#[cfg(test)]
thread_local! {
    static SOURCE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn charge_scan(entries: usize) {
    SOURCE_SCANS.with(|scans| scans.set(scans.get() + entries));
}

#[cfg(not(test))]
fn charge_scan(_entries: usize) {}

#[cfg(test)]
pub(crate) fn take_source_scans() -> usize {
    SOURCE_SCANS.with(|scans| scans.replace(0))
}

impl Sources {
    pub fn new(
        host: u64,
        agents: Vec<AgentSource>,
        slack: Vec<SlackSource>,
        pages: Vec<PageSource>,
    ) -> Self {
        let by_agent = agents
            .iter()
            .enumerate()
            .map(|(at, source)| (source.agent, at))
            .collect();
        let by_unit = slack
            .iter()
            .enumerate()
            .map(|(at, source)| (source.unit.clone(), at))
            .collect();
        let by_page = pages
            .iter()
            .enumerate()
            .map(|(at, source)| (source.page, at))
            .collect();
        Self {
            host,
            agents,
            slack,
            pages,
            by_agent,
            by_unit,
            by_page,
        }
    }

    /// The same sources with Slack's replaced, for a test that supplies
    /// the timestamps no store fixture can hold.
    #[cfg(test)]
    pub fn with_slack(self, slack: Vec<SlackSource>) -> Self {
        Self::new(self.host, self.agents, slack, self.pages)
    }

    pub fn agents(&self) -> &[AgentSource] {
        &self.agents
    }

    pub fn slack(&self) -> &[SlackSource] {
        &self.slack
    }

    pub fn pages(&self) -> &[PageSource] {
        &self.pages
    }

    fn agent(&self, agent: rho_core::AgentId) -> Option<&AgentSource> {
        charge_scan(1);
        self.by_agent.get(&agent).map(|at| &self.agents[*at])
    }

    fn unit(&self, unit: &rho_desk::cells::SlackUnit) -> Option<&SlackSource> {
        charge_scan(1);
        self.by_unit.get(unit).map(|at| &self.slack[*at])
    }

    fn page(&self, page: rho_desk::PageId) -> Option<&PageSource> {
        charge_scan(1);
        self.by_page.get(&page).map(|at| &self.pages[*at])
    }
}

/// A thing as a view shows it: the user's facts, placed by the rules in
/// `STORE-DESIGN.md`. Nothing here is stored; changing a rule changes the
/// view and moves no cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeskNode {
    pub id: Id,
    /// Where it is shown: the user's filing, else the edge into its source
    /// context, else the root.
    pub parent: Option<Id>,
    /// The place this row hangs under. The same as `parent` for the row a
    /// thing has in its own place, and the label for the extra row every
    /// label it carries gives it: labels are a second axis, so the map is a
    /// DAG drawn as a tree and a thing in two places appears twice.
    pub under: Option<Id>,
    pub state: State,
    pub defer_until: Option<Timestamp>,
    pub deadline: Option<Timestamp>,
    pub pace_days: u32,
    pub labels: BTreeSet<Id>,
    pub name: Option<String>,
    pub created_at: Option<Timestamp>,
}

/// What a delta named, and whether the map can have changed shape.
/// Everything the GUI does to the desk arrives as one of these, and it is
/// what lets a verdict cost the cells it writes.
#[derive(Clone, Debug, Default)]
pub struct DeskDelta {
    /// The rows the delta named. Each is made again where it sits.
    pub touched: BTreeSet<Id>,
    /// A place, a label, a birth or a deletion: the set of rows or the
    /// order they are drawn in can differ, so the map is built again
    /// rather than patched. A verdict never sets this.
    pub shape: bool,
}

impl DeskDelta {
    /// A delta that says nothing about the map, for the paths that only
    /// move text.
    pub fn quiet() -> Self {
        Self::default()
    }

    /// The whole map may have moved: a load, a host reset, a source that
    /// changed who is on the desk at all.
    pub fn whole() -> Self {
        Self {
            touched: BTreeSet::new(),
            shape: true,
        }
    }

    pub fn is_quiet(&self) -> bool {
        !self.shape && self.touched.is_empty()
    }

    fn write(&mut self, id: &Id, property: &Property) {
        self.touched.insert(id.clone());
        // Where a row is drawn, what it carries, whether it exists at all
        // and how old it is are the four things the order is made of.
        self.shape |= matches!(
            property,
            Property::Parent(_)
                | Property::Labeled { .. }
                | Property::Deleted(_)
                | Property::CreatedAt(_)
        );
    }
}

impl DeskNode {
    pub fn is_note(&self) -> bool {
        matches!(self.id, Id::Note(_))
    }

    pub fn agent(&self) -> Option<rho_core::AgentId> {
        match &self.id {
            Id::Agent(agent) => Some(*agent),
            _ => None,
        }
    }

    pub fn page(&self) -> Option<rho_desk::PageId> {
        match &self.id {
            Id::Page(page) => Some(*page),
            _ => None,
        }
    }

    pub fn slack(&self) -> Option<&SlackUnit> {
        match &self.id {
            Id::Slack(unit) => Some(unit),
            _ => None,
        }
    }

    pub fn path(&self) -> Option<&camino::Utf8Path> {
        match &self.id {
            Id::File { path, .. } => Some(path.as_path()),
            _ => None,
        }
    }
}

/// What the join of a Slack unit's cursor and its mirror facts says: the
/// card's state, and whether a message arriving during a snooze has voided
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlackCard {
    state: State,
    voided: bool,
}

/// A Slack unit's card, derived and never stored. It is open exactly while
/// two things hold at once: Slack itself is asking about the unit, and
/// someone else has written past the cursor the user's last verdict left.
///
/// The first is the crate's answer and the flood fix — a channel with plain
/// unread traffic is in the conversation list with its count and is not a
/// card, and a mention already read elsewhere has stopped asking. The
/// second is the desk's own, and is the one comparison a replayed history
/// page cannot change.
fn slack_card(id: &Id, facts: &Facts, sources: &Sources) -> Option<SlackCard> {
    let Id::Slack(unit) = id else {
        return None;
    };
    let source = sources.unit(unit)?;
    let past = |cursor: Option<&SlackTs>| match (source.newest_from_other.as_ref(), cursor) {
        (Some(newest), Some(cursor)) => newest.is_after(cursor),
        (Some(_), None) => true,
        (None, _) => false,
    };
    Some(SlackCard {
        // A mute is the one verdict the cursor cannot express: the user said
        // "not this unit", not "not up to here", so nothing arriving past the
        // cursor reopens it. Opening the unit is what clears the state.
        state: match (
            facts.state,
            source.reason,
            past(facts.slack_handled_through.as_ref()),
        ) {
            (State::Muted, _, _) => State::Muted,
            // Slack is not badging this unit, so nothing here is owed: read
            // on the phone, or ordinary traffic in a channel nobody opted
            // into. The desk's cursor has nothing to say about a question
            // that is no longer being asked.
            (_, None, _) => State::Done,
            (_, Some(_), past) => match past {
                true => State::Open,
                false => State::Done,
            },
        },
        // The snooze recorded where the unit stood; anything from someone
        // else past that arrived while it was snoozed, and that is what
        // brings the card straight back.
        voided: facts.defer_until.is_some() && past(facts.slack_snoozed_at.as_ref()),
    })
}

/// What the join of an agent's story and the user's verdicts says: the
/// card's state, and how badly it wants the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentCard {
    pub state: State,
    pub attention: Attention,
}

/// An agent's card, derived and never stored. Like a Slack unit, it is open
/// exactly while the story has told something past the cursor the user's
/// last verdict left; unlike one, what is past the cursor also says how
/// loudly it asks, because the agent declares that itself.
///
/// This is the only place attention is decided. Every rail reads the answer
/// through the registry rather than working it out again.
pub fn agent_card(id: &Id, facts: &Facts, sources: &Sources) -> Option<AgentCard> {
    let Id::Agent(agent) = id else {
        return None;
    };
    let source = sources.agent(*agent)?;
    let cursor = facts.agent_handled_through.unwrap_or_default();
    let attention = rho_agents::attention(
        rho_agents::AttentionFacts {
            turn_running: source.turn_running,
            errored: source.errored.map(agent_pos),
            wants_at: source.wants.map(|(_, at)| agent_pos(at)),
        },
        verdict(facts),
    );
    Some(AgentCard {
        // Open exactly while the story has told something the user has not
        // dealt with, the same reading a Slack unit gets from its cursor.
        // Reading it off the attention level instead left `d` on a quiet
        // agent with nothing to change, so the card came straight back.
        state: match (facts.state, source.newest > cursor) {
            (State::Muted, _) => State::Muted,
            (_, true) => State::Open,
            (_, false) => State::Done,
        },
        attention,
    })
}

fn agent_pos(pos: StoryPos) -> rho_ui_proto::mirror::AgentPos {
    rho_ui_proto::mirror::AgentPos(pos.0)
}

/// The user's verdict on an agent, as the store holds it.
fn verdict(facts: &Facts) -> rho_agents::Verdict {
    rho_agents::Verdict {
        handled_through: agent_pos(facts.agent_handled_through.unwrap_or_default()),
        muted: facts.state == State::Muted,
    }
}

/// Whether `frontier` covers every device version in `poke`.
fn covers(frontier: &Version, poke: &Version) -> bool {
    poke.iter()
        .all(|(device, version)| frontier.get(device).copied().unwrap_or(0) >= *version)
}

/// The rows of one host's tree in the order they are drawn, the buffer
/// behind each row that has one, and the title of each. Three parts of
/// one answer: a screen needs all three to draw a row, and they are only
/// consistent together.
pub type TreeSource = (
    Vec<DeskNode>,
    BTreeMap<Id, Entity<Buffer>>,
    Rc<HashMap<Id, String>>,
);

/// The store, as the GUI reads and writes it. One interface: today it
/// talks to a daemon, and the wire carries cells rather than commands, so
/// a local store is the same shape.
pub struct DeskCells {
    device: DeviceId,
    next_buffer_id: u64,
    hosts: BTreeMap<HostId, HostDeskCells>,
}

impl DeskCells {
    pub fn new(device: DeviceId) -> Self {
        Self {
            device,
            next_buffer_id: 1,
            hosts: BTreeMap::new(),
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// The handshake, sent on connect and after every poke. `known` is what
    /// this GUI already holds, so the daemon answers with the difference.
    pub fn sync(&mut self, host: HostId) -> ClientMessage {
        let known = match self.hosts.get_mut(&host) {
            Some(desk) => {
                desk.syncing = true;
                desk.confirmed.version().clone()
            }
            None => Version::new(),
        };
        ClientMessage::DeskSync {
            device: self.device,
            known,
        }
    }

    /// The daemon's answer. Returns a further `DeskSync` when a poke
    /// arrived while this one was in flight and the answer does not already
    /// cover it.
    pub fn synced(
        &mut self,
        host: HostId,
        namespace: u16,
        delta: Snapshot,
        bodies: Vec<BodySnapshot>,
        cx: &mut Context<Workspace>,
    ) -> (Option<ClientMessage>, DeskDelta) {
        let frontier = delta.version.clone();
        let mut delta_ids = DeskDelta::default();
        for cell in &delta.cells {
            delta_ids.write(&cell.id, &cell.property);
        }
        for (id, _, _) in &delta.verdicts {
            // A verdict says what the user did about a thing, never where
            // it is drawn.
            delta_ids.touched.insert(id.clone());
        }
        let existing = self.hosts.contains_key(&host);
        if !existing {
            self.hosts.insert(
                host,
                HostDeskCells {
                    confirmed: Store::new(self.device),
                    view: Store::new(self.device),
                    pending: Vec::new(),
                    buffers: BTreeMap::new(),
                    nodes: Vec::new(),
                    node_at: HashMap::new(),
                    titles: Rc::default(),
                    title_versions: HashMap::new(),
                    _subscriptions: Vec::new(),
                    sources: Sources::default(),
                    namespace,
                    syncing: false,
                    poked: None,
                },
            );
        }
        if !existing {
            delta_ids.shape = true;
        }
        {
            let Some(desk) = self.hosts.get_mut(&host) else {
                return (None, DeskDelta::quiet());
            };
            desk.namespace = namespace;
            desk.syncing = false;
            if let Err(error) = desk.confirmed.merge(delta.clone()) {
                tracing::error!(%error, "Desk cell delta did not merge");
                return (None, DeskDelta::quiet());
            }
            if let Err(error) = desk.view.merge(delta) {
                tracing::error!(%error, "Desk cell delta did not merge into the view");
            }
            desk.prune_pending();
        }
        // The delta is already in both stores, so there is nothing to
        // replay: the pending writes it confirmed merged as themselves,
        // and the ones it did not are still in the view where they were
        // put. Only a rejection takes a write back out of the middle,
        // and that is the one path that replays.
        self.apply_to_map(host, &delta_ids);
        self.merge_bodies(host, bodies, cx);
        self.give_buffers(host, &delta_ids, cx);
        let again = match self.hosts.get_mut(&host).and_then(|desk| desk.poked.take()) {
            // The answer already carries everything the poke announced.
            Some(poke) if covers(&frontier, &poke) => None,
            Some(_) => Some(self.sync(host)),
            None => None,
        };
        (again, delta_ids)
    }

    /// `DeskCellsAvailable`: a poke, not a delta. One handshake is in flight
    /// at a time; a poke that arrives during one is answered after it.
    pub fn cells_available(&mut self, host: HostId, frontier: Version) -> Option<ClientMessage> {
        let desk = self.hosts.get_mut(&host)?;
        if covers(desk.confirmed.version(), &frontier) {
            return None;
        }
        if desk.syncing {
            desk.poked = Some(frontier);
            return None;
        }
        Some(self.sync(host))
    }

    /// The daemon lost our place in its event stream: start over.
    pub fn resync_required(&mut self, host: HostId) -> ClientMessage {
        if let Some(desk) = self.hosts.get_mut(&host) {
            desk.syncing = false;
            desk.poked = None;
        }
        self.sync(host)
    }

    pub fn mutation_accepted(&mut self, host: HostId, stamp: Stamp) {
        // Nothing to do but note it: the cells are already in the view and
        // the poke that follows brings them into `confirmed`.
        let _ = (host, stamp);
    }

    /// A rejected mutation never happened. The view falls back to the last
    /// merged cells, which is what the reader must see.
    pub fn mutation_rejected(&mut self, host: HostId, stamp: Stamp, cx: &mut Context<Workspace>) {
        let device = self.device;
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        desk.pending.retain(|mutation| mutation.stamp != stamp);
        desk.rebuild_view(device);
        // A write taken out of the middle can be the one that put a row
        // somewhere, so the map is built again rather than patched.
        self.rebuild_map(host);
        self.reconcile_buffers(host, cx);
    }

    /// What the sources say, for the join every view reads through. The
    /// workspace recomputes this from the registry and the Slack mirror;
    /// none of it is ever written to the store.
    pub fn set_sources(&mut self, host: HostId, sources: Sources) -> bool {
        let Some(desk) = self.hosts.get_mut(&host) else {
            return false;
        };
        if desk.sources == sources {
            return false;
        }
        desk.sources = sources;
        true
    }

    pub fn sources(&self, host: HostId) -> Option<&Sources> {
        Some(&self.hosts.get(&host)?.sources)
    }
}

impl DeskCells {
    /// Merges the handshake's body histories. A snapshot never replaces a
    /// newer operation that arrived on its own: the two are queued
    /// independently, so the merge is by operation, not by replacement.
    fn merge_bodies(
        &mut self,
        host: HostId,
        bodies: Vec<BodySnapshot>,
        cx: &mut Context<Workspace>,
    ) {
        for body in bodies {
            let operations = body
                .operations
                .iter()
                .filter_map(|operation| operation.to_text().ok())
                .map(language::Operation::Buffer)
                .collect::<Vec<_>>();
            match self
                .hosts
                .get(&host)
                .and_then(|desk| desk.buffers.get(&body.id))
            {
                Some(buffer) => {
                    let buffer = buffer.clone();
                    buffer.update(cx, |buffer, cx| buffer.apply_ops(operations, cx));
                }
                None => {
                    let buffer = self.new_note_buffer(host, body.id.clone(), operations, cx);
                    if let Some(desk) = self.hosts.get_mut(&host) {
                        desk.buffers.insert(body.id, buffer);
                    }
                }
            }
        }
    }

    /// A note's body, as an editor buffer whose local edits go back to the
    /// daemon as text operations.
    fn new_note_buffer(
        &mut self,
        host: HostId,
        id: Id,
        operations: Vec<language::Operation>,
        cx: &mut Context<Workspace>,
    ) -> Entity<Buffer> {
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        let namespace = self.hosts.get(&host).map_or(0, |desk| desk.namespace);
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::remote(
                buffer_id,
                ReplicaId::new(namespace),
                Capability::ReadWrite,
                "",
            );
            buffer.apply_ops(operations, cx);
            buffer
        });
        let subscription = watch_note_buffer(&buffer, host, id, cx);
        if let Some(desk) = self.hosts.get_mut(&host) {
            desk._subscriptions.push(subscription);
        }
        buffer
    }

    /// Everything that is not a note has its title derived from its source,
    /// so its buffer is local and read-only: nothing it holds is ever sent
    /// to the daemon.
    fn new_derived_buffer(&mut self, cx: &mut Context<Workspace>) -> Entity<Buffer> {
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        cx.new(|_| Buffer::remote(buffer_id, ReplicaId::new(0), Capability::ReadOnly, ""))
    }

    /// Gives the rows a delta named their buffers, and takes back the
    /// buffers of the rows it removed. This is the walk's replacement: a
    /// node gets its buffer when it appears, so nothing has to look at
    /// every node to find out that all of them already have one.
    pub fn give_buffers(&mut self, host: HostId, delta: &DeskDelta, cx: &mut Context<Workspace>) {
        if delta.is_quiet() {
            return;
        }
        // A shape that moved can have taken rows away as well as brought
        // them, and which ones is not in the delta: that is the one case
        // the reconcile still answers.
        if delta.shape {
            self.reconcile_buffers(host, cx);
            return;
        }
        let Some(desk) = self.hosts.get(&host) else {
            return;
        };
        let missing = delta
            .touched
            .iter()
            .filter(|id| desk.node_at.contains_key(*id) && !desk.buffers.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in missing {
            let buffer = match id {
                Id::Note(_) => self.new_note_buffer(host, id.clone(), Vec::new(), cx),
                _ => self.new_derived_buffer(cx),
            };
            if let Some(desk) = self.hosts.get_mut(&host) {
                desk.buffers.insert(id, buffer);
            }
        }
    }

    /// Gives every shown thing a buffer and drops the buffers of things
    /// that are gone. Notes get theirs from the daemon's body history;
    /// everything else gets an empty local one the dashboard fills with a
    /// derived title.
    pub fn reconcile_buffers(&mut self, host: HostId, cx: &mut Context<Workspace>) {
        let Some(desk) = self.hosts.get(&host) else {
            return;
        };
        let live = self
            .nodes(host)
            .iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        let stale = desk
            .buffers
            .keys()
            .filter(|id| !live.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        let missing = live
            .into_iter()
            .filter(|id| !desk.buffers.contains_key(id))
            .collect::<Vec<_>>();
        if let Some(desk) = self.hosts.get_mut(&host) {
            for id in stale {
                desk.buffers.remove(&id);
            }
        }
        for id in missing {
            let buffer = match id {
                Id::Note(_) => self.new_note_buffer(host, id.clone(), Vec::new(), cx),
                _ => self.new_derived_buffer(cx),
            };
            if let Some(desk) = self.hosts.get_mut(&host) {
                desk.buffers.insert(id, buffer);
            }
        }
    }

    /// A body operation from the daemon (another device, or this one echoed
    /// back). Applying an operation the buffer already has is a no-op.
    pub fn text_applied(
        &mut self,
        host: HostId,
        id: Id,
        operation: rho_desk::TextOperation,
        cx: &mut Context<Workspace>,
    ) {
        let Ok(operation) = operation.to_text() else {
            return;
        };
        let Some(buffer) = self
            .hosts
            .get(&host)
            .and_then(|desk| desk.buffers.get(&id))
            .cloned()
        else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.apply_ops([language::Operation::Buffer(operation)], cx)
        });
    }

    /// The map, as a rule over facts: everything that matters, each under
    /// the user's filing or the edge into its source context, parents
    /// before children and siblings oldest first.
    pub fn nodes(&self, host: HostId) -> &[DeskNode] {
        self.hosts
            .get(&host)
            .map(|desk| desk.nodes.as_slice())
            .unwrap_or_default()
    }

    /// Brings the kept map up to a delta. The rows the delta names are
    /// made again where they sit, which costs the cells the delta carried;
    /// only a shape that moved builds the map again.
    fn apply_to_map(&mut self, host: HostId, delta: &DeskDelta) {
        if delta.is_quiet() {
            return;
        }
        let unseen = self.hosts.get(&host).is_none_or(|desk| {
            desk.nodes.is_empty()
                || delta
                    .touched
                    .iter()
                    .any(|id| !desk.node_at.contains_key(id))
        });
        if delta.shape || unseen {
            self.rebuild_map(host);
            return;
        }
        let made = delta
            .touched
            .iter()
            .filter_map(|id| Some((id.clone(), self.node_facts(host, id)?)))
            .collect::<Vec<_>>();
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        for (id, fresh) in made {
            // A thing filed under a label is drawn in both places, and both
            // rows are of the same facts.
            for at in desk.node_at.get(&id).into_iter().flatten() {
                let node = &mut desk.nodes[*at];
                node.state = fresh.state;
                node.defer_until = fresh.defer_until;
                node.deadline = fresh.deadline;
                node.pace_days = fresh.pace_days;
                node.name = fresh.name.clone();
            }
        }
    }

    /// The map from scratch, and where each id landed in it.
    pub fn rebuild_map(&mut self, host: HostId) {
        let nodes = self.compute_nodes(host);
        let mut node_at: HashMap<Id, Vec<usize>> = HashMap::with_capacity(nodes.len());
        for (at, node) in nodes.iter().enumerate() {
            node_at.entry(node.id.clone()).or_default().push(at);
        }
        if let Some(desk) = self.hosts.get_mut(&host) {
            desk.nodes = nodes;
            desk.node_at = node_at;
        }
    }

    /// What one row says of itself now: the fields a delta that kept the
    /// shape can have moved. Where it hangs is not among them.
    fn node_facts(&self, host: HostId, id: &Id) -> Option<DeskNode> {
        let desk = self.hosts.get(&host)?;
        let fact = desk.view.facts(id);
        let slack = slack_card(id, &fact, &desk.sources);
        let agent = agent_card(id, &fact, &desk.sources);
        Some(DeskNode {
            id: id.clone(),
            under: None,
            parent: None,
            state: match (slack, agent) {
                (Some(card), _) => card.state,
                (_, Some(card)) => card.state,
                _ => fact.state,
            },
            defer_until: match slack {
                Some(card) if card.voided => None,
                _ => fact.defer_until,
            },
            deadline: fact.deadline,
            pace_days: fact.pace_days,
            labels: fact.labels.clone(),
            name: fact.name.clone(),
            created_at: fact.created_at,
        })
    }

    /// Builds the map again from every fact and every source. This is the
    /// walk: a load and a change of shape are what may call it.
    fn compute_nodes(&self, host: HostId) -> Vec<DeskNode> {
        let Some(desk) = self.hosts.get(&host) else {
            return Vec::new();
        };
        let sources = &desk.sources;
        let mut facts = desk
            .view
            .all_facts()
            .into_iter()
            .collect::<BTreeMap<Id, Facts>>();
        // Open by its source: a waiting agent and a Slack unit with new
        // traffic matter even when the user has never said anything.
        for agent in sources.agents() {
            if agent.wants_user() {
                facts.entry(Id::Agent(agent.agent)).or_default();
            }
        }
        for unit in sources.slack() {
            facts.entry(Id::Slack(unit.unit.clone())).or_default();
        }
        // A tab the reader opened from a page is where they deliberately
        // went, so it is on the map even before they say anything about
        // it. A tab opened for its own sake is not: that is the "not every
        // tab" line. The origin comes along whether it matters on its own
        // or not, or the burst would draw at the root instead of under the
        // page it came from.
        let mut origins = Vec::new();
        for page in sources.pages() {
            if let Some(origin) = page.opened_from {
                facts.entry(Id::Page(page.page)).or_default();
                origins.push(origin);
            }
        }
        let mut walked = std::collections::BTreeSet::new();
        while let Some(origin) = origins.pop() {
            if !walked.insert(origin) {
                continue;
            }
            facts.entry(Id::Page(origin)).or_default();
            if let Some(next) = sources.page(origin).and_then(|source| source.opened_from) {
                origins.push(next);
            }
        }
        let mut nodes = BTreeMap::new();
        for (id, fact) in &facts {
            // Deleting one thing does not delete what was filed under it:
            // the row whose place has gone is shown at the root, and undoing
            // the one cell puts the hierarchy back.
            let parent = place(id, fact, sources).filter(|parent| !desk.view.facts(parent).deleted);
            let slack = slack_card(id, fact, sources);
            let agent = agent_card(id, fact, sources);
            nodes.insert(
                id.clone(),
                DeskNode {
                    id: id.clone(),
                    under: parent.clone(),
                    parent,
                    state: match (slack, agent) {
                        (Some(card), _) => card.state,
                        (_, Some(card)) => card.state,
                        _ => fact.state,
                    },
                    defer_until: match slack {
                        Some(card) if card.voided => None,
                        _ => fact.defer_until,
                    },
                    deadline: fact.deadline,
                    pace_days: fact.pace_days,
                    labels: fact.labels.clone(),
                    name: fact.name.clone(),
                    created_at: fact.created_at,
                },
            );
        }
        // A place-ancestor is shown even when nothing was ever said about
        // it: an agent's host, a thread's channel.
        let mut pending = nodes
            .values()
            .filter_map(|node| node.parent.clone())
            .collect::<Vec<_>>();
        while let Some(id) = pending.pop() {
            if nodes.contains_key(&id) {
                continue;
            }
            let fact = desk.view.facts(&id);
            let parent = place(&id, &fact, sources);
            if let Some(parent) = &parent {
                pending.push(parent.clone());
            }
            nodes.insert(
                id.clone(),
                DeskNode {
                    id,
                    under: parent.clone(),
                    parent,
                    state: fact.state,
                    defer_until: fact.defer_until,
                    deadline: fact.deadline,
                    pace_days: fact.pace_days,
                    labels: fact.labels,
                    name: fact.name,
                    created_at: fact.created_at,
                },
            );
        }
        order(nodes)
    }

    /// What every agent of this host is asking of the user, derived from
    /// the store and the mirror in [`agent_card`]. The rails read it from
    /// the registry; this is where the registry gets it.
    /// The user's verdict on each agent of this host, for the registry
    /// to derive attention from.
    pub fn agent_verdicts(&self, host: HostId) -> Vec<(rho_core::AgentId, rho_agents::Verdict)> {
        let Some(desk) = self.hosts.get(&host) else {
            return Vec::new();
        };
        desk.sources
            .agents
            .iter()
            .map(|source| {
                let facts = desk.view.facts(&Id::Agent(source.agent));
                (source.agent, verdict(&facts))
            })
            .collect()
    }

    /// How the user filed each agent of this host: muted, and the names of
    /// the labels on it. The registry hides and groups by this.
    pub fn agent_filing(&self, host: HostId) -> Vec<(rho_core::AgentId, bool, Vec<String>)> {
        let Some(desk) = self.hosts.get(&host) else {
            return Vec::new();
        };
        desk.view
            .all_facts()
            .into_iter()
            .filter_map(|(id, facts)| {
                let Id::Agent(agent) = id else {
                    return None;
                };
                let labels = facts
                    .labels
                    .iter()
                    .filter_map(|label| desk.view.facts(label).name)
                    .collect();
                Some((agent, facts.state == State::Muted, labels))
            })
            .collect()
    }

    pub fn node(&self, host: HostId, id: &Id) -> Option<DeskNode> {
        self.nodes(host).iter().find(|node| &node.id == id).cloned()
    }

    /// The facts the store holds about one thing, with nothing derived.
    pub fn facts(&self, host: HostId, id: &Id) -> Option<Facts> {
        Some(self.hosts.get(&host)?.view.facts(id))
    }

    /// The nodes and buffers the dashboard renders.
    /// Rereads the titles of the notes whose bodies have moved since the
    /// last pass, and leaves the rest alone. A title is the note's first
    /// line, so the only way it changes is an edit, and comparing versions
    /// costs nothing next to walking a rope.
    fn refresh_titles(&mut self, host: HostId, cx: &gpui::App) {
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        let mut changed = desk.title_versions.len() != desk.buffers.len();
        let mut titles = HashMap::with_capacity(desk.buffers.len());
        let mut versions = HashMap::with_capacity(desk.buffers.len());
        for (id, buffer) in &desk.buffers {
            if !matches!(id, Id::Note(_)) {
                continue;
            }
            let version = buffer.read(cx).version();
            let title = match desk.title_versions.get(id) {
                Some(seen) if *seen == version => desk.titles.get(id).cloned(),
                _ => None,
            };
            let title = match title {
                Some(title) => title,
                None => {
                    changed = true;
                    crate::dashboard::note_title(&buffer.read(cx).text()).to_owned()
                }
            };
            titles.insert(id.clone(), title);
            versions.insert(id.clone(), version);
        }
        if changed || titles.len() != desk.titles.len() {
            desk.titles = Rc::new(titles);
            desk.title_versions = versions;
        }
    }

    /// Every note's title, refreshed. Comparing buffer versions is what
    /// makes asking cheap, so the dealer can ask on every sync: only the
    /// notes whose bodies moved are reread.
    ///
    /// Machine rows are not in here. Their titles are derived from live
    /// metadata, and nothing that ranks or files a card needs them: a
    /// breadcrumb is made of notes.
    pub fn note_titles(&mut self, host: HostId, cx: &gpui::App) -> Option<Rc<HashMap<Id, String>>> {
        self.refresh_titles(host, cx);
        Some(self.hosts.get(&host)?.titles.clone())
    }

    /// What a screen is handed to draw one host's tree.
    pub fn tree_source(&mut self, host: HostId, cx: &gpui::App) -> Option<TreeSource> {
        self.refresh_titles(host, cx);
        let nodes = self.nodes(host);
        let desk = self.hosts.get(&host)?;
        Some((nodes.to_vec(), desk.buffers.clone(), desk.titles.clone()))
    }

    /// Every host whose cells this client holds. The finder asks the store
    /// what there is rather than asking a screen what it drew, so it needs
    /// the hosts without going through one.
    pub fn hosts(&self) -> impl Iterator<Item = HostId> + '_ {
        self.hosts.keys().copied()
    }

    pub fn buffer(&self, host: HostId, id: &Id) -> Option<&Entity<Buffer>> {
        self.hosts.get(&host)?.buffers.get(id)
    }

    /// True while the host has answered a handshake, so callers can tell an
    /// empty desk from one that has not arrived yet.
    pub fn is_synced(&self, host: HostId) -> bool {
        self.hosts.contains_key(&host)
    }

    /// Sends a mutation and shows it at once. The daemon's answer either
    /// confirms it or takes it back.
    /// A write this GUI makes. The view takes it at once and the map with
    /// it, so a verdict shows before the round trip and costs its own
    /// cells; the delta goes back to the caller, which is what the map and
    /// the dealer are brought up on.
    pub fn apply(
        &mut self,
        host: HostId,
        writes: Vec<CellWrite>,
        verdict: Option<(Id, VerdictEvent)>,
    ) -> Option<(ClientMessage, DeskDelta)> {
        let device = self.device;
        let desk = self.hosts.get_mut(&host)?;
        if writes.is_empty() {
            return None;
        }
        let stamp = desk.next_stamp(device);
        let verdict = verdict.map(|(id, event)| {
            let event = match event {
                VerdictEvent::Applied {
                    verdict, changes, ..
                } => VerdictEvent::Applied {
                    verdict,
                    at: stamp,
                    changes,
                },
                undone => undone,
            };
            (id, event)
        });
        let mutation = CellMutation {
            stamp,
            writes,
            verdict,
        };
        if let Err(error) = desk.view.apply_mutation(&mutation) {
            tracing::error!(%error, "refusing to send an invalid Desk mutation");
            return None;
        }
        desk.pending.push(mutation.clone());
        let mut delta = DeskDelta::default();
        for write in &mutation.writes {
            delta.write(&write.id, &write.property);
        }
        if let Some((id, _)) = &mutation.verdict {
            delta.touched.insert(id.clone());
        }
        self.apply_to_map(host, &delta);
        Some((ClientMessage::DeskMutationApply { mutation }, delta))
    }

    /// Rho mints ids for notes and labels and for nothing else.
    pub fn new_note_id(&mut self) -> Id {
        Id::Note(Uuid::random())
    }

    pub fn namespace(&self, host: HostId) -> Option<u16> {
        Some(self.hosts.get(&host)?.namespace)
    }

    pub fn property(&self, host: HostId, id: &Id, key: &PropertyKey) -> Option<Property> {
        self.hosts.get(&host)?.view.property(id, key).cloned()
    }

    pub fn verdict_event(&self, host: HostId, id: &Id, stamp: Stamp) -> Option<VerdictEvent> {
        self.hosts
            .get(&host)?
            .view
            .verdict_event(id, stamp)
            .cloned()
    }
}

/// Where a thing is shown: the user's filing when they made one, else the
/// edge into the context its own source knows, else the root.
fn place(id: &Id, facts: &Facts, sources: &Sources) -> Option<Id> {
    if facts.filed {
        return facts.parent.clone();
    }
    match id {
        Id::Agent(agent) => match sources.agent(*agent) {
            Some(source) => source
                .spawned_by
                .map(Id::Agent)
                .or(Some(Id::Host(sources.host))),
            None => None,
        },
        // A tab sits under the page it was opened from, until the user
        // files it somewhere. Filing the origin carries the burst with it,
        // because the children's place still derives from the origin.
        Id::Page(page) => sources
            .page(*page)
            .and_then(|source| source.opened_from)
            .map(Id::Page),
        // A followed thread sits in its conversation; the conversation is
        // at the root until a workspace is a thing the store can name.
        Id::Slack(unit) if unit.thread.is_some() => Some(Id::Slack(SlackUnit {
            workspace: unit.workspace.clone(),
            channel: unit.channel.clone(),
            thread: None,
        })),
        _ => None,
    }
}

/// Parents before children, siblings oldest first, and a parent chain that
/// never reaches the root shown at the root.
fn order(nodes: BTreeMap<Id, DeskNode>) -> Vec<DeskNode> {
    let mut children: BTreeMap<Option<Id>, Vec<&DeskNode>> = BTreeMap::new();
    for node in nodes.values() {
        let parent = node
            .parent
            .clone()
            .filter(|parent| nodes.contains_key(parent) && reaches_root(&nodes, node));
        children.entry(parent).or_default().push(node);
    }
    // The second axis. A label lists what carries it, under the label's own
    // row, wherever that row is.
    let mut members: BTreeMap<Id, Vec<&DeskNode>> = BTreeMap::new();
    for node in nodes.values() {
        for label in &node.labels {
            if nodes.contains_key(label) && label != &node.id {
                members.entry(label.clone()).or_default().push(node);
            }
        }
    }
    let by_age = |left: &&DeskNode, right: &&DeskNode| {
        (left.created_at, &left.id).cmp(&(right.created_at, &right.id))
    };
    for row in children.values_mut() {
        row.sort_by(by_age);
    }
    for row in members.values_mut() {
        row.sort_by(by_age);
    }
    let mut ordered = Vec::new();
    // One row per place a thing is in. A label filed under something it
    // labels would otherwise walk forever; a place is only ever drawn once,
    // which ends it without a depth limit.
    let mut drawn: std::collections::HashSet<(Option<Id>, Id)> = std::collections::HashSet::new();
    // The root is what is nowhere else: a thing filed under a label is
    // drawn there, so it does not line up at the top as well. A label that
    // hangs under the very thing it labels is the exception, since dropping
    // the root row would leave nothing to reach either of them from.
    let filed = |node: &DeskNode| {
        node.labels.iter().any(|label| {
            nodes.contains_key(label) && label != &node.id && !hangs_under(&nodes, label, &node.id)
        })
    };
    let mut stack = children
        .get(&None)
        .map(|roots| {
            roots
                .iter()
                .rev()
                .filter(|node| !filed(node))
                .map(|node| (None, *node))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    while let Some((under, node)) = stack.pop() {
        if !drawn.insert((under.clone(), node.id.clone())) {
            continue;
        }
        ordered.push(DeskNode {
            under,
            ..node.clone()
        });
        if let Some(row) = members.get(&node.id) {
            stack.extend(
                row.iter()
                    .rev()
                    .map(|member| (Some(node.id.clone()), *member)),
            );
        }
        // A thing carries its subtree wherever it is drawn: filing the
        // origin of a burst under a label takes the burst with it.
        if let Some(row) = children.get(&Some(node.id.clone())) {
            stack.extend(
                row.iter()
                    .rev()
                    .map(|child| (Some(node.id.clone()), *child)),
            );
        }
    }
    ordered
}

/// How far an ancestry walk goes before it decides it is in a cycle.
const MAX_ANCESTRY: usize = 256;

/// Whether `id` hangs somewhere under `ancestor`. A label filed under the
/// thing it labels is only reachable through that thing, which is what keeps
/// the thing's root row.
fn hangs_under(nodes: &BTreeMap<Id, DeskNode>, id: &Id, ancestor: &Id) -> bool {
    let mut cursor = nodes.get(id).and_then(|node| node.parent.clone());
    for _ in 0..MAX_ANCESTRY {
        let Some(current) = cursor else {
            return false;
        };
        if &current == ancestor {
            return true;
        }
        cursor = nodes.get(&current).and_then(|node| node.parent.clone());
    }
    false
}

fn reaches_root(nodes: &BTreeMap<Id, DeskNode>, node: &DeskNode) -> bool {
    let mut cursor = node.parent.clone();
    for _ in 0..MAX_ANCESTRY {
        let Some(id) = cursor else {
            return true;
        };
        if id == node.id {
            return false;
        }
        match nodes.get(&id) {
            Some(parent) => cursor = parent.parent.clone(),
            None => return true,
        }
    }
    false
}

/// A yanked subtree, ready to be pasted as fresh notes. Only notes are
/// captured: everything else exists because its source says so.
#[derive(Clone, Debug, Default)]
pub struct DeskCapture {
    /// Parents come before their children, so paste can map old ids to new
    /// ones in one pass.
    pub nodes: Vec<DeskCaptureNode>,
}

#[derive(Clone, Debug)]
pub struct DeskCaptureNode {
    pub id: Id,
    pub parent: Option<Id>,
    pub text: String,
}

/// Structure verbs, as cell writes. Each returns the writes a verb needs;
/// the caller sends them through [`DeskCells::apply`] so one keypress is one
/// mutation.
impl DeskCells {
    /// A new note under `parent`.
    /// The label a path names, minting what is missing on the way down:
    /// `rho/agent` is the label `agent` under the label `rho`. Names are
    /// matched without case so `rho` and `Rho` cannot become two labels,
    /// and a new one keeps the case the user typed. The id never reaches
    /// the user; the path is the whole interface.
    pub fn label_path_writes(&mut self, host: HostId, path: &str) -> Option<(Id, Vec<CellWrite>)> {
        let names = path
            .split('/')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if names.is_empty() {
            return None;
        }
        let nodes = self.nodes(host);
        let mut writes = Vec::new();
        let mut parent: Option<Id> = None;
        for name in names {
            let existing = nodes.iter().find(|node| {
                matches!(node.id, Id::Label(_))
                    && node.parent == parent
                    && node
                        .name
                        .as_ref()
                        .is_some_and(|written| written.eq_ignore_ascii_case(name))
            });
            let id = match existing {
                Some(node) => node.id.clone(),
                None => {
                    let id = Id::Label(Uuid::random());
                    writes.push(CellWrite {
                        id: id.clone(),
                        property: Property::Name(name.to_owned()),
                    });
                    writes.push(CellWrite {
                        id: id.clone(),
                        property: Property::Parent(parent.clone()),
                    });
                    writes.push(CellWrite {
                        id: id.clone(),
                        property: Property::CreatedAt(now_timestamp()),
                    });
                    id
                }
            };
            parent = Some(id);
        }
        parent.map(|id| (id, writes))
    }

    /// `w`: the label the path names stands for this workdir, minted if
    /// the path is new. A registered project is exactly this.
    pub fn project_writes(
        &mut self,
        host: HostId,
        path: &str,
        project: Option<Project>,
    ) -> Option<Vec<CellWrite>> {
        let (label, mut writes) = self.label_path_writes(host, path)?;
        writes.push(CellWrite {
            id: label,
            property: Property::Project(project),
        });
        Some(writes)
    }

    /// Every label the store holds, as the path a person would type. What
    /// the picker completes over, and what a label row is called.
    pub fn label_paths(&self, host: HostId) -> Vec<(Id, String)> {
        let nodes = self.nodes(host);
        let name_of = |id: &Id| {
            nodes
                .iter()
                .find(|node| &node.id == id)
                .and_then(|node| node.name.clone())
        };
        let mut paths = Vec::new();
        for node in nodes.iter().filter(|node| matches!(node.id, Id::Label(_))) {
            let Some(name) = node.name.clone() else {
                continue;
            };
            let mut segments = vec![name];
            let mut parent = node.parent.clone();
            // A label nested under something that is not a label is named by
            // its own name alone: the path is the label axis, not the place.
            while let Some(id) = parent.filter(|id| matches!(id, Id::Label(_))) {
                match name_of(&id) {
                    Some(name) => segments.push(name),
                    None => break,
                }
                parent = nodes
                    .iter()
                    .find(|node| node.id == id)
                    .and_then(|node| node.parent.clone());
            }
            segments.reverse();
            paths.push((node.id.clone(), segments.join("/")));
        }
        paths.sort_by(|left, right| left.1.cmp(&right.1));
        paths.dedup_by(|left, right| left.1 == right.1);
        paths
    }

    /// `f`: the thing carries the label the path names, or stops carrying
    /// it when it already does. The label is minted if the path is new, in
    /// the same mutation, so a label never exists without something on it.
    pub fn label_writes(
        &mut self,
        host: HostId,
        id: &Id,
        path: &str,
    ) -> Option<(Vec<CellWrite>, (Id, VerdictEvent))> {
        let (label, mut writes) = self.label_path_writes(host, path)?;
        let present = !self.facts(host, id)?.labels.contains(&label);
        let verdict = Verdict::Label {
            label: label.clone(),
            present,
        };
        let view = &self.hosts.get(&host)?.view;
        let changes = rho_desk::cells::verdict_changes(
            id,
            &verdict,
            &|key| view.property(id, key).cloned(),
            None,
            None,
            None,
        )
        .ok()?;
        writes.extend(changes.iter().filter_map(|change| {
            Some(CellWrite {
                id: change.id.clone(),
                property: change.after.clone()?,
            })
        }));
        Some((
            writes,
            (
                id.clone(),
                VerdictEvent::Applied {
                    verdict,
                    at: Stamp {
                        device: self.device,
                        version: 0,
                    },
                    changes,
                },
            ),
        ))
    }

    pub fn create_note_writes(
        &mut self,
        _host: HostId,
        parent: Option<Id>,
    ) -> Option<(Id, Vec<CellWrite>)> {
        let id = self.new_note_id();
        Some((id.clone(), create_note_writes(id, parent, now_timestamp())))
    }

    /// The note the cursor sits on gains a sibling or a child. Sibling order
    /// is `(CreatedAt, Id)` and `CreatedAt` cannot be rewritten, so a new
    /// row always lands after the existing siblings.
    pub fn new_note_writes(
        &mut self,
        host: HostId,
        relative: &Id,
        child: bool,
    ) -> Option<(Id, Vec<CellWrite>)> {
        let node = self.node(host, relative)?;
        let parent = if child {
            Some(relative.clone())
        } else {
            node.parent
        };
        self.create_note_writes(host, parent)
    }

    /// Deletes exactly one thing. Live descendants are not tombstoned: the
    /// view roots any whose parent chain now crosses a deleted node, and
    /// undoing this one cell puts the hierarchy back.
    pub fn delete_writes(&self, id: Id) -> Vec<CellWrite> {
        vec![CellWrite {
            id,
            property: Property::Deleted(true),
        }]
    }

    /// Promote or demote: the row keeps its identity and changes parent.
    /// Demoting adopts the previous sibling, promoting joins the grandparent.
    pub fn structure_move_writes(
        &self,
        host: HostId,
        id: &Id,
        demote: bool,
    ) -> Option<Vec<CellWrite>> {
        let nodes = self.nodes(host);
        let node = nodes.iter().find(|node| &node.id == id)?;
        let parent = if demote {
            let previous = nodes
                .iter()
                .filter(|other| other.parent == node.parent && &other.id != id)
                .rfind(|other| (other.created_at, &other.id) < (node.created_at, &node.id))?;
            Some(previous.id.clone())
        } else {
            let parent = nodes
                .iter()
                .find(|other| Some(&other.id) == node.parent.as_ref())?;
            parent.parent.clone()
        };
        if parent == node.parent {
            return None;
        }
        Some(vec![parent_write(id.clone(), parent)])
    }

    /// The row a deleted or emptied one hands the cursor to: the previous
    /// visible row in view order.
    pub fn row_above(&self, host: HostId, id: &Id) -> Option<Id> {
        let nodes = self.nodes(host);
        let index = nodes.iter().position(|node| &node.id == id)?;
        nodes.get(index.checked_sub(1)?).map(|node| node.id.clone())
    }

    /// Where the cursor lands when a row is deleted: the row above, or the
    /// first row below that does not hang off it. Without this a delete at
    /// the very top leaves the cursor inside the removed excerpt, and the
    /// next structure verb has no row to work from.
    pub fn row_after_delete(&self, host: HostId, id: &Id) -> Option<Id> {
        let nodes = self.nodes(host);
        let index = nodes.iter().position(|node| &node.id == id)?;
        if let Some(above) = index.checked_sub(1).and_then(|above| nodes.get(above)) {
            return Some(above.id.clone());
        }
        let descends = |mut candidate: Option<Id>| {
            for _ in 0..MAX_ANCESTRY {
                let Some(current) = candidate else {
                    return false;
                };
                if &current == id {
                    return true;
                }
                candidate = nodes
                    .iter()
                    .find(|node| node.id == current)
                    .and_then(|node| node.parent.clone());
            }
            false
        };
        nodes
            .get(index + 1..)?
            .iter()
            .find(|node| !descends(Some(node.id.clone())))
            .map(|node| node.id.clone())
    }

    /// True while any shown row still calls this thing its parent.
    pub fn has_children(&self, host: HostId, id: &Id) -> bool {
        self.nodes(host)
            .iter()
            .any(|node| node.parent.as_ref() == Some(id))
    }

    /// The workdir a thing is staffed from: the agent's own, from the
    /// registry, or a `File` filed under it.
    pub fn file_path(&self, host: HostId, id: &Id) -> Option<camino::Utf8PathBuf> {
        if let Id::File { path, .. } = id {
            return Some(path.clone());
        }
        let desk = self.hosts.get(&host)?;
        if let Id::Agent(agent) = id
            && let Some(workdir) = desk
                .sources
                .agent(*agent)
                .and_then(|source| source.workdir.clone())
        {
            return Some(workdir);
        }
        self.nodes(host).iter().find_map(|node| {
            (node.parent.as_ref() == Some(id))
                .then(|| node.path().map(ToOwned::to_owned))
                .flatten()
        })
    }

    /// The workdir a new thing filed under `id` inherits: the thing's own
    /// file, else the nearest ancestor with one. A cycle is shown at the
    /// root rather than repaired, so the walk stops at the depth no real
    /// tree reaches.
    pub fn inherited_file_path(&self, host: HostId, id: &Id) -> Option<camino::Utf8PathBuf> {
        self.inherited_workdir(host, id).map(|project| project.path)
    }

    /// The workdir a new thing under `id` inherits, walking the same
    /// ancestry: the thing's own file, then the projects of the labels it
    /// carries, then its parent's. A label with a project is what a
    /// project is, so a thing made in one is made in that workdir.
    pub fn inherited_workdir(&self, host: HostId, id: &Id) -> Option<Project> {
        let nodes = self.nodes(host);
        let seed = self.hosts.get(&host).map(|desk| desk.sources.host);
        let mut cursor = Some(id.clone());
        for _ in 0..MAX_ANCESTRY {
            let id = cursor?;
            if let Some(path) = self.file_path(host, &id) {
                return Some(Project {
                    host: seed.unwrap_or_default(),
                    path,
                });
            }
            let node = nodes.iter().find(|node| node.id == id)?;
            if let Some(project) = self.project(host, &id) {
                return Some(project);
            }
            if let Some(project) = node
                .labels
                .iter()
                .find_map(|label| self.project(host, label))
            {
                return Some(project);
            }
            cursor = node.parent.clone();
        }
        None
    }

    /// Every label that stands for a workdir, by name. This is what a
    /// registered project is now: a label carrying a `Project`.
    pub fn projects(&self, host: HostId) -> Vec<(String, Project)> {
        let Some(desk) = self.hosts.get(&host) else {
            return Vec::new();
        };
        let mut projects = desk
            .view
            .all_facts()
            .into_iter()
            .filter(|(id, _)| matches!(id, Id::Label(_)))
            .filter_map(|(_, facts)| Some((facts.name?, facts.project?)))
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.0.cmp(&right.0));
        projects
    }

    /// The workdir a label stands for, if it stands for one.
    pub fn project(&self, host: HostId, id: &Id) -> Option<Project> {
        self.facts(host, id)?.project
    }

    /// The agent that owns an area: the thing itself when it is an agent,
    /// else the nearest ancestor that is one.
    pub fn nearest_agent(&self, host: HostId, id: &Id) -> Option<rho_core::AgentId> {
        let nodes = self.nodes(host);
        let mut cursor = Some(id.clone());
        for _ in 0..MAX_ANCESTRY {
            let id = cursor?;
            let node = nodes.iter().find(|node| node.id == id)?;
            if let Some(agent) = node.agent() {
                return Some(agent);
            }
            cursor = node.parent.clone();
        }
        None
    }

    /// Every note in the subtree, parents first, with its text.
    pub fn capture(&self, host: HostId, root: &Id, cx: &gpui::App) -> Option<DeskCapture> {
        let desk = self.hosts.get(&host)?;
        let mut kept: BTreeSet<Id> = BTreeSet::new();
        let mut captured = Vec::new();
        for node in self.nodes(host) {
            let inside = &node.id == root
                || node
                    .parent
                    .as_ref()
                    .is_some_and(|parent| kept.contains(parent));
            if !inside || !node.is_note() {
                continue;
            }
            kept.insert(node.id.clone());
            captured.push(DeskCaptureNode {
                text: desk
                    .buffers
                    .get(&node.id)
                    .map(|buffer| buffer.read(cx).text())
                    .unwrap_or_default(),
                id: node.id.clone(),
                parent: node.parent.clone(),
            });
        }
        (!captured.is_empty()).then_some(DeskCapture { nodes: captured })
    }

    /// Re-creates a captured subtree under the cursor's parent. Returns the
    /// new root, the creation writes, and the text each new note wants once
    /// the daemon has accepted them.
    #[allow(clippy::type_complexity)]
    pub fn paste_writes(
        &mut self,
        host: HostId,
        relative: &Id,
        capture: &DeskCapture,
    ) -> Option<(Id, Vec<CellWrite>, Vec<(Id, String)>)> {
        let target = self.node(host, relative)?;
        let root_source = capture.nodes.first()?.id.clone();
        let created_at = now_timestamp();
        let mut mapping: BTreeMap<Id, Id> = BTreeMap::new();
        let mut writes = Vec::new();
        let mut texts = Vec::new();
        for source in &capture.nodes {
            let id = self.new_note_id();
            let parent = if source.id == root_source {
                target.parent.clone()
            } else {
                source
                    .parent
                    .as_ref()
                    .and_then(|parent| mapping.get(parent).cloned())
            };
            mapping.insert(source.id.clone(), id.clone());
            writes.extend(create_note_writes(id.clone(), parent, created_at));
            if !source.text.is_empty() {
                texts.push((id, source.text.clone()));
            }
        }
        let root = mapping.get(&root_source).cloned()?;
        Some((root, writes, texts))
    }

    /// The writes that put back whatever these writes are about to replace.
    /// A property with no current cell cannot be unset, so undoing a
    /// creation is a delete rather than an inverse (see
    /// `dashboard_new_heading`).
    pub fn inverse_writes(&self, host: HostId, writes: &[CellWrite]) -> Vec<CellWrite> {
        let Some(desk) = self.hosts.get(&host) else {
            return Vec::new();
        };
        writes
            .iter()
            .filter_map(|write| {
                let key = write.property.key();
                let property = desk
                    .view
                    .property(&write.id, &key)
                    .cloned()
                    .or_else(|| key.unwritten())?;
                (property != write.property).then_some(CellWrite {
                    id: write.id.clone(),
                    property,
                })
            })
            .collect()
    }

    /// What the user has said about one Slack unit. The cursor a card was
    /// closed on is a fact like any other, and opening the card needs it to
    /// know where to land the reader.
    pub fn facts_of_slack_unit(&self, host: Option<HostId>, unit: &SlackUnit) -> Option<Facts> {
        Some(self.hosts.get(&host?)?.view.facts(&Id::Slack(unit.clone())))
    }

    /// What the mirror says a Slack unit's newest message is, for a verdict
    /// about to write a cursor. `None` for everything that is not a Slack
    /// unit, and for a unit no source knows about, which is a verdict on a
    /// card that cannot be dealt.
    fn slack_verdict(&self, host: HostId, id: &Id) -> Option<rho_desk::cells::SlackVerdict> {
        let Id::Slack(unit) = id else {
            return None;
        };
        self.hosts
            .get(&host)?
            .sources
            .slack
            .iter()
            .find(|source| &source.unit == unit)
            .map(|source| rho_desk::cells::SlackVerdict {
                newest: source.newest.clone(),
            })
    }

    /// Where an agent's story stands, for a verdict about to write a
    /// cursor. `None` for everything that is not an agent, and for an agent
    /// no source knows about, which is a verdict on a card that cannot be
    /// dealt.
    fn agent_verdict(&self, host: HostId, id: &Id) -> Option<rho_desk::cells::AgentVerdict> {
        let Id::Agent(agent) = id else {
            return None;
        };
        self.hosts
            .get(&host)?
            .sources
            .agents
            .iter()
            .find(|source| &source.agent == agent)
            .map(|source| rho_desk::cells::AgentVerdict {
                newest: source.newest,
            })
    }

    /// A dealt verdict: the facts it changes, plus the log entry recording
    /// exactly what it changed so an undo can be validated against it.
    pub fn verdict_writes(
        &mut self,
        host: HostId,
        id: &Id,
        verdict: DeskVerdict,
    ) -> Option<(Vec<CellWrite>, (Id, VerdictEvent))> {
        // What a verdict on a Slack unit writes is a message timestamp, and
        // that timestamp is the mirror's rather than the store's.
        let slack = self.slack_verdict(host, id);
        let agent = self.agent_verdict(host, id);
        self.verdict_writes_with_cursor(host, id, verdict, slack, agent)
    }

    /// Done on a Slack unit at a cursor the caller names instead of the
    /// mirror's newest. `mark read before` needs this: what it handles is
    /// everything up to an age, so the cursor lands on the newest message at
    /// or before that age and anything newer stays the user's.
    pub fn slack_done_writes(
        &mut self,
        host: HostId,
        unit: &rho_desk::cells::SlackUnit,
        newest: rho_desk::cells::SlackTs,
    ) -> Option<(Vec<CellWrite>, (Id, VerdictEvent))> {
        self.verdict_writes_with_cursor(
            host,
            &Id::Slack(unit.clone()),
            DeskVerdict::Done,
            Some(rho_desk::cells::SlackVerdict { newest }),
            None,
        )
    }

    fn verdict_writes_with_cursor(
        &mut self,
        host: HostId,
        id: &Id,
        verdict: DeskVerdict,
        slack: Option<rho_desk::cells::SlackVerdict>,
        agent: Option<rho_desk::cells::AgentVerdict>,
    ) -> Option<(Vec<CellWrite>, (Id, VerdictEvent))> {
        let (verdict, mut writes): (Verdict, Vec<CellWrite>) = match verdict {
            DeskVerdict::Done => (Verdict::Done, Vec::new()),
            DeskVerdict::Mute => (Verdict::Mute, Vec::new()),
            DeskVerdict::Defer { until } => (Verdict::Defer { until }, Vec::new()),
            DeskVerdict::File { parent } => (Verdict::File { parent }, Vec::new()),
            DeskVerdict::Todo { defer_until, pace } => {
                let (note, mut writes) = self.create_note_writes(host, Some(id.clone()))?;
                writes.push(CellWrite {
                    id: note.clone(),
                    property: Property::Deleted(false),
                });
                writes.push(CellWrite {
                    id: note.clone(),
                    property: Property::DeferUntil(Some(defer_until)),
                });
                writes.push(CellWrite {
                    id: note.clone(),
                    property: Property::PaceDays(pace),
                });
                // The entry is built by the same constructor the daemon
                // checks it against, so the writer and the checker cannot
                // drift. It also marks the dealt thing done: the todo is
                // what handles it, and without that the dealer offers it
                // again the moment the note exists.
                let verdict = Verdict::Todo { note };
                let view = &self.hosts.get(&host)?.view;
                let changes = rho_desk::cells::verdict_changes(
                    id,
                    &verdict,
                    &|key| view.property(id, key).cloned(),
                    Some(rho_desk::cells::TodoCadence {
                        defer_until,
                        pace_days: pace,
                    }),
                    slack.clone(),
                    agent,
                )
                .ok()?;
                // Every change the entry states has to be a write too, or
                // the daemon rejects the verdict as unapplied. That is how
                // the dealt thing is handled: a state for a note, and the
                // cursor for a Slack unit or an agent.
                for change in &changes {
                    let Some(after) = change.after.clone() else {
                        continue;
                    };
                    match writes
                        .iter_mut()
                        .find(|write| write.id == change.id && write.property.key() == change.key)
                    {
                        Some(write) => write.property = after,
                        None => writes.push(CellWrite {
                            id: change.id.clone(),
                            property: after,
                        }),
                    }
                }
                let event = VerdictEvent::Applied {
                    verdict,
                    at: Stamp {
                        device: self.device,
                        version: 0,
                    },
                    changes,
                };
                return Some((writes, (id.clone(), event)));
            }
        };
        // The writes come out of the same constructor the daemon checks the
        // entry against, so a verdict that touches two facts (a snooze, which
        // zeroes the pace as well) cannot drift between writer and checker.
        let view = &self.hosts.get(&host)?.view;
        let changes = rho_desk::cells::verdict_changes(
            id,
            &verdict,
            &|key| view.property(id, key).cloned(),
            None,
            slack,
            agent,
        )
        .ok()?;
        // A verdict that changes nothing about what it is for is not one:
        // deferring a card to the moment it already wakes leaves it alone.
        let first = changes.first()?;
        if first.before == first.after {
            return None;
        }
        for change in &changes {
            let Some(after) = change.after.clone() else {
                continue;
            };
            match writes
                .iter_mut()
                .find(|write| write.id == change.id && write.property.key() == change.key)
            {
                Some(write) => write.property = after,
                None => writes.push(CellWrite {
                    id: change.id.clone(),
                    property: after,
                }),
            }
        }
        let event = VerdictEvent::Applied {
            verdict,
            at: Stamp {
                device: self.device,
                version: 0,
            },
            changes,
        };
        Some((writes, (id.clone(), event)))
    }

    /// Undoes an applied verdict by reapplying its before-values and
    /// appending the `Undone` log entry the daemon validates against them.
    pub fn undo_verdict_writes(
        &self,
        host: HostId,
        id: &Id,
        at: Stamp,
    ) -> Option<(Vec<CellWrite>, (Id, VerdictEvent))> {
        let VerdictEvent::Applied { changes, .. } = self.verdict_event(host, id, at)? else {
            return None;
        };
        let writes = changes
            .iter()
            .filter_map(|change| {
                Some(CellWrite {
                    id: change.id.clone(),
                    property: change.before.clone()?,
                })
            })
            .collect::<Vec<_>>();
        (!writes.is_empty()).then_some((writes, (id.clone(), VerdictEvent::Undone { of: at })))
    }
}

/// What the dealer asked for, before it becomes cells.
#[derive(Clone, Debug)]
pub enum DeskVerdict {
    Done,
    Mute,
    Defer { until: Timestamp },
    File { parent: Id },
    Todo { defer_until: Timestamp, pace: u32 },
}

pub fn now_timestamp() -> Timestamp {
    Timestamp {
        unix_ms: chrono::Utc::now().timestamp_millis(),
        precision: TimestampPrecision::Millisecond,
    }
}

pub fn day_timestamp(date: chrono::NaiveDate) -> Timestamp {
    Timestamp {
        unix_ms: date
            .and_hms_opt(0, 0, 0)
            .map_or(0, |at| at.and_utc().timestamp_millis()),
        precision: TimestampPrecision::Day,
    }
}

fn parent_write(id: Id, parent: Option<Id>) -> CellWrite {
    CellWrite {
        id,
        property: Property::Parent(parent),
    }
}

/// A new note is the two facts that make it one: where the user put it, and
/// when. Everything else is the default until they say otherwise.
fn create_note_writes(id: Id, parent: Option<Id>, created_at: Timestamp) -> Vec<CellWrite> {
    vec![
        parent_write(id.clone(), parent),
        CellWrite {
            id,
            property: Property::CreatedAt(created_at),
        },
    ]
}

/// Rewrites a derived title. A derived row is not a CRDT: it is replaced
/// wholesale, and its capability keeps the reader from typing into it.
pub(crate) fn write_derived_title(
    buffer: &Entity<Buffer>,
    title: &str,
    cx: &mut Context<Workspace>,
) {
    buffer.update(cx, |buffer, cx| {
        if buffer.text() == title {
            return;
        }
        let end = buffer.len();
        buffer.set_capability(Capability::ReadWrite, cx);
        buffer.edit([(0..end, title)], None, cx);
        buffer.set_capability(Capability::ReadOnly, cx);
    });
}

/// Watches a note body and sends every local edit to the daemon.
fn watch_note_buffer(
    buffer: &Entity<Buffer>,
    host: HostId,
    id: Id,
    cx: &mut Context<Workspace>,
) -> gpui::Subscription {
    cx.subscribe(buffer, move |workspace, _, event, cx| {
        if let BufferEvent::Operation {
            operation: language::Operation::Buffer(operation),
            is_local: true,
        } = event
        {
            let operation = rho_desk::TextOperation::from_text(operation);
            let timestamp = operation.timestamp();
            workspace.send_desk_text(
                host,
                id.clone(),
                operation,
                rho_desk::TextTransaction {
                    id: timestamp,
                    edit_ids: vec![timestamp],
                },
                cx,
            );
        }
    })
}

/// This GUI's device identity, persisted once in the client state directory.
///
/// The daemon binds one writer connection per device, and a device's stamps
/// must keep ascending across restarts, so a fresh id every launch would
/// both lock the GUI out of a second window and lose that ordering.
pub fn desk_device() -> DeviceId {
    {
        // The state directory is `main`'s to resolve and nobody else's: a
        // library that reaches for `dirs::state_dir()` reads the user's own
        // files from a test, which is how a rho-gui test came to open the
        // user's Slack mirror. `mirror::state_dir()` is only ever set by
        // `main`, so a test — which never sets it — gets a fresh id per GUI,
        // which is what several GUIs in one process need anyway. That used to
        // be a `#[cfg(test)]` branch saying the same thing twice.
        let path = rho_mirror::mirror::state_dir().map(|base| base.join("desk-device"));
        if let Some(path) = &path
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(bytes) = <[u8; 16]>::try_from(bytes.as_slice())
        {
            return DeviceId(bytes);
        }
        let device = DeviceId(uuid::Uuid::new_v4().into_bytes());
        if let Some(path) = &path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = std::fs::write(path, device.0) {
                tracing::warn!(%error, "could not persist the Desk device id");
            }
        }
        device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desk device id is written under the state directory `main` named,
    /// and a test names none — so a test reads and writes nothing of the
    /// user's. This used to call `dirs::state_dir()` directly, guarded by a
    /// `#[cfg(test)]` branch that said the same thing a second time; the
    /// guard is what a library gets wrong, and the rule replaces it.
    #[test]
    fn the_desk_device_id_never_reaches_the_user_s_state_directory() {
        assert!(
            rho_mirror::mirror::state_dir().is_none(),
            "only `main` names the state directory, and this is not `main`"
        );
        let first = desk_device();
        let second = desk_device();
        assert_ne!(
            first, second,
            "with no state directory named there is no file to persist to, so \
             each GUI in the process is its own device"
        );
    }

    /// And with one named, it is that one and nowhere else: the id is written
    /// under the directory the caller gave and read back from it.
    #[test]
    fn the_desk_device_id_is_written_under_the_directory_it_was_given() {
        let dir = std::env::temp_dir().join(format!("rho-desk-device-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a state directory of our own");
        let path = dir.join("desk-device");

        // `desk_device` reads whatever `main` named; naming one here would be
        // process-wide and would leak into every other test in this binary,
        // so the file is written and read the way the function does and the
        // path is asserted rather than the global set.
        let device = DeviceId(uuid::Uuid::new_v4().into_bytes());
        std::fs::write(&path, device.0).expect("persist the device id");
        let read = <[u8; 16]>::try_from(std::fs::read(&path).expect("read it back").as_slice())
            .map(DeviceId)
            .expect("sixteen bytes");
        assert_eq!(read, device);
        assert!(
            path.starts_with(std::env::temp_dir()),
            "the id lives under the directory the caller named, {}",
            path.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn node(id: Id, parent: Option<Id>, labels: &[Id]) -> DeskNode {
        DeskNode {
            id,
            parent,
            under: None,
            state: State::Open,
            defer_until: None,
            deadline: None,
            pace_days: 0,
            labels: labels.iter().cloned().collect(),
            name: None,
            created_at: None,
        }
    }

    /// A label is a second axis, so the map is a DAG drawn as a tree: the
    /// same thing is under its place and under every label it carries, and
    /// both rows are the truth rather than one of them being a duplicate.
    #[test]
    fn a_labelled_thing_is_on_the_map_in_its_place_and_under_the_label() {
        let area = Id::Note(Uuid([1; 16]));
        let label = Id::Label(Uuid([2; 16]));
        let thing = Id::Note(Uuid([3; 16]));
        let child = Id::Note(Uuid([4; 16]));
        let nodes = BTreeMap::from([
            (area.clone(), node(area.clone(), None, &[])),
            (label.clone(), node(label.clone(), None, &[])),
            (
                thing.clone(),
                node(
                    thing.clone(),
                    Some(area.clone()),
                    std::slice::from_ref(&label),
                ),
            ),
            (child.clone(), node(child.clone(), Some(thing.clone()), &[])),
        ]);

        let ordered = order(nodes);
        let places = |id: &Id| {
            ordered
                .iter()
                .filter(|row| &row.id == id)
                .map(|row| row.under.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            places(&thing),
            vec![Some(area.clone()), Some(label.clone())],
            "the thing is in its place and under the label it carries"
        );
        // The child hangs off the thing, and a row is one place: the two
        // thing rows share the same child row rather than drawing it twice.
        assert_eq!(places(&child), vec![Some(thing.clone())]);
    }

    /// The map rule: things under their labels, things under their parent
    /// thing, and the rest at the root. Filing the origin of a burst under a
    /// label takes the burst with it and takes the origin off the root.
    #[test]
    fn a_thing_filed_under_a_label_brings_its_subtree_and_leaves_the_root() {
        let label = Id::Label(Uuid([1; 16]));
        let origin = Id::Page(rho_desk::PageId([2; 16]));
        let tab = Id::Page(rho_desk::PageId([3; 16]));
        let nodes = BTreeMap::from([
            (label.clone(), node(label.clone(), None, &[])),
            (
                origin.clone(),
                node(origin.clone(), None, std::slice::from_ref(&label)),
            ),
            (tab.clone(), node(tab.clone(), Some(origin.clone()), &[])),
        ]);

        let ordered = order(nodes);
        let places = |id: &Id| {
            ordered
                .iter()
                .filter(|row| &row.id == id)
                .map(|row| row.under.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            places(&origin),
            vec![Some(label.clone())],
            "the origin is under the label it carries, and nowhere else"
        );
        assert_eq!(
            places(&tab),
            vec![Some(origin.clone())],
            "and the tab comes with it"
        );
    }

    /// A label filed under something it labels is a cycle in the drawing,
    /// not in the store. Each place is drawn once, which ends the walk.
    #[test]
    fn a_label_filed_under_what_it_labels_still_draws() {
        let thing = Id::Note(Uuid([1; 16]));
        let label = Id::Label(Uuid([2; 16]));
        let nodes = BTreeMap::from([
            (
                thing.clone(),
                node(thing.clone(), None, std::slice::from_ref(&label)),
            ),
            (label.clone(), node(label.clone(), Some(thing.clone()), &[])),
        ]);

        let ordered = order(nodes);
        assert_eq!(
            ordered.iter().filter(|row| row.id == thing).count(),
            2,
            "the thing is at the root and under its own label"
        );
        // The label itself is in one place, under the thing it was filed
        // under; the second pass through it is the same place, so it is not
        // drawn again.
        assert_eq!(ordered.iter().filter(|row| row.id == label).count(), 1);
    }
}
