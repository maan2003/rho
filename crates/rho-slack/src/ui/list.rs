//! The way in: every conversation on one line, unread first.
//!
//! Unread before read, mentions before plain unreads, then recency, with the
//! muted ones under a rule at the bottom — the order a person actually wants
//! to walk. The listing is generated read-only
//! text in an ordinary editor, so motions and search come for free, and the
//! cursor is restored by the conversation it was sitting on rather than by
//! line number: an arriving message must not move the selection under a
//! keypress.

use std::ops::Range;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Window, div};
use language::{Buffer, Capability, Point};
use text::Anchor;
use theme::ActiveTheme as _;

use crate::model::ConversationRow;
use crate::session::{Session, Source, Status};
use crate::types::ChannelId;
use crate::ui::{Class, Hooks, Span, apply_highlights, clock_time, lay_out};

pub struct ListView {
    session: Entity<Session>,
    buffer: Entity<Buffer>,
    multi_buffer: Entity<multi_buffer::MultiBuffer>,
    editor: Entity<Editor>,
    /// The conversation each generated row opens, indexed by row.
    rows: Vec<Option<ChannelId>>,
    /// A substring the listing is narrowed to, as typed by the user.
    filter: String,
    _observe: gpui::Subscription,
}

impl ListView {
    pub fn new(
        session: Entity<Session>,
        hooks: Hooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = multi_buffer::MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                buffer.clone(),
                [Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            (hooks.configure_editor)(&mut editor, window, cx);
            editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            editor
        });
        let observe = cx.observe_in(&session, window, |this, _, window, cx| {
            this.refresh(window, cx);
        });
        let mut view = Self {
            session,
            buffer,
            multi_buffer,
            editor,
            rows: Vec::new(),
            filter: String::new(),
            _observe: observe,
        };
        view.refresh(window, cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Narrows the listing. Searching is filing here: the user types the
    /// name they have in mind and the rest of Slack goes away.
    pub fn set_filter(&mut self, filter: String, window: &mut Window, cx: &mut Context<Self>) {
        self.filter = filter;
        self.refresh(window, cx);
    }

    /// The conversation the cursor is on: what `enter` opens.
    pub fn cursor_source(&self, cx: &mut Context<Self>) -> Option<Source> {
        let row = self.cursor_row(cx);
        self.rows
            .get(row)
            .cloned()
            .flatten()
            .map(Source::Conversation)
    }

    fn cursor_row(&self, cx: &mut Context<Self>) -> usize {
        self.editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
                .row as usize
        })
    }

    fn place_cursor(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let point = Point::new(row as u32, 0);
        let anchor = self.multi_buffer.read(cx).snapshot(cx).anchor_before(point);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let held = self.rows.get(self.cursor_row(cx)).cloned().flatten();
        let session = self.session.read(cx);
        // Whatever the mirror holds is shown whatever the socket is doing:
        // a restart reads its conversations before Slack answers, and an
        // offline workspace stays readable. The status line is for when
        // there is genuinely nothing to show yet.
        let known = session.rows();
        let (lines, rows) = match session.status() {
            _ if !known.is_empty() => render_rows(&known, &self.filter),
            Status::Failed(reason) => (
                vec![vec![
                    Span::styled("slack unavailable: ", Class::Error),
                    Span::styled(reason.clone(), Class::Muted),
                ]],
                vec![None],
            ),
            Status::Connecting => (
                vec![vec![Span::styled("connecting to slack…", Class::Muted)]],
                vec![None],
            ),
            Status::Connected => render_rows(&known, &self.filter),
        };
        let mut lines = lines;
        let mut rows = rows;
        if let Some(reason) = session.health_reason() {
            lines.insert(0, vec![Span::styled(reason.to_owned(), Class::Error)]);
            rows.insert(0, None);
        }

        let mut text = String::new();
        let mut styles: Vec<(Class, Range<usize>)> = Vec::new();
        for line in &lines {
            let (line_text, line_styles) = lay_out(line);
            let base = text.len();
            text.push_str(&line_text);
            text.push('\n');
            styles.extend(
                line_styles
                    .into_iter()
                    .map(|(class, range)| (class, base + range.start..base + range.end)),
            );
        }

        let anchored = self.buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
            let snapshot = buffer.snapshot();
            styles
                .into_iter()
                .map(|(class, range)| {
                    let clamp = |offset: usize| offset.min(snapshot.len());
                    (
                        class,
                        vec![
                            snapshot.anchor_before(clamp(range.start))
                                ..snapshot.anchor_after(clamp(range.end)),
                        ],
                    )
                })
                .collect::<Vec<(Class, Vec<Range<Anchor>>)>>()
        });
        self.rows = rows;
        apply_highlights(&self.editor, &self.multi_buffer, &anchored, cx);

        if let Some(held) = held
            && let Some(row) = self
                .rows
                .iter()
                .position(|candidate| candidate.as_ref() == Some(&held))
        {
            self.place_cursor(row, window, cx);
        }
        cx.notify();
    }
}

/// One line per conversation: the name, what is waiting in it, and when it
/// last spoke. No ids and no last-message preview — the list is for choosing
/// where to go, and a preview is the conversation's job.
fn render_rows(rows: &[ConversationRow], filter: &str) -> (Vec<Vec<Span>>, Vec<Option<ChannelId>>) {
    let needle = filter.trim().to_lowercase();
    let matching = rows
        .iter()
        .filter(|row| needle.is_empty() || row.label.to_lowercase().contains(&needle))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        let message = match needle.is_empty() {
            true => "no conversations",
            false => "nothing matches",
        };
        return (vec![vec![Span::styled(message, Class::Muted)]], vec![None]);
    }
    let mut lines = Vec::with_capacity(matching.len());
    let mut targets = Vec::with_capacity(matching.len());
    let mut muted_section = false;
    for row in matching {
        // The one break in the list: everything under it was muted, so an
        // unread there is not something the reader owes anybody.
        if row.muted && !muted_section {
            muted_section = true;
            lines.push(vec![Span::styled("─────", Class::Muted)]);
            targets.push(None);
        }
        let mut spans = vec![Span::styled(row.label.clone(), Class::Conversation)];
        let mut waiting = Vec::new();
        if row.mention_count > 0 {
            waiting.push(Span::styled(
                format!("@{}", row.mention_count),
                Class::Mention,
            ));
        }
        // A number when there is one to give. Slack counts DMs for us and
        // rho counts what it watched land; a channel unread since before
        // the last start has neither, and says so in words.
        if row.unread_count > 0 {
            waiting.push(Span::styled(
                format!("{} new", row.unread_count),
                Class::Unread,
            ));
        } else if row.unread && row.mention_count == 0 {
            waiting.push(Span::styled("unread", Class::Unread));
        }
        for (at, span) in waiting.into_iter().enumerate() {
            spans.push(Span::plain(match at {
                0 => "  ",
                _ => " · ",
            }));
            spans.push(span);
        }
        if let Some(latest) = &row.latest {
            spans.push(Span::plain("  "));
            spans.push(Span::styled(
                clock_time(latest.epoch_seconds() as i64),
                Class::Time,
            ));
        }
        lines.push(spans);
        targets.push(Some(row.id.clone()));
    }
    (lines, targets)
}

impl gpui::Render for ListView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoSlackList")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Ts;

    fn row(label: &str, unread: bool, mentions: u32) -> ConversationRow {
        ConversationRow {
            id: ChannelId(label.to_owned()),
            label: label.to_owned(),
            unread,
            mention_count: mentions,
            unread_count: 0,
            muted: false,
            latest: None,
        }
    }

    fn text(line: &[Span]) -> String {
        line.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn a_row_carries_its_counts_and_the_time_it_last_spoke() {
        let mut design = row("#design", true, 2);
        design.unread_count = 5;
        // A fixed instant so the clock column is the same wherever this
        // runs: the offset is the machine's, the format is what is asserted.
        design.latest = Some(Ts("1755780420.000100".into()));
        let expected = format!(
            "#design  @2 · 5 new  {}",
            crate::ui::clock_time(1_755_780_420)
        );
        let (lines, _) = render_rows(&[design], "");
        assert_eq!(text(&lines[0]), expected);
    }

    #[test]
    fn a_channel_unread_from_before_the_last_start_says_so_in_words() {
        // Slack counts messages for DMs only, so a channel that was already
        // unread at connect has no number to show and must not invent one.
        let (lines, _) = render_rows(&[row("#design", true, 0)], "");
        assert_eq!(text(&lines[0]), "#design  unread");
    }

    #[test]
    fn muted_conversations_sit_at_the_bottom_under_one_break() {
        let mut muted = row("#noise", true, 0);
        muted.muted = true;
        let (lines, targets) = render_rows(&[row("#design", true, 2), muted], "");
        assert_eq!(text(&lines[0]), "#design  @2");
        assert_eq!(text(&lines[1]), "─────", "muted conversations start here");
        assert_eq!(targets[1], None, "the break opens nothing");
        assert_eq!(text(&lines[2]), "#noise  unread");
    }

    #[test]
    fn a_filter_narrows_to_what_was_typed() {
        let rows = [row("#design", false, 0), row("@ada", false, 0)];
        let (lines, targets) = render_rows(&rows, "ad");
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "@ada");
        assert_eq!(targets[0], Some(ChannelId("@ada".into())));

        let (lines, targets) = render_rows(&rows, "zzz");
        assert_eq!(text(&lines[0]), "nothing matches");
        assert_eq!(targets, vec![None]);
    }
}
