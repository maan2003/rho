//! The note surface: a note's text.
//!
//! A note's text is the note. The buffer is local, and what the user types
//! is written to the ledger a moment after they stop ([`NoteSystem`]). A
//! text another device wrote replaces the buffer's when it differs from
//! what this surface last read or handed over, unless the user is still
//! typing: their text is then the newer one.

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Subscription, Window, div};
use language::{Buffer, BufferEvent, Capability};
use multi_buffer::MultiBuffer;
use rho_dealer::Addressed as _;
use rho_frame::{FrameLink, Surface};
use theme::ActiveTheme as _;

use crate::{NoteId, NoteSystem, note_title, text_of};

/// Sets a buffer's whole text, when it differs.
fn set_text(buffer: &Entity<Buffer>, text: &str, cx: &mut App) {
    buffer.update(cx, |buffer, cx| {
        if buffer.text() == text {
            return;
        }
        let capability = buffer.capability();
        buffer.set_capability(Capability::ReadWrite, cx);
        let end = buffer.len();
        buffer.edit([(0..end, text)], None, cx);
        buffer.set_capability(capability, cx);
    });
}

pub struct NoteView {
    system: Entity<NoteSystem>,
    frame: FrameLink,
    note: NoteId,
    editor: Entity<Editor>,
    body: Entity<Buffer>,
    /// The text last read from the ledger or handed over to be written;
    /// `None` until the marks are first read. A buffer edit is handed over
    /// only when it moved away from it, and a text from another device
    /// replaces the buffer only when it differs from it.
    synced: Option<String>,
    title: String,
    _subscriptions: Vec<Subscription>,
}

impl NoteView {
    /// The surface over `note`. It is built while the frame and the
    /// system are both busy opening it, so the marks are read once that
    /// is over.
    pub(crate) fn new(
        system: Entity<NoteSystem>,
        frame: FrameLink,
        note: NoteId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = cx.new(|cx| Buffer::local("", cx));
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(body.clone(), cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_placeholder_text("a note · its first line is its title", window, cx);
            editor
        });
        // Leaving the note writes what was typed at once, so the screen
        // the user goes back to already shows it.
        let typed_in = editor.focus_handle(cx);
        let subscriptions = vec![
            cx.observe(&system, |this, _, cx| this.refresh(cx)),
            cx.subscribe(&body, |this, _, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.edited(cx);
                }
            }),
            cx.on_blur(&typed_in, window, |this, _, cx| {
                let note = this.note;
                this.system.update(cx, |system, cx| system.save(note, cx));
            }),
        ];
        // Not `defer_in`: that runs inside the window's update, where the
        // frame cannot be read.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| this.refresh(cx));
        });
        Self {
            system,
            frame,
            note,
            editor,
            body,
            synced: None,
            title: String::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Brings the surface up to the marks: the text, unless the user is
    /// typing.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let note = self.note;
        let Some((text, title)) = self.frame.read(cx, |frame, _| {
            let marks = frame.marks();
            (text_of(marks, note), note_title(marks, &note.address()))
        }) else {
            return;
        };
        if !self.system.read(cx).typing(note) && self.synced.as_ref() != Some(&text) {
            self.synced = Some(text.clone());
            set_text(&self.body, &text, cx);
        }
        self.title = title;
        if self.synced.is_none() {
            self.synced = Some(text);
        }
        cx.notify();
    }

    /// The body moved: what it holds is handed over, to be written once
    /// the typing stops.
    fn edited(&mut self, cx: &mut Context<Self>) {
        let Some(synced) = &self.synced else {
            return;
        };
        let text = self.body.read(cx).text();
        if &text == synced {
            return;
        }
        self.synced = Some(text.clone());
        self.title = crate::title_of(&text);
        let note = self.note;
        self.system
            .update(cx, |system, cx| system.edited(note, text, cx));
    }
}

impl Surface for NoteView {
    fn title(&self, _cx: &App) -> String {
        self.title.clone()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }
}

impl Focusable for NoteView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for NoteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}
