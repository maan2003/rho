//! One agent model per agent: transcript projection, prompt draft, and
//! local system notices — the buffer role. Editors are the window role:
//! each pane showing the agent builds its own editor over the shared
//! multibuffer via [`AgentModel::build_editor`], with its own cursor,
//! scroll, and folds. The model reconciles every attached editor when
//! content or chrome changes, so the model persists for the session while
//! editors come and go with panes.
//!
//! The multibuffer composes three buffers: the read-only transcript, a lazy
//! read-only system-notice region (local messages that must survive
//! transcript re-renders), and the writable prompt draft.

use std::ops::Range;

use editor::scroll::AutoscrollStrategy;
use editor::{
    Editor, EditorMode, EditorRightPrompt, HighlightKey, Inlay, SelectionEffects, SizingBehavior,
};
use gpui::prelude::*;
use gpui::{Context, Entity, Subscription, WeakEntity, Window};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::InlayId;
use rho_ui_proto::remote::UiAgentState;

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
    prompt_end: text::Anchor,
    status_spans: Vec<(String, gpui::HighlightStyle)>,
    workspace: WeakEntity<Workspace>,
    /// Editors currently displaying this agent, weakly held: panes own
    /// their editors; the model only reconciles whoever is still alive.
    editors: Vec<WeakEntity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentModel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let transcript_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
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
                PathKey::sorted(0),
                transcript_buffer.clone(),
                [Point::zero()..transcript_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(2),
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

        let transcript = TranscriptModel::new(transcript_buffer, multi_buffer.clone());
        Self {
            transcript,
            prompt_buffer,
            system_buffer,
            system_excerpt_added: false,
            system_styles: Vec::new(),
            multi_buffer,
            prompt_end,
            status_spans: Vec::new(),
            workspace,
            editors: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Builds a pane's editor over the shared multibuffer — own cursor,
    /// scroll, and folds — fully caught up with the model.
    pub fn build_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let transcript_id = self.transcript.buffer().read(cx).remote_id();
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
                None,
                window,
                cx,
            );
            crate::editor_config::configure(&mut editor, window, cx);
            editor.disable_header_for_buffer(transcript_id, cx);
            editor.disable_header_for_buffer(system_id, cx);
            editor.disable_header_for_buffer(prompt_id, cx);
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

        crate::banner::insert(&editor, &self.multi_buffer, cx);
        self.transcript
            .attach(&editor, crate::workspace::now_ms(), cx);
        self.editors.push(editor.downgrade());
        self.apply_status_to(&editor, cx);
        self.apply_system_styles_to(&editor, cx);
        self.apply_prompt_chrome_to(&editor, cx);
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
    pub fn take_prompt(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let buffer = self.prompt_buffer.read(cx);
        let text = buffer
            .text_for_range(0..buffer.len())
            .collect::<String>()
            .trim()
            .to_owned();
        if text.is_empty() {
            return None;
        }
        self.prompt_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, "")], None, cx);
        });
        Some(text)
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
        if !self.system_excerpt_added {
            self.system_excerpt_added = true;
            let system_buffer = self.system_buffer.clone();
            self.multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(1),
                    system_buffer.clone(),
                    [Point::zero()..system_buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            });
        }
        for editor in self.live_editors() {
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
        apply_class_highlights(
            editor,
            &self.multi_buffer,
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
        self.status_spans = spans;
        for editor in self.live_editors() {
            self.apply_status_to(&editor, cx);
        }
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
fn format_token_count(tokens: u64) -> String {
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
