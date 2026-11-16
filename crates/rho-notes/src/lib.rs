//! Notes as the frame shows them.
//!
//! A note is the user's own text: the ledger keeps a revision per save and
//! the newest is the note ([`rho_dealer::notes`]). A label files things;
//! its surface lists what is under it. Both are the user's marks, which the
//! frame holds ([`Frame::marks`]), so [`NoteSystem`] keeps nothing of them:
//! it reads the marks when it needs them and says what the user typed
//! through [`Frame::write_marks`]. All it holds is what was typed and not
//! yet written. The machine never writes a note's text: every revision this
//! crate says is a buffer's text as the user left it.
//!
//! Addresses are `note:<id>` and `label:<id>`
//! ([SPEC-note-addresses](../specs/SPEC-note-addresses.md)). A todo or a
//! deadline on a note is the dealer's to rank, like on anything else; a
//! bare reminder is a note with a deadline. This system only opens the
//! card.

mod view;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Subscription, Task, Window};
use rho_dealer::facts::Fact;
use rho_dealer::{Address, Addressed, Marks};
use rho_frame::attention::Write;
use rho_frame::{AnySurface, Candidate, Found, Frame, FrameLink, Point, Row, System};
use rho_window::style::StyleClass;
pub use view::NoteView;

gpui::actions!(
    rho_notes,
    [
        /// Opens what the row under the cursor stands for.
        OpenRow,
    ]
);

/// How long after the last keystroke a note's text is written.
const SAVE_AFTER: Duration = Duration::from_millis(700);

/// What a note without a first line is called.
const UNTITLED: &str = "untitled note";

/// A note, by the id it was minted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoteId(pub uuid::Uuid);

/// A label, by the id it was minted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LabelId(pub uuid::Uuid);

/// The id in an address's rest: a UUID in its simple form, and only that,
/// so one note has one address.
fn minted_id(rest: &str) -> Option<uuid::Uuid> {
    let id = uuid::Uuid::try_parse(rest).ok()?;
    (id.simple().to_string() == rest).then_some(id)
}

impl Addressed for NoteId {
    const KIND: &'static str = Address::NOTE;

    fn rest(&self) -> String {
        self.0.simple().to_string()
    }

    fn from_rest(rest: &str) -> Option<Self> {
        minted_id(rest).map(Self)
    }
}

impl Addressed for LabelId {
    const KIND: &'static str = Address::LABEL;

    fn rest(&self) -> String {
        self.0.simple().to_string()
    }

    fn from_rest(rest: &str) -> Option<Self> {
        minted_id(rest).map(Self)
    }
}

/// What a note surface is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Thing {
    Note(NoteId),
    Label(LabelId),
}

impl Thing {
    pub fn from_address(address: &Address) -> Option<Self> {
        NoteId::from_address(address)
            .map(Self::Note)
            .or_else(|| LabelId::from_address(address).map(Self::Label))
    }

    pub fn address(&self) -> Address {
        match self {
            Self::Note(note) => note.address(),
            Self::Label(label) => label.address(),
        }
    }
}

/// The text a thing's surface edits: a note's body, a label's own name.
fn text_of(marks: &Marks, thing: Thing) -> String {
    let held = marks.get(&thing.address());
    match thing {
        Thing::Note(_) => held.body.clone(),
        Thing::Label(_) => held.name.clone(),
    }
    .unwrap_or_default()
}

/// What a surface is called: a note by its first line, a label by its
/// path.
fn title_of(marks: &Marks, thing: Thing) -> String {
    match thing {
        Thing::Note(note) => note_title(marks, &note.address()),
        Thing::Label(label) => marks.label_path(label.0),
    }
}

fn note_title(marks: &Marks, note: &Address) -> String {
    match marks.get(note).title() {
        "" => UNTITLED.to_owned(),
        title => title.to_owned(),
    }
}

/// Whether a label is there to show: named, and not deleted.
fn label_live(marks: &Marks, label: LabelId) -> bool {
    let held = marks.get(&label.address());
    held.name.is_some() && !held.deleted
}

/// What the text the user left in a thing's buffer says: a note's new
/// revision, a label's new name, or nothing when the ledger already says
/// it. A label's name is its first line, and cannot hold `/`, which
/// separates a path.
fn write_for(marks: &Marks, thing: Thing, text: String) -> Result<Option<Write>, String> {
    if text_of(marks, thing) == text {
        return Ok(None);
    }
    match thing {
        Thing::Note(note) => Ok(Some(Write::Note {
            note: note.0,
            body: Some(text),
            deleted: None,
        })),
        Thing::Label(label) => {
            let name = text.lines().next().unwrap_or_default().trim();
            if name.is_empty() || name.contains('/') {
                return Err("label: a name is one line with no /".to_owned());
            }
            let held = marks.get(&label.address());
            if held.name.as_deref() == Some(name) {
                return Ok(None);
            }
            Ok(Some(Write::Fact(Fact::Label {
                label: label.0,
                name: name.to_owned(),
                parent: held.parent,
            })))
        }
    }
}

/// One row under a label: what it opens, and its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LabelRow {
    pub address: Address,
    pub line: String,
}

/// What is listed under a label: the labels inside it, then everything
/// carrying it, by title. `name` says what another system's thing is
/// called.
fn label_rows(
    marks: &Marks,
    label: LabelId,
    name: &dyn Fn(&Address) -> Option<String>,
) -> Vec<LabelRow> {
    let mut rows: Vec<LabelRow> = marks
        .sublabels(Some(label.0))
        .into_iter()
        .map(|sub| {
            let address = Address::label(sub);
            let line = format!(
                "  # {}",
                marks.get(&address).name.clone().unwrap_or_default()
            );
            LabelRow { address, line }
        })
        .collect();
    let mut carrying: Vec<(String, LabelRow)> = marks
        .labeled(label.0)
        .into_iter()
        .map(|address| {
            let (bullet, title) = match Thing::from_address(&address) {
                Some(Thing::Note(_)) => ("*", note_title(marks, &address)),
                Some(Thing::Label(label)) => ("#", marks.label_path(label.0)),
                None => (
                    "◦",
                    name(&address).unwrap_or_else(|| address.key().to_owned()),
                ),
            };
            let line = format!("  {bullet} {title}");
            (title.to_lowercase(), LabelRow { address, line })
        })
        .collect();
    carrying.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.address.cmp(&b.1.address)));
    rows.extend(carrying.into_iter().map(|(_, row)| row));
    rows
}

/// The note about `subject`, if the user wrote one.
fn note_about(marks: &Marks, subject: &Address) -> Option<NoteId> {
    let mut notes: Vec<(&Address, i64)> = marks
        .notes()
        .filter(|(_, held)| held.about.as_ref() == Some(subject))
        .map(|(note, held)| (note, held.created_ms.unwrap_or(0)))
        .collect();
    // The oldest, so the same note answers every time.
    notes.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    notes
        .first()
        .and_then(|(note, _)| NoteId::from_address(note))
}

/// What a new note says about where it was made. Made on a label, it
/// carries the label; made from anything else, it carries what that
/// carries and says it is about it. No text: the note has none until the
/// user types it.
fn new_note_marks(marks: &Marks, note: NoteId, area: Option<&Address>) -> Vec<Write> {
    let node = note.address();
    let Some(area) = area else {
        return Vec::new();
    };
    let labeled = |label: uuid::Uuid| {
        Write::Fact(Fact::Labeled {
            node: node.clone(),
            label,
            present: true,
        })
    };
    if let Some(label) = LabelId::from_address(area) {
        return vec![labeled(label.0)];
    }
    let mut writes = vec![Write::Fact(Fact::About {
        node: node.clone(),
        about: Some(area.clone()),
    })];
    writes.extend(marks.get(area).labels.iter().map(|label| labeled(*label)));
    writes
}

/// Every note and label, for `find`.
fn found(marks: &Marks) -> Vec<Found> {
    let mut found: Vec<Found> = marks
        .notes()
        .map(|(address, held)| {
            let labels = held
                .labels
                .iter()
                .filter(|label| !marks.get(&Address::label(**label)).deleted)
                .map(|label| marks.label_path(*label))
                .collect::<Vec<_>>();
            let recency = held.written_ms.or(held.created_ms).unwrap_or(0);
            Found {
                address: address.clone(),
                title: note_title(marks, address),
                description: match labels.is_empty() {
                    true => "note".to_owned(),
                    false => format!("note · {}", labels.join(", ")),
                },
                recency: recency.max(0) as u64,
            }
        })
        .collect();
    found.extend(marks.labels().into_iter().map(|(label, path)| {
        let address = Address::label(label);
        Found {
            recency: marks.get(&address).created_ms.unwrap_or(0).max(0) as u64,
            address,
            title: path,
            description: "label".to_owned(),
        }
    }));
    found
}

/// The row of the new-note picker that makes the note from what is in
/// view, and the one that files it nowhere.
const HERE_ROW: &str = "here";
const ROOT_ROW: &str = "root";

/// One place a new note can be filed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Area {
    path: String,
    kind: &'static str,
    /// `None` files at the root.
    target: Option<Address>,
}

/// Where a new note can go, in the order the picker offers them: what is
/// in view, the labels it carries, the other labels, and the root. So
/// Enter alone is a note from here.
fn areas(marks: &Marks, here: Option<&Address>) -> Vec<Area> {
    let mut areas = Vec::new();
    let carried = here
        .map(|node| marks.get(node).labels.clone())
        .unwrap_or_default();
    if let Some(here) = here {
        areas.push(Area {
            path: HERE_ROW.to_owned(),
            kind: "what is in view",
            target: Some(here.clone()),
        });
    }
    let (mut first, mut rest) = (Vec::new(), Vec::new());
    for (label, path) in marks.labels() {
        let area = Area {
            path,
            kind: "label",
            target: Some(Address::label(label)),
        };
        match carried.contains(&label) {
            true => first.push(area),
            false => rest.push(area),
        }
    }
    areas.extend(first);
    areas.extend(rest);
    areas.push(Area {
        path: ROOT_ROW.to_owned(),
        kind: "root",
        target: None,
    });
    areas
}

/// What the user typed and has not been written yet.
struct Pending {
    text: String,
    _save: Task<()>,
}

pub struct NoteSystem {
    frame: FrameLink,
    pending: HashMap<Thing, Pending>,
    _quit: Subscription,
}

impl NoteSystem {
    pub fn new(frame: FrameLink, cx: &mut Context<Self>) -> Self {
        bind_keys(cx);
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
                    for (thing, pending) in pending {
                        write(frame, thing, pending.text, cx);
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

    /// Whether the user typed into `thing` and it is not written yet:
    /// then their text is the newest, whatever else arrives.
    pub fn typing(&self, thing: Thing) -> bool {
        self.pending.contains_key(&thing)
    }

    /// The user typed: `text` is written once they stop.
    fn edited(&mut self, thing: Thing, text: String, cx: &mut Context<Self>) {
        let save = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |this, cx| this.save(thing, cx));
        });
        self.pending.insert(thing, Pending { text, _save: save });
    }

    /// Writes what was typed into `thing`. It leaves the pending set only
    /// as it is written, so a surface never takes another device's text
    /// over it in between.
    fn save(&mut self, thing: Thing, cx: &mut Context<Self>) {
        let this = cx.entity();
        self.frame.with(cx, move |frame, _, cx| {
            let Some(pending) = this.update(cx, |this, _| this.pending.remove(&thing)) else {
                return;
            };
            write(frame, thing, pending.text, cx);
        });
    }
}

/// Says the text the user left in `thing`, if it says anything new.
fn write(frame: &mut Frame, thing: Thing, text: String, cx: &mut Context<Frame>) {
    match write_for(frame.marks(), thing, text) {
        Ok(Some(write)) => {
            frame.write_marks(vec![write], cx);
        }
        Ok(None) => {}
        Err(error) => frame.echo(&error, StyleClass::SystemInfo, cx),
    }
}

/// `notes for this`: the note about what the point is on, made the first
/// time. On a note it is that note.
fn notes_for(subject: &Address, frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    let note = match Thing::from_address(subject) {
        Some(Thing::Note(note)) => note,
        _ => match note_about(frame.marks(), subject) {
            Some(note) => note,
            None => new_note(frame, Some(subject), cx),
        },
    };
    frame.open(note.address(), window, cx);
}

/// A new note filed at `area`. It has no text, and is nothing in the
/// ledger until it is filed or typed into.
fn new_note(frame: &mut Frame, area: Option<&Address>, cx: &mut Context<Frame>) -> NoteId {
    let note = NoteId(uuid::Uuid::new_v4());
    let writes = new_note_marks(frame.marks(), note, area);
    frame.write_marks(writes, cx);
    note
}

/// `new note`: where it goes, then the note.
fn prompt_new_note(
    here: Option<Address>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let listed = here.clone();
    frame.open_prompt(
        "new note in:",
        Rc::new(move |frame: &Frame, input: &str, _: &App| {
            let typed = input.trim();
            let needle = typed.to_lowercase();
            let areas = areas(frame.marks(), listed.as_ref());
            let mut candidates: Vec<Candidate> = areas
                .iter()
                .filter(|area| area.path.to_lowercase().contains(&needle))
                .map(|area| Candidate {
                    value: area.path.clone(),
                    description: area.kind.to_owned(),
                })
                .collect();
            if !typed.is_empty() && areas.iter().all(|area| area.path != typed) {
                candidates.push(Candidate {
                    value: typed.to_owned(),
                    description: "new label".to_owned(),
                });
            }
            candidates
        }),
        Rc::new(move |frame, input, window, cx| {
            let input = input.trim();
            let areas = areas(frame.marks(), here.as_ref());
            let area = match input {
                "" => areas.first().and_then(|area| area.target.clone()),
                input => match areas.iter().find(|area| area.path == input) {
                    Some(area) => area.target.clone(),
                    // A path nobody made yet is made by naming it.
                    None => match frame.mint_label(input, cx) {
                        Some(label) => Some(Address::label(label)),
                        None => {
                            frame.echo("new note: no such place", StyleClass::SystemInfo, cx);
                            return;
                        }
                    },
                },
            };
            let note = new_note(frame, area.as_ref(), cx);
            frame.open(note.address(), window, cx);
        }),
        window,
        cx,
    );
}

/// `open label`: a label by its path.
fn prompt_open_label(frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    frame.open_prompt(
        "label:",
        Rc::new(|frame: &Frame, input: &str, _: &App| {
            let needle = input.trim().to_lowercase();
            frame
                .marks()
                .labels()
                .into_iter()
                .filter(|(_, path)| path.to_lowercase().contains(&needle))
                .map(|(_, path)| Candidate {
                    value: path,
                    description: "label".to_owned(),
                })
                .collect()
        }),
        Rc::new(|frame, input, window, cx| {
            let path = input.trim();
            match frame.marks().label_at(path) {
                Some(label) => frame.open(Address::label(label), window, cx),
                None => frame.echo(
                    &format!("label: no label {path}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
            }
        }),
        window,
        cx,
    );
}

impl System for NoteSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[Address::NOTE, Address::LABEL]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let thing = Thing::from_address(address)?;
        let system = cx.entity();
        let frame = self.frame.clone();
        let view = cx.new(|cx| NoteView::new(system, frame, thing, window, cx));
        Some(AnySurface::new(view))
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, _cx: &mut Context<Self>) -> Vec<Row> {
        let mut rows = Vec::new();
        if let Some(subject) = point.subject.clone() {
            rows.push(Row::run("n", "notes for this", move |frame, window, cx| {
                notes_for(&subject, frame, window, cx)
            }));
        }
        let here = point.subject.clone();
        rows.push(Row::run("w", "new note", move |frame, window, cx| {
            prompt_new_note(here.clone(), frame, window, cx)
        }));
        rows.push(Row::run("l", "open label", prompt_open_label));
        rows
    }

    /// Only asked for a note whose first line is empty: the frame names
    /// notes and labels from the marks.
    fn name(&self, address: &Address, _cx: &App) -> Option<(String, String)> {
        NoteId::from_address(address).map(|_| (UNTITLED.to_owned(), String::new()))
    }

    fn find(&self, marks: &Marks, _cx: &App) -> Vec<Found> {
        found(marks)
    }

    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, cx: &mut Context<Self>) {
        // A label's rows read every thing under it, so any move may be
        // one of theirs.
        cx.notify();
    }
}

fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    let bindings = rho_frame::normal_contexts("RhoNote", "")
        .map(|context| KeyBinding::new("enter", OpenRow, Some(&context)));
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests;
