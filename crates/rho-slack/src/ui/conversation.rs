//! One conversation: a transcript with a compose region at its end.
//!
//! A channel, a group, a DM, and a thread are the same surface. What differs
//! is where a composed message goes, and that is the source's business, not
//! the view's.
//!
//! Block Kit is rendered to text by the model, so the transcript is plain
//! prose carrying the host's Markdown pipeline, exactly like the agent
//! transcript beside it.

use std::collections::HashSet;
use std::ops::Range;

use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorEvent, EditorMode, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_transcript::{BlockSpec, Item, Transcript};
use theme::ActiveTheme as _;

use crate::model::Model;
use crate::session::{Session, Source, Update};
use crate::types::{FileSummary, Message, ThreadKey, Ts};
use crate::ui::{Class, Hooks, Span, clock_time, crosses_day, day_label, lay_out};

pub struct ConversationView {
    session: Entity<Session>,
    source: Source,
    /// The messages on screen, each keyed and each owning its own range: a
    /// new message rewrites one item, not the conversation.
    transcript: Transcript<Row, Class, LineMeta>,
    input: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// How far the session's messages have been applied here.
    revision: u64,
    /// One user action buys one page. Set when a fill is asked for, cleared
    /// when the reader scrolls or moves the cursor themselves, so a landed
    /// page cannot walk the whole conversation back to its beginning.
    fill: Fill,
    /// Where the surface last put the view and the cursor itself. The events
    /// that come back for those are not the reader moving, and must not buy
    /// another page.
    moved: Moved,
    _subscriptions: Vec<gpui::Subscription>,
}

/// One page per user action, and the two cases where nobody has asked yet:
/// opening a conversation whose mirrored run is short, and a gap line on a
/// view that has not asked for anything.
#[derive(Default)]
struct Fill {
    asked: bool,
}

impl Fill {
    /// The reader scrolled or moved the cursor: the next fill is theirs to
    /// buy again.
    fn user_moved(&mut self) {
        self.asked = false;
    }

    /// Whether to ask now, given where the view sits. Asking marks the
    /// action spent.
    fn wants_page(&mut self, near_top: bool) -> bool {
        if self.asked || !near_top {
            return false;
        }
        self.asked = true;
        true
    }
}

/// What the surface moved on its own, so the resulting events can be told
/// apart from the reader's own scrolling and cursor motion.
#[derive(Default)]
struct Moved {
    scroll: Option<f64>,
    cursor: Option<u32>,
}

/// What the transcript keys on. Day rules and the gap notice are items like
/// any other, so they insert and disappear through the same path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Row {
    /// The loading or failure line above everything.
    Notice,
    /// "older messages not loaded", while the run does not reach the start.
    Gap,
    Day(String),
    Message(Ts),
}

/// What a line offers the cursor.
#[derive(Clone, Debug, Default, PartialEq)]
struct LineMeta {
    /// The thread the line belongs to, so `enter` opens the right one.
    thread: Option<Ts>,
    /// The file the line names, which `enter` opens instead.
    file: Option<FileSummary>,
}

type Rendered = Item<Row, Class, LineMeta>;

impl ConversationView {
    pub fn new(
        session: Entity<Session>,
        source: Source,
        hooks: Hooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The transcript is plain text, not Markdown: the compact layout
        // indents continuation lines, which Markdown would read as code
        // blocks. Every class the reader sees comes from a span instead.
        let transcript = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let input = cx.new(|cx| Buffer::local("", cx));
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                transcript.clone(),
                [Point::zero()..transcript.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(1),
                input.clone(),
                [Point::zero()..input.read(cx).max_point()],
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
            (hooks.configure_editor)(&mut editor, window, cx);
            editor.disable_header_for_buffer(transcript.read(cx).remote_id(), cx);
            editor.disable_header_for_buffer(input.read(cx).remote_id(), cx);
            editor
        });

        let mut subscriptions = vec![cx.observe_in(&session, window, |this, _, window, cx| {
            this.refresh(window, cx);
        })];
        // History fills as the reader scrolls, and there is no other way to
        // ask for it. The fetch starts a screen early so the older messages
        // are usually already there by the time the top comes into view.
        subscriptions.push(
            cx.subscribe(&editor, |this, editor, event: &EditorEvent, cx| {
                match event {
                    EditorEvent::ScrollPositionChanged { .. } => {
                        let (position, screen) = editor.update(cx, |editor, cx| {
                            (
                                editor.scroll_position(cx).y,
                                editor.visible_line_count().unwrap_or(0.0),
                            )
                        });
                        // The surface's own re-anchoring comes back as a
                        // scroll event too. Only the reader's counts.
                        if this.moved.scroll == Some(position) {
                            this.moved.scroll = None;
                        } else {
                            this.fill.user_moved();
                        }
                        if this.fill.wants_page(near_top(position, screen)) {
                            this.load_older(cx);
                        }
                    }
                    EditorEvent::SelectionsChanged { local: true } => {
                        let row = this.cursor_row(cx) as u32;
                        if this.moved.cursor == Some(row) {
                            this.moved.cursor = None;
                            return;
                        }
                        this.fill.user_moved();
                        // A reader already at the top who presses `gg` moves
                        // the cursor without moving the view, so the scroll
                        // event never comes: the motion is the action.
                        let (position, screen) = editor.update(cx, |editor, cx| {
                            (
                                editor.scroll_position(cx).y,
                                editor.visible_line_count().unwrap_or(0.0),
                            )
                        });
                        if this.fill.wants_page(near_top(position, screen)) {
                            this.load_older(cx);
                        }
                    }
                    _ => {}
                }
            }),
        );

        let mut view = Self {
            session: session.clone(),
            source: source.clone(),
            transcript: Transcript::new(transcript),
            input,
            multi_buffer,
            editor,
            revision: 0,
            fill: Fill::default(),
            moved: Moved::default(),
            _subscriptions: subscriptions,
        };
        view.transcript.attach(&view.editor.clone(), cx);
        session.update(cx, |session, cx| session.open(&source, cx));
        view.refresh(window, cx);
        view.select_compose(window, cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The file on the cursor's line, if the line is one. A file is opened
    /// before a thread, because the reader who put the cursor on a file line
    /// asked for the file.
    pub fn cursor_file(&self, cx: &mut Context<Self>) -> Option<FileSummary> {
        let row = self.cursor_row(cx) as u32;
        self.transcript
            .line_meta(row, cx)
            .and_then(|meta| meta.file.clone())
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

    /// The thread the cursor is in, for opening a thread from a channel. A
    /// thread surface has none: it is already the thread.
    pub fn cursor_thread(&self, cx: &mut Context<Self>) -> Option<ThreadKey> {
        if matches!(self.source, Source::Thread(_)) {
            return None;
        }
        let row = self.cursor_row(cx) as u32;
        let thread_ts = self.transcript.line_meta(row, cx)?.thread.clone()?;
        Some(
            self.session
                .read(cx)
                .model()
                .key(self.source.channel(), &thread_ts),
        )
    }

    /// Sends the compose region. The message appears when Slack accepts it,
    /// so nothing is shown that was not actually sent.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text();
        if text.trim().is_empty() {
            return;
        }
        self.input.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, "")], None, cx);
        });
        let source = self.source.clone();
        self.session
            .update(cx, |session, cx| session.send(&source, text, cx));
    }

    /// Opens the file under the cursor with whatever the desktop uses for
    /// it, fetching it first if the cache does not have it yet.
    pub fn open_file(&mut self, file: FileSummary, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, cx| session.open_file(&file, cx));
    }

    pub fn mark_read(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        self.session
            .update(cx, |session, cx| session.mark_read(&source, cx));
    }

    pub fn load_older(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        self.session
            .update(cx, |session, cx| session.load_older(&source, cx));
    }

    /// Puts the cursor in the composer: what `i` asks for.
    pub fn select_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let end = {
            let buffer = self.input.read(cx);
            buffer.anchor_after(buffer.len())
        };
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(end)
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_autoscroll_pin(anchor, AutoscrollStrategy::Bottom, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    /// Brings the transcript up to date with the session. Only the messages
    /// the session says changed are rewritten: a socket frame costs one
    /// item, a page costs one insert, and everything else keeps its anchors,
    /// so the cursor and the scroll stay where the reader put them.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((revision, updates)) = self
            .session
            .read(cx)
            .loaded(&self.source)
            .map(|loaded| (loaded.revision(), loaded.updates_since(self.revision)))
        else {
            return;
        };
        match updates {
            // A surface that has fallen too far behind (or has never been
            // filled) renders the run it can see in one insert.
            None => self.rebuild(cx),
            Some(updates) => self.apply_updates(updates, window, cx),
        }
        self.revision = revision;
        self.refresh_chrome(cx);
        self.keep_filling(cx);
        cx.notify();
    }

    /// The two cases where a gap sits on screen and no scroll event will
    /// ever come to ask about it: opening a conversation the mirror holds
    /// only a short run of, and a gap line on a view nobody has asked from
    /// yet. It buys one page, like any other action, and a page landing
    /// does not buy another.
    fn keep_filling(&mut self, cx: &mut Context<Self>) {
        let (position, screen) = self.editor.update(cx, |editor, cx| {
            (
                editor.scroll_position(cx).y,
                editor.visible_line_count().unwrap_or(0.0),
            )
        });
        if self.fill.wants_page(near_top(position, screen)) {
            self.load_older(cx);
        }
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.transcript.clear(cx);
        let messages = self.shown_messages(cx);
        let mut items = Vec::new();
        let mut last: Option<i64> = None;
        for message in &messages {
            let at = message.ts.epoch_seconds() as i64;
            if last.is_none_or(|last| crosses_day(last, at)) {
                items.push(day_item(at));
            }
            last = Some(at);
            items.push(self.item_for(message, cx));
        }
        self.transcript.insert_before(None, items, cx);
    }

    /// Carries out the plan: each operation is one transcript edit.
    fn apply_updates(&mut self, updates: Vec<Update>, window: &mut Window, cx: &mut Context<Self>) {
        let messages = self.shown_messages(cx);
        let ops = plan(&updates, &messages, &self.transcript);
        // The first message of a page arriving above everything on screen:
        // where the cursor goes if it was resting on the gap line.
        let first_loaded = ops
            .iter()
            .find(|op| reaches_the_top(op, &self.transcript))
            .and_then(|op| match op {
                Op::Insert { keys, .. } => keys
                    .iter()
                    .find(|key| matches!(key, Row::Message(_)))
                    .cloned(),
                _ => None,
            });
        // A reader at the very top is anchored to the top itself, so older
        // messages arriving above would slide their line down the screen.
        // Re-taking the anchor after the insertion point first is what keeps
        // the view on the message it was showing.
        if ops.iter().any(|op| reaches_the_top(op, &self.transcript)) {
            let pinned = self.editor.update(cx, |editor, cx| {
                editor.pin_scroll_to_content(window, cx);
                editor.scroll_position(cx).y
            });
            self.moved.scroll = Some(pinned);
        }
        for op in ops {
            match op {
                Op::Insert { before, keys } => {
                    let items = keys
                        .iter()
                        .filter_map(|key| self.item_for_key(key, &messages, cx))
                        .collect::<Vec<_>>();
                    if !items.is_empty() {
                        self.transcript.insert_before(before.as_ref(), items, cx);
                    }
                }
                Op::Replace(key) => {
                    if let Some(item) = self.item_for_key(&key, &messages, cx) {
                        self.transcript.replace(&key, item, cx);
                    }
                }
                Op::Remove(key) => {
                    self.transcript.remove(&key, cx);
                }
            }
        }
        if let Some(key) = first_loaded {
            self.leave_the_gap_line(&key, window, cx);
        }
    }

    /// Puts the cursor on the first message of a page that has just landed,
    /// if it was sitting on the gap line the page arrived under. The reader
    /// ends up looking at content rather than at a line that no longer says
    /// anything, and the view stops being at the very top, so the next
    /// scroll is a fresh action rather than the same one repeating.
    fn leave_the_gap_line(&mut self, key: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(start) = self.transcript.range_of(key).map(|range| range.start) else {
            return;
        };
        // Only a cursor the page arrived above is moved: one sitting in the
        // conversation is the reader's own place and stays put.
        let snapshot = self.transcript.buffer().read(cx).snapshot();
        let first = text::ToPoint::to_point(&start, &snapshot).row;
        if self.cursor_row(cx) as u32 > first {
            return;
        }
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(start)
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
        self.moved.cursor = Some(self.cursor_row(cx) as u32);
    }

    fn item_for_key(
        &mut self,
        key: &Row,
        messages: &[Message],
        cx: &mut Context<Self>,
    ) -> Option<Rendered> {
        match key {
            Row::Day(label) => Some(day_rule(label.clone())),
            Row::Message(ts) => {
                let message = messages.iter().find(|message| &message.ts == ts)?.clone();
                Some(self.item_for(&message, cx))
            }
            Row::Notice | Row::Gap => None,
        }
    }

    /// The loading, failure and gap lines above the transcript. They are
    /// items too, so they cost one edit each and never disturb a message.
    fn refresh_chrome(&mut self, cx: &mut Context<Self>) {
        let Some((error, loading, reached_oldest)) = self
            .session
            .read(cx)
            .loaded(&self.source)
            .map(|loaded| (loaded.error.clone(), loaded.loading, loaded.reached_oldest))
        else {
            return;
        };
        let notice = match (error, loading) {
            (Some(error), _) => Some(muted_item(
                Row::Notice,
                format!("{error}\n\n"),
                Class::Error,
            )),
            (None, true) => Some(muted_item(Row::Notice, "loading…\n\n", Class::Muted)),
            (None, false) => None,
        };
        // A run that does not reach the beginning says so on one muted line.
        // The reader scrolling onto it is what fills it, so it is a state,
        // not a button.
        let gap = (!reached_oldest)
            .then(|| muted_item(Row::Gap, "older messages not loaded\n", Class::Muted));
        let first = self
            .transcript
            .keys()
            .find(|key| !matches!(key, Row::Notice | Row::Gap))
            .cloned();
        self.put_top(Row::Gap, gap, first.clone(), cx);
        let after_notice = self
            .transcript
            .contains(&Row::Gap)
            .then_some(Row::Gap)
            .or(first);
        self.put_top(Row::Notice, notice, after_notice, cx);
    }

    /// Puts one chrome item in place, replacing or removing it as its state
    /// changes, without touching anything below.
    fn put_top(
        &mut self,
        key: Row,
        item: Option<Rendered>,
        before: Option<Row>,
        cx: &mut Context<Self>,
    ) {
        match item {
            None => {
                self.transcript.remove(&key, cx);
            }
            Some(item) if self.transcript.contains(&key) => {
                self.transcript.replace(&key, item, cx);
            }
            Some(item) => {
                self.transcript
                    .insert_before(before.as_ref(), vec![item], cx);
            }
        }
    }

    /// The messages this surface shows: replies live in their thread and
    /// nowhere else, which is what Slack does and what makes a channel
    /// readable. The exception is a reply the sender also sent to the
    /// channel, which was addressed to the room.
    fn shown_messages(&self, cx: &App) -> Vec<Message> {
        let in_thread = matches!(self.source, Source::Thread(_));
        self.session
            .read(cx)
            .loaded(&self.source)
            .map(|loaded| {
                loaded
                    .messages
                    .iter()
                    .filter(|message| in_thread || message.is_top_level())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One message as an item, with its image previews attached. The bytes
    /// are fetched when the line first renders and read from the state cache
    /// after that, so scrolling costs nothing and nothing is downloaded that
    /// the reader never opened.
    fn item_for(&mut self, message: &Message, cx: &mut Context<Self>) -> Rendered {
        let in_thread = matches!(self.source, Source::Thread(_));
        let mut item = {
            let session = self.session.read(cx);
            message_item(message, session.model(), in_thread)
        };
        let images = item
            .lines
            .iter()
            .enumerate()
            .filter_map(|(line, meta)| {
                let file = meta.file.as_ref().filter(|file| file.is_image())?;
                Some((line as u32, file.clone()))
            })
            .collect::<Vec<_>>();
        for (line, file) in images {
            let path = self.session.update(cx, |session, cx| {
                session.cache_file(&file, cx);
                session.cached_file(&file.id).map(std::path::Path::to_owned)
            });
            let Some(path) = path.filter(|path| path.exists()) else {
                continue;
            };
            item.blocks.push(image_block(line, path));
        }
        item
    }
}

/// What the transcript is asked to do about a batch of session updates.
/// Deciding this apart from carrying it out is what lets a test say that a
/// message off the socket costs exactly one append.
#[derive(Clone, Debug, PartialEq)]
enum Op {
    /// A run of items, day rules included, in front of `before` (`None` is
    /// the end): one page of history, or one arriving message.
    Insert {
        before: Option<Row>,
        keys: Vec<Row>,
    },
    Replace(Row),
    Remove(Row),
}

/// Whether the view is close enough to the top to ask for older messages.
/// One screen early, so the page has usually landed by the time the top
/// comes into view. A view that has never been laid out reports no visible
/// lines and counts as at the top: that is what opening looks like.
fn near_top(position: f64, screen: f64) -> bool {
    position <= screen.max(1.0)
}

/// Whether an operation puts text above everything on screen. Only chrome
/// may sit above such a run: the gap notice, a loading or error line, and
/// the day rule heading the message the page arrives over. It is what says
/// the scroll has to be re-anchored and the cursor taken off the gap line.
fn reaches_the_top(op: &Op, placed: &impl Placed) -> bool {
    let Op::Insert { before, .. } = op else {
        return false;
    };
    let Some(before) = before.clone() else {
        return false;
    };
    let mut above = placed.above(&before);
    while let Some(key) = above {
        if !matches!(key, Row::Notice | Row::Gap | Row::Day(_)) {
            return false;
        }
        above = placed.above(&key);
    }
    true
}

/// What the transcript already holds, which is all the planner needs to
/// know about it.
trait Placed {
    fn holds(&self, key: &Row) -> bool;
    fn above(&self, key: &Row) -> Option<Row>;
    fn below(&self, key: &Row) -> Option<Row>;
}

impl Placed for Transcript<Row, Class, LineMeta> {
    fn holds(&self, key: &Row) -> bool {
        self.contains(key)
    }

    fn above(&self, key: &Row) -> Option<Row> {
        self.key_before(key).cloned()
    }

    fn below(&self, key: &Row) -> Option<Row> {
        self.key_after(key).cloned()
    }
}

/// Turns the session's changes into transcript operations. Arriving messages
/// are gathered into runs, so a page of history is one edit at the top
/// rather than fifty, and a day rule rides with the message it heads.
fn plan(updates: &[Update], shown: &[Message], placed: &impl Placed) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut added: HashSet<Row> = HashSet::new();
    let mut gone: HashSet<Row> = HashSet::new();
    let held = |key: &Row, added: &HashSet<Row>, gone: &HashSet<Row>| {
        (placed.holds(key) || added.contains(key)) && !gone.contains(key)
    };
    let arriving = updates
        .iter()
        .filter_map(|update| match update {
            Update::Inserted(ts) => Some(ts.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut pending: Vec<Ts> = Vec::new();
    for update in updates {
        match update {
            Update::Inserted(ts) => pending.push(ts.clone()),
            Update::Replaced(ts) => {
                plan_inserts(
                    std::mem::take(&mut pending),
                    shown,
                    &arriving,
                    placed,
                    &mut added,
                    &gone,
                    &mut ops,
                );
                let key = Row::Message(ts.clone());
                if held(&key, &added, &gone) {
                    ops.push(Op::Replace(key));
                }
            }
            Update::Removed(ts) => {
                plan_inserts(
                    std::mem::take(&mut pending),
                    shown,
                    &arriving,
                    placed,
                    &mut added,
                    &gone,
                    &mut ops,
                );
                let key = Row::Message(ts.clone());
                if !held(&key, &added, &gone) {
                    continue;
                }
                let above = placed.above(&key);
                let below = placed.below(&key);
                ops.push(Op::Remove(key.clone()));
                gone.insert(key);
                // A day rule with nothing left under it goes too.
                let Some(Row::Day(label)) = above else {
                    continue;
                };
                let heads_something = match below {
                    Some(Row::Message(ts)) => day_label(ts.epoch_seconds() as i64) == label,
                    _ => false,
                };
                if !heads_something {
                    ops.push(Op::Remove(Row::Day(label.clone())));
                    gone.insert(Row::Day(label));
                }
            }
        }
    }
    plan_inserts(
        pending, shown, &arriving, placed, &mut added, &gone, &mut ops,
    );
    ops
}

fn plan_inserts(
    mut arrived: Vec<Ts>,
    shown: &[Message],
    arriving: &HashSet<Ts>,
    placed: &impl Placed,
    added: &mut HashSet<Row>,
    gone: &HashSet<Row>,
    ops: &mut Vec<Op>,
) {
    if arrived.is_empty() {
        return;
    }
    arrived.sort_by(|left, right| left.epoch_seconds().total_cmp(&right.epoch_seconds()));
    let held = |key: &Row, added: &HashSet<Row>| {
        (placed.holds(key) || added.contains(key)) && !gone.contains(key)
    };
    let mut keys: Vec<Row> = Vec::new();
    let mut before: Option<Row> = None;
    let mut follows: Option<String> = None;
    let mut previous_day: Option<i64> = None;
    let mut last_day: Option<i64> = None;
    for ts in arrived {
        // A reply in a channel is not shown there at all; it changed the
        // parent's count line instead, which arrives as its own update.
        let Some(message) = shown.iter().find(|message| message.ts == ts) else {
            continue;
        };
        let at = message.ts.epoch_seconds() as i64;
        let next = anchor_after(shown, &ts, arriving, |key| held(key, added));
        if !keys.is_empty() && next != before {
            close_run(
                std::mem::take(&mut keys),
                before.clone(),
                follows.take(),
                last_day,
                ops,
            );
            previous_day = None;
        }
        if keys.is_empty() {
            before = next;
            // The day rule heading the message the run lands on. It stays
            // where it is when the run starts on that same day, and is dealt
            // with by `close_run` when the run reaches back further.
            let rule = match before.as_ref().and_then(|before| placed.above(before)) {
                Some(Row::Day(label)) => Some(label),
                _ => None,
            };
            let covered = rule.as_ref().is_some_and(|label| label == &day_label(at));
            follows = match covered {
                true => None,
                false => rule,
            };
            previous_day = match covered {
                true => Some(at),
                false => day_before(before.as_ref(), shown, arriving, |key| held(key, added)),
            };
        }
        // A day rule heads the first message of its day.
        if previous_day.is_none_or(|last| crosses_day(last, at)) {
            let rule = Row::Day(day_label(at));
            keys.push(rule.clone());
            added.insert(rule);
        }
        previous_day = Some(at);
        last_day = Some(at);
        let key = Row::Message(ts);
        keys.push(key.clone());
        added.insert(key);
    }
    close_run(keys, before, follows, last_day, ops);
}

/// Puts one run in, around the day rule that heads the message it lands on.
/// A run ending on that same day now heads the day itself, so the old rule
/// would sit in the middle of it: it comes down first, and the run takes its
/// place. A run that ends earlier simply goes in above it.
fn close_run(
    keys: Vec<Row>,
    before: Option<Row>,
    follows: Option<String>,
    last_day: Option<i64>,
    ops: &mut Vec<Op>,
) {
    if keys.is_empty() {
        return;
    }
    let stale = follows
        .clone()
        .filter(|label| last_day.is_some_and(|at| &day_label(at) == label));
    match stale {
        Some(label) => {
            ops.push(Op::Remove(Row::Day(label)));
            ops.push(Op::Insert { before, keys });
        }
        None => ops.push(Op::Insert {
            before: follows.map(Row::Day).or(before),
            keys,
        }),
    }
}

/// The item an arriving message goes in front of: the next message already
/// on screen. `None` means the end.
fn anchor_after(
    shown: &[Message],
    ts: &Ts,
    arriving: &HashSet<Ts>,
    held: impl Fn(&Row) -> bool,
) -> Option<Row> {
    shown
        .iter()
        .skip_while(|message| message.ts.epoch_seconds() <= ts.epoch_seconds())
        .find(|message| !arriving.contains(&message.ts))
        .map(|message| Row::Message(message.ts.clone()))
        .filter(|row| held(row))
}

/// The day of the message the run will follow, which decides whether it
/// needs a day rule of its own.
fn day_before(
    anchor: Option<&Row>,
    shown: &[Message],
    arriving: &HashSet<Ts>,
    held: impl Fn(&Row) -> bool,
) -> Option<i64> {
    let before = match anchor {
        Some(Row::Message(ts)) => shown
            .iter()
            .rev()
            .filter(|message| message.ts.epoch_seconds() < ts.epoch_seconds())
            .find(|message| !arriving.contains(&message.ts)),
        _ => shown
            .iter()
            .rev()
            .find(|message| !arriving.contains(&message.ts)),
    }?;
    held(&Row::Message(before.ts.clone())).then(|| before.ts.epoch_seconds() as i64)
}

/// A picture under the line that names it, indented to the body column.
fn image_block(line: u32, path: std::path::PathBuf) -> BlockSpec {
    BlockSpec {
        line,
        height: IMAGE_ROWS,
        render: std::sync::Arc::new(move |cx| {
            // The spacer is real text in the transcript's own font, which is
            // the only way to land the picture exactly under the body column
            // whatever font the reader has set.
            let style = cx.editor_style.text.clone();
            div()
                .flex()
                .items_start()
                .font_family(style.font_family.clone())
                .text_size(style.font_size)
                .child(" ".repeat(BODY_INDENT))
                .child(
                    gpui::img(path.clone())
                        .max_h(cx.line_height * IMAGE_ROWS as f32)
                        .max_w_full(),
                )
                .into_any_element()
        }),
        priority: 0,
    }
}

/// How many lines tall an inline image preview is allowed to be.
const IMAGE_ROWS: u32 = 10;

/// One message as an item: `name: body  time`.
///
/// The shape is a chat log's, not a table's: the name introduces the words,
/// the time trails them, and nothing is padded into a column, so one long
/// name cannot push every other line across the screen. Continuation lines
/// and the chrome under a message indent by [`BODY_INDENT`]. No blank line
/// between messages; a day is the only break.
fn message_item(message: &Message, model: &Model, in_thread: bool) -> Rendered {
    let at = message.ts.epoch_seconds() as i64;
    let thread = Some(message.thread_root());
    let indent = " ".repeat(BODY_INDENT);
    let mut spans = Vec::new();
    let mut lines = Vec::new();

    // Slack shows joins, leaves, and topic changes, and so does rho: leaving
    // them out makes a channel read as if nothing happened. They are one
    // muted line, without a name of their own.
    if let Some(line) = system_line(message, model) {
        spans.push(Span::styled(format!("{line}\n"), Class::Muted));
        lines.push(LineMeta { thread, file: None });
        return item(Row::Message(message.ts.clone()), spans, lines);
    }

    let you = message.user.as_ref() == Some(model.self_id());
    spans.push(Span::styled(
        model.author(message),
        match you {
            true => Class::You,
            false => Class::Sender,
        },
    ));
    spans.push(Span::plain(": "));

    let body = model.render(message);
    let body = body.trim_end().replace('\n', &format!("\n{indent}"));
    push_body(&mut spans, &body, model);
    if message.edited {
        // The reader is told what they are looking at is not what was sent.
        spans.push(Span::styled(" (edited)", Class::Muted));
    }
    // The time trails the words rather than heading them: it is the least
    // of what the reader came for.
    spans.push(Span::styled(format!("  {}", clock_time(at)), Class::Time));
    spans.push(Span::plain("\n"));
    // A file's line is the one that reads as the file: `enter` there opens
    // it rather than the thread.
    lines.extend(body.split('\n').map(|line| {
        LineMeta {
            thread: thread.clone(),
            file: message
                .files
                .iter()
                .find(|file| line.trim() == file.line())
                .cloned(),
        }
    }));

    if !message.reactions.is_empty() {
        spans.push(Span::plain(indent.clone()));
        push_reactions(&mut spans, message, model);
        lines.push(LineMeta {
            thread: thread.clone(),
            file: None,
        });
    }
    if !in_thread && message.is_broadcast() {
        // It was said in a thread and sent here too: the reader should know
        // which, or the thread reads as two conversations.
        spans.push(Span::styled(
            format!("{indent}also sent to the channel\n"),
            Class::Muted,
        ));
        lines.push(LineMeta {
            thread: thread.clone(),
            file: None,
        });
    }
    // The thread under a message is one line, not a fold-out: the reader
    // sees that it exists and opens it with `enter`.
    if !in_thread && message.reply_count > 0 {
        spans.push(Span::styled(
            format!("{indent}{}\n", replies_line(message)),
            Class::Topic,
        ));
        lines.push(LineMeta { thread, file: None });
    }
    item(Row::Message(message.ts.clone()), spans, lines)
}

/// The break between days, which is the only separator the transcript has.
fn day_item(at: i64) -> Rendered {
    day_rule(day_label(at))
}

fn day_rule(label: String) -> Rendered {
    muted_item(
        Row::Day(label.clone()),
        format!("── {label} ──\n"),
        Class::Muted,
    )
}

fn muted_item(key: Row, text: impl Into<String>, class: Class) -> Rendered {
    let text = text.into();
    let lines = text.matches('\n').count().max(1);
    item(
        key,
        vec![Span::styled(text, class)],
        vec![LineMeta::default(); lines],
    )
}

fn item(key: Row, spans: Vec<Span>, lines: Vec<LineMeta>) -> Rendered {
    let (text, styles) = lay_out(&spans);
    Item::new(key, text).with_styles(styles).with_lines(lines)
}

/// Where a continuation line, a reaction row, a thread count and a picture
/// all start: two columns in, so they read as belonging to the message
/// above rather than as a message of their own.
const BODY_INDENT: usize = 2;

/// A membership or housekeeping event, as one line. Slack shows these and a
/// channel reads wrong without them, but they are not what anyone came to
/// read: no author column, no time.
fn system_line(message: &Message, model: &Model) -> Option<String> {
    let author = model.author(message);
    Some(match message.subtype.as_deref()? {
        "channel_join" | "group_join" => format!("— {author} joined —"),
        "channel_leave" | "group_leave" => format!("— {author} left —"),
        // The topic itself is the news here, so the message keeps its words.
        "channel_topic" | "channel_purpose" | "pinned_item" => {
            format!("— {} —", model.render(message).trim_start_matches('@'))
        }
        _ => return None,
    })
}

/// The reactions under a message: `👍 3 · 🎉 1`. One the reader added is in
/// their own class, which is the whole of how they can tell; a word for it
/// would be noise on every line.
fn push_reactions(spans: &mut Vec<Span>, message: &Message, model: &Model) {
    for (index, reaction) in message.reactions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Class::Muted));
        }
        let mine = reaction.users.iter().any(|user| user == model.self_id());
        spans.push(Span::styled(
            format!(
                "{} {}",
                crate::emoji::render(&format!(":{}:", reaction.name)),
                reaction.count
            ),
            match mine {
                true => Class::You,
                false => Class::Muted,
            },
        ));
    }
    spans.push(Span::plain("\n"));
}

/// `↳ 3 replies · 14:41`: how many, and when the thread was last touched.
fn replies_line(message: &Message) -> String {
    let count = message.reply_count;
    let plural = match count {
        1 => "reply",
        _ => "replies",
    };
    match &message.latest_reply {
        Some(latest) => format!(
            "↳ {count} {plural} · {}",
            clock_time(latest.epoch_seconds() as i64)
        ),
        None => format!("↳ {count} {plural}"),
    }
}

/// A body, with the workspace's own emoji muted. `:forrest_gump_wave:` is a
/// picture everywhere but here, so it reads as chrome rather than as a word
/// someone typed.
fn push_body(spans: &mut Vec<Span>, body: &str, model: &Model) {
    let mut marked: Vec<(Range<usize>, Class)> = Vec::new();
    // Lines the renderer added rather than the author: an attachment's card
    // and a collapsed link preview. They read as chrome, not as speech.
    let mut offset = 0;
    for line in body.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("— ") || trimmed.starts_with("↗ ") {
            let start = offset + (line.len() - trimmed.len());
            marked.push((start..offset + line.len(), Class::Muted));
        }
        offset += line.len() + 1;
    }
    for range in crate::emoji::shortcodes(body) {
        if model.is_custom_emoji(&body[range.start + 1..range.end - 1]) {
            marked.push((range, Class::Muted));
        }
    }
    // A mention of the reader carries their name like anyone else's; only
    // the class says it is about them.
    if let Some(mention) = model.self_mention() {
        let mut from = 0;
        while let Some(at) = body[from..].find(&mention) {
            let start = from + at;
            from = start + mention.len();
            marked.push((start..from, Class::You));
        }
    }
    marked.sort_by_key(|(range, _)| range.start);
    let mut cursor = 0;
    for (range, class) in marked {
        if range.start < cursor {
            continue;
        }
        spans.push(Span::plain(body[cursor..range.start].to_owned()));
        spans.push(Span::styled(body[range.clone()].to_owned(), class));
        cursor = range.end;
    }
    spans.push(Span::plain(body[cursor..].to_owned()));
}

impl gpui::Render for ConversationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoSlackConversation")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::WorkspaceName;
    use crate::types::{ChannelId, UserId};

    fn model() -> Model {
        let mut model = Model::new(WorkspaceName("acme".into()));
        model.set_self(UserId("ME".into()));
        model.add_users([crate::types::User {
            id: UserId("U1".into()),
            name: "ada".into(),
            handle: "ada".into(),
        }]);
        model
    }

    fn message(ts: &str, thread_ts: Option<&str>, text: &str) -> Message {
        let mut value = json!({"ts": ts, "user": "U1", "text": text});
        if let Some(thread_ts) = thread_ts {
            value["thread_ts"] = json!(thread_ts);
        }
        crate::api::parse_message(&value, &ChannelId("C1".into())).unwrap()
    }

    fn parsed(value: serde_json::Value) -> Message {
        crate::api::parse_message(&value, &ChannelId("C1".into())).unwrap()
    }

    /// The transcript as one string, its classes, and its line map: what
    /// the surface would put in the buffer, built the way `rebuild` does.
    fn render_messages(
        messages: &[Message],
        model: &Model,
        in_thread: bool,
    ) -> (String, Vec<(Class, Range<usize>)>, Vec<LineMeta>) {
        let shown = messages
            .iter()
            .filter(|message| in_thread || message.is_top_level());
        let mut items = Vec::new();
        let mut last: Option<i64> = None;
        for message in shown {
            let at = message.ts.epoch_seconds() as i64;
            if last.is_none_or(|last| crosses_day(last, at)) {
                items.push(day_item(at));
            }
            last = Some(at);
            items.push(message_item(message, model, in_thread));
        }
        let mut text = String::new();
        let mut styles = Vec::new();
        let mut lines = Vec::new();
        for item in items {
            let offset = text.len();
            text.push_str(&item.text);
            styles.extend(
                item.styles
                    .into_iter()
                    .map(|(class, range)| (class, offset + range.start..offset + range.end)),
            );
            lines.extend(item.lines);
        }
        (text, styles, lines)
    }

    /// A stand-in for what the transcript holds, in display order.
    struct Held(Vec<Row>);

    impl Placed for Held {
        fn holds(&self, key: &Row) -> bool {
            self.0.contains(key)
        }

        fn above(&self, key: &Row) -> Option<Row> {
            let at = self.0.iter().position(|held| held == key)?;
            self.0.get(at.checked_sub(1)?).cloned()
        }

        fn below(&self, key: &Row) -> Option<Row> {
            let at = self.0.iter().position(|held| held == key)?;
            self.0.get(at + 1).cloned()
        }
    }

    /// What a conversation open on the surface looks like: a day rule and
    /// the messages under it.
    fn held(messages: &[Message]) -> Held {
        let mut keys = Vec::new();
        let mut last: Option<i64> = None;
        for message in messages {
            let at = message.ts.epoch_seconds() as i64;
            if last.is_none_or(|last| crosses_day(last, at)) {
                keys.push(Row::Day(day_label(at)));
            }
            last = Some(at);
            keys.push(Row::Message(message.ts.clone()));
        }
        Held(keys)
    }

    /// The text carrying one class, in order: what the reader sees painted.
    fn classed(text: &str, styles: &[(Class, Range<usize>)], class: Class) -> Vec<String> {
        styles
            .iter()
            .filter(|(candidate, _)| *candidate == class)
            .map(|(_, range)| text[range.clone()].to_owned())
            .collect()
    }

    #[test]
    fn a_page_landing_over_a_day_rule_still_reaches_the_top() {
        let older = message("1.0", None, "older");
        let shown = [older.clone(), message("2.0", None, "newest")];
        let held = held(&shown[1..]);
        let ops = plan(&[Update::Inserted(older.ts.clone())], &shown, &held);
        assert!(
            ops.iter().any(|op| reaches_the_top(op, &held)),
            "a day rule above the first message is chrome, not content: {ops:?}"
        );
        let deeper = Held(vec![
            Row::Gap,
            Row::Day("Tue 1 Sep".into()),
            Row::Message(Ts("2.0".into())),
        ]);
        assert!(
            reaches_the_top(
                &Op::Insert {
                    before: Some(Row::Message(Ts("2.0".into()))),
                    keys: vec![Row::Message(Ts("1.0".into()))],
                },
                &deeper
            ),
            "the gap line and its day rule are both chrome"
        );
        assert!(
            !reaches_the_top(
                &Op::Insert {
                    before: Some(Row::Message(Ts("2.0".into()))),
                    keys: vec![Row::Message(Ts("1.5".into()))],
                },
                &Held(vec![
                    Row::Message(Ts("1.0".into())),
                    Row::Message(Ts("2.0".into())),
                ])
            ),
            "a message above means the run landed in the middle"
        );
    }

    #[test]
    fn one_action_buys_exactly_one_page() {
        let mut fill = Fill::default();
        assert!(fill.wants_page(true), "opening onto a gap asks once");
        assert!(
            !fill.wants_page(true),
            "the page landing does not buy another"
        );
        fill.user_moved();
        assert!(fill.wants_page(true), "the reader scrolling buys one more");
        assert!(!fill.wants_page(true), "and only one");
        let mut fill = Fill::default();
        assert!(
            !fill.wants_page(false),
            "a reader away from the top asks for nothing"
        );
        assert!(fill.wants_page(true), "and has spent nothing");
    }

    #[test]
    fn the_top_of_the_view_is_what_asks() {
        assert!(near_top(0.0, 0.0), "a view not laid out yet is at the top");
        assert!(near_top(0.0, 40.0), "the top itself asks");
        assert!(near_top(39.0, 40.0), "a screen early still asks");
        assert!(
            !near_top(41.0, 40.0),
            "a reader who has scrolled away is left alone"
        );
    }

    #[test]
    fn a_message_off_the_socket_costs_one_append() {
        let held = held(&[message("1700000000.0", None, "hello")]);
        let arrived = message("1700000060.0", None, "and another");
        let shown = [message("1700000000.0", None, "hello"), arrived.clone()];

        let ops = plan(&[Update::Inserted(arrived.ts.clone())], &shown, &held);

        assert_eq!(
            ops,
            vec![Op::Insert {
                before: None,
                keys: vec![Row::Message(arrived.ts)],
            }],
            "one item at the end, and nothing else is touched"
        );
    }

    #[test]
    fn an_edit_costs_one_replacement() {
        let shown = [
            message("1700000000.0", None, "hello"),
            message("1700000060.0", None, "fixed"),
        ];
        let held = held(&shown);

        let ops = plan(
            &[Update::Replaced(Ts("1700000060.0".into()))],
            &shown,
            &held,
        );

        assert_eq!(
            ops,
            vec![Op::Replace(Row::Message(Ts("1700000060.0".into())))],
            "the edited message is rewritten where it stands"
        );
    }

    #[test]
    fn a_page_of_history_costs_one_insert_at_the_top() {
        let anchor = message("1700000600.0", None, "already here");
        let held = held(std::slice::from_ref(&anchor));
        let older = [
            message("1700000000.0", None, "older"),
            message("1700000060.0", None, "older still"),
        ];
        let shown = [older[0].clone(), older[1].clone(), anchor.clone()];

        let ops = plan(
            &older
                .iter()
                .map(|message| Update::Inserted(message.ts.clone()))
                .collect::<Vec<_>>(),
            &shown,
            &held,
        );

        assert_eq!(
            ops,
            vec![Op::Insert {
                before: Some(Row::Message(anchor.ts.clone())),
                keys: vec![
                    Row::Message(older[0].ts.clone()),
                    Row::Message(older[1].ts.clone()),
                ],
            }],
            "a whole page arrives as one run, under the day rule already there"
        );
    }

    #[test]
    fn a_page_reaching_back_a_day_takes_over_the_day_rule() {
        // A day and a bit: the page ends on the same day as what is on
        // screen, so the rule that used to head that day would end up in the
        // middle of the page.
        let day = 86_400.0;
        let anchor = message("1700000600.0", None, "already here");
        let older = [
            message(&format!("{}.0", 1700000600.0 - day), None, "the day before"),
            message("1700000000.0", None, "earlier the same day"),
        ];
        let held = held(std::slice::from_ref(&anchor));
        let shown = [older[0].clone(), older[1].clone(), anchor.clone()];
        let rule = |message: &Message| Row::Day(day_label(message.ts.epoch_seconds() as i64));

        let ops = plan(
            &older
                .iter()
                .map(|message| Update::Inserted(message.ts.clone()))
                .collect::<Vec<_>>(),
            &shown,
            &held,
        );

        assert_eq!(
            ops,
            vec![
                Op::Remove(rule(&anchor)),
                Op::Insert {
                    before: Some(Row::Message(anchor.ts.clone())),
                    keys: vec![
                        rule(&older[0]),
                        Row::Message(older[0].ts.clone()),
                        rule(&older[1]),
                        Row::Message(older[1].ts.clone()),
                    ],
                },
            ],
            "the old rule comes down and the page brings its own"
        );
    }

    #[test]
    fn a_deleted_message_takes_its_day_rule_with_it() {
        let only = message("1700000000.0", None, "hello");
        let held = held(std::slice::from_ref(&only));

        let ops = plan(&[Update::Removed(only.ts.clone())], &[], &held);

        assert_eq!(
            ops,
            vec![
                Op::Remove(Row::Message(only.ts.clone())),
                Op::Remove(Row::Day(day_label(only.ts.epoch_seconds() as i64))),
            ],
            "nothing is left under the rule, so the rule goes"
        );
    }

    #[test]
    fn a_message_reads_as_name_body_then_time() {
        let (text, styles, _) = render_messages(
            &[
                message("1700000000.0", None, "hello"),
                message("1700000060.0", None, "over\ntwo lines"),
            ],
            &model(),
            false,
        );
        assert!(!text.contains("1700000000"), "no raw timestamps: {text}");
        let body = text
            .lines()
            .find(|line| line.contains("hello"))
            .expect("the message is on a line");
        assert!(
            body.starts_with("ada: hello  ") && body.len() == "ada: hello  00:00".len(),
            "name, body, then the time trailing it: {body:?}"
        );
        assert!(
            !text.contains("\n\n"),
            "no blank line between messages: {text:?}"
        );
        let continuation = text
            .lines()
            .find(|line| line.contains("two lines"))
            .expect("the second line of a body");
        assert!(
            continuation.starts_with("  two lines  "),
            "a continuation line indents under its message, and the time \
             trails the last line of the body: {continuation:?}"
        );
        let times = classed(&text, &styles, Class::Time);
        assert_eq!(times.len(), 2, "one time per message: {times:?}");
        assert!(
            times
                .iter()
                .all(|time| time.starts_with("  ") && time.len() == 7),
            "the time trails the body, two spaces after it: {times:?}"
        );
    }

    #[test]
    fn the_reader_is_named_like_anyone_else_and_told_apart_by_class() {
        let mut model = model();
        model.add_users([crate::types::User {
            id: UserId("ME".into()),
            name: "Manmeet".into(),
            handle: "manmeet".into(),
        }]);
        let mut own = message("1700000000.0", None, "on it");
        own.user = Some(UserId("ME".into()));
        let mention = message("1700000001.0", None, "can <@ME> take this?");
        let (text, styles, _) = render_messages(&[own, mention], &model, false);

        assert!(!text.contains("you"), "the word never appears: {text}");
        assert!(text.contains("Manmeet"), "{text}");
        assert!(text.contains("can @Manmeet take this?"), "{text}");
        assert_eq!(
            classed(&text, &styles, Class::You),
            vec!["Manmeet", "@Manmeet"],
            "own author line and a mention of the reader, class only"
        );
    }

    #[test]
    fn standard_emoji_are_glyphs_and_workspace_emoji_stay_muted_shortcodes() {
        let mut model = model();
        model.set_custom_emoji(["forrest_gump_wave".to_owned()]);
        let (text, styles, _) = render_messages(
            &[message(
                "1700000000.0",
                None,
                "morning :wave: :forrest_gump_wave:",
            )],
            &model,
            false,
        );
        assert!(text.contains("morning 👋"), "{text}");
        assert!(
            text.contains(":forrest_gump_wave:"),
            "a workspace emoji has no glyph to become: {text}"
        );
        assert!(
            classed(&text, &styles, Class::Muted)
                .iter()
                .any(|muted| muted == ":forrest_gump_wave:"),
            "the shortcode reads as chrome: {text}"
        );
    }

    #[test]
    fn a_channel_shows_top_level_messages_and_a_count_line_for_the_thread() {
        let parent = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "text": "the curve needs a name",
            "reply_count": 3,
            "latest_reply": "1700000900.0",
        }));
        let reply = parsed(json!({
            "ts": "1700000600.0",
            "thread_ts": "1700000000.0",
            "user": "U1",
            "text": "deal curve is fine",
        }));
        let broadcast = parsed(json!({
            "ts": "1700000900.0",
            "thread_ts": "1700000000.0",
            "subtype": "thread_broadcast",
            "user": "U1",
            "text": "named: deal curve",
        }));
        let messages = [parent, reply, broadcast];

        let (text, _, lines) = render_messages(&messages, &model(), false);
        assert!(
            !text.contains("deal curve is fine"),
            "an ordinary reply belongs to its thread alone: {text}"
        );
        assert!(
            text.contains("named: deal curve"),
            "a broadcast was said to the room: {text}"
        );
        assert!(text.contains("also sent to the channel"), "{text}");
        assert!(text.contains("↳ 3 replies · "), "{text}");
        assert!(!text.contains("in thread"), "the marker is gone: {text}");
        assert_eq!(
            lines.len(),
            text.matches('\n').count(),
            "the line map still covers the transcript exactly"
        );
        let root = Ts("1700000000.0".into());
        assert_eq!(
            lines.last().and_then(|line| line.thread.clone()),
            Some(root),
            "enter on the count line opens the thread"
        );

        // The thread surface is where every reply renders.
        let (text, _, _) = render_messages(&messages, &model(), true);
        assert!(text.contains("deal curve is fine"), "{text}");
        assert!(
            !text.contains("↳ 3 replies"),
            "a thread does not count itself: {text}"
        );
    }

    #[test]
    fn reactions_read_as_glyphs_and_the_readers_own_is_told_apart_by_class() {
        let mut model = model();
        model.add_users([crate::types::User {
            id: UserId("ME".into()),
            name: "Manmeet".into(),
            handle: "manmeet".into(),
        }]);
        let message = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "text": "friday?",
            "reactions": [
                {"name": "thumbsup", "count": 2, "users": ["U1", "ME"]},
                {"name": "tada", "count": 1, "users": ["U1"]},
            ],
        }));
        let (text, styles, lines) = render_messages(&[message], &model, false);
        assert!(text.contains("👍 2 · 🎉 1"), "{text}");
        assert!(!text.contains("you"), "no word for it: {text}");
        assert!(
            classed(&text, &styles, Class::You).contains(&"👍 2".to_owned()),
            "one the reader added is theirs"
        );
        assert!(classed(&text, &styles, Class::Muted).contains(&"🎉 1".to_owned()));
        assert_eq!(lines.len(), text.matches('\n').count());
    }

    #[test]
    fn an_edited_message_says_so() {
        let message = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "text": "friday it is",
            "edited": {"user": "U1", "ts": "1700000100.0"},
        }));
        let (text, styles, lines) = render_messages(&[message], &model(), false);
        assert!(text.contains("friday it is (edited)  "), "{text}");
        assert!(
            classed(&text, &styles, Class::Muted).contains(&" (edited)".to_owned()),
            "{text}"
        );
        assert_eq!(lines.len(), text.matches('\n').count());
    }

    #[test]
    fn a_join_is_one_muted_line_and_an_app_card_reads_as_chrome() {
        let join = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "subtype": "channel_join",
            "text": "<@U1> has joined the channel",
        }));
        let bot = parsed(json!({
            "ts": "1700000060.0",
            "username": "deploybot",
            "text": "deploy finished",
            "attachments": [{
                "title": "build #412",
                "pretext": "pipeline",
                "text": "all checks passed",
                "fields": [{"title": "branch", "value": "main", "short": true}],
            }],
        }));
        let preview = parsed(json!({
            "ts": "1700000120.0",
            "user": "U1",
            "text": "worth a read",
            "attachments": [{"is_msg_unfurl": true, "title": "Worth a read", "text": "buried"}],
        }));
        let (text, styles, lines) = render_messages(&[join, bot, preview], &model(), false);

        assert!(text.contains("— ada joined —"), "{text}");
        assert!(
            !text.contains("has joined the channel"),
            "one line, not a sentence: {text}"
        );
        assert!(
            text.contains("deploybot"),
            "a bot is named like anyone: {text}"
        );
        assert!(text.contains("branch: main"), "{text}");
        assert!(text.contains("↗ Worth a read"), "{text}");
        assert!(
            !text.contains("buried"),
            "a preview never paints its body: {text}"
        );
        let muted = classed(&text, &styles, Class::Muted)
            .iter()
            .map(|span| span.trim().to_owned())
            .collect::<Vec<_>>();
        assert!(
            muted.iter().any(|span| span == "↗ Worth a read"),
            "{muted:?}"
        );
        assert!(muted.iter().any(|span| span == "— pipeline"), "{muted:?}");
        assert_eq!(lines.len(), text.matches('\n').count());
    }

    #[test]
    fn every_line_of_a_message_opens_its_thread() {
        let messages = [
            message("1700000000.0", None, "first line\nsecond line"),
            message("1700000001.0", Some("1700000000.0"), "a reply"),
        ];
        let (text, _, lines) = render_messages(&messages, &model(), true);
        assert_eq!(
            lines.len(),
            text.matches('\n').count(),
            "the line map must cover the transcript exactly"
        );
        let root = Ts("1700000000.0".into());
        assert!(
            lines
                .iter()
                .filter(|line| line.thread.as_ref() == Some(&root))
                .count()
                >= 3,
            "both messages belong to the same thread"
        );
    }
}
