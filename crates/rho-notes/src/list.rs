//! Notes has a writable find-or-start line followed by a read-only keyed
//! transcript. Searching never edits the list or any note.

use std::ops::Range;
use std::time::Duration;

use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, HighlightStyle, MouseButton, Subscription, Task,
    Window, div,
};
use jiff::Timestamp;
use language::{Buffer, Capability, Point, ToPoint as _};
use multi_buffer::{MultiBuffer, ToPoint as _};
use rho_dealer::Addressed as _;
use rho_frame::{FrameLink, Layout, Mode, PhoneTool, Surface, phone_editor_tap};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use crate::{
    Listed, NoteId, NoteSystem, NoteViewed, ReplyContext, excerpt, listed, make_note,
    notes_address, start_note,
};

gpui::actions!(
    rho_notes,
    [
        OpenNote,
        FindOrStart,
        EditOrStart,
        StartNote,
        LeaveQuery,
        NextNote,
        PreviousNote
    ]
);

/// Clear of the transcript's semantic slots at zero.
const KEY_BASE: usize = usize::MAX / 4;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Note(NoteId),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    Title,
    Muted,
}

impl rho_transcript::Style for Class {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(KEY_BASE + bucket as usize * 2 + self as usize)
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
    for context in rho_frame::normal_contexts("RhoNotes", " && vim_operator == none") {
        cx.bind_keys([
            gpui::KeyBinding::new("enter", OpenNote, Some(&context)),
            gpui::KeyBinding::new("/", FindOrStart, Some(&context)),
            gpui::KeyBinding::new("o", StartNote, Some(&context)),
            gpui::KeyBinding::new("shift-o", StartNote, Some(&context)),
            gpui::KeyBinding::new("i", EditOrStart, Some(&context)),
            gpui::KeyBinding::new("shift-i", EditOrStart, Some(&context)),
            gpui::KeyBinding::new("a", EditOrStart, Some(&context)),
            gpui::KeyBinding::new("shift-a", EditOrStart, Some(&context)),
        ]);
    }
    let query = Some("RhoNotesQuery > Editor");
    cx.bind_keys([
        gpui::KeyBinding::new("enter", OpenNote, query),
        gpui::KeyBinding::new("escape", LeaveQuery, query),
        gpui::KeyBinding::new("down", NextNote, query),
        gpui::KeyBinding::new("up", PreviousNote, query),
        gpui::KeyBinding::new("ctrl-n", NextNote, query),
        gpui::KeyBinding::new("ctrl-p", PreviousNote, query),
    ]);
}

pub struct NotesView {
    frame: FrameLink,
    system: Entity<NoteSystem>,
    transcript: Transcript<Key, Class, Option<NoteId>>,
    editor: Entity<Editor>,
    query: Entity<Editor>,
    finding: bool,
    phone: bool,
    /// A note may be left before its first write reaches the list.
    return_to: Option<NoteId>,
    _subscriptions: Vec<Subscription>,
    _age_beat: Task<()>,
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
        let style = editor.update(cx, |editor, cx| editor.style(cx).text.clone());
        let query = cx.new(|cx| {
            let mut query = Editor::single_line(window, cx);
            query.set_text_style_refinement(gpui::TextStyleRefinement {
                font_family: Some(style.font_family),
                font_size: Some(style.font_size),
                ..Default::default()
            });
            query.set_placeholder_text("words from a note", window, cx);
            query
        });
        let mut transcript = Transcript::new(buffer);
        transcript.attach(&editor, cx);

        let focus = query.focus_handle(cx);
        let rows_focus = editor.focus_handle(cx);
        let subscriptions = vec![
            cx.observe(&system, |this, _, cx| this.refresh(false, cx)),
            cx.subscribe(&system, |this, _, viewed: &NoteViewed, cx| {
                this.return_to(viewed.0, cx);
            }),
            cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.refresh(true, cx);
                }
            }),
            cx.on_focus(&focus, window, |this, _, cx| {
                this.finding = true;
                cx.notify();
            }),
            cx.on_blur(&focus, window, |this, _, cx| {
                this.finding = false;
                cx.notify();
            }),
            cx.on_focus(&rows_focus, window, |_, _, cx| Self::defer_refresh(cx)),
        ];
        Self::defer_refresh(cx);
        let age_beat = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;
                if this.update(cx, |this, cx| this.refresh(false, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            frame,
            system,
            transcript,
            editor,
            query,
            finding: false,
            phone: Layout::of(window.viewport_size().width) == Layout::Phone,
            return_to: None,
            _subscriptions: subscriptions,
            _age_beat: age_beat,
        }
    }

    pub(crate) fn return_to(&mut self, note: NoteId, cx: &mut Context<Self>) {
        self.return_to = Some(note);
        self.editor.update(cx, |editor, cx| {
            editor.clear_background_highlights(HighlightKey::BufferSearchHighlights, cx);
        });
        if !self.query.read(cx).text(cx).is_empty() {
            let buffer = self
                .query
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("the find line is a single buffer");
            buffer.update(cx, |buffer, cx| {
                buffer.set_text("", cx);
            });
        }
        Self::defer_refresh(cx);
    }

    fn defer_refresh(cx: &mut Context<Self>) {
        // The frame may still be opening or returning to this surface.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| this.refresh(false, cx));
        });
    }

    fn refresh(&mut self, changed_query: bool, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        let filtering = !query.trim().is_empty();
        let Some((notes, active, waiting_to_list)) = self.frame.read(cx, |frame, cx| {
            let system = self.system.read(cx);
            let active = *frame.current_address() == notes_address();
            let waiting_to_list = self.return_to.is_some_and(|note| {
                !active
                    || system
                        .pending_text(note)
                        .is_some_and(|text| !text.is_empty())
            });
            let notes = listed(frame.marks(), &query, |about| ReplyContext {
                mail: system
                    .mail_excerpt
                    .as_ref()
                    .and_then(|read| read(about, cx)),
                thread: frame
                    .shown_in(about, cx)
                    .and_then(|thread| frame.name_of(&thread, cx))
                    .map(|(name, _)| name),
            });
            (notes, active, waiting_to_list)
        }) else {
            return;
        };
        let selected = self.cursor_note(cx);
        self.transcript
            .reconcile(items(&notes, filtering, self.phone, Timestamp::now()), cx);
        // Clearing the field on departure also emits an edit event. It
        // restores the full list without choosing its first row.
        let new_filter = changed_query && filtering;
        if new_filter {
            self.return_to = None;
        }
        let returning = self.return_to.filter(|_| active);
        let target = returning.or_else(|| selected.filter(|_| !new_filter));
        if let Some(target) = target
            && let Some(row) = self.item_row(&Key::Note(target), cx)
        {
            if returning == Some(target) {
                self.return_to = None;
                self.select_row(row, cx);
            } else if self.cursor_note(cx) != Some(target) {
                self.select_row(row, cx);
            }
        } else {
            if !waiting_to_list {
                self.return_to = None;
            }
            if new_filter || self.cursor_note(cx).is_none() {
                self.select_row(0, cx);
            }
        }
        cx.notify();
    }

    fn select_row(&mut self, row: u32, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let point = Point::new(row, 0);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([point..point]);
            });
            editor.request_autoscroll(Autoscroll::fit(), cx);
            cx.notify();
        });
    }

    fn cursor_row(&self, cx: &App) -> u32 {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row
    }

    fn cursor_note(&self, cx: &App) -> Option<NoteId> {
        self.note_at_row(self.cursor_row(cx), cx)
    }

    fn note_at_row(&self, row: u32, cx: &App) -> Option<NoteId> {
        self.transcript.line_meta(row, cx).copied().flatten()
    }

    fn item_row(&self, key: &Key, cx: &App) -> Option<u32> {
        let anchor = self.transcript.range_of(key)?.start;
        Some(
            anchor
                .to_point(&self.transcript.buffer().read(cx).snapshot())
                .row,
        )
    }

    fn step(&mut self, next: bool, cx: &mut Context<Self>) {
        let current = self.cursor_row(cx);
        let candidate = {
            let mut rows = self
                .transcript
                .keys()
                .filter_map(|key| self.item_row(key, cx));
            if next {
                rows.find(|row| *row > current)
            } else {
                rows.rfind(|row| *row < current)
            }
        };
        if let Some(row) = candidate {
            self.select_row(row, cx);
        }
    }

    fn find(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.query.focus_handle(cx), cx);
    }

    fn open_note(&mut self, _: &OpenNote, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.cursor_note(cx).or_else(|| self.note_at_row(0, cx)) {
            self.frame.open(note.address(), cx);
        } else {
            let text = self.query.read(cx).text(cx);
            self.frame.with(cx, move |frame, window, cx| {
                start_note(frame, text, window, cx);
            });
        }
    }

    fn tap_list(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx);
        if let Some(at) = phone_editor_tap(editor, event, window) {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            if let Some(note) = self.note_at_row(at.to_point(&snapshot).row, cx) {
                self.frame.open(note.address(), cx);
                window.prevent_default();
                cx.stop_propagation();
            }
        }
    }
}

fn items(
    notes: &[Listed],
    filtering: bool,
    phone: bool,
    now: Timestamp,
) -> Vec<Item<Key, Class, Option<NoteId>>> {
    if notes.is_empty() {
        let text = match (filtering, phone) {
            (true, _) => "  no match · enter to start a note",
            (false, true) => "  no notes yet · tap new to start",
            (false, false) => "  no notes yet · i or o to start",
        };
        return vec![
            Item::new(Key::Empty, text)
                .with_styles(vec![(Class::Muted, 0..text.len())])
                .with_lines(vec![None]),
        ];
    }
    notes
        .iter()
        .map(|listed| {
            let (text, styles) = row(listed, now);
            Item::new(Key::Note(listed.note), text)
                .with_styles(styles)
                .with_lines(vec![Some(listed.note)])
        })
        .collect()
}

pub(crate) fn row(listed: &Listed, now: Timestamp) -> (String, Vec<(Class, Range<usize>)>) {
    let mut text = format!("  {}", listed.title);
    let mut styles = vec![(Class::Title, 2..text.len())];
    let muted = text.len();
    if !listed.preview.is_empty() {
        text.push_str("  ");
        text.push_str(&listed.preview);
    }
    if let Some(reply) = &listed.reply {
        text.push_str("  ↳ ");
        match (&reply.mail, &reply.thread) {
            (Some(mail), Some(thread)) if mail != thread => {
                text.push_str(&excerpt(mail.split_whitespace()));
                text.push_str(" · ");
                text.push_str(&excerpt(thread.split_whitespace()));
            }
            (Some(name), _) | (_, Some(name)) => text.push_str(&excerpt(name.split_whitespace())),
            (None, None) => text.push_str("private reply"),
        }
    }
    text.push_str("  · ");
    text.push_str(&rho_dealer::curve::age(listed.written, now));
    styles.push((Class::Muted, muted..text.len()));
    (text, styles)
}

impl Surface for NotesView {
    fn title(&self, _cx: &App) -> String {
        "notes".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn mode(&self, _cx: &App) -> Option<Mode> {
        self.finding.then_some(Mode::Insert)
    }

    fn phone_tools(&self, _typing: bool, _cx: &App) -> Vec<PhoneTool> {
        let frame = self.frame.clone();
        vec![PhoneTool::new("+", "new", move |_, cx| {
            frame.with(cx, make_note)
        })]
    }
}

impl Focusable for NotesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for NotesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let phone = Layout::of(window.viewport_size().width) == Layout::Phone;
        if self.phone != phone {
            self.phone = phone;
            self.refresh(false, cx);
        }
        let style = self
            .editor
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let colors = cx.theme().colors();
        div()
            .key_context("RhoNotes")
            .on_action(cx.listener(Self::open_note))
            .on_action(cx.listener(|this, _: &FindOrStart, window, cx| this.find(window, cx)))
            .on_action(cx.listener(|this, _: &EditOrStart, window, cx| {
                if this.note_at_row(0, cx).is_none() && this.query.read(cx).text(cx).is_empty() {
                    this.frame.with(cx, make_note);
                } else {
                    this.find(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &StartNote, _, cx| this.frame.with(cx, make_note)))
            .on_action(cx.listener(|this, _: &LeaveQuery, window, cx| {
                window.focus(&this.editor.focus_handle(cx), cx);
            }))
            .on_action(cx.listener(|this, _: &NextNote, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousNote, _, cx| this.step(false, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(
                div()
                    .key_context("RhoNotesQuery")
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .font_family(style.font_family)
                    .text_size(style.font_size)
                    .line_height(style.line_height)
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.text_muted)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.find(window, cx);
                                }),
                            )
                            .child("find or start:"),
                    )
                    .child(div().flex_1().min_w_0().child(self.query.clone())),
            )
            .child(
                div()
                    .capture_any_mouse_down(cx.listener(Self::tap_list))
                    .flex_1()
                    .min_h_0()
                    .child(self.editor.clone()),
            )
    }
}
