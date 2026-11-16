//! What the user said, and how it is kept.
//!
//! What the user says lives in the ledger, which syncs it sealed between
//! their devices through the hosts: facts, every one kept, a note's text
//! among them. The dealer reads them with what every system claims
//! into the hand Home shows and a pull deals from. A verdict is a fact the
//! user said; a system may do something of its own about it too (Slack
//! moves its cursor), and hands back how to undo that.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedReceiver;
use rho_dealer::facts::{Device, Entry, EntryId, Fact, Seen};
use rho_dealer::{Address, Marks, Skips, Until};
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Channel, Item, Ledger, Secret};
use senax_encoder::Decode;

use crate::frame::Frame;
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

/// What takes one write back: a retract of its entry. For a note's
/// revision, the revision before it shows again.
#[derive(Clone, Debug)]
pub struct Takeback(pub EntryId);

impl Takeback {
    pub fn write(self) -> Write {
        Write::Fact(Fact::Retract { of: self.0 })
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
}

impl Verdict {
    pub fn journal(&self) -> rho_journal::DealerVerdict {
        match self {
            Self::Done => rho_journal::DealerVerdict::Done,
            Self::Mute => rho_journal::DealerVerdict::Mute,
            Self::Snooze(_) => rho_journal::DealerVerdict::Defer,
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

pub struct Attention {
    streams: Arc<LedgerStreams>,
    /// This device, as its entries name it.
    device: Device,
    pub marks: Marks,
    pub skips: Skips,
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
        let slot_notes = ledger.notes();
        let (streams, events) = LedgerStreams::new(ledger, Box::new(|_, _| false));
        let mut attention = Self {
            streams,
            device,
            marks,
            skips: Skips::default(),
            undo: Vec::new(),
            next_undo: 0,
        };
        attention.adopt_slot_notes(&slot_notes);
        (attention, events)
    }

    /// Temp migration: notes used to sync as one sealed blob per note in a
    /// slot on each host, and now are `Wrote` facts in the log. Each note
    /// this device still holds only as a slot blob is said once as a fact.
    /// Two devices that both do this say the same text under the same id,
    /// which comes to one note. Goes with the slot tables and the `Put` and
    /// `Slot` frames once every device has run this.
    fn adopt_slot_notes(&mut self, blobs: &[Vec<u8>]) {
        let writes: Vec<Write> = blobs
            .iter()
            .filter_map(|plain| SlotNote::decode(plain))
            .filter(|rev| !rev.deleted && !rev.body.trim().is_empty())
            .filter(|rev| self.marks.get(&Address::note(rev.note)).body.is_none())
            .map(|rev| {
                Write::Fact(Fact::Wrote {
                    note: rev.note,
                    text: rev.body,
                    created: rev.created,
                })
            })
            .collect();
        if !writes.is_empty() {
            tracing::info!(
                notes = writes.len(),
                "notes move from slots into the ledger log"
            );
            self.say(writes);
        }
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
        for write in writes {
            let facts = match write {
                Write::Fact(fact) => vec![fact],
                Write::Note {
                    note,
                    body,
                    deleted,
                } => {
                    let held = self.marks.get(&Address::note(note));
                    let created = held.created_ms.map_or(now.timestamp(), |ms| {
                        jiff::Timestamp::from_millisecond(ms).expect("a created instant read back")
                    });
                    // A note brought back is its text said again.
                    let restored = deleted == Some(false) && held.deleted && body.is_none();
                    let text =
                        body.or_else(|| restored.then(|| held.body.clone().unwrap_or_default()));
                    let mut facts = Vec::new();
                    if let Some(text) = text {
                        facts.push(Fact::Wrote {
                            note,
                            text,
                            created,
                        });
                    }
                    if deleted == Some(true) {
                        facts.push(Fact::Deleted { note });
                    }
                    facts
                }
            };
            for fact in facts {
                let entry = Entry {
                    device: self.device,
                    at: self.marks.next_at(&now),
                    fact,
                };
                takeback.push(Takeback(entry.id()));
                touched.extend(self.marks.apply([entry.clone()]));
                entries.push(entry.encode());
            }
        }
        futures::executor::block_on(async {
            if !entries.is_empty() {
                self.streams.append(Channel::Facts, entries).await;
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
            // Slot notes are what older builds put; what they hold has moved
            // into the log, so nothing arrives this way any more.
            LedgerEvent::Note(_) => Default::default(),
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
        }
    }
}

impl Frame {
    /// What the user said, as this device holds it: every fact, and each
    /// note's newest revision ([`Marks::note`]). A system whose things are
    /// the user's own (notes) reads them here and says new ones
    /// through [`Frame::write_marks`]; a note's revision carries only text
    /// the user typed.
    pub fn marks(&self) -> &Marks {
        &self.attention.marks
    }
}

fn read_entries(items: &[Item]) -> Vec<Entry> {
    items
        .iter()
        .filter_map(|item| Entry::decode(&item.bytes))
        .collect()
}

/// A note as the slot form held it, read only by the temp migration.
#[derive(Decode)]
struct SlotNote {
    note: uuid::Uuid,
    #[allow(dead_code)]
    device: Device,
    #[allow(dead_code)]
    at: jiff::Zoned,
    created: jiff::Timestamp,
    body: String,
    deleted: bool,
}

impl SlotNote {
    fn decode(mut bytes: &[u8]) -> Option<Self> {
        senax_encoder::decode(&mut bytes).ok()
    }
}
