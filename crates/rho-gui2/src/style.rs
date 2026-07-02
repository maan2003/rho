//! Semantic style classes and their mapping onto theme colors and editor
//! highlight keys.
//!
//! Every rendered span carries a [`StyleClass`]; colors are resolved only at
//! application time so themes stay client-side. Each class maps to two stable
//! editor highlight keys: one for settled transcript history (updated once
//! per turn) and one for the live turn (updated per streaming event).

use editor::HighlightKey;
use gpui::{App, FontWeight, HighlightStyle, Hsla};
use theme::ActiveTheme as _;

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
            Self::UserMessage => (colors.text_accent, false),
            Self::SystemInfo => (colors.text_muted, false),
            Self::SystemImportant => (colors.terminal_ansi_yellow, true),
            Self::Disconnect => (colors.terminal_ansi_red, false),
            Self::ToolName => (colors.terminal_ansi_yellow, false),
            Self::ToolShell | Self::ToolDetail => (hint_color(cx), false),
            Self::StatusRunning => (colors.terminal_ansi_cyan, false),
            Self::StatusOk => (colors.terminal_ansi_green, false),
            Self::StatusError => (colors.terminal_ansi_red, false),
            Self::StatusCancelled => (colors.terminal_ansi_yellow, false),
            Self::Time => (colors.text_muted, false),
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
        .unwrap_or(cx.theme().status().hint)
}

pub fn user_prompt_gutter_color(cx: &App) -> Hsla {
    cx.theme().colors().text_accent
}

pub fn role_chip_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().terminal_ansi_magenta),
        ..HighlightStyle::default()
    }
}

pub fn muted_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().text_muted),
        ..HighlightStyle::default()
    }
}

pub fn cwd_chip_style(cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: Some(cx.theme().colors().terminal_foreground),
        ..HighlightStyle::default()
    }
}
