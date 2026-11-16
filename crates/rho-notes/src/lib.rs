//! Notes as the frame shows them.
//!
//! A note is the user's own text, a private message: the ledger keeps a
//! revision per save and the newest is the note. It is the user's marks,
//! which the frame holds ([`Frame::marks`]), so [`NoteSystem`] keeps
//! nothing of it: it reads the marks when it needs them and says what the
//! user typed through [`Frame::write_marks`]. All it holds is what was
//! typed and not yet written. The machine never writes a note's text:
//! every revision this crate says is a buffer's text as the user left it.
//!
//! Addresses are `note:<id>`
//! ([SPEC-note-addresses](../specs/SPEC-note-addresses.md)). A deadline on
//! a note is the dealer's to rank, like on anything else; a bare reminder
//! is a note with a deadline. This system only opens the card.

mod view;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Subscription, Task, Window};
use rho_dealer::facts::Fact;
use rho_dealer::{Address, Addressed, Marks};
use rho_frame::attention::Write;
use rho_frame::{AnySurface, Found, Frame, FrameLink, Point, RoomRow, Row, System};
pub use view::NoteView;

/// How long after the last keystroke a note's text is written.
const SAVE_AFTER: Duration = Duration::from_millis(700);

/// What a note without a first line is called.
const UNTITLED: &str = "untitled note";

/// A note, by the id it was minted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoteId(pub uuid::Uuid);

impl Addressed for NoteId {
    const KIND: &'static str = Address::NOTE;

    fn rest(&self) -> String {
        self.0.simple().to_string()
    }

    /// A UUID in its simple form, and only that, so one note has one
    /// address.
    fn from_rest(rest: &str) -> Option<Self> {
        let id = uuid::Uuid::try_parse(rest).ok()?;
        (id.simple().to_string() == rest).then_some(Self(id))
    }
}

/// The text a note's surface edits: its body.
fn text_of(marks: &Marks, note: NoteId) -> String {
    marks.get(&note.address()).body.clone().unwrap_or_default()
}

/// What a note is called: its first line.
fn note_title(marks: &Marks, note: &Address) -> String {
    match marks.get(note).title() {
        "" => UNTITLED.to_owned(),
        title => title.to_owned(),
    }
}

/// The note's new revision, when the text the user left in its buffer
/// is not what the ledger already says.
fn write_for(marks: &Marks, note: NoteId, text: String) -> Option<Write> {
    (text_of(marks, note) != text).then_some(Write::Note {
        note: note.0,
        body: Some(text),
        deleted: None,
    })
}

/// What a new note says about where it was made: that it is about it. No
/// text: the note has none until the user types it.
fn new_note_marks(note: NoteId, about: Option<&Address>) -> Vec<Write> {
    about
        .map(|about| {
            Write::Fact(Fact::About {
                node: note.address(),
                about: Some(about.clone()),
            })
        })
        .into_iter()
        .collect()
}

/// The private rooms: every note that is about nothing, the one last
/// written first.
fn private_rooms(marks: &Marks) -> Vec<RoomRow> {
    marks
        .private_roots()
        .into_iter()
        .map(|(address, held)| RoomRow {
            address: address.clone(),
            name: note_title(marks, address),
            tag: "private".to_owned(),
            waiting: String::new(),
            at: held.written_ms.or(held.created_ms).unwrap_or(0).max(0) as u64,
        })
        .collect()
}

/// Every note, for `find`.
fn found(marks: &Marks) -> Vec<Found> {
    marks
        .notes()
        .map(|(address, held)| Found {
            address: address.clone(),
            title: note_title(marks, address),
            description: "note".to_owned(),
            recency: held.written_ms.or(held.created_ms).unwrap_or(0).max(0) as u64,
        })
        .collect()
}

/// What the user typed and has not been written yet.
struct Pending {
    text: String,
    _save: Task<()>,
}

pub struct NoteSystem {
    frame: FrameLink,
    pending: HashMap<NoteId, Pending>,
    _quit: Subscription,
}

impl NoteSystem {
    pub fn new(frame: FrameLink, cx: &mut Context<Self>) -> Self {
        let this = cx.weak_entity();
        let app: &App = cx;
        // What was typed in the last moment before quitting is written
        // now: the window is gone by the time a deferred write would run.
        let quit = app.on_app_quit(move |cx| {
            if let Some(this) = this.upgrade() {
                let (frame, pending) = this.update(cx, |this, _| {
                    (this.frame.clone(), std::mem::take(&mut this.pending))
                });
                frame.update(cx, |frame, _, cx| {
                    for (note, pending) in pending {
                        write(frame, note, pending.text, cx);
                    }
                });
            }
            std::future::ready(())
        });
        Self {
            frame,
            pending: HashMap::new(),
            _quit: quit,
        }
    }

    /// Whether the user typed into `note` and it is not written yet: then
    /// their text is the newest, whatever else arrives.
    pub fn typing(&self, note: NoteId) -> bool {
        self.pending.contains_key(&note)
    }

    /// The user typed: `text` is written once they stop.
    fn edited(&mut self, note: NoteId, text: String, cx: &mut Context<Self>) {
        let save = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |this, cx| this.save(note, cx));
        });
        self.pending.insert(note, Pending { text, _save: save });
    }

    /// Writes what was typed into `note`. It leaves the pending set only
    /// as it is written, so a surface never takes another device's text
    /// over it in between.
    fn save(&mut self, note: NoteId, cx: &mut Context<Self>) {
        let this = cx.entity();
        self.frame.with(cx, move |frame, _, cx| {
            let Some(pending) = this.update(cx, |this, _| this.pending.remove(&note)) else {
                return;
            };
            write(frame, note, pending.text, cx);
        });
    }
}

/// Says the text the user left in `note`, if it says anything new.
fn write(frame: &mut Frame, note: NoteId, text: String, cx: &mut Context<Frame>) {
    if let Some(write) = write_for(frame.marks(), note, text) {
        frame.write_marks(vec![write], cx);
    }
}

/// `reply privately`: a new note about what the point is on, which the
/// thread shows under it as the user's own line. On a note it is a note
/// under that note.
fn reply_privately(
    subject: &Address,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let note = new_note(frame, Some(subject), cx);
    frame.open(note.address(), window, cx);
}

/// A new note about `about`, or about nothing: a private room. It has no
/// text, and is nothing in the ledger until it is about something or
/// typed into.
fn new_note(frame: &mut Frame, about: Option<&Address>, cx: &mut Context<Frame>) -> NoteId {
    let note = NoteId(uuid::Uuid::new_v4());
    frame.write_marks(new_note_marks(note, about), cx);
    note
}

/// `new note`: a private room of its own, opened to type into.
fn make_note(frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    let note = new_note(frame, None, cx);
    frame.open(note.address(), window, cx);
}

impl System for NoteSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[Address::NOTE]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let note = NoteId::from_address(address)?;
        let system = cx.entity();
        let frame = self.frame.clone();
        let view = cx.new(|cx| NoteView::new(system, frame, note, window, cx));
        Some(AnySurface::new(view))
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, _cx: &mut Context<Self>) -> Vec<Row> {
        let mut rows = Vec::new();
        if let Some(subject) = point.subject.clone() {
            rows.push(Row::run(
                "n",
                "reply privately",
                move |frame, window, cx| reply_privately(&subject, frame, window, cx),
            ));
        }
        rows.push(Row::run("w", "new note", make_note));
        rows
    }

    /// Only asked for a note whose first line is empty: the frame names
    /// notes from the marks.
    fn name(&self, address: &Address, _cx: &App) -> Option<(String, String)> {
        NoteId::from_address(address).map(|_| (UNTITLED.to_owned(), String::new()))
    }

    fn find(&self, marks: &Marks, _cx: &App) -> Vec<Found> {
        found(marks)
    }

    fn rooms(&self, marks: &Marks, _cx: &App) -> Vec<RoomRow> {
        private_rooms(marks)
    }

    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, cx: &mut Context<Self>) {
        // A note surface reads its text from the marks.
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
