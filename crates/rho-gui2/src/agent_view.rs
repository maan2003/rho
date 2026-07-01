//! One live view per agent: editor, transcript projection, prompt draft, and
//! local system notices.
//!
//! Views persist for the lifetime of the session once created, so cursor,
//! scroll position, open folds, and the prompt draft survive agent switches.
//! The multibuffer composes three buffers: the read-only transcript, a lazy
//! read-only system-notice region (local messages that must survive
//! transcript re-renders), and the writable prompt draft.

use std::ops::Range;
use std::sync::Arc;

use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::scroll::AutoscrollStrategy;
use editor::{
    Editor, EditorMode, EditorRightPrompt, HighlightKey, Inlay, SelectionEffects, SizingBehavior,
};
use gpui::prelude::*;
use gpui::{Context, Entity, FontWeight, Subscription, WeakEntity, Window, div, px, svg};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::InlayId;
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::UiAgentState;
use theme::ActiveTheme as _;

use crate::commands::WorkspaceCompletionProvider;
use crate::store::FrameSummary;
use crate::style::{self, PROMPT_DRAFT_HIGHLIGHT_KEY, Region, StyleClass};
use crate::transcript::TranscriptModel;
use crate::workspace::Workspace;
use crate::{
    AgentNew, AgentNext, AgentPrevious, RoleCycle, RoleCycleGroup, SubmitPrompt, TaskBoard,
};

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
        agent_id: Option<AgentId>,
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
            editor.set_show_gutter(false, cx);
            editor.set_show_compact_gutter(true, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.set_show_horizontal_scrollbar(false, cx);
            editor.set_offset_content(false, cx);
            editor.set_mouse_click_selection_enabled(false, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_autoindent(false);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_selection_highlight(false);
            editor.disable_header_for_buffer(transcript_buffer.read(cx).remote_id(), cx);
            editor.disable_header_for_buffer(system_buffer.read(cx).remote_id(), cx);
            editor.disable_header_for_buffer(prompt_buffer.read(cx).remote_id(), cx);
            editor.disable_expand_excerpt_buttons(cx);
            editor.set_completion_provider(Some(WorkspaceCompletionProvider::new(
                workspace.clone(),
            )));
            editor
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&prompt_buffer, |this, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.update_prompt_chrome(cx);
            }
        }));
        subscriptions.extend(register_actions(
            &editor,
            agent_id.clone(),
            cx.entity().downgrade(),
            workspace,
            cx,
        ));

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
        this.insert_banner(cx);
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
    pub fn system_notice(
        &mut self,
        text: &str,
        class: StyleClass,
        cx: &mut Context<Self>,
    ) {
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
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut by_class: Vec<(StyleClass, Vec<Range<multi_buffer::Anchor>>)> = Vec::new();
        for (class, range) in &self.system_styles {
            let Some(start) = snapshot.anchor_in_excerpt(range.start) else {
                continue;
            };
            let Some(end) = snapshot.anchor_in_excerpt(range.end) else {
                continue;
            };
            match by_class.iter_mut().find(|(existing, _)| existing == class) {
                Some((_, ranges)) => ranges.push(start..end),
                None => by_class.push((*class, vec![start..end])),
            }
        }
        self.editor.update(cx, |editor, cx| {
            for (class, ranges) in by_class {
                editor.highlight_text(
                    class.highlight_key(Region::System),
                    ranges,
                    class.resolve(cx),
                    cx,
                );
            }
        });
    }

    pub fn set_status(
        &mut self,
        role: Option<&str>,
        project_label: &str,
        cx: &mut Context<Self>,
    ) {
        let separator = style::muted_style(cx);
        let mut spans = Vec::new();
        if let Some(role) = role {
            spans.push((role.to_owned(), style::role_chip_style(cx)));
            spans.push((" ".to_owned(), separator));
        }
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
        let draft_empty = buffer.len() == 0;
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

    fn insert_banner(&self, cx: &mut Context<Self>) {
        let anchor = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_before(Point::new(0, 0));
        let version = format!("rho {}", env!("CARGO_PKG_VERSION"));
        let pun = startup_pun().to_owned();
        self.editor.update(cx, |editor, cx| {
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Above(anchor),
                    height: Some(4),
                    style: BlockStyle::Fixed,
                    render: Arc::new(move |cx| {
                        render_banner_block(&version, &pun, cx).into_any_element()
                    }),
                    priority: 0,
                }],
                None,
                cx,
            );
        });
    }
}

fn register_actions(
    editor: &Entity<Editor>,
    agent_id: Option<AgentId>,
    view: WeakEntity<AgentView>,
    workspace: WeakEntity<Workspace>,
    cx: &mut Context<AgentView>,
) -> Vec<Subscription> {
    let mut subscriptions = Vec::new();
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let view = view.clone();
        let workspace = workspace.clone();
        let agent_id = agent_id.clone();
        editor.register_action(move |_: &SubmitPrompt, window, cx| {
            let Some(view) = view.upgrade() else {
                return;
            };
            let Some(text) = view.update(cx, |view, cx| view.take_prompt(cx)) else {
                return;
            };
            let agent_id = agent_id.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.handle_submit(agent_id, text, window, cx);
            });
        })
    }));
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let workspace = workspace.clone();
        editor.register_action(move |_: &AgentPrevious, window, cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.switch_agent_by_delta(-1, window, cx);
            });
        })
    }));
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let workspace = workspace.clone();
        editor.register_action(move |_: &AgentNext, window, cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.switch_agent_by_delta(1, window, cx);
            });
        })
    }));
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let workspace = workspace.clone();
        editor.register_action(move |_: &AgentNew, window, cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.select_agent(None, window, cx);
            });
        })
    }));
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let view = view.clone();
        editor.register_action(move |_: &TaskBoard, _window, cx| {
            let _ = view.update(cx, |view, cx| {
                view.system_notice(
                    "task board is not available yet",
                    StyleClass::SystemInfo,
                    cx,
                );
            });
        })
    }));
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let view = view.clone();
        editor.register_action(move |_: &RoleCycle, _window, cx| {
            let _ = view.update(cx, |view, cx| {
                view.system_notice(
                    "role cycling is not available yet",
                    StyleClass::SystemInfo,
                    cx,
                );
            });
        })
    }));
    subscriptions.push(editor.update(cx, |editor, _cx| {
        let view = view.clone();
        editor.register_action(move |_: &RoleCycleGroup, _window, cx| {
            let _ = view.update(cx, |view, cx| {
                view.system_notice(
                    "role cycling is not available yet",
                    StyleClass::SystemInfo,
                    cx,
                );
            });
        })
    }));
    subscriptions
}

const STARTUP_PUNS: &[&str] = &[
    "Rho is ready.",
    "Rows, roles, and rho.",
    "Rho-native, Unix-shaped.",
    "A small symbol for a large context.",
    "rho marks the prompt.",
    "Good tools, tight loops.",
    "Protocol first, pixels last.",
    "Streaming at terminal speed.",
    "A fresh path through the graph.",
    "Keep the context flowing.",
];

fn startup_pun() -> &'static str {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as usize)
        .unwrap_or(0);
    STARTUP_PUNS[nanos % STARTUP_PUNS.len()]
}

fn render_banner_block(version: &str, pun: &str, cx: &mut BlockContext<'_, '_>) -> impl IntoElement {
    let colors = cx.theme().colors();
    let text_style = cx.editor_style.text.clone();
    div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .ml(px(6.))
        .h(px(64.))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            svg()
                .path("icons/rho.svg")
                .w(px(31.))
                .h(px(48.))
                .text_color(colors.text_accent),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(0.))
                .font_family(text_style.font_family.clone())
                .text_size(text_style.font_size)
                .line_height(text_style.line_height)
                .text_color(text_style.color)
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .child(div().font_weight(FontWeight::BOLD).child("rho"))
                        .child(
                            div()
                                .text_color(text_style.color.opacity(0.7))
                                .child(version.trim_start_matches("rho").to_owned()),
                        ),
                )
                .child(
                    div()
                        .text_color(text_style.color.opacity(0.7))
                        .child(pun.to_owned()),
                ),
        )
}
