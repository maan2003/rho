//! What the user said, and how it is kept.
//!
//! What the user says lives in the ledger, which syncs it sealed between
//! their devices through the hosts: facts, every one kept, and notes, a
//! revision per save. The dealer reads them with what every system claims
//! into the hand Home shows and a pull deals from. A verdict is a fact the
//! user said; a system may do something of its own about it too (Slack
//! moves its cursor), and hands back how to undo that.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedReceiver;
use rho_dealer::facts::{Device, Entry, EntryId, Fact, Seen};
use rho_dealer::notes::NoteRev;
use rho_dealer::{Address, Marks, Skips, Until};
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Channel, Item, Ledger, Secret};

use crate::system::Reversal;

/// Something the user says: a fact, or a note's new text or state.
#[derive(Clone, Debug)]
pub enum Write {
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
pub enum Takeback {
    Retract(EntryId),
    Note(NoteRev),
}

impl Takeback {
    pub fn write(self) -> Write {
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

/// The verdicts a card can take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Dealt with: everything the source has said so far is handled.
    Done,
    /// Nothing from this address reaches the user again.
    Mute,
    /// Out of the way until then.
    Snooze(Until),
    /// Handled here, and on the user's plate until done: from `start`, or
    /// from now.
    Todo { start: Option<Until> },
    /// Out of the way on the named pile, until the user opens it.
    Pile(String),
}

impl Verdict {
    pub fn journal(&self) -> rho_journal::DealerVerdict {
        match self {
            Self::Done | Self::Todo { .. } => rho_journal::DealerVerdict::Done,
            Self::Mute => rho_journal::DealerVerdict::Mute,
            Self::Snooze(_) | Self::Pile(_) => rho_journal::DealerVerdict::Defer,
        }
    }
}

/// What `shift-u` takes back: what a verdict said, and what it did
/// outside the ledger.
pub struct Undo {
    /// Which verdict this is, in the order they were taken.
    pub sequence: u64,
    pub verb: String,
    /// What takes back what it said.
    pub takeback: Vec<Takeback>,
    /// The address the verdict was on, dealt again by the undo, and what
    /// the journal called the verdict. `None` for a batch with no card of
    /// its own.
    pub card: Option<(Address, rho_journal::DealerVerdict)>,
    /// What the systems did about it, undone in order.
    pub reversals: Vec<Reversal>,
}

/// The pile a pull deals from instead of the hand, and what this sitting
/// already passed over in it.
pub struct OpenPile {
    /// `None` for everything snoozed.
    pub name: Option<String>,
    pub passed: std::collections::HashSet<Address>,
}

pub struct Attention {
    streams: Arc<LedgerStreams>,
    /// This device, as its entries name it.
    device: Device,
    pub marks: Marks,
    pub skips: Skips,
    pub open_pile: Option<OpenPile>,
    undo: Vec<Undo>,
    next_undo: u64,
}

impl Attention {
    /// The ledger in `db`, read out.
    pub fn open(db: rho_db::RhoDb) -> (Self, UnboundedReceiver<LedgerEvent>) {
        let ledger = futures::executor::block_on(Ledger::open(db));
        let device = Device(ledger.device().0);
        let mut marks = Marks::default();
        marks.apply(read_entries(&ledger.items(Channel::Facts)));
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
                open_pile: None,
                undo: Vec::new(),
                next_undo: 0,
            },
            events,
        )
    }

    /// Keeps a verdict for `shift-u`, numbered after every one before it.
    pub fn push_undo(&mut self, mut undo: Undo) -> u64 {
        undo.sequence = self.next_undo;
        self.next_undo += 1;
        let sequence = undo.sequence;
        self.undo.push(undo);
        sequence
    }

    pub fn pop_undo(&mut self) -> Option<Undo> {
        self.undo.pop()
    }

    /// The ledger's stream, for a host to carry.
    pub fn stream(&self) -> Arc<dyn rho_agent_hosts::HostStream> {
        self.streams.stream()
    }

    pub fn streams(&self) -> &Arc<LedgerStreams> {
        &self.streams
    }

    pub fn secret(&self) -> Option<Secret> {
        self.streams.ledger().secret()
    }

    /// Says `writes` on this device, and returns what takes each back and
    /// which addresses moved.
    pub fn say(&mut self, writes: Vec<Write>) -> (Vec<Takeback>, BTreeSet<Address>) {
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

    /// What a ledger event changes in the marks, and what it says.
    pub fn ledger_event(&mut self, event: LedgerEvent) -> (BTreeSet<Address>, Vec<String>) {
        match event {
            LedgerEvent::Appended(mut items) => {
                items.retain(|item| matches!(item.channel, Channel::Facts));
                (self.marks.apply(read_entries(&items)), Vec::new())
            }
            LedgerEvent::Note(arrived) => {
                let Some(theirs) = NoteRev::decode(&arrived.plain) else {
                    return Default::default();
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
                let mut touched = self.marks.apply_notes([theirs]);
                let mut said = Vec::new();
                if let Some(mine) = lost {
                    // Written here while another device wrote it too: theirs
                    // is newer, and what was written here stays, as its own
                    // note.
                    let title = self.marks.get(&Address::note(mine.note)).title().to_owned();
                    let (_, kept) = self.say(vec![Write::Note {
                        note: uuid::Uuid::new_v4(),
                        body: Some(mine.body),
                        deleted: Some(false),
                    }]);
                    touched.extend(kept);
                    said.push(format!(
                        "notes: `{title}` changed on another device too; what was written here is kept as a new note"
                    ));
                }
                (touched, said)
            }
            LedgerEvent::Unreadable { device } => (
                BTreeSet::new(),
                vec![format!(
                    "ledger: device {} writes with another key; what it says is not read",
                    device.map_or_else(
                        || "unknown".to_owned(),
                        |device| device.0.iter().map(|byte| format!("{byte:02x}")).collect()
                    )
                )],
            ),
            LedgerEvent::NeedsKey => (
                BTreeSet::new(),
                vec![
                    "ledger: other devices have written marks; `space s s` enters the secret phrase they share"
                        .to_owned(),
                ],
            ),
        }
    }

    /// The fact a verdict is, given how far the user has seen.
    pub fn fact(address: &Address, verdict: &Verdict, seen: Seen) -> Fact {
        let node = address.clone();
        match verdict.clone() {
            Verdict::Done => Fact::Settled { node, seen },
            Verdict::Mute => Fact::Mute { node },
            Verdict::Snooze(until) => Fact::Snooze { node, until },
            Verdict::Todo { start } => Fact::Todo { node, start, seen },
            Verdict::Pile(pile) => Fact::Piled { node, pile },
        }
    }
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

/// A label path split into its names, with empty ones dropped.
pub fn path_names(path: &str) -> Vec<&str> {
    path.split('/')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}
