//! Notes as the frame shows them.
//!
//! A note is the user's own text, a private message: the ledger keeps a
//! revision per save and the newest is the note. It is the user's marks,
//! which the frame holds ([`Frame::marks`]), so [`NoteSystem`] keeps
//! no disk copy of it: it reads the marks and says what the user typed
//! through [`Frame::write_marks`]. `Places`' in-memory projection
//! follows those marks and the text not yet written. The machine never writes a
//! note's text: every revision this crate says is a buffer's text as the user
//! left it.
//!
//! Notes stay off Desk. The `list notes` command opens [`NotesView`], where the
//! user can find or start a note without sending their words anywhere.
//!
//! Addresses are `note:<id>`
//! ([SPEC-note-addresses](../specs/SPEC-note-addresses.md)), and the list
//! of them is `notes:all`. A note is never a card: the dealer reads
//! nothing of the deadlines older builds wrote on notes.

pub(crate) mod list;
mod view;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, EventEmitter, Subscription, Task, Window};
use jiff::Timestamp;
pub use list::NotesView;
use rho_dealer::facts::Fact;
use rho_dealer::{Address, Addressed, Marks};
use rho_frame::attention::Write;
use rho_frame::{
    AnySurface, Command, Found, FoundText, Frame, FrameLink, Opening, Point, SearchPoint, Spelling,
    System, TextIndex, TextKind, TextPresenter, TextSearch, TextTarget,
};
pub use view::NoteView;

pub static NEW_NOTE: Spelling = Spelling {
    name: "new note",
    aliases: &[
        ("note", 8),
        ("write note", 8),
        ("jot", 6),
        ("memo", 6),
        ("remember", 4),
    ],
    argument: None,
};

pub static LIST_NOTES: Spelling = Spelling {
    name: "list notes",
    aliases: &[("notes", 9), ("my notes", 7), ("find note", 5)],
    argument: None,
};

/// How the notes' commands are spelled, for ranking every system's together.
pub static SPELLINGS: &[&Spelling] = &[&NEW_NOTE, &LIST_NOTES];
/// How long after the last keystroke a note's text is written.
const SAVE_AFTER: Duration = Duration::from_millis(700);

/// What a note without a first line is called.
const UNTITLED: &str = "untitled note";

/// The kind of the one list of every note.
const NOTES: &str = "notes";

/// The list of every note.
fn notes_address() -> Address {
    Address::new(NOTES, "all")
}

/// A note, by the id it was minted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// The current text; a deletion leaves older revisions in the ledger,
/// but presents an empty editor until the user restores or writes text.
fn revision_of<'a>(marks: &'a Marks, address: &Address) -> &'a str {
    let held = marks.get(address);
    if held.deleted {
        ""
    } else {
        held.body.as_deref().unwrap_or_default()
    }
}

fn text_of(marks: &Marks, note: NoteId) -> String {
    revision_of(marks, &note.address()).to_owned()
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
    title_of(revision_of(marks, note))
}

/// The note's new revision, when the text the user left in its buffer
/// is not what the ledger already says.
fn write_for(marks: &Marks, note: NoteId, text: String) -> Option<Write> {
    if revision_of(marks, &note.address()) == text {
        return None;
    }
    // Keep the same identity and the older revision. A subsequent editor
    // undo writes the restored text at this address, superseding the deletion.
    let (body, deleted) = if text.is_empty() {
        (None, Some(true))
    } else {
        (Some(text), None)
    };
    Some(Write::Note {
        note: note.0,
        body,
        deleted,
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

/// The mail's words and the thread's title can be different.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ReplyContext {
    mail: Option<String>,
    thread: Option<String>,
}

/// A visible row, derived from the latest ledger revision.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    note: NoteId,
    title: String,
    preview: String,
    written: Timestamp,
    reply: Option<ReplyContext>,
}

/// Match every typed term across the entire body, in any order, without
/// requiring a word to be finished before the list narrows.
fn listed(marks: &Marks, query: &str, place: impl Fn(&Address) -> ReplyContext) -> Vec<Listed> {
    let query = query.to_lowercase();
    let mut notes: Vec<_> = marks
        .notes()
        .filter_map(|(address, held)| {
            let note = NoteId::from_address(address)?;
            let body = held.body.as_deref()?;
            if !query.is_empty() {
                let folded = body.to_lowercase();
                if !query.split_whitespace().all(|term| folded.contains(term)) {
                    return None;
                }
            }
            Some(Listed {
                note,
                title: title_of(body),
                preview: excerpt(body.lines().skip(1).flat_map(str::split_whitespace)),
                written: Timestamp::from_millisecond(held.written_ms?).ok()?,
                reply: held.about.as_ref().map(&place),
            })
        })
        .collect();
    notes.sort_by_key(|listed| (std::cmp::Reverse(listed.written), listed.note.0));
    notes
}

/// Keep just a few words of context, with a bound even for a pasted URL.
fn excerpt<'a>(words: impl Iterator<Item = &'a str>) -> String {
    let mut result = String::new();
    const MAX_CHARS: usize = 52;
    for (index, word) in words.enumerate() {
        let used = result.chars().count();
        let separator = usize::from(index != 0);
        if index == 7 || used + separator + word.chars().count() > MAX_CHARS {
            if index == 0 {
                result.extend(word.chars().take(MAX_CHARS - 1));
            } else if used == MAX_CHARS {
                result.pop();
            }
            result.push('…');
            break;
        }
        if index != 0 {
            result.push(' ');
        }
        result.push_str(word);
    }
    result
}

/// Every note as `Places` lists it, by the first line `title` reads:
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
                thread: None,
                recency: held.written_ms.or(held.created_ms).unwrap_or(0).max(0) as u64,
                also: Vec::new(),
            })
        })
        .collect()
}

/// What the user typed and has not been written yet.
struct Pending {
    _save: Task<()>,
}

/// Reads the first line of the mail a private reply is about, at its owner.
pub type MailExcerpt = Rc<dyn Fn(&Address, &App) -> Option<String>>;

pub(crate) struct NoteViewed(NoteId);

pub struct NoteSystem {
    frame: FrameLink,
    mail_excerpt: Option<MailExcerpt>,
    pending: HashMap<NoteId, Pending>,
    search: TextIndex<NoteId>,
    _quit: Subscription,
}

impl EventEmitter<NoteViewed> for NoteSystem {}

impl NoteSystem {
    pub fn new(frame: FrameLink, cx: &mut Context<Self>) -> Self {
        let this = cx.weak_entity();
        let initialize = this.clone();
        frame.with(cx, move |frame, _, cx| {
            let _ = initialize.update(cx, |system, _| {
                for (address, _) in frame.marks().notes() {
                    system.index_note(frame.marks(), address);
                }
            });
        });
        let app: &App = cx;
        // What was typed in the last moment before quitting is written
        // now: the window is gone by the time a deferred write would run.
        let quit = app.on_app_quit(move |cx| {
            if let Some(this) = this.upgrade() {
                let (frame, pending) = this.update(cx, |this, _| {
                    let pending: Vec<_> = std::mem::take(&mut this.pending)
                        .into_keys()
                        .map(|note| {
                            (
                                note,
                                this.search
                                    .body(&note)
                                    .expect("pending note has text")
                                    .to_owned(),
                            )
                        })
                        .collect();
                    (this.frame.clone(), pending)
                });
                frame.update(cx, |frame, _, cx| {
                    for (note, pending) in pending {
                        write(frame, note, pending, cx);
                    }
                });
            }
            std::future::ready(())
        });
        // Override the frame's in-buffer search after its defaults are bound.
        cx.defer(list::bind_keys);
        Self {
            frame,
            mail_excerpt: None,
            pending: HashMap::new(),
            search: TextIndex::default(),
            _quit: quit,
        }
    }

    pub fn set_mail_excerpt(&mut self, read: MailExcerpt) {
        self.mail_excerpt = Some(read);
    }

    /// The current text when another surface starts editing this note.
    pub fn text(&self, marks: &Marks, note: NoteId) -> String {
        self.pending_text(note)
            .map_or_else(|| text_of(marks, note), str::to_owned)
    }

    /// Whether the user typed into `note` and it is not written yet: then
    /// their text is the newest, whatever else arrives.
    pub fn typing(&self, note: NoteId) -> bool {
        self.pending.contains_key(&note)
    }

    /// The user typed: `text` is written once they stop.
    pub fn edited(&mut self, note: NoteId, text: String, cx: &mut Context<Self>) {
        let save = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |this, cx| this.save(note, cx));
        });
        self.search
            .update(note, text, Timestamp::now().as_millisecond().max(0) as u64);
        self.pending.insert(note, Pending { _save: save });
    }

    /// Writes what was typed into `note`, if anything. It leaves the pending
    /// set only as it is written, so a surface never takes another device's
    /// text over it in between.
    pub fn save(&mut self, note: NoteId, cx: &mut Context<Self>) {
        let this = cx.entity();
        self.frame.with(cx, move |frame, _, cx| {
            let Some(text) = this.update(cx, |this, _| {
                this.pending.remove(&note)?;
                Some(
                    this.search
                        .body(&note)
                        .expect("pending note has text")
                        .to_owned(),
                )
            }) else {
                return;
            };
            write(frame, note, text, cx);
            this.update(cx, |this, _| {
                this.index_note(frame.marks(), &note.address())
            });
        });
    }

    fn pending_text(&self, note: NoteId) -> Option<&str> {
        self.typing(note)
            .then(|| self.search.body(&note).expect("pending note has text"))
    }

    fn index_note(&mut self, marks: &Marks, address: &Address) {
        let Some(note) = NoteId::from_address(address) else {
            return;
        };
        if self.typing(note) {
            // The pending editor is the newest text, even when a remote
            // revision or deletion arrives before it has been saved.
            return;
        }
        let text = revision_of(marks, address);
        if text.is_empty() {
            self.search.remove(&note);
        } else {
            let held = marks.get(address);
            let time = held.written_ms.or(held.created_ms).unwrap_or(0).max(0) as u64;
            self.search.update(note, text.to_owned(), time);
        }
    }
}

/// Says the text the user left in `note`, if it says anything new.
fn write(frame: &mut Frame, note: NoteId, text: String, cx: &mut Context<Frame>) {
    if let Some(write) = write_for(frame.marks(), note, text) {
        frame.write_marks(vec![write], cx);
    }
}

/// Makes a note about a mail without opening a separate screen.
pub fn create_private(frame: &mut Frame, subject: &Address, cx: &mut Context<Frame>) -> NoteId {
    new_note(frame, Some(subject), cx)
}

/// A new note about `about`, or about nothing. It has no text, and is
/// nothing in the ledger until it is about something or typed into.
fn new_note(frame: &mut Frame, about: Option<&Address>, cx: &mut Context<Frame>) -> NoteId {
    let note = NoteId(uuid::Uuid::new_v4());
    frame.write_marks(new_note_marks(note, about), cx);
    note
}

/// Opens a newly minted note ready to write. `text` is the exact query
/// the user submitted to start one, or empty for the new-note action.
fn start_note(frame: &mut Frame, text: String, window: &mut Window, cx: &mut Context<Frame>) {
    let note = new_note(frame, None, cx);
    frame.open(note.address(), window, cx);
    if let Ok(view) = frame.current_surface().view().downcast::<NoteView>() {
        view.update(cx, |view, cx| view.begin(text, window, cx));
    }
}

fn make_note(frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    start_note(frame, String::new(), window, cx);
}

impl System for NoteSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[Address::NOTE, NOTES]
    }

    fn open(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) -> Opening {
        let system = cx.entity();
        let frame = self.frame.clone();
        if *address == notes_address() {
            let view = cx.new(|cx| NotesView::new(system, frame, window, cx));
            return Opening::Surface(AnySurface::new(view));
        }
        let Some(note) = NoteId::from_address(address) else {
            return Opening::Gone;
        };
        let view = cx.new(|cx| NoteView::new(system, frame, note, window, cx));
        Opening::Surface(AnySurface::new(view))
    }

    fn active_surface(&mut self, address: Option<&Address>, cx: &mut Context<Self>) {
        if let Some(note) = address.and_then(NoteId::from_address) {
            cx.emit(NoteViewed(note));
        }
    }

    fn commands(
        &mut self,
        _point: &Point,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Vec<Command> {
        vec![
            Command::simple(&NEW_NOTE, make_note),
            // The one door to the notes, on every screen.
            Command::simple(&LIST_NOTES, |frame, window, cx| {
                let note = NoteId::from_address(frame.current_address());
                frame.open(notes_address(), window, cx);
                if let Some(note) = note
                    && let Ok(list) = frame.current_surface().view().downcast::<NotesView>()
                {
                    list.update(cx, |list, cx| list.return_to(note, cx));
                }
            }),
        ]
    }

    /// A note is called by its first line: the one being typed while the
    /// user types, else the one written.
    fn name(&self, address: &Address, marks: &Marks, _cx: &App) -> Option<(String, String)> {
        let note = NoteId::from_address(address)?;
        let title = match self.pending_text(note) {
            Some(text) => title_of(text),
            None => note_title(marks, address),
        };
        Some((title, String::new()))
    }

    /// Every note, by its first line as the user is typing it, for
    /// `Places`.
    fn find(&self, marks: &Marks, _cx: &App) -> Vec<Found> {
        found(marks, |note| match self.pending_text(note) {
            Some(text) => title_of(text),
            None => note_title(marks, &note.address()),
        })
    }

    fn text_sources(&self, search: &mut TextSearch, _current: &Address, _cx: &App) {
        if search.includes(TextKind::Note) {
            search.add(self.search.clone(), |id| TextTarget::Note(id.address()));
        }
    }

    fn text_presenter(&self, _cx: &App) -> Option<TextPresenter<'_>> {
        Some(Box::new(|hit| {
            let TextTarget::Note(address) = &hit.target else {
                return None;
            };
            let id = NoteId::from_address(address)?;
            hit.is_from(&self.search, &id).then(|| FoundText {
                address: address.clone(),
                title: format!("Note · {}", title_of(hit.body())),
                line: hit.line().to_owned(),
                point: SearchPoint::Line(hit.row),
            })
        }))
    }

    fn marks_moved(&mut self, touched: &BTreeSet<Address>, cx: &mut Context<Self>) {
        let changed: Vec<_> = touched
            .iter()
            .filter(|address| address.kind() == Address::NOTE)
            .cloned()
            .collect();
        if !changed.is_empty() {
            let this = cx.weak_entity();
            self.frame.with(cx, move |frame, _, cx| {
                let _ = this.update(cx, |system, _| {
                    for address in changed {
                        system.index_note(frame.marks(), &address);
                    }
                });
            });
        }
        // A note surface reads its text from the marks, and the list its notes.
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
