//! One agent model per agent: transcript projection, prompt draft, and
//! local system notices — the buffer role. Editors are the window role:
//! each surface showing the agent builds its editor over the shared
//! multibuffer via [`AgentModel::build_editor`], with its own cursor,
//! scroll, and folds. The model reconciles every attached editor when
//! content or chrome changes, so the model persists for the session while
//! editors come and go with surfaces.
//!
//! The multibuffer composes the transcript's per-turn buffers, a lazy read-only
//! system-notice region (local messages that must survive transcript
//! re-renders), and the writable prompt draft.

use std::collections::HashMap;
use std::ops::Range;

use collections::HashSet;
use editor::display_map::CustomBlockId;
use editor::scroll::AutoscrollStrategy;
use editor::{
    Editor, EditorMode, EditorRightPrompt, HighlightKey, Inlay, SelectionEffects, SizingBehavior,
};
use futures::future::join_all;
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable, Subscription, Task, WeakEntity, Window};
use language::{Buffer, BufferEvent, Capability, InlayId, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_core::ContentPart;
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::UiAgentState;
use text::{Buffer as TextBuffer, BufferId, ReplicaId};

#[cfg(feature = "native")]
use crate::commands::WorkspaceCompletionProvider;
use crate::highlights::apply_class_highlights;
use crate::store::FrameSummary;
use crate::style::{self, PROMPT_DRAFT_HIGHLIGHT_KEY, Region, StyleClass};
use crate::transcript::TranscriptModel;
use crate::workspace::Workspace;

const PROMPT_PLACEHOLDER_INLAY_ID: usize = 0;

pub struct PromptGutter;

pub struct AgentModel {
    transcript: TranscriptModel,
    prompt_buffer: Entity<Buffer>,
    system_buffer: Entity<Buffer>,
    system_excerpt_added: bool,
    system_styles: Vec<(StyleClass, Range<text::Anchor>)>,
    multi_buffer: Entity<MultiBuffer>,
    /// The document multibuffer: transcript (cropped flush when the turn
    /// is closed) plus system notices, without the prompt. The dashboard
    /// preview reads this — a pure document that ends where the words do.
    document_multi_buffer: Entity<MultiBuffer>,
    /// The read-only editor over the document multibuffer, built lazily
    /// for the dashboard preview and kept for the model's lifetime.
    preview_editor: Option<Entity<Editor>>,
    prompt_end: text::Anchor,
    attachments: Vec<ContentPart>,
    attachment_blocks: Vec<(WeakEntity<Editor>, CustomBlockId)>,
    status_spans: Vec<(String, gpui::HighlightStyle)>,
    workspace: WeakEntity<Workspace>,
    initial_load_started: bool,
    initial_load_ready: bool,
    initial_load: Option<Task<()>>,
    /// Full-multibuffer editors currently displaying this agent, weakly
    /// held: surfaces own their editors; the model only reconciles whoever
    /// is still alive. The preview editor lives apart — prompt chrome
    /// must never reach it.
    editors: Vec<WeakEntity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentModel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        visualization_client: crate::connection::VisualizationClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let system_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let prompt_buffer = cx.new(|cx| Buffer::local("", cx));
        let prompt_end = prompt_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(u64::MAX),
                prompt_buffer.clone(),
                [Point::zero()..prompt_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });

        let subscriptions = vec![cx.subscribe(&prompt_buffer, |this, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.update_prompt_chrome(cx);
            }
        })];

        let document_multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
        let transcript = TranscriptModel::new(
            multi_buffer.clone(),
            document_multi_buffer.clone(),
            visualization_client,
        );
        Self {
            transcript,
            prompt_buffer,
            system_buffer,
            system_excerpt_added: false,
            system_styles: Vec::new(),
            multi_buffer,
            document_multi_buffer,
            preview_editor: None,
            prompt_end,
            attachments: Vec::new(),
            attachment_blocks: Vec::new(),
            status_spans: Vec::new(),
            workspace,
            initial_load_started: false,
            initial_load_ready: false,
            initial_load: None,
            editors: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn initial_load_started(&self) -> bool {
        self.initial_load_started
    }

    pub fn initial_load_ready(&self) -> bool {
        self.initial_load_ready
    }

    /// Starts the one asynchronous bulk load for a subscribed agent. Normal
    /// streamed updates remain synchronous after this reaches Ready.
    pub fn start_initial_load(
        &mut self,
        agent_id: AgentId,
        state: UiAgentState,
        agent_labels: HashMap<AgentId, String>,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) {
        if self.initial_load_started {
            return;
        }
        self.initial_load_started = true;
        let workspace = self.workspace.clone();
        let background = cx.background_executor().clone();
        self.initial_load = Some(cx.spawn(async move |this, cx| {
            let mut prepared = background
                .spawn(async move { TranscriptModel::prepare_initial(state, now_ms, agent_labels) })
                .await;
            let reservations = (0..prepared.buffer_count())
                .map(|_| cx.reserve_entity::<Buffer>())
                .collect::<Vec<_>>();
            let buffer_ids = reservations
                .iter()
                .map(|reservation| BufferId::from(reservation.entity_id().as_non_zero_u64()))
                .collect::<Vec<_>>();
            let texts = prepared.take_texts();
            let text_buffers = background
                .spawn(async move {
                    buffer_ids
                        .into_iter()
                        .zip(texts)
                        .map(|(buffer_id, text)| TextBuffer::new(ReplicaId::LOCAL, buffer_id, text))
                        .collect::<Vec<_>>()
                })
                .await;
            let text_buffers = reservations.into_iter().zip(text_buffers).collect();
            let Ok(parsing) = this.update(cx, |this, cx| {
                this.transcript
                    .install_initial(prepared, text_buffers, now_ms, cx)
            }) else {
                return;
            };
            join_all(parsing).await;
            if this
                .update(cx, |this, _| {
                    this.initial_load_ready = true;
                })
                .is_err()
            {
                return;
            }
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_initial_agent_load(agent_id, cx)
            });
        }));
    }

    /// The read-only preview editor over the document multibuffer, built
    /// on first use: no prompt, no banner, pinned to the latest content.
    pub fn preview_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        if let Some(editor) = &self.preview_editor {
            return editor.clone();
        }
        let multi_buffer = self.document_multi_buffer.clone();
        let system_id = self.system_buffer.read(cx).remote_id();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                #[cfg(feature = "native")]
                None,
                window,
                cx,
            );
            crate::editor_config::configure_preview(&mut editor, window, cx);
            editor.disable_bracket_colorization(cx);
            editor.disable_header_for_buffer(system_id, cx);
            editor.set_read_only(true);
            editor.set_autoscroll_pin(multi_buffer::Anchor::Max, AutoscrollStrategy::Bottom, cx);
            editor
        });
        self.transcript
            .attach(&editor, crate::workspace::now_ms(), cx);
        self.apply_system_styles_to(&editor, cx);
        self.preview_editor = Some(editor.clone());
        editor
    }

    /// Releases the dashboard-only editor while retaining the shared
    /// transcript buffers. A later preview recreates it lazily.
    pub fn clear_preview_editor(&mut self) {
        self.preview_editor = None;
    }

    /// Builds an editor over the shared multibuffer — own cursor,
    /// scroll, and folds — fully caught up with the model.
    pub fn build_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let workspace = self.workspace.clone();
        let multi_buffer = self.multi_buffer.clone();
        let system_id = self.system_buffer.read(cx).remote_id();
        let prompt_id = self.prompt_buffer.read(cx).remote_id();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                #[cfg(feature = "native")]
                None,
                window,
                cx,
            );
            crate::editor_config::configure(&mut editor, window, cx);
            editor.disable_bracket_colorization(cx);
            editor.disable_header_for_buffer(system_id, cx);
            editor.disable_header_for_buffer(prompt_id, cx);
            #[cfg(feature = "native")]
            editor.set_completion_provider(Some(WorkspaceCompletionProvider::new(
                workspace, None, None, None,
            )));
            editor
        });

        if let Some(draft_anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.prompt_end)
        {
            editor.update(cx, |editor, cx| {
                editor.set_autoscroll_pin(draft_anchor, AutoscrollStrategy::Bottom, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_anchor_ranges([draft_anchor..draft_anchor]);
                });
            });
        }

        self.transcript
            .attach(&editor, crate::workspace::now_ms(), cx);
        self.editors.push(editor.downgrade());
        self.apply_status_to(&editor, cx);
        self.apply_system_styles_to(&editor, cx);
        self.apply_prompt_chrome_to(&editor, cx);
        self.refresh_attachment_blocks(cx);
        editor
    }

    /// The editors still alive, pruning dropped ones.
    fn live_editors(&mut self) -> Vec<Entity<Editor>> {
        self.editors.retain(|editor| editor.upgrade().is_some());
        self.editors
            .iter()
            .filter_map(|editor| editor.upgrade())
            .collect()
    }

    pub fn sync(
        &mut self,
        state: &UiAgentState,
        summary: FrameSummary,
        now_ms: u64,
        agent_label: &impl Fn(rho_ui_proto::AgentId) -> String,
        cx: &mut Context<Self>,
    ) {
        self.transcript
            .sync(state, summary, now_ms, agent_label, cx);
    }

    pub fn tick_timers(&mut self, now_ms: u64, cx: &mut Context<Self>) {
        self.transcript.tick_timers(now_ms, cx);
    }

    pub fn has_timers(&self) -> bool {
        self.transcript.has_timers()
    }

    /// Takes the trimmed prompt draft, clearing it. Returns `None` when empty.
    pub fn take_prompt(&mut self, cx: &mut Context<Self>) -> Option<Vec<ContentPart>> {
        let buffer = self.prompt_buffer.read(cx);
        let text = buffer
            .text_for_range(0..buffer.len())
            .collect::<String>()
            .trim()
            .to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return None;
        }
        self.prompt_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, "")], None, cx);
        });
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text { text });
        }
        content.append(&mut self.attachments);
        self.update_prompt_chrome(cx);
        self.refresh_attachment_blocks(cx);
        Some(content)
    }

    /// Whether the newest selection head sits in the editable prompt tail of
    /// the transcript. The phone layout shows the keyboard only then.
    pub fn selection_in_prompt(&self, editor: &Entity<Editor>, cx: &App) -> bool {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(prompt_start) = snapshot.anchor_in_excerpt(self.prompt_end) else {
            return false;
        };
        let head = editor.read(cx).selections.newest_anchor().head();
        head.cmp(&prompt_start, &snapshot).is_ge()
    }

    /// Places this editor's cursor in the writable prompt tail. Transcript
    /// previews can retain an older cursor when promoted from Deal mode; edit
    /// commands must not start against the read-only conversation above it.
    pub fn focus_prompt(
        &self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt_end) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.prompt_end)
        else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.set_autoscroll_pin(prompt_end, AutoscrollStrategy::Bottom, cx);
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_anchor_ranges([prompt_end..prompt_end]);
            });
        });
        window.focus(&editor.focus_handle(cx), cx);
    }

    pub fn add_image(&mut self, media_type: String, data: Vec<u8>, cx: &mut Context<Self>) {
        self.attachments
            .push(ContentPart::Image { media_type, data });
        self.update_prompt_chrome(cx);
        self.refresh_attachment_blocks(cx);
    }

    pub fn clear_attachments(&mut self, cx: &mut Context<Self>) -> bool {
        let had = !self.attachments.is_empty();
        self.attachments.clear();
        self.update_prompt_chrome(cx);
        self.refresh_attachment_blocks(cx);
        had
    }

    /// Appends a local system notice that survives transcript re-renders.
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
        let system_buffer = self.system_buffer.clone();
        if !self.system_excerpt_added {
            self.system_excerpt_added = true;
            self.multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(u64::MAX - 1),
                    system_buffer.clone(),
                    [Point::zero()..system_buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            });
        }
        // The document's system excerpt is cropped flush (no trailing
        // blank row) and re-set as notices accumulate.
        let end = {
            let buffer = system_buffer.read(cx);
            buffer.offset_to_point(buffer.len().saturating_sub(1))
        };
        self.document_multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(u64::MAX - 1),
                system_buffer.clone(),
                [Point::zero()..end],
                0,
                cx,
            );
        });
        for editor in self.live_editors() {
            self.apply_system_styles_to(&editor, cx);
        }
        if let Some(editor) = self.preview_editor.clone() {
            self.apply_system_styles_to(&editor, cx);
        }
        cx.notify();
    }

    fn apply_system_styles_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let mut by_class: Vec<(StyleClass, Vec<Range<text::Anchor>>)> = Vec::new();
        for (class, range) in &self.system_styles {
            match by_class.iter_mut().find(|(existing, _)| existing == class) {
                Some((_, ranges)) => ranges.push(range.clone()),
                None => by_class.push((*class, vec![range.clone()])),
            }
        }
        // Resolve against the editor's own multibuffer: full editors and
        // the document preview compose the system buffer differently.
        let multi_buffer = editor.read(cx).buffer().clone();
        apply_class_highlights(
            editor,
            &multi_buffer,
            Region::System,
            by_class
                .iter()
                .map(|(class, ranges)| (*class, ranges.as_slice())),
            cx,
        );
    }

    pub fn set_status(
        &mut self,
        project_label: &str,
        workspace_label: Option<&str>,
        usage_label: Option<&str>,
        role_label: Option<(&str, style::RoleFamily)>,
        context_used: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let mut spans = Vec::new();
        if !project_label.is_empty() {
            spans.push((project_label.to_owned(), style::cwd_chip_style(cx)));
        }
        if let Some(workspace_label) = workspace_label
            && !workspace_label.is_empty()
        {
            if !spans.is_empty() {
                spans.push((" ".to_owned(), style::cwd_chip_style(cx)));
            }
            spans.push((workspace_label.to_owned(), style::workspace_chip_style(cx)));
        }
        if let Some((role_label, mode_family)) = role_label
            && !role_label.is_empty()
        {
            if !spans.is_empty() {
                spans.push((" ".to_owned(), style::cwd_chip_style(cx)));
            }
            spans.push((
                role_label.to_owned(),
                style::role_chip_style(mode_family, cx),
            ));
        }
        if let Some(context_used) = context_used {
            if !spans.is_empty() {
                spans.push((" ".to_owned(), style::cwd_chip_style(cx)));
            }
            spans.push((
                format_token_count(context_used),
                style::context_chip_style(cx),
            ));
        }
        if let Some(usage_label) = usage_label
            && !usage_label.is_empty()
        {
            if !spans.is_empty() {
                spans.push((" ".to_owned(), style::cwd_chip_style(cx)));
            }
            spans.push((usage_label.to_owned(), style::context_chip_style(cx)));
        }
        self.status_spans = spans;
        for editor in self.live_editors() {
            self.apply_status_to(&editor, cx);
        }
    }

    /// The status chips as styled spans; the preview sheet's header
    /// renders these as real UI outside the editor.
    pub fn status_spans(&self) -> &[(String, gpui::HighlightStyle)] {
        &self.status_spans
    }

    #[cfg(test)]
    pub(crate) fn status_span_text(&self) -> String {
        self.status_spans
            .iter()
            .map(|(text, _)| text.as_str())
            .collect()
    }

    /// The composed multibuffer text; lets tests observe what the model
    /// would display without requiring an attached editor.
    #[cfg(test)]
    pub(crate) fn buffer_text(&self, cx: &Context<Self>) -> String {
        self.multi_buffer.read(cx).snapshot(cx).text()
    }

    fn apply_status_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.prompt_end)
        else {
            return;
        };
        let right_prompt = (!self.status_spans.is_empty()).then(|| EditorRightPrompt {
            anchor,
            spans: self.status_spans.clone(),
        });
        editor.update(cx, |editor, cx| {
            editor.set_right_prompt(right_prompt, cx);
        });
    }

    fn update_prompt_chrome(&mut self, cx: &mut Context<Self>) {
        for editor in self.live_editors() {
            self.apply_prompt_chrome_to(&editor, cx);
        }
        cx.notify();
    }

    fn refresh_attachment_blocks(&mut self, cx: &mut Context<Self>) {
        for (editor, block_id) in self.attachment_blocks.drain(..) {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(
                        std::iter::once(block_id).collect::<HashSet<_>>(),
                        None,
                        cx,
                    );
                });
            }
        }
        if self.attachments.is_empty() {
            return;
        }
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.prompt_end)
        else {
            return;
        };
        let block = style::attachment_block(anchor, &self.attachments);
        for editor in self.live_editors() {
            let block = block.clone();
            let ids = editor.update(cx, |editor, cx| editor.insert_blocks([block], None, cx));
            if let Some(block_id) = ids.into_iter().next() {
                self.attachment_blocks.push((editor.downgrade(), block_id));
            }
        }
    }

    fn apply_prompt_chrome_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let buffer = self.prompt_buffer.read(cx);
        let draft_empty = buffer.is_empty();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(prompt_start) =
            snapshot.anchor_in_excerpt(self.prompt_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        let Some(prompt_end) = snapshot.anchor_in_excerpt(self.prompt_end) else {
            return;
        };

        let mut inlays = Vec::new();
        if draft_empty {
            inlays.push(Inlay::custom(
                PROMPT_PLACEHOLDER_INLAY_ID,
                prompt_end,
                "Write a message…",
            ));
        }
        let draft_highlight = if draft_empty {
            Vec::new()
        } else {
            vec![prompt_start..prompt_end]
        };
        let draft_style = StyleClass::UserMessage.resolve(cx);
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[InlayId::Custom(PROMPT_PLACEHOLDER_INLAY_ID)], inlays, cx);
            editor.highlight_text(
                HighlightKey::SyntaxTreeView(PROMPT_DRAFT_HIGHLIGHT_KEY),
                draft_highlight,
                draft_style,
                cx,
            );
            editor.highlight_gutter::<PromptGutter>(
                vec![prompt_start..prompt_end],
                style::user_prompt_gutter_color,
                cx,
            );
        });
    }
}

/// Renders a token count compactly for the status chip: bare below a
/// thousand, then `k`/`M` with one decimal while a single digit (`9.5k`,
/// `1.2M`) and whole numbers after (`62k`, `12M`).
pub(crate) fn format_token_count(tokens: u64) -> String {
    fn scaled(value: f64, suffix: &str) -> String {
        if value < 9.95 {
            format!("{value:.1}{suffix}")
        } else {
            format!("{value:.0}{suffix}")
        }
    }
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 999_500 {
        scaled(tokens as f64 / 1_000.0, "k")
    } else {
        scaled(tokens as f64 / 1_000_000.0, "M")
    }
}

#[cfg(test)]
mod tests {
    use super::format_token_count;

    #[test]
    fn token_counts_render_compactly() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1.0k");
        assert_eq!(format_token_count(9_400), "9.4k");
        assert_eq!(format_token_count(9_950), "10k");
        assert_eq!(format_token_count(62_300), "62k");
        assert_eq!(format_token_count(999_499), "999k");
        assert_eq!(format_token_count(999_500), "1.0M");
        assert_eq!(format_token_count(1_250_000), "1.2M");
        assert_eq!(format_token_count(12_000_000), "12M");
    }
}
