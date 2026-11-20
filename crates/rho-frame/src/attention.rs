//! What the user said, and how it is kept.
//!
//! What the user says lives in the ledger, which syncs it sealed between
//! their devices through the hosts: facts, every one kept, a note's text
//! among them. The dealer reads them with what every system claims
//! into the hand the desk shows and a pull deals from. A verdict is a fact the
//! user said, and undo retracts the fact.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedReceiver;
use gpui::{Context, Window};
use rho_dealer::facts::{Device, Entry, EntryId, Fact, Seen};
use rho_dealer::{Address, Marks, Skips, Until};
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Channel, Item, Ledger, Pairing, Secret};
use rho_window::style::StyleClass;

use crate::frame::Frame;
use crate::minibuffer::{Candidate, token_start};

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

/// What verdict undo takes back.
pub struct Undo {
    pub verb: String,
    pub takeback: Vec<Takeback>,
    pub address: Address,
    pub verdict: rho_journal::DealerVerdict,
}

pub struct Attention {
    streams: Arc<LedgerStreams>,
    /// This device, as its entries name it.
    device: Device,
    pub marks: Marks,
    pub skips: Skips,
    undo: Vec<Undo>,
}

impl Attention {
    /// The ledger in `db`, read out.
    pub fn open(db: rho_db::RhoDb) -> (Self, UnboundedReceiver<LedgerEvent>) {
        let ledger = futures::executor::block_on(Ledger::open(db));
        let device = Device(ledger.device().0);
        let mut marks = Marks::default();
        marks.apply(read_entries(&ledger.items(Channel::Facts)));
        let (streams, events) = LedgerStreams::new(ledger);
        let attention = Self {
            streams,
            device,
            marks,
            skips: Skips::default(),
            undo: Vec::new(),
        };
        (attention, events)
    }

    /// Keeps a verdict for `space u`.
    pub fn push_undo(&mut self, undo: Undo) {
        self.undo.push(undo);
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
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

    pub fn pairing(&self) -> Pairing {
        self.streams.ledger().pairing()
    }

    pub fn secret(&self) -> Option<Secret> {
        self.streams.ledger().secret()
    }

    /// Takes the phrase the user typed, and everything the other devices
    /// wrote that it opens.
    pub fn pair(&self, secret: Secret) -> anyhow::Result<()> {
        futures::executor::block_on(self.streams.set_secret(secret))
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
            // The status line says it for as long as it lasts.
            LedgerEvent::NeedsKey => (BTreeSet::new(), Vec::new()),
            // The frame shows it, once.
            LedgerEvent::SecretMade(_) => (BTreeSet::new(), Vec::new()),
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

    pub(crate) fn ledger_event(
        &mut self,
        event: LedgerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let LedgerEvent::SecretMade(secret) = &event {
            self.show_secret(
                *secret,
                "write down your secret phrase · it pairs your other devices · enter:",
                window,
                cx,
            );
        }
        let (touched, said) = self.attention.ledger_event(event);
        for line in said {
            self.echo(&line, StyleClass::SystemInfo, cx);
        }
        self.marks_moved(touched, cx);
        // Whether this device is paired shows on the status line.
        cx.notify();
    }

    /// The phrase on the prompt line, for the user to write down: when the
    /// first device makes it, and by name (`show my secret phrase`) on a
    /// device that holds it. Enter or escape puts it away. The phrase is a
    /// row under the question rather than the question itself, since the
    /// journal keeps every question asked.
    pub(crate) fn show_secret(
        &mut self,
        secret: Secret,
        question: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let phrase = Candidate {
            value: secret.to_words(),
            description: "your secret phrase".to_owned(),
        };
        self.open_prompt(
            question,
            Rc::new(move |_, _, _| vec![phrase.clone()]),
            Rc::new(|_, _, _, _| {}),
            window,
            cx,
        );
        if let Some(minibuffer) = self.minibuffer_mut() {
            minibuffer.set_secret();
        }
    }

    /// `pair this device`: asks for the phrase another device made, word
    /// by word with completion. A phrase that is wrong asks again with
    /// what was typed, and says what is wrong with it.
    pub(crate) fn prompt_pair(
        &mut self,
        typed: String,
        problem: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = match problem {
            Some(problem) => format!("secret phrase ({problem}):"),
            None => "secret phrase:".to_owned(),
        };
        self.open_prompt(
            prompt,
            Rc::new(|_, input, _| {
                let word = &input[token_start(input)..];
                if word.is_empty() {
                    return Vec::new();
                }
                Secret::words_starting(&word.to_lowercase())
                    .map(|word| Candidate {
                        value: word.to_owned(),
                        description: String::new(),
                    })
                    .collect()
            }),
            Rc::new(|frame, input, window, cx| {
                let paired = Secret::from_words(&input)
                    .and_then(|secret| frame.attention.pair(secret).map_err(|e| e.to_string()));
                match paired {
                    Ok(()) => frame.echo(
                        "paired: this device holds what your other devices said and wrote",
                        StyleClass::SystemInfo,
                        cx,
                    ),
                    Err(problem) => frame.prompt_pair(input, Some(problem), window, cx),
                }
                cx.notify();
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = self.minibuffer_mut() {
            minibuffer.set_secret();
            minibuffer.complete_last_word();
            minibuffer.set_input(typed, window, cx);
        }
    }
}

fn read_entries(items: &[Item]) -> Vec<Entry> {
    items
        .iter()
        .filter_map(|item| Entry::decode(&item.bytes))
        .collect()
}
