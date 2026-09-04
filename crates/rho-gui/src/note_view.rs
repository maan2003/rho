//! The note surface: one note's body, with its children listed under it.
//!
//! The map shows the whole tree; this shows one node the way a page shows
//! one page. The body is the node's text CRDT buffer itself, so editing
//! here and editing the same note on the map are the same edit; the child
//! rows are generated, read-only, and named the way the map names them.

use std::collections::BTreeMap;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::{AppContext as _, Context, Entity};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use multi_buffer::composition::{Composition, CompositionSpec, RowSpec, SectionSpec};
use text::{BufferId, ReplicaId};

use crate::registry::HostId;
use crate::workspace::Workspace;

/// Generated row buffers share a multibuffer with a note's body, whose id
/// comes from the Desk's own counter. Two buffers with one id inside one
/// multibuffer are the same buffer to it, so these start well past that
/// counter rather than at one.
fn next_row_buffer_id() -> BufferId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
    BufferId::new(NEXT.fetch_add(1, Ordering::Relaxed)).expect("nonzero note row buffer id")
}

pub struct NoteView {
    host: HostId,
    node_id: rho_desk::cells::Id,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    composition: Composition,
    /// The body buffer this surface was built over. A resync that hands
    /// out a new buffer for the node rebuilds the surface.
    body: Entity<Buffer>,
    /// One generated line per child, in the order the children are shown.
    rows: Vec<(rho_desk::cells::Id, Entity<Buffer>)>,
    headers_disabled: std::collections::HashSet<BufferId>,
}

impl NoteView {
    pub fn new(
        host: HostId,
        node_id: rho_desk::cells::Id,
        body: Entity<Buffer>,
        window: &mut gpui::Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
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
            crate::editor_config::configure(&mut editor, window, cx);
            editor.set_mouse_click_selection_enabled(true, cx);
            editor
        });
        Self {
            host,
            node_id,
            multi_buffer,
            editor,
            composition: Composition::default(),
            body,
            rows: Vec::new(),
            headers_disabled: std::collections::HashSet::new(),
        }
    }

    pub fn host(&self) -> HostId {
        self.host
    }

    pub fn node_id(&self) -> rho_desk::cells::Id {
        self.node_id.clone()
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

    /// The children shown under the body, as of the last sync.
    pub fn children(&self) -> Vec<rho_desk::cells::Id> {
        self.rows
            .iter()
            .map(|(node_id, _)| node_id.clone())
            .collect()
    }

    /// What the cursor is on: a child row, or `None` when it is in the body.
    pub fn child_at_cursor(&self, cx: &gpui::App) -> Option<rho_desk::cells::Id> {
        let editor = self.editor.read(cx);
        let head = editor.selections.newest_anchor().head();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (_, buffer) = snapshot.anchor_to_buffer_anchor(head)?;
        let id = buffer.remote_id();
        self.rows
            .iter()
            .find(|(_, row)| row.read(cx).remote_id() == id)
            .map(|(node_id, _)| node_id.clone())
    }

    /// Rebuilds the child rows against the tree. Cheap and idempotent: a
    /// row whose title has not changed keeps its buffer, and with it the
    /// cursor and the scroll position.
    pub fn sync(
        &mut self,
        nodes: &[crate::desk_view::DeskNode],
        titles: &BTreeMap<rho_desk::cells::Id, String>,
        cx: &mut Context<Workspace>,
    ) {
        let children = nodes
            .iter()
            .filter(|node| node.parent.as_ref() == Some(&self.node_id))
            .map(|node| {
                (
                    node.id.clone(),
                    format!(
                        "  {} {}",
                        if node.is_note() { "*" } else { "◦" },
                        titles.get(&node.id).map_or("", String::as_str)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let mut rows = Vec::with_capacity(children.len());
        for (node_id, line) in children {
            let buffer = match self
                .rows
                .iter()
                .find(|(existing, _)| *existing == node_id)
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
            crate::desk_view::write_derived_title(&buffer, &line, cx);
            rows.push((node_id, buffer));
        }
        self.rows = rows;
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
