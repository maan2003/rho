//! The note surface: a note's text, or a label and everything under it.
//!
//! A note's text is the note. The buffer is local, and what the user types
//! is written to the ledger a moment after they stop ([`NoteSystem`]). A
//! text another device wrote replaces the buffer's when it differs from
//! what this surface last read or handed over, unless the user is still
//! typing: their text is then the newer one. A label's surface is its name
//! over the labels under it and the things carrying it, one row each;
//! `enter` on a row opens it.

use std::collections::HashSet;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Subscription, Window, div};
use language::{Buffer, BufferEvent, Capability};
use multi_buffer::MultiBuffer;
use multi_buffer::composition::{Composition, CompositionSpec, RowSpec, SectionSpec};
use rho_dealer::Address;
use rho_frame::{FrameLink, Subject, Surface};
use rho_window::style::StyleClass;
use text::{BufferId, ReplicaId};
use theme::ActiveTheme as _;

use crate::{LabelRow, NoteSystem, OpenRow, Thing, label_live, label_rows, text_of, title_of};

/// Row buffers share a multibuffer with the body, whose id comes from the
/// local buffer counter, so these start well past it.
fn next_row_buffer_id() -> BufferId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
    BufferId::new(NEXT.fetch_add(1, Ordering::Relaxed)).expect("nonzero note row buffer id")
}

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
    thing: Thing,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    composition: Composition,
    body: Entity<Buffer>,
    /// The text last read from the ledger or handed over to be written;
    /// `None` until the marks are first read. A buffer edit is handed over
    /// only when it moved away from it, and a text from another device
    /// replaces the buffer only when it differs from it.
    synced: Option<String>,
    title: String,
    /// One line per row under a label, in the order they are shown.
    rows: Vec<(Address, Entity<Buffer>)>,
    headers_disabled: HashSet<BufferId>,
    _subscriptions: Vec<Subscription>,
}

impl NoteView {
    /// The surface over `thing`. It is built while the frame and the
    /// system are both busy opening it, so the marks are read once that
    /// is over.
    pub(crate) fn new(
        system: Entity<NoteSystem>,
        frame: FrameLink,
        thing: Thing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = cx.new(|cx| Buffer::local("", cx));
        let multi_buffer = cx.new(|_| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_multiple_paths_per_buffer(true);
            multi_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_placeholder_text(
                match thing {
                    Thing::Note(_) => "a note · its first line is its title",
                    Thing::Label(_) => "the label's name",
                },
                window,
                cx,
            );
            editor
        });
        let subscriptions = vec![
            cx.observe(&system, |this, _, cx| this.refresh(cx)),
            cx.subscribe(&body, |this, _, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.edited(cx);
                }
            }),
        ];
        let mut view = Self {
            system,
            frame,
            thing,
            multi_buffer,
            editor,
            composition: Composition::default(),
            body,
            synced: None,
            title: String::new(),
            rows: Vec::new(),
            headers_disabled: HashSet::new(),
            _subscriptions: subscriptions,
        };
        view.sync_rows(Vec::new(), cx);
        // Not `defer_in`: that runs inside the window's update, where the
        // frame cannot be read.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| this.refresh(cx));
        });
        view
    }

    /// Brings the surface up to the marks: the text, unless the user is
    /// typing, and the rows. A label that is gone closes its surface.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let thing = self.thing;
        let Some((text, title, rows, live)) = self.frame.read(cx, |frame, cx| {
            let marks = frame.marks();
            let (rows, live) = match thing {
                Thing::Note(_) => (Vec::new(), true),
                Thing::Label(label) => {
                    let name =
                        |address: &Address| frame.name_of(address, cx).map(|(title, _)| title);
                    (label_rows(marks, label, &name), label_live(marks, label))
                }
            };
            (text_of(marks, thing), title_of(marks, thing), rows, live)
        }) else {
            return;
        };
        if !live {
            self.frame
                .echo("label: no such label", StyleClass::SystemInfo, cx);
            self.frame.close(thing.address(), cx);
            return;
        }
        if !self.system.read(cx).typing(thing) && self.synced.as_ref() != Some(&text) {
            self.synced = Some(text.clone());
            set_text(&self.body, &text, cx);
        }
        self.title = title;
        if self.synced.is_none() {
            self.synced = Some(text);
        }
        self.sync_rows(rows, cx);
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
        if let Thing::Note(_) = self.thing {
            self.title = match text.lines().next().map(str::trim) {
                None | Some("") => crate::UNTITLED.to_owned(),
                Some(title) => title.to_owned(),
            };
        }
        let thing = self.thing;
        self.system
            .update(cx, |system, cx| system.edited(thing, text, cx));
    }

    /// Lays the rows out again. A row whose line has not changed keeps its
    /// buffer, and with it the cursor and the scroll position.
    fn sync_rows(&mut self, rows: Vec<LabelRow>, cx: &mut Context<Self>) {
        let mut held = Vec::with_capacity(rows.len());
        for row in rows {
            let buffer = match self
                .rows
                .iter()
                .find(|(existing, _)| *existing == row.address)
                .map(|(_, buffer)| buffer.clone())
            {
                Some(buffer) => buffer,
                None => cx.new(|cx| {
                    Buffer::remote(
                        next_row_buffer_id(),
                        ReplicaId::new(0),
                        Capability::ReadOnly,
                        "",
                        cx,
                    )
                }),
            };
            set_text(&buffer, &row.line, cx);
            held.push((row.address, buffer));
        }
        self.rows = held;
        let mut spec = CompositionSpec::default();
        spec.sections.push(SectionSpec {
            host: self.body.clone(),
            start: 0,
            end: None,
            lead: Vec::new(),
            cuts: Vec::new(),
        });
        // Element keys are the row's position: the rows are a listing, so a
        // row that moves is a different row and may lose its excerpt.
        spec.tail = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, (_, buffer))| RowSpec {
                id: index as u64 + 1,
                buffer: buffer.clone(),
            })
            .collect();
        self.composition.sync(&self.multi_buffer, &spec, cx);
        let ids = std::iter::once(self.body.clone())
            .chain(self.rows.iter().map(|(_, buffer)| buffer.clone()))
            .map(|buffer| buffer.read(cx).remote_id())
            .filter(|id| !self.headers_disabled.contains(id))
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            for id in &ids {
                editor.disable_header_for_buffer(*id, cx);
            }
        });
        self.headers_disabled.extend(ids);
    }

    /// What the cursor is on: a row, or `None` in the body.
    fn row_at_cursor(&self, cx: &App) -> Option<Address> {
        let editor = self.editor.read(cx);
        let head = editor.selections.newest_anchor().head();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (_, buffer) = snapshot.anchor_to_buffer_anchor(head)?;
        let id = buffer.remote_id();
        self.rows
            .iter()
            .find(|(_, row)| row.read(cx).remote_id() == id)
            .map(|(address, _)| address.clone())
    }

    /// `enter` on a row opens what it stands for. In the body it is the
    /// editor's own key.
    fn open_row(&mut self, _: &OpenRow, _window: &mut Window, cx: &mut Context<Self>) {
        match self.row_at_cursor(cx) {
            Some(address) => self.frame.open(address, cx),
            None => cx.propagate(),
        }
    }
}

impl Surface for NoteView {
    fn title(&self, _cx: &App) -> String {
        self.title.clone()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    /// The thing itself, or the row the cursor is on.
    fn subject(&self, cx: &App) -> Subject {
        match self.row_at_cursor(cx) {
            Some(address) => Subject::Address(address),
            None => Subject::Own,
        }
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
            .key_context("RhoNote")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::open_row))
            .child(self.editor.clone())
    }
}
