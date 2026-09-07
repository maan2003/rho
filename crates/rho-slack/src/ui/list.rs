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
    /// What each line of the buffer currently says, so a redraw can edit
    /// the lines that moved instead of writing the listing again. Empty
    /// when nothing incremental is possible and the next draw must build
    /// the whole thing.
    drawn: Vec<DrawnLine>,
    /// How many of the drawn rows are unmuted, which is where the break
    /// between the two sections sits. Kept because it is what turns a
    /// row's place in the list into a line number in the buffer.
    unmuted: usize,
    muted: usize,
    /// Lines above the listing: the health notice, when there is one.
    banner: usize,
    _observe: gpui::Subscription,
}

/// One line of the buffer as it currently stands.
struct DrawnLine {
    /// The conversation the line opens, or `None` for the break and the
    /// health notice, which open nothing.
    id: Option<ChannelId>,
    muted: bool,
    text: String,
    styles: Vec<(Class, Range<usize>)>,
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
            drawn: Vec::new(),
            unmuted: 0,
            muted: 0,
            banner: 0,
            _observe: observe,
        };
        view.refresh(window, cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The names a query reaches, for a caller that wants to show matches
    /// without narrowing to them: the minibuffer offering completions
    /// while the reader types. Answered off the same word index, so it
    /// costs the range scan and the matches returned, not the workspace.
    pub fn reached_by(&self, query: &str, most: usize, cx: &gpui::App) -> Vec<ConversationRow> {
        self.session.read(cx).reached_by(query, most)
    }

    pub fn filter(&self, cx: &Context<Self>) -> String {
        self.session.read(cx).query()
    }

    /// Narrows the listing. Searching is filing here: the reader types a
    /// word of the name they have in mind and the rest of Slack goes away.
    ///
    /// The narrowing is the model's, not the view's: the model holds the
    /// index the query is answered from and says which rows left and which
    /// arrived, so a keystroke edits those lines and no others.
    pub fn set_filter(&mut self, filter: String, window: &mut Window, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, _| session.narrow(&filter));
        self.refresh(window, cx);
    }

    /// Where a conversation sits in the listing, and where the point is
    /// put. Both exist so that the rule the point follows — the
    /// conversation, never the line number — can be asserted from outside.
    #[cfg(any(test, feature = "fake"))]
    pub fn row_of_for_test(&self, channel: &ChannelId) -> Option<usize> {
        self.line_of_id(channel)
    }

    /// The conversation names the buffer currently holds, in the order
    /// they are drawn. What a narrowing is asserted against from outside.
    #[cfg(any(test, feature = "fake"))]
    pub fn drawn_conversations_for_test(&self, _cx: &gpui::App) -> Vec<String> {
        self.drawn
            .iter()
            .filter(|line| line.id.is_some())
            .map(|line| line.text.split("  ").next().unwrap_or_default().to_owned())
            .collect()
    }

    #[cfg(any(test, feature = "fake"))]
    pub fn place_cursor_for_test(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_cursor(row, window, cx);
    }

    /// The conversation the cursor is on: what `enter` opens.
    pub fn cursor_source(&self, cx: &mut Context<Self>) -> Option<Source> {
        let row = self.cursor_row(cx);
        self.id_at(row).map(Source::Conversation)
    }

    /// The conversation a buffer line opens. `drawn` is the buffer as it
    /// stands, one entry per line, so this is the lookup and there is no
    /// second copy of the ids to keep in step with it.
    fn id_at(&self, row: usize) -> Option<ChannelId> {
        self.drawn.get(row).and_then(|line| line.id.clone())
    }

    /// The line a conversation is on. A scan, and the only caller is a
    /// keypress or a redraw that has already moved the point, never a
    /// frame.
    fn line_of_id(&self, channel: &ChannelId) -> Option<usize> {
        self.drawn
            .iter()
            .position(|line| line.id.as_ref() == Some(channel))
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

    /// Brings the listing up to date.
    ///
    /// Two ways, and the cheap one is the ordinary one. When the buffer
    /// already holds the listing and the model can say what moved, the
    /// lines that moved are edited and nothing else is touched: a message
    /// arriving is one line out and one line in, whatever the workspace
    /// holds, and so is a letter typed into the narrowing. Everything else
    /// — the first draw, entering or leaving a narrowing, a status line
    /// instead of a listing, or a model that had to give up its log —
    /// writes the whole buffer, because in those cases the buffer and the
    /// listing no longer have a line in common to build from.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let held = self.id_at(self.cursor_row(cx));
        let banner = self
            .session
            .read(cx)
            .health_reason()
            .map(|reason| vec![Span::styled(reason.to_owned(), Class::Error)]);
        let listing = self.session.read(cx).has_rows();
        let edits = match listing {
            true => self
                .session
                .update(cx, |session, _| session.take_row_edits()),
            false => {
                self.session
                    .update(cx, |session, _| session.forget_row_edits());
                None
            }
        };
        let incremental =
            listing && !self.drawn.is_empty() && banner.is_some() == (self.banner == 1);
        match (incremental, edits) {
            (true, Some(edits)) => self.apply_edits(edits, cx),
            _ => self.rebuild(banner, cx),
        }
        // The point follows the conversation, not the line number: rows
        // moving above the reader must not move the reader.
        if let Some(held) = held
            && let Some(row) = self.line_of_id(&held)
        {
            self.place_cursor(row, window, cx);
        }
        cx.notify();
    }

    /// Writes the whole listing. The first draw, and the fallback whenever
    /// an edit cannot be placed against what the buffer holds.
    fn rebuild(&mut self, banner: Option<Vec<Span>>, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        // Whatever the mirror holds is shown whatever the socket is doing:
        // a restart reads its conversations before Slack answers, and an
        // offline workspace stays readable. The status line is for when
        // there is genuinely nothing to show yet.
        let known = session.rows();
        let narrowed = session.is_narrowed();
        let (lines, rows) = match session.status() {
            _ if !known.is_empty() => render_rows(&known),
            // A query that reaches nothing says so, rather than falling
            // through to a status line about the socket, which is not what
            // the reader just did.
            _ if narrowed => (
                vec![vec![Span::styled("nothing matches", Class::Muted)]],
                vec![None],
            ),
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
            Status::Connected => (
                vec![vec![Span::styled("no conversations", Class::Muted)]],
                vec![None],
            ),
        };
        let muted = known.iter().filter(|row| row.muted).count();
        let listed = !known.is_empty();

        self.drawn = Vec::with_capacity(lines.len() + 1);
        self.banner = usize::from(banner.is_some());
        // The counters only mean anything when the buffer holds the whole
        // listing; a narrowed or status-line buffer has no places to
        // compute, and the next draw rebuilds anyway.
        self.unmuted = match listed {
            true => known.len() - muted,
            false => 0,
        };
        self.muted = match listed {
            true => muted,
            false => 0,
        };
        for line in banner.into_iter().chain(lines) {
            let (text, styles) = lay_out(&line);
            self.drawn.push(DrawnLine {
                id: None,
                muted: false,
                text,
                styles,
            });
        }
        // The ids come from the render, which knows which lines are rows
        // and which are the break; the banner is one more line in front.
        for (at, id) in rows.iter().enumerate() {
            let line = &mut self.drawn[at + self.banner];
            line.id = id.clone();
            line.muted = id
                .as_ref()
                .and_then(|id| known.iter().find(|row| &row.id == id))
                .is_some_and(|row| row.muted);
        }
        // A rebuild has drawn everything, so anything the model was still
        // holding has been drawn too.
        self.session
            .update(cx, |session, _| session.forget_row_edits());
        let text = self.drawn.iter().fold(String::new(), |mut text, line| {
            text.push_str(&line.text);
            text.push('\n');
            text
        });
        self.buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.paint(cx);
    }

    /// Applies the model's list edits to the buffer, one line at a time.
    ///
    /// An edit names a conversation, not a place: the line to take out is
    /// the one this view put that conversation on, and the line to put it
    /// back on is the one its new neighbour is on. Neither is a number the
    /// model had to count. A row moving is a line deleted and a line
    /// inserted; a badge changing in place is the same two edits at the
    /// same line, which the rope handles as a rewrite of one line.
    fn apply_edits(&mut self, edits: Vec<crate::model::RowEdit>, cx: &mut Context<Self>) {
        for edit in edits {
            let leaving = match edit.was_shown {
                true => self.line_of_id(&edit.channel),
                false => None,
            };
            let left_muted = leaving.is_some_and(|line| self.drawn[line].muted);
            // Muting moves the break between the two sections, and the
            // break is a line the model knows nothing about. Rather than
            // reason about it here, the listing is written again: it
            // happens when the reader mutes something, not when a message
            // arrives.
            let muted_after = self.muted - usize::from(left_muted)
                + usize::from(edit.row.as_ref().is_some_and(|row| row.muted));
            if (self.muted > 0) != (muted_after > 0) {
                self.rebuild(self.banner_span(cx), cx);
                return;
            }
            if let Some(line) = leaving {
                match left_muted {
                    true => self.muted -= 1,
                    false => self.unmuted -= 1,
                }
                self.remove_line(line, cx);
            }
            if let Some(row) = edit.row {
                let at = self.line_before(edit.before.as_ref(), row.muted);
                match row.muted {
                    true => self.muted += 1,
                    false => self.unmuted += 1,
                }
                let line = render_row(&row);
                let (text, styles) = lay_out(&line);
                self.insert_line(
                    at,
                    DrawnLine {
                        id: Some(row.id.clone()),
                        muted: row.muted,
                        text,
                        styles,
                    },
                    cx,
                );
            }
        }
        self.paint(cx);
    }

    /// The buffer line a row goes on, given the conversation it now sits
    /// above. `None` for a neighbour means the end of the list.
    ///
    /// The clamp is the break line. An unmuted row can never land below
    /// the break, and its neighbour in the model's order may well be the
    /// first muted row, whose line is one past it — the model does not
    /// know the break exists, so this is where it is accounted for.
    fn line_before(&self, before: Option<&ChannelId>, muted: bool) -> usize {
        let at = before
            .and_then(|before| self.line_of_id(before))
            .unwrap_or(self.drawn.len());
        match muted {
            true => at,
            false => at.min(self.banner + self.unmuted),
        }
    }

    fn banner_span(&self, cx: &Context<Self>) -> Option<Vec<Span>> {
        self.session
            .read(cx)
            .health_reason()
            .map(|reason| vec![Span::styled(reason.to_owned(), Class::Error)])
    }

    fn remove_line(&mut self, at: usize, cx: &mut Context<Self>) {
        if at >= self.drawn.len() {
            return;
        }
        self.drawn.remove(at);
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(
                    Point::new(at as u32, 0)..Point::new(at as u32 + 1, 0),
                    String::new(),
                )],
                None,
                cx,
            );
        });
    }

    fn insert_line(&mut self, at: usize, line: DrawnLine, cx: &mut Context<Self>) {
        let at = at.min(self.drawn.len());
        let text = format!("{}\n", line.text);
        self.drawn.insert(at, line);
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(Point::new(at as u32, 0)..Point::new(at as u32, 0), text)],
                None,
                cx,
            );
        });
    }

    /// Re-anchors and re-applies the highlights.
    ///
    /// This is the one part of a redraw still proportional to the listing
    /// rather than to what moved: the editor takes the whole set of ranges
    /// for a class at once, so every line's ranges are handed over again
    /// even when one line changed. It is anchor arithmetic and no string
    /// work, and it is the next thing to fix — with an incremental
    /// highlight on the editor side, not by drawing the list some other
    /// way.
    fn paint(&mut self, cx: &mut Context<Self>) {
        let mut styles: Vec<(Class, Range<usize>)> = Vec::new();
        let mut base = 0usize;
        for line in &self.drawn {
            styles.extend(
                line.styles
                    .iter()
                    .map(|(class, range)| (*class, base + range.start..base + range.end)),
            );
            base += line.text.len() + 1;
        }
        let anchored = self.buffer.update(cx, |buffer, _| {
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
        apply_highlights(&self.editor, &self.multi_buffer, &anchored, cx);
    }
}

/// One line per conversation: the name, what is waiting in it, and when it
/// last spoke. No ids and no last-message preview — the list is for choosing
/// where to go, and a preview is the conversation's job.
/// The rows the model handed over, laid out. Narrowing happened before
/// this: the model answers a query from its own index, so nothing here
/// looks at every conversation to decide what to draw.
fn render_rows(rows: &[ConversationRow]) -> (Vec<Vec<Span>>, Vec<Option<ChannelId>>) {
    let matching = rows.iter().collect::<Vec<_>>();
    let mut lines = Vec::with_capacity(matching.len());
    let mut targets = Vec::with_capacity(matching.len());
    let mut muted_section = false;
    for row in matching {
        // The one break in the list: everything under it was muted, so an
        // unread there is not something the reader owes anybody.
        if row.muted && !muted_section {
            muted_section = true;
            lines.push(break_line());
            targets.push(None);
        }
        lines.push(render_row(row));
        targets.push(Some(row.id.clone()));
    }
    (lines, targets)
}

/// The break between the two sections. Its own function because an
/// incremental redraw has to put it back exactly as a full one drew it.
fn break_line() -> Vec<Span> {
    vec![Span::styled("─────", Class::Muted)]
}

/// One conversation's line. Factored out of the listing so that redrawing
/// one row and redrawing all of them cannot drift apart.
fn render_row(row: &ConversationRow) -> Vec<Span> {
    let mut spans = vec![Span::styled(row.label.clone(), Class::Conversation)];
    let mut waiting = Vec::new();
    if row.mention_count > 0 {
        waiting.push(Span::styled(
            format!("@{}", row.mention_count),
            Class::Mention,
        ));
    }
    // A number when there is one to give. Slack counts DMs for us and rho
    // counts what it watched land; a channel unread since before the last
    // start has neither, and says so in words.
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
    // The opt-in reads where it was made, and reads as a word rather than a
    // glyph: a mark nobody can name is a mark nobody undoes.
    if row.watched {
        spans.push(Span::plain("  "));
        spans.push(Span::styled("watched", Class::Muted));
    }
    if let Some(latest) = &row.latest {
        spans.push(Span::plain("  "));
        spans.push(Span::styled(
            clock_time(latest.epoch_seconds() as i64),
            Class::Time,
        ));
    }
    spans
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
            watched: false,
            latest: None,
        }
    }

    /// The buffer the incremental path builds must be the buffer a full
    /// draw would have built, line for line, after any run of events.
    ///
    /// This drives the model, takes its edit log, and applies it with the
    /// same arithmetic `apply_edits` uses — the lines above the listing,
    /// the row, and the break below the unmuted ones — then compares the
    /// result with what `render_rows` produces from scratch. The break is
    /// the part worth testing: the model knows nothing about it, so the
    /// view's counting is the only thing keeping it in the right place.
    #[test]
    fn the_lines_an_edit_moves_land_where_a_full_draw_would_have_put_them() {
        use crate::model::{Model, RowEdit};
        use crate::types::{Conversation, ConversationKind, Message, UserId};

        fn message(channel: &str, seconds: &str) -> Message {
            Message {
                ts: Ts(format!("{seconds}.000000")),
                thread_ts: None,
                channel: ChannelId(channel.to_owned()),
                user: Some(UserId("U1".to_owned())),
                bot_name: None,
                blocks: Vec::new(),
                text: "traffic".to_owned(),
                attachments: Vec::new(),
                files: Vec::new(),
                subtype: None,
                reply_count: 0,
                latest_reply: None,
                edited: false,
                reactions: Vec::new(),
            }
        }

        /// The view's own bookkeeping, kept here in the shape the view
        /// keeps it: which line is which, and how many rows are on each
        /// side of the break.
        struct Lines {
            drawn: Vec<Option<ChannelId>>,
            unmuted: usize,
            muted: usize,
        }

        impl Lines {
            fn line_of_id(&self, channel: &ChannelId) -> Option<usize> {
                self.drawn
                    .iter()
                    .position(|line| line.as_ref() == Some(channel))
            }

            fn apply(&mut self, edit: &RowEdit) {
                let leaving = match edit.was_shown {
                    true => self.line_of_id(&edit.channel),
                    false => None,
                };
                if let Some(line) = leaving {
                    match line >= self.unmuted {
                        true => self.muted -= 1,
                        false => self.unmuted -= 1,
                    }
                    self.drawn.remove(line);
                }
                if let Some(row) = edit.row.as_ref() {
                    let at = edit
                        .before
                        .as_ref()
                        .and_then(|before| self.line_of_id(before))
                        .unwrap_or(self.drawn.len());
                    let at = match row.muted {
                        true => at,
                        false => at.min(self.unmuted),
                    };
                    match row.muted {
                        true => self.muted += 1,
                        false => self.unmuted += 1,
                    }
                    self.drawn.insert(at, Some(row.id.clone()));
                }
            }
        }

        let mut model = Model::new(crate::config::WorkspaceName("acme".into()));
        model.set_self(UserId("ME".into()));
        model.add_conversations((0..6).map(|at| Conversation {
            id: ChannelId(format!("C{at}")),
            kind: ConversationKind::Channel,
            name: format!("room-{at}"),
            user: None,
            members: Vec::new(),
        }));
        // One muted conversation from the start, so the break is in the
        // buffer and every later edit has to count around it.
        model.set_muted([ChannelId("C5".into())]);

        let known = model.conversation_rows();
        let (_, targets) = render_rows(&known);
        let mut lines = Lines {
            drawn: targets,
            unmuted: known.iter().filter(|row| !row.muted).count(),
            muted: known.iter().filter(|row| row.muted).count(),
        };
        model.forget_row_edits();

        let check = |model: &mut Model, lines: &mut Lines, at: &str| {
            let edits = model.take_row_edits().expect("the log stands");
            for edit in &edits {
                lines.apply(edit);
            }
            let (_, expected) = render_rows(&model.conversation_rows());
            assert_eq!(lines.drawn, expected, "{at}");
        };

        model.note_counts(&message("C2", "100"));
        check(
            &mut model,
            &mut lines,
            "a message in the middle of the list",
        );
        model.note_counts(&message("C0", "200"));
        check(&mut model, &mut lines, "and one in the first row");
        model.note_counts(&message("C5", "300"));
        check(&mut model, &mut lines, "and one below the break");
        model.note_counts(&message("C4", "400"));
        check(&mut model, &mut lines, "and one in the last unmuted row");
        model.mark_read(&ChannelId("C2".into()), &Ts("100.000000".into()));
        check(&mut model, &mut lines, "and a row loses its badge in place");
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
        let (lines, _) = render_rows(&[design]);
        assert_eq!(text(&lines[0]), expected);
    }

    #[test]
    fn a_channel_unread_from_before_the_last_start_says_so_in_words() {
        // Slack counts messages for DMs only, so a channel that was already
        // unread at connect has no number to show and must not invent one.
        let (lines, _) = render_rows(&[row("#design", true, 0)]);
        assert_eq!(text(&lines[0]), "#design  unread");
    }

    #[test]
    fn muted_conversations_sit_at_the_bottom_under_one_break() {
        let mut muted = row("#noise", true, 0);
        muted.muted = true;
        let (lines, targets) = render_rows(&[row("#design", true, 2), muted]);
        assert_eq!(text(&lines[0]), "#design  @2");
        assert_eq!(text(&lines[1]), "─────", "muted conversations start here");
        assert_eq!(targets[1], None, "the break opens nothing");
        assert_eq!(text(&lines[2]), "#noise  unread");
    }

    /// Narrowing is the model's now, so the rows the view is handed are
    /// already the matches. What is asserted here is that the view draws
    /// what it is given and says so when it is given nothing.
    #[test]
    fn an_empty_narrowing_says_nothing_matches_rather_than_drawing_a_list() {
        let (lines, targets) = render_rows(&[row("@ada", false, 0)]);
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "@ada");
        assert_eq!(targets[0], Some(ChannelId("@ada".into())));
    }
}
