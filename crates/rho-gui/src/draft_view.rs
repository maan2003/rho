//! The draft compose view: pick a workdir, write the first message, submit
//! to create the agent.
//!
//! Entirely separate from [`crate::agent_view::AgentView`] — there is no
//! transcript here. The multibuffer composes three small buffers: read-only
//! local notices, the workdir field, and the message body. The excerpt
//! boundary separates field from body, so there is no scaffold text to
//! parse: submission just reads the two writable buffers.

use std::ops::Range;

use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Subscription, WeakEntity, Window};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToOffset as _};
use project::InlayId;

use crate::commands::WorkspaceCompletionProvider;
use crate::highlights::apply_class_highlights;
use crate::style::{self, PROMPT_DRAFT_HIGHLIGHT_KEY, Region, StyleClass};
use crate::workspace::Workspace;

const BODY_PLACEHOLDER_INLAY_ID: usize = 0;
const WORKDIR_LABEL_INLAY_ID: usize = 1;
const MODE_LABEL_INLAY_ID: usize = 2;
const START_LABEL_INLAY_ID: usize = 3;
const START_TARGET_HINT_INLAY_ID: usize = 4;

/// The start field's default base: the parents of the user's working copy —
/// visible and editable rather than an empty field with implicit meaning.
pub const DEFAULT_START: &str = "@-";
pub const DEFAULT_ROLE: &str = "eng";

/// How the start field's target is interpreted; cycled with Shift-Tab while
/// the cursor is in the field. The field label shows the current mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFieldMode {
    /// A fresh workspace with a new change on top of the target.
    NewOn,
    /// The same workspace as the target: shared checkout and namespace.
    Join,
    /// A VCS-masked workspace with restricted filesystem and network access.
    Sandbox,
}

impl StartFieldMode {
    fn label(self) -> &'static str {
        match self {
            Self::NewOn => "On top of: ",
            Self::Join => "Join: ",
            Self::Sandbox => "Sandbox: ",
        }
    }
}

pub struct DraftGutter;

pub struct DraftView {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    multi_buffer: Entity<MultiBuffer>,
    system_buffer: Entity<Buffer>,
    system_styles: Vec<(StyleClass, Range<text::Anchor>)>,
    workdir_buffer: Entity<Buffer>,
    role_buffer: Entity<Buffer>,
    start_buffer: Entity<Buffer>,
    start_mode: StartFieldMode,
    start_target_hints: Vec<(String, String)>,
    body_buffer: Entity<Buffer>,
    body_end: text::Anchor,
    suppress_draft_activation: bool,
    _subscriptions: Vec<Subscription>,
}

impl DraftView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let system_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let workdir_buffer = cx.new(|cx| Buffer::local("", cx));
        let role_buffer = cx.new(|cx| Buffer::local(DEFAULT_ROLE, cx));
        let start_buffer = cx.new(|cx| Buffer::local(DEFAULT_START, cx));
        let body_buffer = cx.new(|cx| Buffer::local("", cx));
        let body_end = body_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            for (key, buffer) in [
                (0, &system_buffer),
                (1, &workdir_buffer),
                (2, &role_buffer),
                (3, &start_buffer),
                (4, &body_buffer),
            ] {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(key),
                    buffer.clone(),
                    [Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            }
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
            for buffer in [
                &system_buffer,
                &workdir_buffer,
                &role_buffer,
                &start_buffer,
                &body_buffer,
            ] {
                editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            }
            editor.set_completion_provider(Some(WorkspaceCompletionProvider::new(
                workspace.clone(),
                Some(workdir_buffer.entity_id()),
                Some(role_buffer.entity_id()),
                Some(start_buffer.entity_id()),
            )));
            editor
        });

        let subscriptions = vec![
            cx.subscribe(&body_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                    this.update_body_chrome(cx);
                }
            }),
            cx.subscribe(&workdir_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                }
            }),
            cx.subscribe(&role_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                }
            }),
            cx.subscribe(&start_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                    this.update_start_target_hint(cx);
                }
            }),
        ];

        let mut this = Self {
            workspace,
            editor,
            multi_buffer,
            system_buffer,
            system_styles: Vec::new(),
            workdir_buffer,
            role_buffer,
            start_buffer,
            start_mode: StartFieldMode::NewOn,
            start_target_hints: Vec::new(),
            body_buffer,
            body_end,
            suppress_draft_activation: false,
            _subscriptions: subscriptions,
        };
        crate::banner::insert(&this.editor, &this.multi_buffer, cx);
        this.insert_workdir_label(cx);
        this.insert_role_label(cx);
        this.insert_start_label(cx);
        this.update_start_target_hint(cx);
        this.insert_body_gap(cx);
        this.pin_autoscroll(cx);
        this.update_body_chrome(cx);
        this.focus_body(window, cx);
        this
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn workdir_text(&self, cx: &gpui::App) -> String {
        let buffer = self.workdir_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_workdir_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.workdir_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
    }

    pub fn role_text(&self, cx: &gpui::App) -> String {
        let buffer = self.role_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_role_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.role_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
    }

    pub fn start_mode(&self) -> StartFieldMode {
        self.start_mode
    }

    pub fn start_text(&self, cx: &gpui::App) -> String {
        let buffer = self.start_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_start_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.start_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
        self.update_start_target_hint(cx);
    }

    pub fn set_start_target_hints(&mut self, hints: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.start_target_hints = hints;
        self.update_start_target_hint(cx);
    }

    /// Shift-Tab while the cursor is in the start field: cycle how the
    /// target is interpreted (the field label shows the mode).
    pub fn cycle_start_mode(&mut self, cx: &mut Context<Self>) {
        self.start_mode = match self.start_mode {
            StartFieldMode::NewOn => StartFieldMode::Join,
            StartFieldMode::Join => StartFieldMode::Sandbox,
            StartFieldMode::Sandbox => StartFieldMode::NewOn,
        };
        self.insert_start_label(cx);
        self.update_start_target_hint(cx);
        cx.notify();
    }

    pub fn cursor_in_start_field(&self, cx: &gpui::App) -> bool {
        self.cursor_in(&self.start_buffer, cx)
    }

    pub fn cursor_in_role_field(&self, cx: &gpui::App) -> bool {
        self.cursor_in(&self.role_buffer, cx)
    }

    fn cursor_in(&self, buffer: &Entity<Buffer>, cx: &gpui::App) -> bool {
        let field = buffer.read(cx);
        let range = field.anchor_before(0)..field.anchor_after(field.len());
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let (Some(start), Some(end)) = (
            snapshot.anchor_in_excerpt(range.start),
            snapshot.anchor_in_excerpt(range.end),
        ) else {
            return false;
        };
        let cursor = self
            .editor
            .read(cx)
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot);
        cursor >= start.to_offset(&snapshot) && cursor <= end.to_offset(&snapshot)
    }

    /// The message body, without clearing it. Submissions read instead of
    /// taking: the buffers survive until the daemon confirms creation.
    pub fn body_text(&self, cx: &gpui::App) -> String {
        let buffer = self.body_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_body_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.body_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
    }

    /// (Re)writes the workdir field with the given label. With an empty body
    /// the cursor lands in the body — the default is usually right, so
    /// typing composes the message immediately (Tab jumps into the field to
    /// change it). A non-empty body keeps its field unless `force` (an
    /// explicit choice, e.g. `:agent new <path>`) asks for the rewrite.
    pub fn seed(
        &mut self,
        workdir: &str,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.body_text(cx).trim().is_empty() {
            self.set_workdir_text(workdir, cx);
            self.focus_body(window, cx);
        } else if force {
            self.set_workdir_text(workdir, cx);
        }
    }

    /// Tab: cycles workdir field → role field → start field → message body
    /// (field values arrive selected, so typing replaces them).
    pub fn toggle_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.cursor_in(&self.workdir_buffer, cx) {
            &self.role_buffer
        } else if self.cursor_in(&self.role_buffer, cx) {
            &self.start_buffer
        } else if self.cursor_in(&self.start_buffer, cx) {
            self.focus_body(window, cx);
            return;
        } else {
            &self.workdir_buffer
        };
        let field = target.read(cx);
        let range = field.anchor_before(0)..field.anchor_after(field.len());
        self.select_range(range, window, cx);
    }

    /// Puts the cursor at the end of the message body.
    pub fn focus_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_range(self.body_end..self.body_end, window, cx);
    }

    /// Appends a local system notice (connection problems, rejected
    /// creations) above the compose form.
    pub fn system_notice(&mut self, text: &str, class: StyleClass, cx: &mut Context<Self>) {
        let range = self.system_buffer.update(cx, |buffer, cx| {
            let start = buffer.len();
            let mut line = text.to_owned();
            if !line.ends_with('\n') {
                line.push('\n');
            }
            buffer.edit([(start..start, line.as_str())], None, cx);
            buffer.anchor_before(start)..buffer.anchor_before(start + line.len())
        });
        self.system_styles.push((class, range));
        let mut by_class: Vec<(StyleClass, Vec<Range<text::Anchor>>)> = Vec::new();
        for (class, range) in &self.system_styles {
            match by_class.iter_mut().find(|(existing, _)| existing == class) {
                Some((_, ranges)) => ranges.push(range.clone()),
                None => by_class.push((*class, vec![range.clone()])),
            }
        }
        apply_class_highlights(
            &self.editor,
            &self.multi_buffer,
            Region::System,
            by_class
                .iter()
                .map(|(class, ranges)| (*class, ranges.as_slice())),
            cx,
        );
        cx.notify();
    }

    fn select_range(
        &self,
        range: Range<text::Anchor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let (Some(start), Some(end)) = (
            snapshot.anchor_in_excerpt(range.start),
            snapshot.anchor_in_excerpt(range.end),
        ) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_anchor_ranges([start..end]);
            });
        });
    }

    fn insert_workdir_label(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(field_start) =
            snapshot.anchor_in_excerpt(self.workdir_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[],
                vec![Inlay::custom(
                    WORKDIR_LABEL_INLAY_ID,
                    field_start,
                    "Workdir: ",
                )],
                cx,
            );
        });
    }

    fn insert_role_label(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(field_start) =
            snapshot.anchor_in_excerpt(self.role_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[],
                vec![Inlay::custom(MODE_LABEL_INLAY_ID, field_start, "Role: ")],
                cx,
            );
        });
    }

    fn insert_start_label(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(field_start) =
            snapshot.anchor_in_excerpt(self.start_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[InlayId::Custom(START_LABEL_INLAY_ID)],
                vec![Inlay::custom(
                    START_LABEL_INLAY_ID,
                    field_start,
                    self.start_mode.label(),
                )],
                cx,
            );
        });
    }

    fn update_start_target_hint(&mut self, cx: &mut Context<Self>) {
        let target = self.start_text(cx).trim().to_owned();
        let hint = self
            .start_target_hints
            .iter()
            .find(|(label, _)| label == &target)
            .map(|(_, hint)| hint);
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let start_buffer = self.start_buffer.read(cx);
        let Some(field_end) =
            snapshot.anchor_in_excerpt(start_buffer.anchor_after(start_buffer.len()))
        else {
            return;
        };
        let inlays = hint
            .map(|hint| Inlay::custom(START_TARGET_HINT_INLAY_ID, field_end, format!("  {hint}")))
            .into_iter()
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[InlayId::Custom(START_TARGET_HINT_INLAY_ID)], inlays, cx);
        });
    }

    /// A blank line's worth of breathing room between the workdir field and
    /// the message body.
    fn insert_body_gap(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(body_start) =
            snapshot.anchor_in_excerpt(self.body_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.insert_blocks(
                [editor::display_map::BlockProperties {
                    placement: editor::display_map::BlockPlacement::Above(body_start),
                    height: Some(1),
                    style: editor::display_map::BlockStyle::Fixed,
                    render: std::sync::Arc::new(|_| gpui::Empty.into_any_element()),
                    priority: 0,
                }],
                None,
                cx,
            );
        });
    }

    fn pin_autoscroll(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(body_end) = snapshot.anchor_in_excerpt(self.body_end) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_autoscroll_pin(body_end, AutoscrollStrategy::Bottom, cx);
        });
    }

    fn update_body_chrome(&mut self, cx: &mut Context<Self>) {
        let body_empty = self.body_buffer.read(cx).is_empty();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(body_start) =
            snapshot.anchor_in_excerpt(self.body_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        let Some(body_end) = snapshot.anchor_in_excerpt(self.body_end) else {
            return;
        };

        let mut inlays = Vec::new();
        if body_empty {
            inlays.push(Inlay::custom(
                BODY_PLACEHOLDER_INLAY_ID,
                body_end,
                "Write a message…",
            ));
        }
        let body_highlight = if body_empty {
            Vec::new()
        } else {
            vec![body_start..body_end]
        };
        let body_style = StyleClass::UserMessage.resolve(cx);
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[InlayId::Custom(BODY_PLACEHOLDER_INLAY_ID)], inlays, cx);
            editor.highlight_text(
                HighlightKey::SyntaxTreeView(PROMPT_DRAFT_HIGHLIGHT_KEY),
                body_highlight,
                body_style,
                cx,
            );
            editor.highlight_gutter::<DraftGutter>(
                vec![body_start..body_end],
                style::user_prompt_gutter_color,
                cx,
            );
        });
        cx.notify();
    }

    fn note_draft_edit(&mut self, cx: &mut Context<Self>) {
        if self.suppress_draft_activation {
            return;
        }
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.mark_draft_active_from_edit(cx);
            });
        }
    }
}
