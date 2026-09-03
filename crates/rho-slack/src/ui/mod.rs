//! The Slack surfaces: a conversation list and one conversation transcript.
//!
//! Both are ordinary editors over multibuffers, so motions, search, and Vim
//! come from the editor rather than bespoke list chrome — the same trick
//! Rho's dashboard and the Zulip client play.
//!
//! A channel, a group, a DM, and a thread are all the same surface: they
//! differ in where a composed message goes, not in how they read.

pub mod conversation;
pub mod list;

use std::ops::Range;

pub use conversation::ConversationView;
use editor::{Editor, HighlightKey};
use gpui::{App, Context, Entity, FontWeight, HighlightStyle, Window};
use language::Buffer;
pub use list::ListView;
use multi_buffer::MultiBuffer;
use text::Anchor;
use theme::ActiveTheme as _;

/// Host-supplied editor and buffer configuration.
///
/// The client owns its views but not the frame they live in: editor chrome
/// and the Markdown syntax pipeline belong to the host application, which
/// keeps them consistent with every other surface and keeps this crate off
/// the host's internals.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Applies the host's editor chrome (gutters, wrapping, affordances).
    pub configure_editor: fn(&mut Editor, &mut Window, &mut Context<Editor>),
    /// Attaches the host's Markdown syntax pipeline to a message buffer.
    pub configure_markdown: fn(&mut Buffer, &mut Context<Buffer>),
}

impl Hooks {
    /// Hooks that do nothing, for tests and for hosts with no Markdown
    /// pipeline of their own.
    pub fn inert() -> Self {
        Self {
            configure_editor: |_, _, _| {},
            configure_markdown: |_, _| {},
        }
    }
}

/// Highlight-key space for Slack surfaces, kept clear of the Zulip client's
/// slots so a frame holding both cannot collide.
const SLACK_KEY_BASE: usize = usize::MAX - 500;

/// A semantic span class. Colors resolve against the active theme at
/// application time, so surfaces follow the host's theme for free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// A message's sender name.
    Sender,
    /// Your own name, so your messages are findable while scrolling.
    You,
    /// Timestamps and other chrome.
    Time,
    /// A conversation name in the list.
    Conversation,
    /// A thread's summary line.
    Topic,
    /// An unread count.
    Unread,
    /// A count that includes a mention of you.
    Mention,
    /// Muted chrome: section headers, empty-state text.
    Muted,
    /// A failure notice.
    Error,
}

impl Class {
    pub const ALL: [Class; 9] = [
        Class::Sender,
        Class::You,
        Class::Time,
        Class::Conversation,
        Class::Topic,
        Class::Unread,
        Class::Mention,
        Class::Muted,
        Class::Error,
    ];

    pub fn highlight_key(self) -> HighlightKey {
        let slot = match self {
            Self::Sender => 0,
            Self::You => 1,
            Self::Time => 2,
            Self::Conversation => 3,
            Self::Topic => 4,
            Self::Unread => 5,
            Self::Mention => 6,
            Self::Muted => 7,
            Self::Error => 8,
        };
        HighlightKey::SyntaxTreeView(SLACK_KEY_BASE + slot)
    }

    pub fn resolve(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let (color, weight) = match self {
            Self::Sender => (colors.terminal_ansi_cyan, FontWeight::BOLD),
            Self::You => (colors.text_accent, FontWeight::BOLD),
            Self::Time => (colors.text_muted, FontWeight::NORMAL),
            Self::Conversation => (colors.terminal_ansi_green, FontWeight::BOLD),
            Self::Topic => (colors.text, FontWeight::NORMAL),
            Self::Unread => (colors.text_accent, FontWeight::NORMAL),
            Self::Mention => (colors.terminal_ansi_yellow, FontWeight::BOLD),
            Self::Muted => (colors.text_muted, FontWeight::NORMAL),
            Self::Error => (colors.terminal_ansi_red, FontWeight::NORMAL),
        };
        HighlightStyle {
            color: Some(color.into()),
            font_weight: Some(weight),
            ..HighlightStyle::default()
        }
    }
}

/// A run of text carrying its class, the unit both surfaces render into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub class: Option<Class>,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            class: None,
        }
    }

    pub fn styled(text: impl Into<String>, class: Class) -> Self {
        Self {
            text: text.into(),
            class: Some(class),
        }
    }
}

/// Renders spans into a string, collecting each classed run's byte range.
pub fn lay_out(spans: &[Span]) -> (String, Vec<(Class, Range<usize>)>) {
    let mut text = String::new();
    let mut ranges = Vec::new();
    for span in spans {
        let start = text.len();
        text.push_str(&span.text);
        if let Some(class) = span.class {
            ranges.push((class, start..text.len()));
        }
    }
    (text, ranges)
}

/// Applies class highlights to an editor, clearing classes with no ranges
/// so a re-render cannot leave stale color behind.
pub fn apply_highlights<V: 'static>(
    editor: &Entity<Editor>,
    multi_buffer: &Entity<MultiBuffer>,
    styles: &[(Class, Vec<Range<Anchor>>)],
    cx: &mut Context<V>,
) {
    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let mut resolved: Vec<(Class, Vec<multi_buffer::Anchor>, Vec<multi_buffer::Anchor>)> =
        Vec::new();
    for class in Class::ALL {
        let ranges = styles
            .iter()
            .filter(|(candidate, _)| *candidate == class)
            .flat_map(|(_, ranges)| ranges.iter())
            .filter_map(|range| {
                Some((
                    snapshot.anchor_in_excerpt(range.start)?,
                    snapshot.anchor_in_excerpt(range.end)?,
                ))
            })
            .collect::<Vec<_>>();
        let (starts, ends) = ranges.into_iter().unzip();
        resolved.push((class, starts, ends));
    }
    editor.update(cx, |editor, cx| {
        for (class, starts, ends) in resolved {
            let ranges = starts
                .into_iter()
                .zip(ends)
                .map(|(start, end)| start..end)
                .collect::<Vec<_>>();
            editor.highlight_text(class.highlight_key(), ranges, class.resolve(cx), cx);
        }
    });
}

/// A wall-clock `HH:MM` in the local timezone, for message headers.
pub fn clock_time(timestamp: i64) -> String {
    use chrono::{Local, TimeZone as _};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(time) => time.format("%H:%M").to_string(),
        None => "--:--".to_owned(),
    }
}

/// A calendar day label (`Mon 4 Aug`), for the separator between days.
pub fn day_label(timestamp: i64) -> String {
    use chrono::{Local, TimeZone as _};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(time) => time.format("%a %-d %b").to_string(),
        None => "unknown date".to_owned(),
    }
}

/// Whether two timestamps fall on different local days, so the transcript
/// can break between them.
pub fn crosses_day(earlier: i64, later: i64) -> bool {
    use chrono::{Local, TimeZone as _};
    let (Some(earlier), Some(later)) = (
        Local.timestamp_opt(earlier, 0).single(),
        Local.timestamp_opt(later, 0).single(),
    ) else {
        return false;
    };
    earlier.date_naive() != later.date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lay_out_records_only_classed_runs() {
        let (text, ranges) = lay_out(&[
            Span::styled("alice", Class::Sender),
            Span::plain(" · "),
            Span::styled("14:32", Class::Time),
        ]);
        assert_eq!(text, "alice · 14:32");
        assert_eq!(
            ranges,
            // The separator is four bytes: the middle dot is not ASCII,
            // and these ranges index bytes, not characters.
            vec![(Class::Sender, 0..5), (Class::Time, 9..14)],
            "byte ranges must skip unclassed text"
        );
    }
}
