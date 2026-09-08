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
pub mod results;

use std::ops::Range;

pub use conversation::{ConversationView, ReactionChoice, ReactionChoices};
use editor::{Editor, HighlightKey};
use gpui::{App, Context, Entity, FontWeight, HighlightStyle, Window};
use language::Buffer;
pub use list::ListView;
use multi_buffer::MultiBuffer;
pub use results::{Place, ResultsView};
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

/// The transcript needs a key per class per bucket, so it takes a wide block
/// of its own rather than a handful of slots: room for far more messages
/// than a conversation will ever hold on screen.
const SLACK_TRANSCRIPT_KEY_BASE: usize = usize::MAX / 4;

impl rho_transcript::Style for Class {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(
            SLACK_TRANSCRIPT_KEY_BASE + bucket as usize * Class::ALL.len() + self.slot(),
        )
    }

    fn highlight_style(self, cx: &gpui::App) -> gpui::HighlightStyle {
        self.resolve(cx)
    }

    /// An unfurl is a card, not a run of words: a faint tint behind the whole
    /// of it is what makes it read as one.
    fn background(self, bucket: u32, cx: &App) -> Option<(HighlightKey, gpui::Hsla)> {
        let colors = cx.theme().colors();
        let tint = match self {
            Class::Unfurl => colors.element_background,
            Class::Dealt => colors.element_selected,
            Class::Found => colors.element_hover,
            _ => return None,
        };
        let key = HighlightKey::SyntaxTreeView(
            SLACK_TRANSCRIPT_KEY_BASE + (bucket as usize + 1) * Class::ALL.len() * 2 + self.slot(),
        );
        Some((key, tint.into()))
    }
}

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
    /// A link: the label the reader sees, whose URL `enter` opens.
    Link,
    /// An unfurl's quote box, which reads as one card rather than as loose
    /// lines: the bar down its left and a faint tint over the whole of it.
    Unfurl,
    /// Slack's own emphasis, markers and all: the text keeps `*bold*` so a
    /// yanked line pastes back, and the style is what makes it read as
    /// formatting rather than as punctuation.
    Bold,
    Italic,
    Struck,
    /// The message a deal is about, tinted so the reader lands on it and can
    /// still see it after scrolling around. It stays until the surface
    /// closes: a deal is one thing to answer, not a flash.
    Dealt,
    /// The message a search landed on. Its own tint rather than `Dealt`'s:
    /// "what I went looking for" and "what rho is asking me to answer" are
    /// two different reasons for a message to be lit, and a reader should
    /// not have to work out which one they are looking at.
    Found,
}

impl Class {
    pub const ALL: [Class; 16] = [
        Class::Sender,
        Class::You,
        Class::Time,
        Class::Conversation,
        Class::Topic,
        Class::Unread,
        Class::Mention,
        Class::Muted,
        Class::Error,
        Class::Link,
        Class::Unfurl,
        Class::Bold,
        Class::Italic,
        Class::Struck,
        Class::Dealt,
        Class::Found,
    ];

    fn slot(self) -> usize {
        match self {
            Self::Sender => 0,
            Self::You => 1,
            Self::Time => 2,
            Self::Conversation => 3,
            Self::Topic => 4,
            Self::Unread => 5,
            Self::Mention => 6,
            Self::Muted => 7,
            Self::Error => 8,
            Self::Link => 9,
            Self::Unfurl => 10,
            Self::Bold => 11,
            Self::Italic => 12,
            Self::Struck => 13,
            Self::Dealt => 14,
            Self::Found => 15,
        }
    }

    pub fn highlight_key(self) -> HighlightKey {
        HighlightKey::SyntaxTreeView(SLACK_KEY_BASE + self.slot())
    }

    pub fn resolve(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        // Emphasis is shape, not colour: the run stays the body's own text
        // colour and reads as bold, italic or struck through.
        if matches!(self, Self::Bold | Self::Italic | Self::Struck) {
            return HighlightStyle {
                color: Some(colors.text.into()),
                font_weight: (self == Self::Bold).then_some(FontWeight::BOLD),
                font_style: (self == Self::Italic).then_some(gpui::FontStyle::Italic),
                strikethrough: (self == Self::Struck).then(|| gpui::StrikethroughStyle {
                    thickness: gpui::px(1.0),
                    color: Some(colors.text.into()),
                }),
                ..HighlightStyle::default()
            };
        }
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
            Self::Link | Self::Unfurl => (colors.link_text_hover, FontWeight::NORMAL),
            Self::Dealt | Self::Found => (colors.text, FontWeight::NORMAL),
            Self::Bold | Self::Italic | Self::Struck => unreachable!("styled above"),
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

/// When something was last said, as short as it can be while still saying
/// which day: the clock for today, the weekday for the last week, the date
/// beyond that, and the year when the year is not this one.
///
/// The list's column was `clock_time`, which reads as today whatever day it
/// is: a channel that last spoke on Friday afternoon showed `17:32` on
/// Monday morning, beside one that spoke ten minutes ago. The words are
/// `day_label`'s, and the day boundary is `crosses_day`'s -- local calendar
/// days, not a count of seconds -- so the list and the transcript cannot
/// disagree about where a day starts.
///
/// `now` is a parameter so the boundaries are a test rather than a wait.
pub fn when_label(timestamp: i64, now: i64) -> String {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let (Some(then), Some(now)) = (
        Local.timestamp_opt(timestamp, 0).single(),
        Local.timestamp_opt(now, 0).single(),
    ) else {
        return "--:--".to_owned();
    };
    match (now.date_naive() - then.date_naive()).num_days() {
        // A message dated ahead of the clock is a clock disagreeing, not a
        // day to name, so it reads as the time like any other of today's.
        ..=0 => then.format("%H:%M").to_string(),
        1..=6 => then.format("%a").to_string(),
        _ if then.year() == now.year() => then.format("%-d %b").to_string(),
        _ => then.format("%-d %b %Y").to_string(),
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

    /// The list's time column has to say which day, because the reader
    /// scans it to decide what to open. It said a clock and nothing else,
    /// so a channel that last spoke on Friday afternoon read `17:32` on
    /// Monday morning beside one that spoke ten minutes ago.
    #[test]
    fn a_time_says_which_day_once_the_day_is_not_this_one() {
        use chrono::{Local, TimeZone as _};

        // Midday, so that subtracting whole days cannot land on the day
        // before or after wherever this runs.
        let now = Local
            .with_ymd_and_hms(2026, 9, 8, 12, 0, 0)
            .earliest()
            .expect("a midday that exists in this timezone");
        let days_back = |days: i64| (now - chrono::Duration::days(days)).timestamp();
        let label = |days: i64| when_label(days_back(days), now.timestamp());

        assert_eq!(label(0), "12:00", "today is the clock, as it always was");
        assert_eq!(label(1), "Mon", "yesterday is named, not timed");
        assert_eq!(label(6), "Wed", "and so is the far end of the week");
        assert_eq!(
            label(7),
            "1 Sep",
            "a week back is a date: two Tuesdays would read alike"
        );
        assert_eq!(
            label(365),
            "8 Sep 2025",
            "and last year says the year, or it reads as this one"
        );
    }

    /// A message dated ahead of the clock is a clock disagreeing, not a day
    /// to name.
    #[test]
    fn a_time_in_the_future_reads_as_a_time() {
        use chrono::{Local, TimeZone as _};

        let now = Local
            .with_ymd_and_hms(2026, 9, 8, 12, 0, 0)
            .earliest()
            .expect("a midday that exists in this timezone");
        let ahead = (now + chrono::Duration::hours(26)).timestamp();
        assert_eq!(when_label(ahead, now.timestamp()), "14:00");
    }

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
