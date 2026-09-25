//! The note surface: a note's text, or a label and everything carrying it.
//!
//! A note's text is the note: the buffer is local, and what the user types
//! is written to the ledger a moment after they stop. A text another device
//! wrote replaces the buffer's when it differs. A label's surface is its
//! name over the labels under it and the things that carry it, one row
//! each; `enter` on a row opens it.

use std::collections::BTreeSet;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::{AppContext as _, Context, Entity, Task, Window};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use multi_buffer::composition::{Composition, CompositionSpec, RowSpec, SectionSpec};
use rho_dealer::{NodeId, marks};
use rho_window::style::StyleClass;
use text::{BufferId, ReplicaId};

use crate::pane::SurfaceKey;
use crate::workspace::Workspace;

/// How long after the last keystroke a note's text is written.
const SAVE_AFTER: std::time::Duration = std::time::Duration::from_millis(700);

/// Row buffers share a multibuffer with the body, whose id comes from the
/// local buffer counter, so these start well past it.
fn next_row_buffer_id() -> BufferId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
    BufferId::new(NEXT.fetch_add(1, Ordering::Relaxed)).expect("nonzero note row buffer id")
}

/// Sets a buffer's whole text, when it differs.
fn set_text(buffer: &Entity<Buffer>, text: &str, cx: &mut gpui::App) {
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
    node: NodeId,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    composition: Composition,
    body: Entity<Buffer>,
    /// The text last read from the ledger or written to it. An edit is
    /// saved only when the buffer has moved away from it, and a text from
    /// another device replaces the buffer only when it differs from it.
    synced: String,
    /// One generated line per row, in the order they are shown.
    rows: Vec<(NodeId, Entity<Buffer>)>,
    headers_disabled: std::collections::HashSet<BufferId>,
    save: Option<Task<()>>,
    _edits: gpui::Subscription,
}

impl NoteView {
    fn new(
        node: NodeId,
        text: String,
        editable: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let body = cx.new(|cx| {
            let mut buffer = Buffer::local(text.clone(), cx);
            if !editable {
                buffer.set_capability(Capability::Read, cx);
            }
            buffer
        });
        let edited = node.clone();
        let edits = cx.subscribe(&body, move |workspace, _, event, cx| {
            if matches!(event, language::BufferEvent::Edited { .. }) {
                workspace.note_edited(&edited, cx);
            }
        });
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
            editor.set_mouse_click_selection_enabled(true, cx);
            editor
        });
        Self {
            node,
            multi_buffer,
            editor,
            composition: Composition::default(),
            body,
            synced: text,
            rows: Vec::new(),
            headers_disabled: std::collections::HashSet::new(),
            save: None,
            _edits: edits,
        }
    }

    pub fn node(&self) -> &NodeId {
        &self.node
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn body(&self) -> &Entity<Buffer> {
        &self.body
    }

    pub fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        use gpui::Focusable as _;
        self.editor.read(cx).focus_handle(cx)
    }

    /// The rows shown under the body, as of the last sync.
    pub fn children(&self) -> Vec<NodeId> {
        self.rows.iter().map(|(node, _)| node.clone()).collect()
    }

    /// What the cursor is on: a row, or `None` when it is in the body.
    pub fn child_at_cursor(&self, cx: &gpui::App) -> Option<NodeId> {
        let editor = self.editor.read(cx);
        let head = editor.selections.newest_anchor().head();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (_, buffer) = snapshot.anchor_to_buffer_anchor(head)?;
        let id = buffer.remote_id();
        self.rows
            .iter()
            .find(|(_, row)| row.read(cx).remote_id() == id)
            .map(|(node, _)| node.clone())
    }

    /// Lays the rows out again. A row whose line has not changed keeps its
    /// buffer, and with it the cursor and the scroll position.
    fn sync_rows(&mut self, rows: Vec<(NodeId, String)>, cx: &mut Context<Workspace>) {
        let mut held = Vec::with_capacity(rows.len());
        for (node, line) in rows {
            let buffer = match self
                .rows
                .iter()
                .find(|(existing, _)| *existing == node)
                .map(|(_, buffer)| buffer.clone())
            {
                Some(buffer) => buffer,
                None => cx.new(|_| {
                    Buffer::remote(
                        next_row_buffer_id(),
                        ReplicaId::new(0),
                        Capability::ReadOnly,
                        "",
                    )
                }),
            };
            set_text(&buffer, &line, cx);
            held.push((node, buffer));
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
}

impl Workspace {
    /// A node's text as its surface shows it: a note's body, a label's
    /// name, and anything else's title.
    fn note_text(&self, node: &NodeId, cx: &gpui::App) -> String {
        let marks = self.attention.marks.get(node);
        match node {
            NodeId::Note(_) => marks.body.clone().unwrap_or_default(),
            NodeId::Label(_) => marks.name.clone().unwrap_or_default(),
            _ => self.node_title(node, cx),
        }
    }

    /// What is listed under a node: under a label, the labels inside it
    /// and then everything carrying it.
    fn note_rows(&self, node: &NodeId, cx: &gpui::App) -> Vec<(NodeId, String)> {
        let NodeId::Label(label) = node else {
            return Vec::new();
        };
        let marks = &self.attention.marks;
        let mut rows: Vec<(NodeId, String)> = marks
            .sublabels(Some(*label))
            .into_iter()
            .map(|sub| {
                let name = marks
                    .get(&NodeId::Label(sub))
                    .name
                    .clone()
                    .unwrap_or_default();
                (NodeId::Label(sub), format!("  # {name}"))
            })
            .collect();
        let mut carrying: Vec<(NodeId, String)> = marks
            .labeled(*label)
            .into_iter()
            .filter(|node| !matches!(node, NodeId::Agent(agent) if !self.registry.created_by_user(*agent)))
            .map(|node| {
                let bullet = match node {
                    NodeId::Note(_) => "*",
                    _ => "◦",
                };
                let line = format!("  {bullet} {}", self.node_title(&node, cx));
                (node, line)
            })
            .collect();
        carrying.sort_by(|a, b| a.1.cmp(&b.1));
        rows.extend(carrying);
        rows
    }

    /// The surface for a node, built on first open and kept after, so the
    /// cursor and scroll survive leaving and coming back.
    pub(crate) fn note_view_for(
        &mut self,
        node: &NodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> &NoteView {
        if !self.note_views.contains_key(node) {
            let editable = node.is_minted();
            let view = NoteView::new(node.clone(), self.note_text(node, cx), editable, window, cx);
            self.note_views.insert(node.clone(), view);
            self.sync_note_view(node, cx);
        }
        &self.note_views[node]
    }

    fn sync_note_view(&mut self, node: &NodeId, cx: &mut Context<Self>) {
        let text = self.note_text(node, cx);
        let rows = self.note_rows(node, cx);
        let Some(view) = self.note_views.get_mut(node) else {
            return;
        };
        if text != view.synced {
            view.synced = text.clone();
            set_text(&view.body, &text, cx);
        }
        view.sync_rows(rows, cx);
    }

    /// Brings every open note surface up to the marks: a text another
    /// device wrote, and the rows under a label.
    pub(crate) fn sync_note_views(&mut self, touched: &BTreeSet<NodeId>, cx: &mut Context<Self>) {
        let nodes: Vec<NodeId> = self
            .note_views
            .keys()
            .filter(|node| touched.contains(*node) || matches!(node, NodeId::Label(_)))
            .cloned()
            .collect();
        for node in nodes {
            self.sync_note_view(&node, cx);
        }
    }

    /// The body moved: it is written once the typing stops.
    fn note_edited(&mut self, node: &NodeId, cx: &mut Context<Self>) {
        let Some(view) = self.note_views.get_mut(node) else {
            return;
        };
        if view.body.read(cx).text() == view.synced {
            return;
        }
        let node = node.clone();
        view.save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |this, cx| this.save_note(&node, cx));
        }));
    }

    /// Writes a note's text, or a label's name, as the buffer holds it.
    pub(crate) fn save_note(&mut self, node: &NodeId, cx: &mut Context<Self>) {
        let Some(view) = self.note_views.get_mut(node) else {
            return;
        };
        view.save = None;
        let text = view.body.read(cx).text();
        if text == view.synced {
            return;
        }
        view.synced = text.clone();
        let write = match node {
            NodeId::Note(_) => marks::body(node, &text),
            NodeId::Label(_) => {
                let name = text.lines().next().unwrap_or_default().trim();
                if name.is_empty() || name.contains('/') {
                    self.echo(
                        "label: a name is one line with no /",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                marks::name(node, Some(name.to_owned()))
            }
            _ => return,
        };
        self.write_marks(vec![write], cx);
    }

    /// Writes every note whose typing has not stopped yet, before the
    /// window goes.
    pub(crate) fn save_notes(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<NodeId> = self
            .note_views
            .iter()
            .filter(|(_, view)| view.save.is_some())
            .map(|(node, _)| node.clone())
            .collect();
        for node in pending {
            self.save_note(&node, cx);
        }
    }

    /// Opens whatever a node is: a transcript, a conversation, or the note
    /// surface. What `enter` on a row means, wherever the row is.
    pub(crate) fn open_node(
        &mut self,
        node: &NodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match node {
            NodeId::Agent(agent_id) => {
                self.open_agent(*agent_id, window, cx);
                true
            }
            NodeId::Slack(unit) => {
                self.open_slack_source(crate::slack::unit_source(unit), window, cx);
                true
            }
            _ => self.open_note(node, window, cx),
        }
    }

    /// Opens a node's own surface.
    pub(crate) fn open_note(
        &mut self,
        node: &NodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let surface = self.make_surface(SurfaceKey::Note(node.clone()), window, cx);
        self.display_surface(surface, cx);
        self.focus_active_surface(window, cx);
        true
    }

    /// "Notes for this": the note about whatever the reader is looking at,
    /// made the first time the key is pressed. A note surface answers with
    /// itself, so the key is idempotent there. The new note carries the
    /// labels the thing carries, or the label itself on a label's surface,
    /// and says what it is about.
    pub(crate) fn open_notes_for_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.surface_node(cx) else {
            self.notice_on(
                None,
                "notes: nothing here to write a note about",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        if matches!(node, NodeId::Note(_)) {
            self.open_note(&node, window, cx);
            return;
        }
        let existing = self
            .attention
            .marks
            .notes()
            .find(|(_, marks)| marks.about.as_ref() == Some(&node))
            .map(|(note, _)| note.clone());
        let note = match existing {
            Some(note) => note,
            None => {
                let note = NodeId::Note(uuid::Uuid::new_v4());
                let mut writes = vec![
                    marks::created(&note, chrono::Local::now().timestamp_millis()),
                    marks::body(&note, ""),
                ];
                writes.extend(self.new_thing_marks(&note, Some(&node)));
                if !matches!(node, NodeId::Label(_)) {
                    writes.push(marks::about(&note, Some(&node)));
                }
                self.write_marks(writes, cx);
                note
            }
        };
        self.open_note(&note, window, cx);
    }

    /// `enter` on a row of a note surface opens what it stands for. In the
    /// body it is an ordinary newline, so the handler propagates.
    pub(crate) fn note_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let SurfaceKey::Note(node) = self.active_surface().key.clone() else {
            return false;
        };
        let Some(child) = self
            .note_views
            .get(&node)
            .and_then(|view| view.child_at_cursor(cx))
        else {
            return false;
        };
        self.open_node(&child, window, cx)
    }

    /// The marks a new thing gets from where the reader made it. On a
    /// label's surface it carries the label; made from anything else, it
    /// carries what that thing carries and says it is about it.
    pub(crate) fn new_thing_marks(
        &self,
        thing: &NodeId,
        area: Option<&NodeId>,
    ) -> Vec<rho_dealer::marks::Write> {
        let Some(area) = area else {
            return Vec::new();
        };
        if let NodeId::Label(label) = area {
            return vec![marks::label(thing, *label, true)];
        }
        let mut writes = vec![marks::about(thing, Some(area))];
        writes.extend(
            self.attention
                .marks
                .get(area)
                .labels
                .iter()
                .map(|label| marks::label(thing, *label, true)),
        );
        writes
    }

    /// The node the surface in view stands for: the note or label shown,
    /// the agent whose transcript, shell, file or terminal it is, or the
    /// Slack conversation. Everything else stands for nothing.
    pub(crate) fn surface_node(&self, cx: &gpui::App) -> Option<NodeId> {
        match &self.active_surface().key {
            SurfaceKey::Note(node) => Some(node.clone()),
            SurfaceKey::Transcript(agent_id)
            | SurfaceKey::Shell(agent_id)
            | SurfaceKey::File { agent_id, .. }
            | SurfaceKey::Terminal { agent_id, .. } => Some(NodeId::Agent(*agent_id)),
            SurfaceKey::SlackConversation(source) => {
                Some(NodeId::Slack(self.slack_surface_unit(source, cx)?))
            }
            _ => None,
        }
    }
}
