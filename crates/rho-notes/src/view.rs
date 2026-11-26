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
use jiff::{Timestamp, Zoned};
use language::{Buffer, BufferEvent, Capability};
use multi_buffer::{Anchor, MultiBuffer};
use rho_dealer::Addressed as _;
use rho_frame::when::reminder_said;
use rho_frame::{CardHere, FrameLink, Surface, phone_editor_tap};
use rho_window::draft::focus_writing_at;
use theme::ActiveTheme as _;

use crate::{NoteId, NoteSystem, reminder, title_of};

/// Sets a buffer's whole text, when it differs.
fn set_text(buffer: &Entity<Buffer>, text: &str, cx: &mut App) {
    buffer.update(cx, |buffer, cx| {
        if buffer.text() == text {
            return;
        }
        let capability = buffer.capability();
        buffer.set_capability(Capability::ReadWrite, cx);
        // Loading another device's revision is not a local editor edit.
        // Isolate it so an immediate `u` cannot undo loading the note.
        buffer.finalize_last_transaction();
        buffer.start_transaction();
        buffer.set_text(text, cx);
        if let Some(transaction) = buffer.end_transaction(cx) {
            buffer.forget_transaction(transaction);
        }
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
    /// When the reminder the user set on the note comes due, as last read
    /// with its text: the status line is drawn while the frame is busy.
    reminder: Option<Timestamp>,
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
            reminder: None,
            _subscriptions: subscriptions,
        }
    }

    /// Only a newly minted note starts in Insert. The optional starting
    /// text came from the user's submitted query, so it is an editor edit.
    pub(crate) fn begin(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.synced = Some(String::new());
        if !text.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.set_text(text, window, cx);
            });
        }
        // The editor's new-entity observer attaches Vim at the end of the
        // opening update, so start Insert after it has run.
        cx.defer_in(window, |this, window, cx| {
            this.focus_writing(None, window, cx);
        });
    }

    fn focus_writing(&self, at: Option<Anchor>, window: &mut Window, cx: &mut App) {
        let snapshot = self.editor.read(cx).buffer().read(cx).snapshot(cx);
        let start = snapshot.anchor_after(language::Point::zero());
        let at = at.unwrap_or_else(|| snapshot.anchor_after(snapshot.max_point()));
        focus_writing_at(&self.editor, start, at, window, cx);
    }

    fn tap_text(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(at) = phone_editor_tap(self.editor.read(cx), event, window) {
            self.focus_writing(Some(at), window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    /// Brings the surface up to the marks unless the user is typing.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let note = self.note;
        let Some((text, reminder)) = self.frame.read(cx, |frame, cx| {
            (
                self.system.read(cx).text(frame.marks(), note),
                reminder(frame.marks(), &note.address()).map(|snooze| snooze.until),
            )
        }) else {
            return;
        };
        self.reminder = reminder;
        if self.synced.as_ref() != Some(&text) {
            set_text(&self.body, &text, cx);
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
        let note = self.note;
        self.system
            .update(cx, |system, cx| system.edited(note, text, cx));
        cx.notify();
    }
}

impl Surface for NoteView {
    fn title(&self, cx: &App) -> String {
        title_of(&self.body.read(cx).text())
    }

    /// When the reminder comes due, quietly, while the note is no card.
    fn status(&self, _cx: &App) -> Option<String> {
        let now = Zoned::now();
        self.reminder
            .map(|until| reminder_said(&until.to_zoned(now.time_zone().clone()), &now))
    }

    /// A note is a card the user schedules: `space s` sets its reminder,
    /// and it deals once that is due.
    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Reminder
    }

    fn edits_at_point(&self, _cx: &App) -> bool {
        true
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
            .capture_any_mouse_down(cx.listener(Self::tap_text))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}
