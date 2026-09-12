//! The way in: every conversation on one line, unread first.
//!
//! Unread before read, mentions before plain unreads, then recency, with the
//! muted ones under a rule at the bottom — the order a person actually wants
//! to walk. The listing is generated read-only
//! text in an ordinary editor, so motions and search come for free, and the
//! cursor is restored by the conversation it was sitting on rather than by
//! line number: an arriving message must not move the selection under a
//! keypress.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::ToPoint as _;
use text::Anchor;
use theme::ActiveTheme as _;

use crate::model::{ConversationRow, Empty};
use crate::session::{Session, Source, Status};
use crate::types::ChannelId;
use crate::ui::{Class, Hooks, Span, lay_out, when_label};

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
    /// The buckets whose highlights this draw has to re-send, and how many
    /// lines each bucket holds. A bucket is a run of lines sharing one
    /// highlight key per class, so a line that moved costs its bucket and
    /// not the listing.
    touched: BTreeSet<u32>,
    held: BTreeMap<u32, usize>,
    /// Classes each bucket currently paints, so a bucket that has lost one
    /// can be told to paint it no longer. The editor keeps what it was last
    /// given under a key until it is given something else.
    painted: BTreeMap<u32, HashSet<Class>>,
    /// Only ever goes up: a bucket number is never reused, so a line
    /// inserted next to another cannot be given a number that still has
    /// ranges under it.
    next_bucket: u32,
    /// How long the last redraw took. The per-event path is one of the few
    /// where a number is the requirement, so the surface times itself and a
    /// test reads it, rather than a test timing the socket and the executor
    /// along with it.
    #[cfg(any(test, feature = "fake"))]
    last_refresh: std::time::Duration,
    _observe: gpui::Subscription,
}

/// Lines to a bucket. The listing's rows are one line each, so this is the
/// number of rows repainted when one of them moves; the transcript's own
/// number, because there is no reason for two.
const BUCKET: usize = rho_transcript::BUCKET;

/// One line of the buffer as it currently stands.
struct DrawnLine {
    /// The conversation the line opens, or `None` for the break and the
    /// health notice, which open nothing.
    id: Option<ChannelId>,
    muted: bool,
    text: String,
    styles: Vec<(Class, Range<usize>)>,
    /// The run of lines this one is painted with.
    bucket: u32,
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
            touched: BTreeSet::new(),
            held: BTreeMap::new(),
            painted: BTreeMap::new(),
            next_bucket: 0,
            #[cfg(any(test, feature = "fake"))]
            last_refresh: std::time::Duration::ZERO,
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

    /// The lines above the listing as the reader reads them. Its own
    /// accessor because `drawn_conversations_for_test` is about rows, and
    /// chrome is not a row.
    #[cfg(any(test, feature = "fake"))]
    pub fn drawn_banner_for_test(&self) -> Vec<String> {
        self.drawn
            .iter()
            .take(self.banner)
            .map(|line| line.text.clone())
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

    /// The first line that opens a conversation, past whatever chrome is
    /// above the listing.
    fn first_row(&self) -> Option<usize> {
        self.drawn.iter().position(|line| line.id.is_some())
    }

    /// The line the cursor is on.
    ///
    /// Read as a position in the buffer, not on the screen. Asking the
    /// editor for a display snapshot makes it resync the whole buffer
    /// however little of it moved, and this is read once a message: a pass
    /// over the listing on the path an arriving message takes. The answer
    /// is the same either way -- the dimension asked for is a buffer point
    /// in both -- so only the resync is given up.
    fn cursor_row(&self, cx: &mut Context<Self>) -> usize {
        let head = self.editor.read(cx).selections.newest_anchor().head();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        head.to_point(&snapshot).row as usize
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
        #[cfg(any(test, feature = "fake"))]
        let started = std::time::Instant::now();
        let held = self.id_at(self.cursor_row(cx));
        let banner = self.banner_spans(cx);
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
        let incremental = listing && !self.drawn.is_empty() && banner.len() == self.banner;
        match (incremental, edits) {
            (true, Some(edits)) => {
                self.redraw_banner(banner, cx);
                self.apply_edits(edits, cx);
            }
            _ => self.rebuild(banner, cx),
        }
        // The point follows the conversation, not the line number: rows
        // moving above the reader must not move the reader. When the
        // conversation it was on is not on screen at all -- a query
        // narrowed it away -- it goes to the first row, which is the match
        // the reader typed for. Something has to decide: left alone the
        // point falls to wherever the editor clamps it, which today is the
        // blank line under the listing, so `enter` answers nothing.
        //
        // Only when it was on a conversation. A point the reader put on the
        // break, or below the rows, is theirs; moving it because a message
        // arrived is the very thing this rule exists to prevent.
        if let Some(held) = held
            && let Some(row) = self.line_of_id(&held).or_else(|| self.first_row())
        {
            self.place_cursor(row, window, cx);
        }
        cx.notify();
        #[cfg(any(test, feature = "fake"))]
        {
            self.last_refresh = started.elapsed();
        }
    }

    /// What the last redraw cost, for the test that holds the per-event
    /// path to a number.
    #[cfg(any(test, feature = "fake"))]
    pub fn last_refresh_for_test(&self) -> std::time::Duration {
        self.last_refresh
    }

    /// How many lines the listing is drawing, so a cost has a size beside
    /// it.
    #[cfg(any(test, feature = "fake"))]
    pub fn drawn_line_count_for_test(&self) -> usize {
        self.drawn.len()
    }

    /// Writes the whole listing. The first draw, and the fallback whenever
    /// an edit cannot be placed against what the buffer holds.
    fn rebuild(&mut self, banner: Vec<Vec<Span>>, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        // Whatever the mirror holds is shown whatever the socket is doing:
        // a restart reads its conversations before Slack answers, and an
        // offline workspace stays readable. The status line is for when
        // there is genuinely nothing to show yet.
        let known = session.rows();
        let narrowed = session.empty_narrowing();
        // A query that reaches nothing says so, rather than falling through
        // to a status line about the socket, which is not what the reader
        // just did. Which of the two ways it emptied decides the line: only
        // the one the reader did not do is told the way out.
        let (lines, rows) = match (known.is_empty(), narrowed) {
            (true, Some(why)) => (
                vec![vec![Span::styled(empty_line(why), Class::Muted)]],
                vec![None],
            ),
            _ => match session.status() {
                _ if !known.is_empty() => render_rows(&known),
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
            },
        };
        let muted = known.iter().filter(|row| row.muted).count();
        let listed = !known.is_empty();

        self.drawn = Vec::with_capacity(lines.len() + banner.len());
        self.banner = banner.len();
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
                bucket: 0,
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
        self.renumber();
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
                self.rebuild(self.banner_spans(cx), cx);
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
                let line = render_row(&row, now_seconds());
                let (text, styles) = lay_out(&line);
                self.insert_line(
                    at,
                    DrawnLine {
                        id: Some(row.id.clone()),
                        muted: row.muted,
                        text,
                        styles,
                        bucket: 0,
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

    /// The lines above the listing: why the session cannot be trusted to be
    /// current, and what the listing is narrowed to.
    ///
    /// The narrowing is a state of the model that nothing clears but another
    /// search, so without a line for it a reader who searched an hour ago
    /// sees a short list and no reason for it.
    fn banner_spans(&self, cx: &Context<Self>) -> Vec<Vec<Span>> {
        let session = self.session.read(cx);
        let mut lines = Vec::new();
        if let Some(reason) = session.health_reason() {
            lines.push(vec![Span::styled(reason.to_owned(), Class::Error)]);
        }
        let query = session.query();
        if !query.is_empty() {
            lines.push(narrowed_line(
                &query,
                session.rows().len(),
                session.conversation_count(),
            ));
        }
        lines
    }

    /// Rewrites the banner lines that say something different now.
    ///
    /// The chrome was drawn by `rebuild` alone, and a redraw takes the
    /// incremental path whenever the number of banner lines is unchanged.
    /// So a reason replaced by another reason -- the count the same, the
    /// words not -- stayed on screen saying what was true before, which is
    /// the chrome telling the reader something untrue about whether the
    /// session is keeping up.
    fn redraw_banner(&mut self, banner: Vec<Vec<Span>>, cx: &mut Context<Self>) {
        let drawn = self
            .drawn
            .iter()
            .take(self.banner)
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        let laid = banner
            .into_iter()
            .map(|line| lay_out(&line))
            .collect::<Vec<_>>();
        let words = laid
            .iter()
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>();
        for at in stale_banner(&drawn, &words) {
            let (text, styles) = laid[at].clone();
            self.remove_line(at, cx);
            self.insert_line(
                at,
                DrawnLine {
                    id: None,
                    muted: false,
                    text,
                    styles,
                    bucket: 0,
                },
                cx,
            );
        }
    }

    fn remove_line(&mut self, at: usize, cx: &mut Context<Self>) {
        if at >= self.drawn.len() {
            return;
        }
        let gone = self.drawn.remove(at);
        self.touched.insert(gone.bucket);
        if let Some(held) = self.held.get_mut(&gone.bucket) {
            *held = held.saturating_sub(1);
        }
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

    fn insert_line(&mut self, at: usize, mut line: DrawnLine, cx: &mut Context<Self>) {
        let at = at.min(self.drawn.len());
        let text = format!("{}\n", line.text);
        line.bucket = self.bucket_beside(at);
        self.touched.insert(line.bucket);
        *self.held.entry(line.bucket).or_default() += 1;
        self.drawn.insert(at, line);
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(Point::new(at as u32, 0)..Point::new(at as u32, 0), text)],
                None,
                cx,
            );
        });
    }

    /// The bucket a line inserted here joins: the one its neighbour is
    /// painted with, while that bucket has room, and a fresh one otherwise.
    ///
    /// Never a number that has been used before. The editor holds what it
    /// was last given under a key, so a reused number would put a line
    /// under ranges belonging to lines that have since gone.
    fn bucket_beside(&mut self, at: usize) -> u32 {
        let beside = self
            .drawn
            .get(at)
            .or_else(|| self.drawn.get(at.wrapping_sub(1)))
            .map(|line| line.bucket);
        match beside {
            Some(bucket) if self.held.get(&bucket).is_none_or(|held| *held < BUCKET) => bucket,
            _ => {
                let bucket = self.next_bucket;
                self.next_bucket += 1;
                bucket
            }
        }
    }

    /// Numbers the whole listing afresh, in runs of `BUCKET`, and says that
    /// every bucket it has ever painted needs re-sending.
    ///
    /// A rebuild rewrites the buffer, so the anchors every old bucket holds
    /// are anchors into text that is gone. Clearing them is what stops a
    /// colour from before the rebuild sitting on a line drawn after it.
    fn renumber(&mut self) {
        self.touched.extend(self.painted.keys().copied());
        self.held.clear();
        for (at, line) in self.drawn.iter_mut().enumerate() {
            let bucket = self.next_bucket + (at / BUCKET) as u32;
            line.bucket = bucket;
        }
        for line in &self.drawn {
            *self.held.entry(line.bucket).or_default() += 1;
        }
        self.touched.extend(self.held.keys().copied());
        self.next_bucket += self.drawn.len().div_ceil(BUCKET).max(1) as u32;
    }

    /// Re-anchors and re-sends the highlights of the buckets that changed,
    /// and of no others.
    ///
    /// The editor replaces the ranges under a key, so a key that covered
    /// the whole listing could only ever be replaced whole: one message
    /// arriving re-anchored every range of every row and handed all sixteen
    /// classes back. A key per class per bucket is what makes a row that
    /// moved cost its own run of rows. Which lines the listing holds is
    /// still walked, because a line's offset is the text above it, but that
    /// is length arithmetic with no anchor and no editor in it.
    fn paint(&mut self, cx: &mut Context<Self>) {
        let touched = std::mem::take(&mut self.touched);
        if touched.is_empty() {
            return;
        }
        let mut styles: BTreeMap<u32, Vec<(Class, Range<usize>)>> = BTreeMap::new();
        for bucket in &touched {
            styles.entry(*bucket).or_default();
        }
        let mut base = 0usize;
        for line in &self.drawn {
            if let Some(held) = styles.get_mut(&line.bucket) {
                held.extend(
                    line.styles
                        .iter()
                        .map(|(class, range)| (*class, base + range.start..base + range.end)),
                );
            }
            base += line.text.len() + 1;
        }
        let anchored = self.buffer.update(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            styles
                .into_iter()
                .map(|(bucket, ranges)| {
                    let ranges = ranges
                        .into_iter()
                        .map(|(class, range)| {
                            let clamp = |offset: usize| offset.min(snapshot.len());
                            (
                                class,
                                snapshot.anchor_before(clamp(range.start))
                                    ..snapshot.anchor_after(clamp(range.end)),
                            )
                        })
                        .collect::<Vec<(Class, Range<Anchor>)>>();
                    (bucket, ranges)
                })
                .collect::<Vec<(u32, Vec<(Class, Range<Anchor>)>)>>()
        });
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut sending: Vec<(
            u32,
            Class,
            Vec<multi_buffer::Anchor>,
            Vec<multi_buffer::Anchor>,
        )> = Vec::new();
        for (bucket, ranges) in anchored {
            let mut per_class: HashMap<Class, (Vec<_>, Vec<_>)> = HashMap::new();
            for (class, range) in ranges {
                let Some(start) = snapshot.anchor_in_excerpt(range.start) else {
                    continue;
                };
                let Some(end) = snapshot.anchor_in_excerpt(range.end) else {
                    continue;
                };
                let held = per_class.entry(class).or_default();
                held.0.push(start);
                held.1.push(end);
            }
            let present = per_class.keys().copied().collect::<HashSet<_>>();
            // A class this bucket no longer paints still has ranges under
            // its key, so it is sent back empty rather than left behind.
            let stale = self
                .painted
                .get(&bucket)
                .map(|painted| painted.difference(&present).copied().collect::<Vec<_>>())
                .unwrap_or_default();
            match present.is_empty() {
                true => self.painted.remove(&bucket),
                false => self.painted.insert(bucket, present),
            };
            for (class, (starts, ends)) in per_class {
                sending.push((bucket, class, starts, ends));
            }
            for class in stale {
                sending.push((bucket, class, Vec::new(), Vec::new()));
            }
        }
        self.editor.update(cx, |editor, cx| {
            for (bucket, class, starts, ends) in sending {
                let ranges = starts
                    .into_iter()
                    .zip(ends)
                    .map(|(start, end)| start..end)
                    .collect::<Vec<_>>();
                editor.highlight_text(
                    class.list_highlight_key(bucket),
                    ranges,
                    class.resolve(cx),
                    cx,
                );
            }
        });
    }
}

/// The rows the model handed over, laid out: one line per conversation, the
/// name, what is waiting in it, and when it last spoke. No ids and no
/// last-message preview — the list is for choosing where to go, and a
/// preview is the conversation's job. Narrowing happened before this: the
/// model answers a query from its own index, so nothing here looks at every
/// conversation to decide what to draw.
fn render_rows(rows: &[ConversationRow]) -> (Vec<Vec<Span>>, Vec<Option<ChannelId>>) {
    // Read once for the whole listing rather than once a row: every row is
    // asking the same question, and the answer moving between two of them
    // would put two days on one frame.
    let now = now_seconds();
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
        lines.push(render_row(row, now));
        targets.push(Some(row.id.clone()));
    }
    (lines, targets)
}

/// The wall clock, read at the top of a draw. Its own function so the two
/// places that draw a row ask the same thing.
fn now_seconds() -> i64 {
    chrono::Local::now().timestamp()
}

/// The one line under a banner whose narrowing reaches nothing.
///
/// A word nothing answers is the reader's own last keystroke and needs no
/// explaining. A list that emptied under them is not: they are told what
/// happened, and told the way out, because `s` with an empty query is the
/// only way back to the whole list and nothing else on screen says so.
fn empty_line(why: Empty) -> &'static str {
    match why {
        Empty::Never => "nothing matches",
        Empty::Gone => "what matched has gone; s with an empty query shows every conversation",
    }
}

/// The line that says the listing is narrowed, and by how much. The count is
/// against the whole workspace, which is what says how much a query is
/// keeping off the screen rather than only that something is.
fn narrowed_line(query: &str, shown: usize, whole: usize) -> Vec<Span> {
    vec![Span::styled(
        format!("matching \"{query}\" · {shown} of {whole}"),
        Class::Muted,
    )]
}

/// Which banner lines have to be written again: the ones whose words differ
/// from what the buffer already holds.
///
/// Its own function because the case that was wrong is invisible from
/// either side alone -- the count is right, so the incremental path is
/// taken, and the words are not, so the reader is told something that was
/// true a draw ago. Called only when the two are the same length, which is
/// what makes each index a line already in the buffer.
fn stale_banner(drawn: &[String], banner: &[String]) -> Vec<usize> {
    banner
        .iter()
        .enumerate()
        .filter(|(at, line)| drawn.get(*at) != Some(*line))
        .map(|(at, _)| at)
        .collect()
}

/// The break between the two sections. Its own function because an
/// incremental redraw has to put it back exactly as a full one drew it.
fn break_line() -> Vec<Span> {
    vec![Span::styled("─────", Class::Muted)]
}

/// One conversation's line. Factored out of the listing so that redrawing
/// one row and redrawing all of them cannot drift apart.
fn render_row(row: &ConversationRow, now: i64) -> Vec<Span> {
    let mut spans = vec![Span::styled(row.label.clone(), Class::Conversation)];
    let mut waiting = Vec::new();
    if row.mention_count > 0 {
        waiting.push(Span::styled(
            format!("@{}", row.mention_count),
            Class::Mention,
        ));
    }
    // A number when there is one to give. Slack counts DMs for us and rho
    // counts what it has seen land; a channel unread since before the last
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
    if let Some(latest) = &row.latest {
        spans.push(Span::plain("  "));
        spans.push(Span::styled(
            when_label(latest.epoch_seconds() as i64, now),
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
            latest: None,
        }
    }

    /// The two empty-state lines, as the reader reads them. The first is
    /// about the word they just typed and says nothing else, because they
    /// know what they did. The second is about something Slack did, and
    /// names the way back to the whole list, which is otherwise written
    /// nowhere on screen.
    #[test]
    fn an_empty_narrowing_reads_as_the_thing_that_emptied_it() {
        assert_eq!(empty_line(Empty::Never), "nothing matches");
        assert_eq!(
            empty_line(Empty::Gone),
            "what matched has gone; s with an empty query shows every conversation"
        );
        assert!(
            !empty_line(Empty::Gone).contains("filter"),
            "the way out is the key the reader presses, not a word for it"
        );
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

    /// The chrome is drawn by a full rebuild alone, and a redraw takes the
    /// incremental path whenever the number of banner lines is unchanged.
    /// One health reason replaced by another is exactly that case: the
    /// count is right and the words are not, and the old reason stood on
    /// screen telling the reader the session had a problem it no longer
    /// had.
    #[test]
    fn a_banner_line_whose_words_changed_is_written_again() {
        let lost = "slack: connection lost".to_owned();
        let refused = "slack: connecting to Slack: 401".to_owned();
        assert_eq!(
            stale_banner(std::slice::from_ref(&lost), std::slice::from_ref(&refused)),
            vec![0],
            "one reason replaced by another, with the count unchanged"
        );
        assert!(
            stale_banner(std::slice::from_ref(&lost), std::slice::from_ref(&lost)).is_empty(),
            "and the ordinary draw, where the chrome has not moved, writes nothing"
        );
        assert!(
            stale_banner(&[], &[]).is_empty(),
            "nor does a listing with no chrome at all"
        );
    }

    fn text(line: &[Span]) -> String {
        line.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn a_row_carries_its_counts_and_the_time_it_last_spoke() {
        let mut design = row("#design", true, 2);
        design.unread_count = 5;
        // A fixed instant, long enough ago to be a date rather than a
        // clock: what is asserted here is the row's layout, and where the
        // day boundaries fall is `when_label`'s own test.
        design.latest = Some(Ts("1755780420.000100".into()));
        let expected = format!(
            "#design  @2 · 5 new  {}",
            crate::ui::when_label(1_755_780_420, now_seconds())
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
