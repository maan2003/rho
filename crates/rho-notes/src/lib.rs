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
//! Notes live apart from the work: no note is a room on Home or a place
//! in `find`. Their one door is the command `notes`, which opens
//! [`NotesView`], every note by its first line, the last written first.
//!
//! Addresses are `note:<id>`
//! ([SPEC-note-addresses](../specs/SPEC-note-addresses.md)), and the list
//! of them is `notes:all`. A deadline on
//! a note is the dealer's to rank, like on anything else; a bare reminder
//! is a note with a deadline. This system only opens the card.

pub(crate) mod list;
mod view;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Subscription, Task, Window};
pub use list::NotesView;
use rho_dealer::facts::Fact;
use rho_dealer::{Address, Addressed, Marks};
use rho_frame::attention::Write;
use rho_frame::{AnySurface, Found, Frame, FrameLink, Point, Row, System};
use rho_window::style::StyleClass;
pub use view::NoteView;

/// How long after the last keystroke a note's text is written.
const SAVE_AFTER: Duration = Duration::from_millis(700);

/// What a note without a first line is called.
const UNTITLED: &str = "untitled note";

/// What `reply privately` says with the point on nothing a reply could
/// be under.
const NOT_ON_A_MESSAGE: &str = "reply privately: put the cursor on a message";

/// The kind of the one list of every note.
const NOTES: &str = "notes";

/// The list of every note.
fn notes_address() -> Address {
    Address::new(NOTES, "all")
}

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

/// What a note with `text` is called: its first line, or [`UNTITLED`].
fn title_of(text: &str) -> String {
    match text.lines().next().map(str::trim) {
        None | Some("") => UNTITLED.to_owned(),
        Some(title) => title.to_owned(),
    }
}

/// What a note is called, as written: its first line.
fn note_title(marks: &Marks, note: &Address) -> String {
    title_of(marks.get(note).body.as_deref().unwrap_or_default())
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

/// A note as `notes` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    note: Address,
    title: String,
    /// For a private reply: where it shows, by the name of the thread
    /// that shows the message it is under, when that can be named.
    reply: Option<Option<String>>,
}

/// Every note, about something or about nothing, by its first line: the
/// one last written first. `place` names where a reply about an address
/// shows.
fn listed(marks: &Marks, place: impl Fn(&Address) -> Option<String>) -> Vec<Listed> {
    let mut notes: Vec<_> = marks.notes().collect();
    notes.sort_by_key(|(address, held)| {
        (
            std::cmp::Reverse(held.written_ms.or(held.created_ms)),
            *address,
        )
    });
    notes
        .into_iter()
        .map(|(address, held)| Listed {
            note: address.clone(),
            title: note_title(marks, address),
            reply: held.about.as_ref().map(&place),
        })
        .collect()
}

/// Every note as the palette lists it, by the first line `title` reads:
/// a private reply as one, a note about nothing as a note, each as new as
/// its last revision.
fn found(marks: &Marks, title: impl Fn(NoteId) -> String) -> Vec<Found> {
    marks
        .notes()
        .filter_map(|(address, held)| {
            Some(Found {
                address: address.clone(),
                title: title(NoteId::from_address(address)?),
                description: match held.about {
                    Some(_) => "private reply".to_owned(),
                    None => "note".to_owned(),
                },
                recency: held.written_ms.or(held.created_ms).unwrap_or(0).max(0) as u64,
            })
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
        list::bind_keys(cx);
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

    /// Writes what was typed into `note`, if anything. It leaves the pending
    /// set only as it is written, so a surface never takes another device's
    /// text over it in between.
    pub(crate) fn save(&mut self, note: NoteId, cx: &mut Context<Self>) {
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

/// A new note about `about`, or about nothing. It has no text, and is
/// nothing in the ledger until it is about something or typed into.
fn new_note(frame: &mut Frame, about: Option<&Address>, cx: &mut Context<Frame>) -> NoteId {
    let note = NoteId(uuid::Uuid::new_v4());
    frame.write_marks(new_note_marks(note, about), cx);
    note
}

/// `new note`: a note about nothing, opened to type into.
fn make_note(frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    let note = new_note(frame, None, cx);
    frame.open(note.address(), window, cx);
}

impl System for NoteSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[Address::NOTE, NOTES]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let system = cx.entity();
        let frame = self.frame.clone();
        if *address == notes_address() {
            let view = cx.new(|cx| NotesView::new(system, frame, window, cx));
            return Some(AnySurface::new(view));
        }
        let note = NoteId::from_address(address)?;
        let view = cx.new(|cx| NoteView::new(system, frame, note, window, cx));
        Some(AnySurface::new(view))
    }

    /// `reply privately` is listed everywhere, so it is found by name;
    /// with the point on nothing a reply could be under, it says so rather
    /// than make a note about nothing.
    fn rows(&mut self, point: &Point, _window: &mut Window, _cx: &mut Context<Self>) -> Vec<Row> {
        let mut rows = Vec::new();
        let subject = point.subject.clone();
        rows.push(Row::run(
            "n",
            "reply privately",
            move |frame, window, cx| match &subject {
                Some(subject) => reply_privately(subject, frame, window, cx),
                None => frame.echo(NOT_ON_A_MESSAGE, StyleClass::SystemInfo, cx),
            },
        ));
        rows.push(Row::run("w", "new note", make_note));
        // The one door to the notes, on every screen: one of the window's
        // own commands.
        rows.push(
            Row::run("shift-w", "notes", |frame, window, cx| {
                frame.open(notes_address(), window, cx)
            })
            .window_wide(),
        );
        rows
    }

    /// A note is called by its first line: the one being typed while the
    /// user types, else the one written.
    fn name(&self, address: &Address, marks: &Marks, _cx: &App) -> Option<(String, String)> {
        let note = NoteId::from_address(address)?;
        let title = match self.pending.get(&note) {
            Some(pending) => title_of(&pending.text),
            None => note_title(marks, address),
        };
        Some((title, String::new()))
    }

    /// Every note, by its first line as the user is typing it, for the
    /// palette.
    fn find(&self, marks: &Marks, _cx: &App) -> Vec<Found> {
        found(marks, |note| match self.pending.get(&note) {
            Some(pending) => title_of(&pending.text),
            None => note_title(marks, &note.address()),
        })
    }

    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, cx: &mut Context<Self>) {
        // A note surface reads its text from the marks, and the list its
        // notes.
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
