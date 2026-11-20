//! `notes`: every note, one line each by its first line, the one last
//! written first. Enter opens the note under the cursor.
//!
//! The list is read from the marks, like a note's text, so a note another
//! device wrote, or one just typed here, moves to the top once it is
//! written. The surface is one keyed transcript, as Home is: a row is an
//! item keyed by its note, so a change leaves the cursor where it was.

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Subscription, Window, div,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, ToPoint as _};
use rho_dealer::Address;
use rho_frame::{FrameLink, Row, Subject, Surface};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use crate::{NoteSystem, listed};

gpui::actions!(
    rho_notes,
    [
        /// Opens the note under the cursor.
        OpenNote,
    ]
);

/// This list's own highlight-key space, clear of the transcript's
/// semantic slots at zero.
const KEY_BASE: usize = usize::MAX / 4;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Note(Address),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Class {
    Title,
    Muted,
}

impl Class {
    fn slot(self) -> usize {
        match self {
            Self::Title => 0,
            Self::Muted => 1,
        }
    }

    const COUNT: usize = 2;
}

impl rho_transcript::Style for Class {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
            Self::Title => colors.text,
            Self::Muted => colors.text_muted,
        };
        HighlightStyle {
            color: Some(color.into()),
            ..HighlightStyle::default()
        }
    }
}

pub(crate) fn bind_keys(cx: &mut App) {
    let bindings = rho_frame::normal_contexts("RhoNotes", "")
        .map(|context| gpui::KeyBinding::new("enter", OpenNote, Some(&context)));
    cx.bind_keys(bindings);
}

pub struct NotesView {
    frame: FrameLink,
    transcript: Transcript<Key, Class, Option<Address>>,
    editor: Entity<Editor>,
    /// The notes as last read: each address and first line, in order.
    notes: Option<Vec<(Address, String)>>,
    _subscriptions: Vec<Subscription>,
}

impl NotesView {
    pub(crate) fn new(
        system: Entity<NoteSystem>,
        frame: FrameLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        let mut transcript = Transcript::new(buffer);
        transcript.attach(&editor, cx);
        // Not `defer_in`: that runs inside the window's update, where the
        // frame cannot be read.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| this.refresh(cx));
        });
        Self {
            frame,
            transcript,
            editor,
            notes: None,
            _subscriptions: vec![cx.observe(&system, |this, _, cx| this.refresh(cx))],
        }
    }

    /// Brings the list up to the marks.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(notes) = self.frame.read(cx, |frame, _| listed(frame.marks())) else {
            return;
        };
        if self.notes.as_ref() == Some(&notes) {
            return;
        }
        let first = self.notes.is_none();
        self.transcript.reconcile(items(&notes), cx);
        self.notes = Some(notes);
        // A reader who has not moved yet is put on the first note, so the
        // very first Enter opens it.
        if first || self.cursor_note(cx).is_none() {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::zero()..Point::zero()]);
            });
            cx.notify();
        });
    }

    /// The note the cursor is on.
    fn cursor_note(&self, cx: &App) -> Option<Address> {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let row = editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row;
        self.transcript.line_meta(row, cx).cloned().flatten()
    }

    fn open_note(&mut self, _: &OpenNote, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.cursor_note(cx) {
            self.frame.open(note, cx);
        }
    }
}

/// The notes as transcript items: one line each, its first line.
fn items(notes: &[(Address, String)]) -> Vec<Item<Key, Class, Option<Address>>> {
    if notes.is_empty() {
        let text = "  no notes yet";
        return vec![
            Item::new(Key::Empty, text)
                .with_styles(vec![(Class::Muted, 0..text.len())])
                .with_lines(vec![None]),
        ];
    }
    notes
        .iter()
        .map(|(address, title)| {
            let text = format!("  {title}");
            Item::new(Key::Note(address.clone()), text.clone())
                .with_styles(vec![(Class::Title, 2..text.len())])
                .with_lines(vec![Some(address.clone())])
        })
        .collect()
}

impl Surface for NotesView {
    fn title(&self, _cx: &App) -> String {
        "notes".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    /// Nothing: the list is a door to the notes, not a deal. A verdict on
    /// a row would close the list as it closes a card in view, so a
    /// note's card is answered on the note.
    fn subject(&self, _cx: &App) -> Subject {
        Subject::None
    }

    /// `open`, while the cursor is on a note: what the phone's bar offers
    /// for `enter`.
    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let Some(note) = self.cursor_note(cx) else {
            return Vec::new();
        };
        vec![Row::run("enter", "open", move |frame, window, cx| {
            frame.open(note.clone(), window, cx)
        })]
    }
}

impl Focusable for NotesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for NotesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoNotes")
            .on_action(cx.listener(Self::open_note))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}
