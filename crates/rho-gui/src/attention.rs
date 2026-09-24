//! What wants the user, and what they said about it.
//!
//! The user's marks live in the ledger, which syncs them sealed between
//! their devices through the hosts. Every source — the agents, Slack, the
//! notes — says what it wants from the user, folding in the marks that
//! concern it, and the dealer ranks those wants into the hand Home shows
//! and a pull deals from. A verdict is a source's own action: most write
//! marks, and a Slack one moves Slack's cursor or mutes the unit in Slack.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{Context, Window};
use rho_agent_types::AgentId;
use rho_dealer::marks::{self, Cursor, Todo, Write};
use rho_dealer::{Card, CardKind, Curve, DateMark, Dealer, Marks, NodeId, SlackUnit, Want};
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Ledger, Secret};
use rho_window::style::StyleClass;

use crate::sources::{AgentsSource, NotesSource, SlackSource};
use crate::workspace::Workspace;

/// What `shift-u` takes back: the marks a verdict wrote, as they were,
/// and what it did outside the ledger.
pub(crate) struct Undo {
    /// Which verdict this is, in the order they were taken.
    pub(crate) sequence: u64,
    pub(crate) verb: String,
    /// The writes that put the marks back.
    pub(crate) writes: Vec<Write>,
    /// The card the verdict was on, dealt again by the undo, and what the
    /// journal called the verdict. `None` for a batch like `mark read
    /// before`, which has no card of its own.
    pub(crate) card: Option<(Card, rho_journal::DealerVerdict)>,
    /// rho's own Slack cursors the verdict moved, and where each stood.
    pub(crate) slack_cursors: Vec<(rho_slack::model::Unit, rho_slack::session::HandledBefore)>,
    /// A unit the verdict muted in Slack, unmuted again.
    pub(crate) slack_muted: Option<SlackUnit>,
}

/// The verdicts a card can take. Each source decides what one means for
/// its nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Dealt with: everything the source has said so far is handled.
    Done,
    /// Nothing from this node reaches the user again.
    Mute,
    /// Out of the way until then, and back then.
    Snooze(DateMark),
    /// Handled here, and owed again: back after `pace_days`.
    Todo { pace_days: u32 },
}

pub(crate) struct Attention {
    streams: Arc<LedgerStreams>,
    pub(crate) marks: Marks,
    pub(crate) dealer: Dealer,
    pub(crate) undo: Vec<Undo>,
    next_undo: u64,
    pub(crate) slack: SlackSource,
}

impl Attention {
    /// The ledger in `db`, with its marks read out. A device's first start
    /// on the ledger carries over what the desk held.
    pub(crate) fn open(db: rho_db::RhoDb) -> (Self, UnboundedReceiver<LedgerEvent>) {
        let ledger = futures::executor::block_on(async {
            let ledger = Ledger::open(db.clone()).await;
            if ledger.head() == 0 {
                migrate(&db).await;
            }
            ledger
        });
        let mut marks = Marks::default();
        marks.apply(
            ledger
                .scan(b"n/")
                .into_iter()
                .map(|(key, value)| (key, Some(value))),
        );
        let (streams, events) = LedgerStreams::new(ledger);
        (
            Self {
                streams,
                marks,
                dealer: Dealer::default(),
                undo: Vec::new(),
                next_undo: 0,
                slack: SlackSource::default(),
            },
            events,
        )
    }

    /// Keeps a verdict for `shift-u`, numbered after every one before it.
    pub(crate) fn push_undo(&mut self, mut undo: Undo) -> u64 {
        undo.sequence = self.next_undo;
        self.next_undo += 1;
        let sequence = undo.sequence;
        self.undo.push(undo);
        sequence
    }

    pub(crate) fn last_undo(&self) -> Option<u64> {
        self.undo.last().map(|undo| undo.sequence)
    }

    /// The ledger's stream, for a host to carry.
    pub(crate) fn stream(&self) -> Arc<dyn rho_agent_hosts::HostStream> {
        self.streams.stream()
    }

    pub(crate) fn secret(&self) -> Option<Secret> {
        self.streams.ledger().secret()
    }

    /// Reads `keys` back from the ledger into the marks: the merged value
    /// is the one that counts, whatever the write or event said.
    fn reread(&mut self, keys: impl IntoIterator<Item = Vec<u8>>) -> BTreeSet<NodeId> {
        let ledger = self.streams.ledger();
        let entries: Vec<Write> = keys
            .into_iter()
            .map(|key| {
                let value = ledger.get(&key);
                (key, value)
            })
            .collect();
        self.marks.apply(entries)
    }
}

/// Reads the desk this client held into marks, stamped as old as the
/// ledger allows: a device that migrates after the user has already
/// worked elsewhere must not win over that work.
async fn migrate(db: &rho_db::RhoDb) {
    let held = rho_desk_client::export::held(db);
    let writes = desk_marks(&held);
    if writes.is_empty() {
        return;
    }
    let count = writes.len();
    let old = Ledger::open_with_clock(db.clone(), Arc::new(|| 1)).await;
    old.write(writes).await;
    tracing::info!(marks = count, "carried the desk over into the ledger");
}

fn migrated_node(id: &rho_desk_client::protocol::cells::Id) -> Option<NodeId> {
    use rho_desk_client::protocol::cells::Id;
    Some(match id {
        Id::Note(uuid) => NodeId::Note(uuid::Uuid::from_bytes(uuid.0)),
        Id::Label(uuid) => NodeId::Label(uuid::Uuid::from_bytes(uuid.0)),
        Id::Agent(agent) => NodeId::Agent(*agent),
        Id::Slack(unit) => NodeId::Slack(SlackUnit {
            workspace: unit.workspace.clone(),
            channel: unit.channel.clone(),
            thread: unit.thread.clone(),
        }),
        Id::PullRequest { repo, number } => NodeId::PullRequest {
            repo: repo.clone(),
            number: *number,
        },
        Id::Host(_) | Id::Page(_) | Id::File { .. } => return None,
    })
}

fn date_mark(at: rho_desk_client::protocol::cells::Timestamp) -> DateMark {
    DateMark {
        unix_ms: at.unix_ms,
        day: at.precision == rho_desk_client::protocol::cells::TimestampPrecision::Day,
    }
}

/// The marks that say what the desk said. Labels keep their names,
/// nesting and repositories; notes that are not deleted keep their text,
/// labels, state and dates; agents keep their labels, names, mutes, dates
/// and how far they were handled; Slack units keep their labels and
/// dates. A date with no pace is a snooze, one with a pace a todo.
pub(crate) fn desk_marks(
    held: &std::collections::BTreeMap<
        rho_desk_client::protocol::cells::Id,
        rho_desk_client::export::HeldNode,
    >,
) -> Vec<Write> {
    use rho_desk_client::protocol::cells::{Id, Property, State};
    let mut writes = Vec::new();
    for (id, node) in held {
        let Some(target) = migrated_node(id) else {
            continue;
        };
        let deleted = node
            .properties
            .iter()
            .any(|property| matches!(property, Property::Deleted(true)));
        if deleted && matches!(target, NodeId::Note(_)) {
            continue;
        }
        let mut wakes = None;
        let mut deadline = None;
        let mut pace_days = 0;
        for property in &node.properties {
            match property {
                Property::Parent(Some(Id::Label(parent))) if matches!(target, NodeId::Label(_)) => {
                    writes.push(marks::parent(
                        &target,
                        Some(uuid::Uuid::from_bytes(parent.0)),
                    ));
                }
                Property::About(about) => {
                    if let Some(about) = migrated_node(about) {
                        writes.push(marks::about(&target, Some(&about)));
                    }
                }
                Property::Labeled {
                    label: Id::Label(label),
                    present: true,
                } => writes.push(marks::label(&target, uuid::Uuid::from_bytes(label.0), true)),
                Property::Name(name) if !name.trim().is_empty() => {
                    writes.push(marks::name(&target, Some(name.clone())));
                }
                Property::Repository(Some(repository)) => {
                    writes.push(marks::repository(&target, Some(repository.url.clone())));
                }
                Property::State(State::Muted) if !matches!(target, NodeId::Slack(_)) => {
                    writes.push(marks::muted(&target, true));
                }
                Property::State(State::Done) if matches!(target, NodeId::Note(_)) => {
                    writes.push(marks::handled(&target, Some(Cursor::Done)));
                }
                Property::AgentHandledThrough(pos) if matches!(target, NodeId::Agent(_)) => {
                    writes.push(marks::handled(&target, Some(Cursor::Story(pos.0))));
                }
                Property::DeferUntil(Some(at)) => wakes = Some(date_mark(*at)),
                Property::Deadline(Some(at)) => deadline = Some(date_mark(*at)),
                Property::PaceDays(pace) => pace_days = *pace,
                Property::Deleted(true) => writes.push(marks::deleted(&target, true)),
                Property::CreatedAt(at) => writes.push(marks::created(&target, at.unix_ms)),
                _ => {}
            }
        }
        match (wakes, deadline, pace_days) {
            (None, None, _) => {}
            (Some(until), None, 0) if !matches!(target, NodeId::Note(_)) => {
                writes.push(marks::snooze(&target, Some(until)));
            }
            (wakes, deadline, pace_days) => writes.push(marks::todo(
                &target,
                Some(Todo {
                    wakes,
                    deadline,
                    pace_days,
                }),
            )),
        }
        if let (NodeId::Note(_), Some(body)) = (&target, &node.body) {
            writes.push(marks::body(&target, body));
        }
    }
    writes
}

/// What an agent's last turn says, in the words a card uses.
/// The wants a node's own dates make: a todo coming due or a deadline, and
/// a snooze that has come back.
fn dated_wants(marks: &rho_dealer::marks::NodeMarks, title: &str, context: &str) -> Vec<Want> {
    let want = |curve: Curve, cursor: String| Want {
        kind: CardKind::Dated,
        title: title.to_owned(),
        context: context.to_owned(),
        reason: String::new(),
        curve,
        touched_ms: None,
        cursor,
    };
    let mut wants = Vec::new();
    if let Some(todo) = marks.todo {
        if let Some(at) = todo.wakes {
            wants.push(want(
                Curve::Wakes {
                    at,
                    pace_days: todo.pace_days,
                },
                format!("wakes {}", at.unix_ms),
            ));
        }
        if let Some(at) = todo.deadline {
            wants.push(want(
                Curve::Deadline {
                    at,
                    pace_days: todo.pace_days,
                },
                format!("deadline {}", at.unix_ms),
            ));
        }
    }
    if let Some(at) = marks.snoozed {
        wants.push(want(
            Curve::Wakes { at, pace_days: 0 },
            format!("snooze {}", at.unix_ms),
        ));
    }
    wants
}

/// A label path split into its names, with empty ones dropped.
fn path_names(path: &str) -> Vec<&str> {
    path.split('/')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

impl Workspace {
    /// Starts reading what the ledger's streams hear.
    pub(crate) fn listen_to_ledger(
        events: UnboundedReceiver<LedgerEvent>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let mut events = events;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let updated = this.update(cx, |this, cx| {
                    for event in batch {
                        this.ledger_event(event, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        })
    }

    fn ledger_event(&mut self, event: LedgerEvent, cx: &mut Context<Self>) {
        match event {
            LedgerEvent::Changed(changes) => {
                let touched = self
                    .attention
                    .reread(changes.into_iter().map(|change| change.key));
                self.marks_moved(touched, cx);
            }
            LedgerEvent::Unreadable { device } => self.append_message(
                format!(
                    "ledger: device {} writes with another key; its marks are not read",
                    device
                        .0
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                ),
                StyleClass::SystemInfo,
                cx,
            ),
            LedgerEvent::NeedsKey => self.append_message(
                "ledger: other devices have written marks; `space s s` enters the secret phrase they share"
                    .to_owned(),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    /// Writes marks, and returns the writes that put them back.
    pub(crate) fn write_marks(&mut self, writes: Vec<Write>, cx: &mut Context<Self>) -> Vec<Write> {
        if writes.is_empty() {
            return Vec::new();
        }
        let inverse = self.attention.marks.inverse(&writes);
        let keys: Vec<Vec<u8>> = writes.iter().map(|(key, _)| key.clone()).collect();
        futures::executor::block_on(self.attention.streams.write(writes));
        let touched = self.attention.reread(keys);
        self.marks_moved(touched, cx);
        inverse
    }

    /// Everything that follows from marks moving: the agents' own view of
    /// their verdicts and filing, the wants of every node that moved, and
    /// the surfaces that show them.
    pub(crate) fn marks_moved(&mut self, touched: BTreeSet<NodeId>, cx: &mut Context<Self>) {
        if touched.is_empty() {
            return;
        }
        let mut labels_moved = false;
        let mut agents = Vec::new();
        for node in &touched {
            match node {
                NodeId::Agent(agent_id) => agents.push(*agent_id),
                NodeId::Label(_) => labels_moved = true,
                _ => {}
            }
        }
        if labels_moved {
            // A label renamed or moved renames every agent filed under it.
            agents = self.registry.known_agents().copied().collect();
            self.refresh_workdirs();
        }
        self.push_agent_marks(&agents);
        let now = chrono::Local::now().fixed_offset();
        for node in &touched {
            self.refresh_node_wants(node, now);
        }
        self.sync_note_views(&touched, cx);
        self.invalidate_dealer_signals(cx);
    }

    /// Hands the agents map what the user said about each agent: how far
    /// it is handled, whether it is muted, its name and its labels.
    pub(crate) fn push_agent_marks(&mut self, agents: &[AgentId]) {
        let mut filings = Vec::with_capacity(agents.len());
        for agent_id in agents {
            let node = NodeId::Agent(*agent_id);
            let marks = self.attention.marks.get(&node);
            let verdict = AgentsSource::map_verdict(marks);
            filings.push((
                *agent_id,
                rho_agents_client::AgentFiling {
                    muted: marks.muted,
                    labels: marks
                        .labels
                        .iter()
                        .filter(|label| !self.attention.marks.get(&NodeId::Label(**label)).deleted)
                        .map(|label| self.attention.marks.label_path(*label))
                        .collect(),
                    name: marks.name.clone(),
                },
            ));
            if self.registry.set_agent_verdict(*agent_id, verdict) {
                self.agents_client.set_verdict(*agent_id, verdict);
            }
        }
        self.registry.set_agent_filings(filings);
    }

    /// What a node is called on a card, in Find, and on its own surface.
    pub(crate) fn node_title(&self, node: &NodeId) -> String {
        match node {
            NodeId::Note(_) => self.notes_source().note_title(node),
            NodeId::Label(id) => self.notes_source().label_title(*id),
            NodeId::Agent(agent_id) => self.agents_source().title(*agent_id),
            NodeId::Slack(unit) => self.attention.slack.title(unit),
            NodeId::PullRequest { repo, number } => format!("{repo}#{number}"),
        }
    }

    pub(crate) fn agents_source(&self) -> AgentsSource<'_> {
        AgentsSource {
            map: &self.registry,
            touched: &self.agent_last_interaction,
        }
    }

    pub(crate) fn notes_source(&self) -> NotesSource<'_> {
        NotesSource {
            marks: &self.attention.marks,
        }
    }

    /// Where a node is: a conversation for a Slack unit, the labels it
    /// carries for anything else.
    pub(crate) fn node_context(&self, node: &NodeId) -> String {
        if let NodeId::Slack(unit) = node
            && let Some(conversation) = self.attention.slack.context(unit)
        {
            return conversation;
        }
        let marks = self.attention.marks.get(node);
        marks
            .labels
            .iter()
            .filter(|label| !self.attention.marks.get(&NodeId::Label(**label)).deleted)
            .map(|label| self.attention.marks.label_path(*label))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Everything `node` wants of the user right now.
    fn wants_for(&self, node: &NodeId, now: chrono::DateTime<chrono::FixedOffset>) -> Vec<Want> {
        let marks = self.attention.marks.get(node);
        if marks.muted || marks.deleted {
            return Vec::new();
        }
        if marks.snoozed.is_some_and(|until| until.is_ahead(now)) {
            return Vec::new();
        }
        let title = self.node_title(node);
        let context = self.node_context(node);
        let mut wants = dated_wants(marks, &title, &context);
        match node {
            NodeId::Agent(agent_id) => {
                wants.extend(self.agents_source().want(*agent_id, marks, &context))
            }
            NodeId::Slack(unit) => {
                wants.extend(self.attention.slack.want(unit, now.timestamp_millis()))
            }
            _ => {}
        }
        wants
    }

    fn refresh_node_wants(&mut self, node: &NodeId, now: chrono::DateTime<chrono::FixedOffset>) {
        let wants = self.wants_for(node, now);
        self.attention.dealer.set(node.clone(), wants);
    }

    /// Makes the named agents' wants again, after their stories moved.
    pub(crate) fn refresh_agent_wants(&mut self, agents: impl IntoIterator<Item = AgentId>) {
        let now = chrono::Local::now().fixed_offset();
        for agent_id in agents {
            self.refresh_node_wants(&NodeId::Agent(agent_id), now);
        }
    }

    /// Reads Slack's units again and makes their wants.
    pub(crate) fn refresh_slack_wants(&mut self, cx: &gpui::App) {
        let facts = self.slack_thread_facts(cx);
        let now = chrono::Local::now().fixed_offset();
        let units = self.attention.slack.read(facts);
        for unit in units {
            self.refresh_node_wants(&NodeId::Slack(unit), now);
        }
    }

    /// Every want made again: every agent, every Slack unit and every
    /// marked node.
    pub(crate) fn rebuild_wants(&mut self, cx: &gpui::App) {
        let now = chrono::Local::now().fixed_offset();
        let agents: Vec<AgentId> = self.registry.known_agents().copied().collect();
        self.push_agent_marks(&agents);
        self.attention.dealer.retain(|_| false);
        let facts = self.slack_thread_facts(cx);
        self.attention.slack.read(facts);
        let mut nodes: BTreeSet<NodeId> = agents.into_iter().map(NodeId::Agent).collect();
        nodes.extend(self.attention.slack.units().cloned().map(NodeId::Slack));
        nodes.extend(
            self.attention
                .marks
                .nodes()
                .filter(|(_, marks)| marks.todo.is_some() || marks.snoozed.is_some())
                .map(|(node, _)| node.clone()),
        );
        for node in nodes {
            self.refresh_node_wants(&node, now);
        }
    }

    /// The ranking as it stands.
    pub(crate) fn hand(&self) -> Vec<Card> {
        self.attention
            .dealer
            .hand(chrono::Local::now().fixed_offset())
    }

    /// The card for a node the reader is on: its card in the hand, or the
    /// node itself when the hand holds none, because reading a thing can
    /// be what quiets it and a verdict on what is on screen still lands.
    pub(crate) fn card_for(&self, node: &NodeId) -> Card {
        let now = chrono::Local::now().fixed_offset();
        if let Some(card) = self
            .attention
            .dealer
            .hand(now)
            .into_iter()
            .find(|card| &card.node == node)
        {
            return card;
        }
        let kind = match node {
            NodeId::Agent(_) => CardKind::Agent,
            NodeId::Slack(_) => CardKind::Slack,
            _ => CardKind::Dated,
        };
        Card {
            node: node.clone(),
            kind,
            title: self.node_title(node),
            context: self.node_context(node),
            label: String::new(),
            priority: f64::NEG_INFINITY,
            cursor: String::new(),
            skipped: false,
        }
    }

    /// Takes a verdict on `node`, and says what it did in the words the
    /// undo will use. `None` when the node cannot take it.
    pub(crate) fn take_verdict(
        &mut self,
        node: &NodeId,
        verdict: Verdict,
        cx: &mut Context<Self>,
    ) -> Option<Undo> {
        let mut writes = Vec::new();
        let mut slack_cursors = Vec::new();
        let mut slack_muted = None;
        let marks = self.attention.marks.get(node).clone();
        // Whatever says "dealt with" also takes back the dates that would
        // bring the node back.
        let clear_dates = |writes: &mut Vec<Write>| {
            if marks.todo.is_some() {
                writes.push(marks::todo(node, None));
            }
            if marks.snoozed.is_some() {
                writes.push(marks::snooze(node, None));
            }
        };
        match node {
            NodeId::Agent(agent_id) => {
                writes.extend(self.agents_source().verdict(*agent_id, verdict))
            }
            // Slack's verdicts are Slack's own, through its session.
            NodeId::Slack(unit) => match verdict {
                Verdict::Done | Verdict::Todo { .. } | Verdict::Mute => {
                    slack_cursors.extend(self.advance_slack_cursor(unit, None, cx));
                    if verdict == Verdict::Mute {
                        self.slack_set_unit_muted(unit, true, cx);
                        slack_muted = Some(unit.clone());
                    }
                }
                Verdict::Snooze(_) => {}
            },
            _ => writes.extend(self.notes_source().verdict(node, verdict)),
        }

        match verdict {
            Verdict::Done | Verdict::Mute => clear_dates(&mut writes),
            Verdict::Snooze(until) => writes.push(marks::snooze(node, Some(until))),
            Verdict::Todo { pace_days } => {
                if marks.snoozed.is_some() {
                    writes.push(marks::snooze(node, None));
                }
                writes.push(marks::todo(
                    node,
                    Some(Todo {
                        wakes: Some(DateMark::day(chrono::Local::now().date_naive())),
                        deadline: marks.todo.and_then(|todo| todo.deadline),
                        pace_days,
                    }),
                ));
            }
        }
        if writes.is_empty() && slack_cursors.is_empty() && slack_muted.is_none() {
            return None;
        }
        let writes = self.write_marks(writes, cx);
        if !slack_cursors.is_empty() || slack_muted.is_some() {
            self.refresh_slack_wants(cx);
            self.invalidate_dealer_signals(cx);
        }
        Some(Undo {
            sequence: 0,
            verb: String::new(),
            writes,
            card: None,
            slack_cursors,
            slack_muted,
        })
    }

    /// `shift-u`: the last verdict, taken back.
    pub(crate) fn undo_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone_snap_in_progress() {
            return;
        }
        let Some(undo) = self.attention.undo.pop() else {
            self.echo("nothing to undo", StyleClass::SystemInfo, cx);
            return;
        };
        self.restore_slack_cursors(&undo.slack_cursors, cx);
        if let Some(unit) = &undo.slack_muted {
            self.slack_set_unit_muted(unit, false, cx);
        }
        self.write_marks(undo.writes, cx);
        self.refresh_slack_wants(cx);
        let Some((card, verdict)) = undo.card else {
            rho_journal::record(rho_journal::Event::SlackMarkReadBeforeUndone {
                cards: undo.slack_cursors.len(),
            });
            self.echo(
                &format!("undid {}: {} reopened", undo.verb, undo.slack_cursors.len()),
                StyleClass::SystemInfo,
                cx,
            );
            self.invalidate_dealer_signals(cx);
            return;
        };
        self.attention.dealer.clear_skip(&card.node);
        rho_journal::record(rho_journal::Event::VerdictUndone {
            card: Self::journal_card_identity(&card.node),
            verdict,
        });
        self.echo(
            &format!("undid {}: {}", undo.verb, card.title),
            StyleClass::SystemInfo,
            cx,
        );
        self.open_card(card, window, cx);
        self.invalidate_dealer_signals(cx);
    }

    /// The label at `path`, made if nobody has made it yet, with every
    /// label above it.
    pub(crate) fn mint_label(&mut self, path: &str, cx: &mut Context<Self>) -> Option<uuid::Uuid> {
        let names = path_names(path);
        if names.is_empty() {
            return None;
        }
        let mut parent: Option<uuid::Uuid> = None;
        let mut writes = Vec::new();
        let mut minted: HashMap<String, uuid::Uuid> = HashMap::new();
        for depth in 0..names.len() {
            let prefix = names[..=depth].join("/");
            let found = self
                .attention
                .marks
                .label_at(&prefix)
                .or_else(|| minted.get(&prefix).copied());
            let id = match found {
                Some(id) => id,
                None => {
                    let id = uuid::Uuid::new_v4();
                    let node = NodeId::Label(id);
                    writes.push(marks::name(&node, Some(names[depth].to_owned())));
                    writes.push(marks::parent(&node, parent));
                    writes.push(marks::created(
                        &node,
                        chrono::Local::now().timestamp_millis(),
                    ));
                    minted.insert(prefix, id);
                    id
                }
            };
            parent = Some(id);
        }
        self.write_marks(writes, cx);
        parent
    }

    /// Puts the label at `path` on `node`, or takes it off when the node
    /// already carries it. Says whether it is now on, with the writes that
    /// undo it.
    pub(crate) fn toggle_label(
        &mut self,
        node: &NodeId,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<(bool, Vec<Write>)> {
        let existing = self.attention.marks.label_at(path.trim());
        let carried =
            existing.is_some_and(|label| self.attention.marks.get(node).labels.contains(&label));
        let label = match existing {
            Some(label) => label,
            None => self.mint_label(path, cx)?,
        };
        let undo = self.write_marks(vec![marks::label(node, label, !carried)], cx);
        Some((!carried, undo))
    }

    /// A new note, made from `area` the way any new thing is.
    pub(crate) fn create_note(&mut self, area: Option<&NodeId>, cx: &mut Context<Self>) -> NodeId {
        let node = NodeId::Note(uuid::Uuid::new_v4());
        let mut writes = vec![marks::created(
            &node,
            chrono::Local::now().timestamp_millis(),
        )];
        writes.extend(self.new_thing_marks(&node, area));
        self.write_marks(writes, cx);
        node
    }

    /// The repositories labels name, which are where agents work.
    pub(crate) fn refresh_workdirs(&mut self) {
        let repositories: Vec<(String, camino::Utf8PathBuf)> = self
            .attention
            .marks
            .labels()
            .into_iter()
            .filter_map(|(id, path)| {
                let url = self
                    .attention
                    .marks
                    .get(&NodeId::Label(id))
                    .repository
                    .clone()?;
                Some((path, url.into()))
            })
            .collect();
        for host in self.hosts.ids() {
            self.hosts.set_workdirs(host, repositories.clone());
        }
    }

    /// `space s s`: the secret phrase, shown so another device can take
    /// it, or taken from the user when this device has none yet.
    pub(crate) fn prompt_secret_phrase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(secret) = self.attention.secret() {
            self.append_message(
                format!(
                    "secret phrase (enter it on your other devices): {}",
                    secret.to_words()
                ),
                StyleClass::SystemInfo,
                cx,
            );
            self.echo(
                "secret phrase: in the message log",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        self.open_prompt(
            "secret phrase from another device (empty makes a new one):",
            std::rc::Rc::new(|_, input, _| {
                let word = &input[crate::minibuffer::token_start(input)..];
                if word.is_empty() {
                    return Vec::new();
                }
                Secret::words_starting(&word.to_lowercase())
                    .iter()
                    .map(|word| crate::minibuffer::Candidate {
                        value: (*word).to_owned(),
                        description: String::new(),
                    })
                    .collect()
            }),
            std::rc::Rc::new(|workspace, input, _window, cx| {
                let input = input.trim();
                let (secret, made) = match input.is_empty() {
                    true => (Secret::generate(), true),
                    false => match Secret::from_words(input) {
                        Ok(secret) => (secret, false),
                        Err(error) => {
                            workspace.echo(
                                &format!("secret phrase: {error}"),
                                StyleClass::SystemInfo,
                                cx,
                            );
                            return;
                        }
                    },
                };
                let streams = workspace.attention.streams.clone();
                if let Err(error) = futures::executor::block_on(streams.set_secret(secret)) {
                    workspace.echo(
                        &format!("secret phrase: {error}"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                if made {
                    workspace.append_message(
                        format!(
                            "secret phrase (save it, and enter it on your other devices): {}",
                            secret.to_words()
                        ),
                        StyleClass::SystemInfo,
                        cx,
                    );
                    workspace.echo(
                        "secret phrase made: in the message log",
                        StyleClass::SystemInfo,
                        cx,
                    );
                } else {
                    workspace.echo("secret phrase set", StyleClass::SystemInfo, cx);
                }
            }),
            window,
            cx,
        );
    }
}

/// What an agent is doing, for the status line: how long its turn has run,
/// or how its last turn ended and how long ago.
pub(crate) fn agent_state_label(
    facts: &rho_agents_client::AgentFacts,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> Option<String> {
    if facts.turn_running {
        return Some(match facts.turn_started_at {
            Some(started) => format!(
                "working · {}",
                crate::home::elapsed_label(started.0 as i64, now.timestamp_millis())
            ),
            // The head says a turn runs without saying since when, which is
            // every turn that started before the client was listening.
            None => "working".to_owned(),
        });
    }
    let ended = facts.last_turn_ended?;
    let wait_days = (now.timestamp_millis() - ended.0 as i64) as f64 / 86_400_000.0;
    let age = rho_dealer::curve::age_label(wait_days);
    Some(if facts.errored {
        format!("errored · {age} ago")
    } else if facts.needs_you_hint {
        format!("waiting on reply · {age}")
    } else {
        format!("finished · {age} ago")
    })
}

#[cfg(test)]
mod tests {
    use rho_desk_client::export::HeldNode;
    use rho_desk_client::protocol::cells::{
        Id, Property, State, StoryPos, Timestamp, TimestampPrecision, Uuid,
    };

    use super::*;

    fn day(unix_ms: i64) -> Timestamp {
        Timestamp {
            unix_ms,
            precision: TimestampPrecision::Day,
        }
    }

    #[test]
    fn the_desk_carries_over_as_marks() {
        let label = Id::Label(Uuid([1; 16]));
        let child = Id::Label(Uuid([2; 16]));
        let note = Id::Note(Uuid([3; 16]));
        let gone = Id::Note(Uuid([4; 16]));
        let agent_id = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let agent = Id::Agent(agent_id);
        let mut held = std::collections::BTreeMap::new();
        held.insert(
            label.clone(),
            HeldNode {
                properties: vec![Property::Name("rho".into())],
                body: None,
            },
        );
        held.insert(
            child.clone(),
            HeldNode {
                properties: vec![
                    Property::Name("gui".into()),
                    Property::Parent(Some(label.clone())),
                ],
                body: None,
            },
        );
        held.insert(
            note.clone(),
            HeldNode {
                properties: vec![
                    Property::Labeled {
                        label: child.clone(),
                        present: true,
                    },
                    Property::DeferUntil(Some(day(86_400_000))),
                    Property::PaceDays(3),
                ],
                body: Some("buy milk\nsoon".into()),
            },
        );
        held.insert(
            gone,
            HeldNode {
                properties: vec![Property::Deleted(true)],
                body: Some("old".into()),
            },
        );
        held.insert(
            agent,
            HeldNode {
                properties: vec![
                    Property::State(State::Muted),
                    Property::AgentHandledThrough(StoryPos(9)),
                    Property::DeferUntil(Some(day(0))),
                    Property::Name("fixer".into()),
                ],
                body: None,
            },
        );
        held.insert(
            Id::Host(7),
            HeldNode {
                properties: vec![Property::Name("host".into())],
                body: None,
            },
        );
        let mut marks = Marks::default();
        marks.apply(desk_marks(&held));

        let gui = uuid::Uuid::from_bytes([2; 16]);
        assert_eq!(marks.label_path(gui), "rho/gui");
        let note = marks.get(&NodeId::Note(uuid::Uuid::from_bytes([3; 16])));
        assert_eq!(note.title(), "buy milk");
        assert!(note.labels.contains(&gui));
        assert_eq!(
            note.todo,
            Some(Todo {
                wakes: Some(DateMark {
                    unix_ms: 86_400_000,
                    day: true
                }),
                deadline: None,
                pace_days: 3,
            })
        );
        assert_eq!(
            marks.notes().count(),
            1,
            "a deleted note is not carried over"
        );
        let agent = marks.get(&NodeId::Agent(agent_id));
        assert!(agent.muted);
        assert_eq!(agent.handled, Some(Cursor::Story(9)));
        assert_eq!(agent.name.as_deref(), Some("fixer"));
        assert_eq!(
            agent.snoozed,
            Some(DateMark {
                unix_ms: 0,
                day: true
            }),
            "a date with no pace is a snooze"
        );
        assert_eq!(agent.todo, None);
    }
}
