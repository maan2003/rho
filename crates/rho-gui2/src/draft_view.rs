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
use editor::{
    Editor, EditorMode, HighlightKey, Inlay, SelectionEffects, SizingBehavior,
};
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

pub struct DraftGutter;

pub struct DraftView {
    editor: Entity<Editor>,
    multi_buffer: Entity<MultiBuffer>,
    system_buffer: Entity<Buffer>,
    system_styles: Vec<(StyleClass, Range<text::Anchor>)>,
    workdir_buffer: Entity<Buffer>,
    body_buffer: Entity<Buffer>,
    body_end: text::Anchor,
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
        let body_buffer = cx.new(|cx| Buffer::local("", cx));
        let body_end = body_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            for (key, buffer) in [
                (0, &system_buffer),
                (1, &workdir_buffer),
                (2, &body_buffer),
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
            for buffer in [&system_buffer, &workdir_buffer, &body_buffer] {
                editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            }
            editor.set_completion_provider(Some(WorkspaceCompletionProvider::new(
                workspace,
                Some(workdir_buffer.entity_id()),
            )));
            editor
        });

        let subscriptions = vec![cx.subscribe(&body_buffer, |this, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.update_body_chrome(cx);
            }
        })];

        let mut this = Self {
            editor,
            multi_buffer,
            system_buffer,
            system_styles: Vec::new(),
            workdir_buffer,
            body_buffer,
            body_end,
            _subscriptions: subscriptions,
        };
        crate::banner::insert(&this.editor, &this.multi_buffer, cx);
        this.insert_workdir_label(cx);
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
        self.workdir_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
    }

    /// The message body, without clearing it. Submissions read instead of
    /// taking: the buffers survive until the daemon confirms creation.
    pub fn body_text(&self, cx: &gpui::App) -> String {
        let buffer = self.body_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_body_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.body_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
    }

    /// (Re)writes the workdir field with the given label. With an empty body
    /// the cursor lands in the body — the default is usually right, so
    /// typing composes the message immediately (Tab jumps into the field to
    /// change it). A non-empty body keeps its field unless `force` (an
    /// explicit choice, e.g. `:agent new <path>`) asks for the rewrite.
    pub fn seed(&mut self, workdir: &str, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.body_text(cx).trim().is_empty() {
            self.set_workdir_text(workdir, cx);
            self.focus_body(window, cx);
        } else if force {
            self.set_workdir_text(workdir, cx);
        }
    }

    /// Tab: jumps between the workdir field (value selected, so typing
    /// replaces it) and the message body.
    pub fn toggle_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = self.workdir_buffer.read(cx);
        let field_range = field.anchor_before(0)..field.anchor_after(field.len());
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let (Some(field_start), Some(field_end)) = (
            snapshot.anchor_in_excerpt(field_range.start),
            snapshot.anchor_in_excerpt(field_range.end),
        ) else {
            return;
        };
        let cursor = self
            .editor
            .read(cx)
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot);
        let in_field = cursor >= field_start.to_offset(&snapshot)
            && cursor <= field_end.to_offset(&snapshot);
        if in_field {
            self.focus_body(window, cx);
        } else {
            self.select_range(field_range, window, cx);
        }
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
}
