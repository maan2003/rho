//! One live view per agent: editor, transcript projection, prompt draft, and
//! local system notices.
//!
//! Views persist for the lifetime of the session once created, so cursor,
//! scroll position, open folds, and the prompt draft survive agent switches.
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

pub struct AgentView {
    editor: Entity<Editor>,
    transcript: TranscriptModel,
    prompt_buffer: Entity<Buffer>,
    system_buffer: Entity<Buffer>,
    system_excerpt_added: bool,
    system_styles: Vec<(StyleClass, Range<text::Anchor>)>,
    multi_buffer: Entity<MultiBuffer>,
    prompt_end: text::Anchor,
    status_spans: Vec<(String, gpui::HighlightStyle)>,
    _subscriptions: Vec<Subscription>,
}

impl AgentView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
            editor.disable_header_for_buffer(transcript_buffer.read(cx).remote_id(), cx);
            editor.disable_header_for_buffer(system_buffer.read(cx).remote_id(), cx);
            editor.disable_header_for_buffer(prompt_buffer.read(cx).remote_id(), cx);
            editor.set_completion_provider(Some(WorkspaceCompletionProvider::new(workspace, None, None)));
            editor
        });

        let subscriptions = vec![cx.subscribe(&prompt_buffer, |this, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.update_prompt_chrome(cx);
            }
        })];

        if let Some(draft_anchor) = multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(prompt_end)
        {
            editor.update(cx, |editor, cx| {
                editor.set_autoscroll_pin(draft_anchor, AutoscrollStrategy::Bottom, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_anchor_ranges([draft_anchor..draft_anchor]);
                });
            });
        }

        let transcript =
            TranscriptModel::new(transcript_buffer, multi_buffer.clone(), editor.clone(), cx);
        let mut this = Self {
            editor,
            transcript,
            prompt_buffer,
            system_buffer,
            system_excerpt_added: false,
            system_styles: Vec::new(),
            multi_buffer,
            prompt_end,
            status_spans: Vec::new(),
            _subscriptions: subscriptions,
        };
        crate::banner::insert(&this.editor, &this.multi_buffer, cx);
        this.update_prompt_chrome(cx);
        this
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn sync(
        &mut self,
        state: &UiAgentState,
        summary: FrameSummary,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) {
        self.transcript.sync(state, summary, now_ms, cx);
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
        self.apply_system_styles(cx);
        cx.notify();
    }

    fn apply_system_styles(&self, cx: &mut Context<Self>) {
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
    }

    pub fn set_status(&mut self, project_label: &str, cx: &mut Context<Self>) {
        let mut spans = Vec::new();
        spans.push((project_label.to_owned(), style::cwd_chip_style(cx)));
        self.status_spans = spans;
        self.apply_status(cx);
    }

    fn apply_status(&self, cx: &mut Context<Self>) {
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
        self.editor.update(cx, |editor, cx| {
            editor.set_right_prompt(right_prompt, cx);
        });
    }

    fn update_prompt_chrome(&mut self, cx: &mut Context<Self>) {
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
        self.editor.update(cx, |editor, cx| {
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
        cx.notify();
    }
}
