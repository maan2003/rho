//! The way in: every conversation on one line, unread first.
//!
//! Unread before read, mentions before plain unreads, then recency — the
//! order a person actually wants to walk. The listing is generated read-only
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
use crate::ui::{Class, Hooks, Span, apply_highlights, lay_out};

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
        let (lines, rows) = match session.status() {
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
            Status::Connected => render_rows(&session.rows(), &self.filter),
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

/// One line per conversation: the name, then its unread count, then how
/// recently it spoke. No ids and no timestamps.
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
    let mut unread_section = false;
    for row in matching {
        let mut spans = Vec::new();
        // One break between what is waiting and what is merely there, so
        // the eye stops where the reading stops.
        if !unread_section && row.unread {
            unread_section = true;
        } else if unread_section && !row.unread {
            unread_section = false;
            lines.push(vec![Span::styled("─────", Class::Muted)]);
            targets.push(None);
        }
        spans.push(Span::styled(row.label.clone(), Class::Conversation));
        if row.mention_count > 0 {
            spans.push(Span::styled(
                format!("  @{}", row.mention_count),
                Class::Mention,
            ));
        } else if row.unread {
            spans.push(Span::styled("  unread", Class::Unread));
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
            latest: Some(Ts("1.0".into())),
        }
    }

    fn text(line: &[Span]) -> String {
        line.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn unread_conversations_sit_above_the_rest_behind_one_break() {
        let (lines, targets) = render_rows(
            &[
                row("#design", true, 2),
                row("@ada", true, 0),
                row("#random", false, 0),
            ],
            "",
        );
        assert_eq!(text(&lines[0]), "#design  @2");
        assert_eq!(text(&lines[1]), "@ada  unread");
        assert_eq!(text(&lines[2]), "─────", "read conversations start here");
        assert_eq!(targets[2], None, "the break opens nothing");
        assert_eq!(text(&lines[3]), "#random");
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
