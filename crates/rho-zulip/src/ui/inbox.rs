//! The inbox: Gnus' group buffer for Zulip.
//!
//! Streams and their unread topics are generated read-only text in an
//! ordinary editor, so motions, search, and Vim come for free and `enter`
//! only has to answer "which row is the cursor on". Rows carry no state:
//! the listing is regenerated from the model whenever the session
//! notifies, and the cursor is restored by the narrow it was sitting on
//! rather than by line number, so an arriving message cannot move the
//! selection out from under a keypress.

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Window, div};
use theme::ActiveTheme as _;
use language::{Buffer, Capability, Point};
use std::ops::Range;
use text::Anchor;

use crate::Narrow;
use crate::model::{InboxRow, InboxRowKind};
use crate::session::{Session, Status};
use crate::ui::{Class, Hooks, Span, apply_highlights, lay_out};

pub struct InboxView {
    session: Entity<Session>,
    buffer: Entity<Buffer>,
    multi_buffer: Entity<multi_buffer::MultiBuffer>,
    editor: Entity<Editor>,
    /// The narrow each generated row opens, indexed by row.
    rows: Vec<Option<Narrow>>,
    _observe: gpui::Subscription,
}

impl InboxView {
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
            _observe: observe,
        };
        view.refresh(window, cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The narrow the cursor is on, if any: what `enter` opens.
    pub fn cursor_narrow(&self, cx: &mut Context<Self>) -> Option<Narrow> {
        let row = self.cursor_row(cx);
        self.rows.get(row).cloned().flatten()
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

    /// Places the cursor on the row for `narrow`, if the listing has one.
    pub fn focus_narrow(&mut self, narrow: &Narrow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self
            .rows
            .iter()
            .position(|candidate| candidate.as_ref() == Some(narrow))
        else {
            return;
        };
        self.place_cursor(row, window, cx);
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
                    Span::styled("zulip unavailable: ", Class::Error),
                    Span::styled(reason.clone(), Class::Muted),
                ]],
                vec![None],
            ),
            Status::Connecting => (
                vec![vec![Span::styled("connecting to zulip…", Class::Muted)]],
                vec![None],
            ),
            Status::Connected => render_rows(&session.model().inbox_rows()),
        };

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

        // The listing was just rewritten under the cursor. Put it back on
        // the conversation it was on, not the line number it was on.
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

/// Lays out the model's rows: one line of spans per row, and the narrow
/// each row opens.
fn render_rows(rows: &[InboxRow]) -> (Vec<Vec<Span>>, Vec<Option<Narrow>>) {
    if rows.is_empty() {
        return (
            vec![vec![Span::styled("no unread messages", Class::Muted)]],
            vec![None],
        );
    }
    let mut lines = Vec::with_capacity(rows.len());
    let mut targets = Vec::with_capacity(rows.len());
    for row in rows {
        let mut spans = Vec::new();
        match row.kind {
            InboxRowKind::Section => spans.push(Span::styled(row.label.clone(), Class::Muted)),
            InboxRowKind::Stream => {
                spans.push(Span::styled(format!("#{}", row.label), Class::Stream));
            }
            InboxRowKind::Topic => {
                spans.push(Span::plain("  "));
                spans.push(Span::styled(row.label.clone(), Class::Topic));
            }
            InboxRowKind::Dm => {
                spans.push(Span::plain("  "));
                spans.push(Span::styled(row.label.clone(), Class::Sender));
            }
        }
        if row.unread > 0 {
            let class = if row.mentions > 0 {
                Class::Mention
            } else {
                Class::Unread
            };
            let count = if row.mentions > 0 {
                format!("  {} (@{})", row.unread, row.mentions)
            } else {
                format!("  {}", row.unread)
            };
            spans.push(Span::styled(count, class));
        }
        lines.push(spans);
        targets.push(row.narrow.clone());
    }
    (lines, targets)
}

impl gpui::Render for InboxView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoZulipInbox")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: InboxRowKind, label: &str, unread: u32, mentions: u32) -> InboxRow {
        InboxRow {
            kind,
            label: label.to_owned(),
            unread,
            mentions,
            narrow: None,
        }
    }

    #[test]
    fn empty_listing_says_so() {
        let (lines, targets) = render_rows(&[]);
        assert_eq!(lines.len(), 1);
        assert_eq!(targets, vec![None]);
    }

    #[test]
    fn mention_counts_read_apart_from_plain_unreads() {
        let (lines, _) = render_rows(&[
            row(InboxRowKind::Topic, "colors", 3, 0),
            row(InboxRowKind::Topic, "fonts", 3, 1),
        ]);
        let text = |line: &Vec<Span>| {
            line.iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        };
        assert_eq!(text(&lines[0]), "  colors  3");
        assert_eq!(text(&lines[1]), "  fonts  3 (@1)");
        assert!(
            lines[1]
                .iter()
                .any(|span| span.class == Some(Class::Mention))
        );
    }
}
