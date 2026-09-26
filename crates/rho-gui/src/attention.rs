//! What wants the user, and what they said about it.
//!
//! What the user says lives in the ledger, which syncs it sealed between
//! their devices through the hosts: facts, every one kept, and notes, a
//! revision per save. The dealer reads them with the agents map and Slack
//! into the hand Home shows and a pull deals from. A verdict is a fact the
//! user said, and a Slack one also moves Slack's cursor or mutes the unit
//! in Slack.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{Context, Window};
use rho_agent_types::AgentId;
use rho_dealer::facts::{Device, Entry, EntryId, Fact, Seen};
use rho_dealer::notes::NoteRev;
use rho_dealer::rank::{self, Cache, Sources};
use rho_dealer::{Card, CardKind, Hand, Marks, NodeId, Skips, SlackUnit, Until};
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Channel, Item, Ledger, Secret};
use rho_window::style::StyleClass;

use crate::workspace::Workspace;

/// Something the user says: a fact, or a note's new text or state.
#[derive(Clone, Debug)]
pub(crate) enum Write {
    Fact(Fact),
    Note {
        note: uuid::Uuid,
        body: Option<String>,
        deleted: Option<bool>,
    },
}

impl From<Fact> for Write {
    fn from(fact: Fact) -> Self {
        Self::Fact(fact)
    }
}

/// What takes one write back: a retract of the entry, or a note's
/// revision before it, said again.
#[derive(Clone, Debug)]
pub(crate) enum Takeback {
    Retract(EntryId),
    Note(NoteRev),
}

impl Takeback {
    fn write(self) -> Write {
        match self {
            Self::Retract(of) => Write::Fact(Fact::Retract { of }),
            Self::Note(rev) => Write::Note {
                note: rev.note,
                body: Some(rev.body),
                deleted: Some(rev.deleted),
            },
        }
    }
}

/// What `shift-u` takes back: what a verdict said, and what it did
/// outside the ledger.
pub(crate) struct Undo {
    /// Which verdict this is, in the order they were taken.
    pub(crate) sequence: u64,
    pub(crate) verb: String,
    /// What takes back what it said.
    pub(crate) takeback: Vec<Takeback>,
    /// The node the verdict was on, dealt again by the undo, and what the
    /// journal called the verdict. `None` for a batch like `mark read
    /// before`, which has no card of its own.
    pub(crate) card: Option<(NodeId, rho_journal::DealerVerdict)>,
    /// rho's own Slack cursors the verdict moved, and where each stood.
    pub(crate) slack_cursors: Vec<(rho_slack::model::Unit, rho_slack::session::HandledBefore)>,
    /// A unit the verdict muted in Slack, unmuted again.
    pub(crate) slack_muted: Option<SlackUnit>,
}

/// The verdicts a card can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Dealt with: everything the source has said so far is handled.
    Done,
    /// Nothing from this node reaches the user again.
    Mute,
    /// Out of the way until then.
    Snooze(Until),
    /// Handled here, and on the user's plate until done: from `start`, or
    /// from now.
    Todo { start: Option<Until> },
}

pub(crate) struct Attention {
    streams: Arc<LedgerStreams>,
    /// This device, as its entries name it.
    device: Device,
    pub(crate) marks: Marks,
    pub(crate) skips: Skips,
    cache: RefCell<Cache>,
    undo: Vec<Undo>,
    next_undo: u64,
}

impl Attention {
    /// The ledger in `db`, read out.
    pub(crate) fn open(db: rho_db::RhoDb) -> (Self, UnboundedReceiver<LedgerEvent>) {
        futures::executor::block_on(drop_the_desk(&db));
        let ledger = futures::executor::block_on(Ledger::open(db.clone()));
        let device = Device(ledger.device().0);
        let mut marks = Marks::default();
        marks.apply(read_entries(&ledger.items(Channel::Facts)));
        if ledger.notes().is_empty() {
            // Notes were revisions in a log; every device carries its log's
            // newest of each into slots once. Remove with `Channel::Notes`.
            let mut old = Marks::default();
            old.apply_notes(read_notes(&ledger.items(Channel::Notes)));
            futures::executor::block_on(async {
                for rev in old.note_revs() {
                    ledger.put_note(*rev.note.as_bytes(), rev.encode()).await;
                }
            });
        }
        marks.apply_notes(
            ledger
                .notes()
                .iter()
                .filter_map(|plain| NoteRev::decode(plain)),
        );
        let (streams, events) = LedgerStreams::new(ledger, Box::new(keep_theirs));
        (
            Self {
                streams,
                device,
                marks,
                skips: Skips::default(),
                cache: RefCell::default(),
                undo: Vec::new(),
                next_undo: 0,
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

    pub(crate) fn pop_undo(&mut self) -> Option<Undo> {
        self.undo.pop()
    }

    #[cfg(test)]
    pub(crate) fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// The ledger's stream, for a host to carry.
    pub(crate) fn stream(&self) -> Arc<dyn rho_agent_hosts::HostStream> {
        self.streams.stream()
    }

    pub(crate) fn secret(&self) -> Option<Secret> {
        self.streams.ledger().secret()
    }

    /// Says `writes` on this device, and returns what takes each back and
    /// which nodes moved.
    fn say(&mut self, writes: Vec<Write>) -> (Vec<Takeback>, BTreeSet<NodeId>) {
        let now = jiff::Zoned::now();
        let mut takeback = Vec::new();
        let mut touched = BTreeSet::new();
        let mut entries = Vec::new();
        let mut notes = Vec::new();
        for write in writes {
            let at = self.marks.next_at(&now);
            match write {
                Write::Fact(fact) => {
                    let entry = Entry {
                        device: self.device,
                        at,
                        fact,
                    };
                    takeback.push(Takeback::Retract(entry.id()));
                    touched.extend(self.marks.apply([entry.clone()]));
                    entries.push(entry.encode());
                }
                Write::Note {
                    note,
                    body,
                    deleted,
                } => {
                    let held = self.marks.note(note).cloned();
                    let rev = NoteRev {
                        note,
                        device: self.device,
                        created: held.as_ref().map_or(at.timestamp(), |held| held.created),
                        body: body
                            .or_else(|| held.as_ref().map(|held| held.body.clone()))
                            .unwrap_or_default(),
                        deleted: deleted
                            .or_else(|| held.as_ref().map(|held| held.deleted))
                            .unwrap_or(false),
                        at,
                    };
                    // A note that did not exist before is taken back by
                    // deleting it.
                    takeback.push(Takeback::Note(held.unwrap_or_else(|| NoteRev {
                        deleted: true,
                        ..rev.clone()
                    })));
                    touched.extend(self.marks.apply_notes([rev.clone()]));
                    notes.push(rev);
                }
            }
        }
        futures::executor::block_on(async {
            if !entries.is_empty() {
                self.streams.append(Channel::Facts, entries).await;
            }
            for rev in notes {
                self.streams
                    .put_note(*rev.note.as_bytes(), rev.encode())
                    .await;
            }
        });
        takeback.reverse();
        (takeback, touched)
    }
}

/// Every device has carried the desk over into the ledger; its cache goes.
async fn drop_the_desk(db: &rho_db::RhoDb) {
    let mut write = db.write().await;
    for table in ["gui_desk_cells_v1", "gui_desk_bodies_v1"] {
        write.delete_table(table);
    }
    write.commit();
}

fn read_entries(items: &[Item]) -> Vec<Entry> {
    items
        .iter()
        .filter_map(|item| Entry::decode(&item.bytes))
        .collect()
}

/// Their revision replaces this device's only if it is newer.
fn keep_theirs(theirs: &[u8], mine: &[u8]) -> bool {
    match (NoteRev::decode(theirs), NoteRev::decode(mine)) {
        (Some(theirs), Some(mine)) => theirs.id() > mine.id(),
        (theirs, _) => theirs.is_some(),
    }
}

fn read_notes(items: &[Item]) -> Vec<NoteRev> {
    items
        .iter()
        .filter_map(|item| NoteRev::decode(&item.bytes))
        .collect()
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
            LedgerEvent::Appended(mut items) => {
                items.retain(|item| matches!(item.channel, Channel::Facts));
                let touched = self.attention.marks.apply(read_entries(&items));
                self.marks_moved(touched, cx);
            }
            LedgerEvent::Note(arrived) => {
                let Some(theirs) = NoteRev::decode(&arrived.plain) else {
                    return;
                };
                let lost = arrived
                    .replaced
                    .as_deref()
                    .and_then(NoteRev::decode)
                    .filter(|mine| {
                        arrived.replaced_unsent
                            && !mine.deleted
                            && mine.body != theirs.body
                            && !mine.body.trim().is_empty()
                    });
                let touched = self.attention.marks.apply_notes([theirs]);
                self.marks_moved(touched, cx);
                if let Some(mine) = lost {
                    // Written here while another device wrote it too: theirs
                    // is newer, and what was written here stays, as its own
                    // note.
                    let title = self
                        .attention
                        .marks
                        .get(&NodeId::Note(mine.note))
                        .title()
                        .to_owned();
                    self.write_marks(
                        vec![Write::Note {
                            note: uuid::Uuid::new_v4(),
                            body: Some(mine.body),
                            deleted: Some(false),
                        }],
                        cx,
                    );
                    self.append_message(
                        format!("notes: `{title}` changed on another device too; what was written here is kept as a new note"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }
            LedgerEvent::Unreadable { device } => self.append_message(
                format!(
                    "ledger: device {} writes with another key; what it says is not read",
                    device.map_or_else(
                        || "unknown".to_owned(),
                        |device| device.0.iter().map(|byte| format!("{byte:02x}")).collect()
                    )
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

    /// Says `writes`, and returns what takes them back.
    pub(crate) fn write_marks(
        &mut self,
        writes: Vec<Write>,
        cx: &mut Context<Self>,
    ) -> Vec<Takeback> {
        if writes.is_empty() {
            return Vec::new();
        }
        let (takeback, touched) = self.attention.say(writes);
        self.marks_moved(touched, cx);
        takeback
    }

    /// Takes back what a verdict said.
    pub(crate) fn take_back(&mut self, takeback: Vec<Takeback>, cx: &mut Context<Self>) {
        self.write_marks(takeback.into_iter().map(Takeback::write).collect(), cx);
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
            let said = marks.facts();
            let handled = said.seen_agent().unwrap_or(0);
            let verdict = rho_agents_client::Verdict {
                handled_through: rho_agent_types::AgentPos(handled),
                muted: said.muted(),
            };
            filings.push((
                *agent_id,
                rho_agents_client::AgentFiling {
                    muted: said.muted(),
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

    /// Everything the dealer reads, as it stands.
    fn with_sources<R>(&self, cx: &gpui::App, read: impl FnOnce(&Sources<'_>) -> R) -> R {
        let session = self.slack.session().map(|session| session.read(cx));
        let agents: HashMap<_, _> = self
            .agent2
            .iter()
            .map(|(id, (_, info))| (*id, info.clone()))
            .collect();
        let sources = Sources {
            agents: &agents,
            slack: session.map(|session| rank::Slack {
                model: session.model(),
                mirror: session.mirror(),
            }),
            marks: &self.attention.marks,
            skips: &self.attention.skips,
        };
        read(&sources)
    }

    /// What a node is called on a card, in Find, and on its own surface.
    pub(crate) fn node_title(&self, node: &NodeId, cx: &gpui::App) -> String {
        self.with_sources(cx, |sources| {
            rank::title(sources, node, &mut self.attention.cache.borrow_mut())
        })
    }

    /// Where a node is: a conversation for a Slack unit, the labels it
    /// carries for anything else.
    pub(crate) fn node_context(&self, node: &NodeId, cx: &gpui::App) -> String {
        self.with_sources(cx, |sources| rank::context(sources, node))
    }

    /// The ranking as it stands, with everything it weighed, into the
    /// journal: `dealt` is the card the user is shown, if any.
    pub(crate) fn journal_deal(
        &self,
        trigger: rho_journal::DealTrigger,
        dealt: Option<&NodeId>,
        cx: &gpui::App,
    ) {
        let now = jiff::Zoned::now();
        let (hand, trace) = self.with_sources(cx, |sources| {
            rank::rank_traced(sources, &now, &mut self.attention.cache.borrow_mut())
        });
        rho_journal::record(rho_journal::Event::Deal {
            trigger,
            occurred_at: now.timestamp().to_string(),
            zone: now.time_zone().iana_name().unwrap_or("unnamed").to_owned(),
            dealt: dealt.map(Self::journal_card_identity),
            hand: hand
                .cards
                .iter()
                .map(|card| rho_journal::DealtCard {
                    card: Self::journal_card_identity(&card.node),
                    kind: Self::journal_card_kind(card.kind),
                    priority: card.priority,
                    label: card.label.clone(),
                    title: card.title.clone(),
                    context: card.context.clone(),
                    cursor: card.cursor.clone(),
                    skipped: card.skipped,
                })
                .collect(),
            weighed: trace
                .nodes
                .into_iter()
                .map(|(node, weighed)| rho_journal::WeighedNode {
                    card: Self::journal_card_identity(&node),
                    outcome: weighed.outcome,
                    inputs: weighed
                        .inputs
                        .into_iter()
                        .map(|(key, value)| rho_journal::Input {
                            key: key.to_owned(),
                            value,
                        })
                        .collect(),
                    parts: weighed.parts,
                })
                .collect(),
            next_change: hand.next_change.map(|at| at.to_string()),
        });
    }

    /// The ranking as it stands.
    pub(crate) fn hand(&self, cx: &gpui::App) -> Hand {
        let now = jiff::Zoned::now();
        self.with_sources(cx, |sources| {
            rank::rank(sources, &now, &mut self.attention.cache.borrow_mut())
        })
    }

    /// The card for a node the reader is on: its card in the hand, or the
    /// node itself when the hand holds none, because reading a thing can
    /// be what quiets it and a verdict on what is on screen still lands.
    pub(crate) fn card_for(&self, node: &NodeId, cx: &gpui::App) -> Card {
        if let Some(card) = self
            .hand(cx)
            .cards
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
            title: self.node_title(node, cx),
            context: self.node_context(node, cx),
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
        tracing::debug!(node = %node.key(), ?verdict, "verdict");
        let mut slack_cursors = Vec::new();
        let mut slack_muted = None;
        let seen = self.seen(node, cx);
        // Slack keeps its own cursor too, moved here, so Slack's own apps
        // agree.
        if let NodeId::Slack(unit) = node
            && !matches!(verdict, Verdict::Snooze(_))
        {
            slack_cursors.extend(self.advance_slack_cursor(unit, None, cx));
            if verdict == Verdict::Mute {
                self.slack_set_unit_muted(unit, true, cx);
                slack_muted = Some(unit.clone());
            }
        }
        let node = node.clone();
        let fact = match verdict {
            Verdict::Done => Fact::Settled { node, seen },
            // Slack mutes the unit itself; here it is only settled.
            Verdict::Mute if matches!(node, NodeId::Slack(_)) => Fact::Settled { node, seen },
            Verdict::Mute => Fact::Mute { node },
            Verdict::Snooze(until) => Fact::Snooze { node, until },
            Verdict::Todo { start } => Fact::Todo { node, start, seen },
        };
        let takeback = self.write_marks(vec![fact.into()], cx);
        Some(Undo {
            sequence: 0,
            verb: String::new(),
            takeback,
            card: None,
            slack_cursors,
            slack_muted,
        })
    }

    /// How far the user has seen `node`: everything its source has now.
    pub(crate) fn seen(&self, node: &NodeId, cx: &gpui::App) -> Seen {
        match node {
            NodeId::Agent(agent_id) => Seen::Agent(
                self.agent2
                    .get(agent_id)
                    .map(|(_, info)| info.chat.iter().map(|event| event.seq).max().unwrap_or(0))
                    .unwrap_or_else(|| {
                        self.registry
                            .agent_digest(*agent_id)
                            .map_or(0, |digest| digest.newest.0)
                    }),
            ),
            NodeId::Slack(unit) => self
                .slack
                .session()
                .and_then(|session| {
                    let model = session.read(cx).model();
                    model
                        .unit(&rank::model_unit(unit))
                        .map(|facts| Seen::Slack(facts.newest.0.clone()))
                })
                .unwrap_or(Seen::Whole),
            _ => Seen::Whole,
        }
    }

    /// `shift-u`: the last verdict, taken back.
    pub(crate) fn undo_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone_snap_in_progress() {
            return;
        }
        let Some(undo) = self.attention.pop_undo() else {
            self.echo("nothing to undo", StyleClass::SystemInfo, cx);
            return;
        };
        self.restore_slack_cursors(&undo.slack_cursors, cx);
        if let Some(unit) = &undo.slack_muted {
            self.slack_set_unit_muted(unit, false, cx);
        }
        self.take_back(undo.takeback, cx);
        let Some((node, verdict)) = undo.card else {
            if undo.slack_cursors.is_empty() {
                self.echo(&format!("undid {}", undo.verb), StyleClass::SystemInfo, cx);
                self.invalidate_dealer_signals(cx);
                return;
            }
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
        self.attention.skips.clear(&node);
        let card = self.card_for(&node, cx);
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
                    writes.push(Write::from(Fact::Label {
                        label: id,
                        name: names[depth].to_owned(),
                        parent,
                    }));
                    minted.insert(prefix, id);
                    id
                }
            };
            parent = Some(id);
        }
        self.write_marks(writes, cx);
        parent
    }

    /// `space d`: deletes the note or label in view, and every label under
    /// a label. Deleting is a mark like any other, so it syncs, and
    /// `shift-u` takes it back.
    pub(crate) fn delete_made(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self
            .surface_node(cx)
            .filter(|node| matches!(node, NodeId::Note(_) | NodeId::Label(_)))
        else {
            self.echo(
                "delete: no note or label in view",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let title = self.node_title(&node, cx);
        let mut doomed = vec![node.clone()];
        if let NodeId::Label(label) = node {
            let mut under = vec![label];
            while let Some(label) = under.pop() {
                for sublabel in self.attention.marks.sublabels(Some(label)) {
                    doomed.push(NodeId::Label(sublabel));
                    under.push(sublabel);
                }
            }
        }
        let said = match (&node, doomed.len() - 1) {
            (NodeId::Note(_), _) => format!("deleted note: {title}"),
            (_, 0) => format!("deleted label: {title}"),
            (_, under) => format!("deleted label: {title}, and {under} under it"),
        };
        let writes = doomed
            .iter()
            .map(|node| match node {
                NodeId::Note(note) => Write::Note {
                    note: *note,
                    body: None,
                    deleted: Some(true),
                },
                NodeId::Label(label) => Fact::Unlabel { label: *label }.into(),
                _ => unreachable!("only notes and labels are deleted"),
            })
            .collect();
        let takeback = self.write_marks(writes, cx);
        self.attention.push_undo(Undo {
            sequence: 0,
            verb: said.clone(),
            takeback,
            card: None,
            slack_cursors: Vec::new(),
            slack_muted: None,
        });
        self.close_current_surface(window, cx);
        self.echo(
            &format!("{said} · shift-u undoes"),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// `space r`: the label in view, renamed or moved to a new path. Every
    /// label on the way that nobody has made yet is made.
    pub(crate) fn prompt_move_label(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(NodeId::Label(label)) = self.surface_node(cx) else {
            self.echo("move: no label in view", StyleClass::SystemInfo, cx);
            return;
        };
        let from = self.attention.marks.label_path(label);
        let under = format!("{from}/");
        self.pending_filing_destinations = self
            .attention
            .marks
            .labels()
            .into_iter()
            .filter(|(_, path)| *path != from && !path.starts_with(&under))
            .map(|(_, path)| (path, "label".to_owned()))
            .collect();
        self.open_prompt(
            format!("move {from} to:"),
            std::rc::Rc::new(|workspace, needle, _cx| {
                let needle = needle.to_lowercase();
                workspace
                    .pending_filing_destinations
                    .iter()
                    .filter(|(value, _)| value.to_lowercase().contains(&needle))
                    .map(|(value, description)| crate::minibuffer::Candidate {
                        value: value.clone(),
                        description: description.clone(),
                    })
                    .collect()
            }),
            std::rc::Rc::new(move |workspace, path, _window, cx| {
                workspace.move_label(label, &path, cx);
            }),
            window,
            cx,
        );
        self.set_prompt_complete_whole_input();
    }

    pub(crate) fn move_label(&mut self, label: uuid::Uuid, path: &str, cx: &mut Context<Self>) {
        let from = self.attention.marks.label_path(label);
        let names = path_names(path);
        let Some((name, parents)) = names.split_last() else {
            self.echo("move: no path", StyleClass::SystemInfo, cx);
            return;
        };
        let to = names.join("/");
        if to == from {
            return;
        }
        if to.starts_with(&format!("{from}/")) {
            self.echo(
                &format!("move: {to} is under {from}"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        if self.attention.marks.label_at(&to).is_some() {
            self.echo(
                &format!("move: {to} already exists"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let parent = match parents.is_empty() {
            true => None,
            false => self.mint_label(&parents.join("/"), cx),
        };
        let takeback = self.write_marks(
            vec![
                Fact::Label {
                    label,
                    name: (*name).to_owned(),
                    parent,
                }
                .into(),
            ],
            cx,
        );
        let said = format!("moved label: {from} to {to}");
        self.attention.push_undo(Undo {
            sequence: 0,
            verb: said.clone(),
            takeback,
            card: None,
            slack_cursors: Vec::new(),
            slack_muted: None,
        });
        self.echo(&said, StyleClass::SystemInfo, cx);
    }

    /// Puts the label at `path` on `node`, or takes it off when the node
    /// already carries it. Says whether it is now on, with the writes that
    /// undo it.
    pub(crate) fn toggle_label(
        &mut self,
        node: &NodeId,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<(bool, Vec<Takeback>)> {
        let existing = self.attention.marks.label_at(path.trim());
        let carried =
            existing.is_some_and(|label| self.attention.marks.get(node).labels.contains(&label));
        let label = match existing {
            Some(label) => label,
            None => self.mint_label(path, cx)?,
        };
        let undo = self.write_marks(
            vec![
                Fact::Labeled {
                    node: node.clone(),
                    label,
                    present: !carried,
                }
                .into(),
            ],
            cx,
        );
        Some((!carried, undo))
    }

    /// A new note, made from `area` the way any new thing is.
    pub(crate) fn create_note(&mut self, area: Option<&NodeId>, cx: &mut Context<Self>) -> NodeId {
        let note = uuid::Uuid::new_v4();
        let node = NodeId::Note(note);
        let mut writes = vec![Write::Note {
            note,
            body: Some(String::new()),
            deleted: None,
        }];
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
            self.append_secret_message(
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
                    workspace.append_secret_message(
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
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_secret();
        }
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
