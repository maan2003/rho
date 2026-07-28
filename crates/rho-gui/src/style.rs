//! Semantic style classes and their mapping onto theme colors and editor
//! highlight keys.
//!
//! Every rendered span carries a [`StyleClass`]; colors are resolved only at
//! application time so themes stay client-side. Each class maps to two stable
//! editor highlight keys: one for settled transcript history (updated once
//! per turn) and one for the live turn (updated per streaming event).

use std::sync::Arc;

use editor::HighlightKey;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use gpui::prelude::*;
use gpui::{App, FontWeight, HighlightStyle, Hsla, div};
use multi_buffer::Anchor;
use rho_core::ContentPart;
use theme::ActiveTheme as _;

/// How much larger a user message renders than everything around it.
///
/// A transcript is mostly agent prose and tool output, and a turn of your own
/// is a couple of lines in a thousand: color alone does not find it when you
/// are scrolling. Size does, because it survives being seen out of the corner
/// of an eye, which is how a transcript is actually read.
///
/// A little goes a long way: this is 18px against the default 16px buffer
/// font, which reads as a different weight of message from across the screen
/// without turning your own words into a headline.
///
/// Row height does not vary with this - the editor's rows are uniform - so a
/// scaled row keeps the transcript's leading rather than getting its own.
/// Scaling the editor's line height to fit instead would spend the leading on
/// every row of the document, including the ones you are writing, which is a
/// worse deal than a tight row on the few rows that are large.
pub const USER_MESSAGE_SCALE: f32 = 1.125;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleFamily {
    Deep,
    Fable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StyleClass {
    Default,
    UserMessage,
    SystemInfo,
    SystemImportant,
    Disconnect,
    ToolName,
    ToolShell,
    ToolDetail,
    StatusRunning,
    StatusOk,
    StatusError,
    StatusCancelled,
    Time,
    AgentMessage,
    AgentLabel,
    ShellPrompt,
    ShellCommand,
    /// Tree-sitter highlight, by syntax-theme index (see
    /// `language::HighlightId`).
    Syntax(u32),
}

/// Which highlight-key space a style range lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    History,
    LiveTurn,
    /// Local system notices, kept apart from the transcript projection.
    System,
}

const SEMANTIC_KEY_BASE: usize = 0;
const SYNTAX_KEY_BASE: usize = 1_000;
pub const PROMPT_DRAFT_HIGHLIGHT_KEY: usize = usize::MAX - 1;

impl StyleClass {
    pub fn highlight_key(self, region: Region) -> HighlightKey {
        let slot = match self {
            Self::Default => 0,
            Self::UserMessage => 1,
            Self::SystemInfo => 2,
            Self::SystemImportant => 3,
            Self::Disconnect => 4,
            Self::ToolName => 5,
            Self::ToolShell => 6,
            Self::ToolDetail => 7,
            Self::StatusRunning => 8,
            Self::StatusOk => 9,
            Self::StatusError => 10,
            Self::StatusCancelled => 11,
            Self::Time => 12,
            Self::AgentMessage => 13,
            Self::ShellPrompt => 14,
            Self::ShellCommand => 15,
            Self::AgentLabel => 16,
            Self::Syntax(id) => SYNTAX_KEY_BASE + id as usize,
        };
        let region_bit = match region {
            Region::History => 0,
            Region::LiveTurn => 1,
            Region::System => 2,
        };
        HighlightKey::SyntaxTreeView(SEMANTIC_KEY_BASE + slot * 3 + region_bit)
    }

    pub fn resolve(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let (color, bold) = match self {
            Self::Default => return HighlightStyle::default(),
            Self::UserMessage => (colors.text_accent.into(), false),
            Self::SystemInfo => (colors.text_muted.into(), false),
            Self::SystemImportant => (colors.terminal_ansi_yellow.into(), true),
            Self::Disconnect => (colors.terminal_ansi_red.into(), false),
            Self::ToolName | Self::ToolShell | Self::ToolDetail => {
                (colors.text_muted.into(), false)
            }
            Self::StatusRunning => (colors.terminal_ansi_cyan.into(), false),
            Self::StatusOk => (colors.terminal_ansi_green.into(), false),
            Self::StatusError => (colors.terminal_ansi_red.into(), false),
            Self::StatusCancelled => (colors.terminal_ansi_yellow.into(), false),
            Self::Time => (colors.text_muted.into(), false),
            Self::AgentMessage => (agent_message_color(cx), false),
            Self::AgentLabel => (agent_message_color(cx), false),
            Self::ShellPrompt => (colors.terminal_ansi_green.into(), false),
            Self::ShellCommand => (colors.text_accent.into(), false),
            Self::Syntax(id) => {
                return cx
                    .theme()
                    .syntax()
                    .get(id as usize)
                    .copied()
                    .unwrap_or_default();
            }
        };
        HighlightStyle {
            color: Some(color),
            font_weight: bold.then_some(FontWeight::BOLD),
            ..HighlightStyle::default()
        }
    }
}

pub fn hint_color(cx: &App) -> Hsla {
    cx.theme()
        .syntax()
        .style_for_name("hint")
        .and_then(|style| style.color)
        .unwrap_or(cx.theme().status().hint.into())
}

pub fn user_prompt_gutter_color(cx: &App) -> Hsla {
    cx.theme().colors().text_accent.into()
}

pub fn agent_message_gutter_color(cx: &App) -> Hsla {
    agent_message_color(cx)
}

fn agent_message_color(cx: &App) -> Hsla {
    cx.theme().colors().terminal_ansi_cyan.into()
}

pub fn cwd_chip_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().terminal_foreground.into()),
        ..HighlightStyle::default()
    }
}

pub fn workspace_chip_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().terminal_ansi_green.into()),
        ..HighlightStyle::default()
    }
}

pub fn context_chip_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().text_muted.into()),
        ..HighlightStyle::default()
    }
}

pub fn role_chip_style(family: RoleFamily, cx: &App) -> HighlightStyle {
    let colors = cx.theme().colors();
    let color = match family {
        RoleFamily::Deep => colors.terminal_ansi_cyan,
        RoleFamily::Fable => colors.terminal_ansi_magenta,
    };
    HighlightStyle {
        color: Some(color.into()),
        ..HighlightStyle::default()
    }
}

/// One editor row of compact media chips below a writable prompt.
pub fn attachment_block(anchor: Anchor, attachments: &[ContentPart]) -> BlockProperties<Anchor> {
    let labels = attachments
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(format!(
                "{} · {} KB",
                media_type
                    .strip_prefix("image/")
                    .unwrap_or(media_type)
                    .to_ascii_uppercase(),
                data.len().div_ceil(1024)
            )),
            ContentPart::Text { .. } => None,
        })
        .collect::<Vec<_>>();
    BlockProperties {
        placement: BlockPlacement::Below(anchor),
        height: Some(1),
        style: BlockStyle::Fixed,
        render: Arc::new(move |cx| render_attachment_block(&labels, cx).into_any_element()),
        priority: 0,
    }
}

fn render_attachment_block(labels: &[String], cx: &mut BlockContext<'_, '_>) -> impl IntoElement {
    let text_style = cx.editor_style.text.clone();
    let colors = cx.app.theme().colors();
    let mut row = div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .h(cx.line_height)
        .flex()
        .items_center()
        .gap_1()
        .font_family(text_style.font_family.clone())
        .text_size(text_style.font_size)
        .line_height(text_style.line_height);
    for label in labels {
        row = row.child(
            div()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.element_background)
                .text_color(colors.text_muted)
                .child(format!("image · {label}")),
        );
    }
    row
}
