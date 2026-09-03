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

use editor::scroll::{Autoscroll, AutoscrollStrategy};
use editor::{Editor, EditorEvent, EditorMode, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, EventEmitter, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_transcript::{BlockSpec, Item, Transcript};
use theme::ActiveTheme as _;

use crate::model::Model;
use crate::session::{Session, Source, Update};
use crate::types::{CELL_ASPECT, FileSummary, IMAGE_COLUMNS, Message, ThreadKey, Ts};
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
    /// Whether the surface is writing the transcript. The edits move the
    /// cursor and the view on their own, and none of that is the reader
    /// asking for anything.
    editing: bool,
    /// One user action buys one page. Set when a fill is asked for, cleared
    /// when the reader scrolls or moves the cursor themselves, so a landed
    /// page cannot walk the whole conversation back to its beginning.
    fill: Fill,
    /// Where the surface last put the view and the cursor itself. The events
    /// that come back for those are not the reader moving, and must not buy
    /// another page.
    moved: Moved,
    /// The message a deal is about: tinted, and the place the surface puts
    /// the cursor when it opens. It stays for the life of the surface, so
    /// the reader can scroll away and still find what they were dealt.
    dealt: Option<Ts>,
    /// Whether the dealt message has been scrolled to yet. It may not be
    /// loaded when the deal opens; the first refresh that brings it in does
    /// the scroll.
    dealt_placed: bool,
    /// Slack's read cursor as it stood when the surface opened. Opening
    /// marks the conversation read, so this is taken once and kept: the
    /// rule has to stay where the reader found it for as long as they are
    /// reading, or it would vanish out from under them.
    unread_from: Option<Ts>,
    /// The message the rule currently sits above, and whether the cursor
    /// has been put there yet. The first unread may arrive a page later
    /// than the surface, so both are settled on the refresh that brings it.
    unread_at: Option<Ts>,
    unread_placed: bool,
    /// The picture the next message carries, if the reader attached one.
    /// One at a time: a second attachment replaces it, which is what the
    /// chip shows.
    attached: Option<Attached>,
    /// The message being rewritten, if `e` is open on one: tinted, and what
    /// `enter` updates instead of sending. The composer's own text is held
    /// beside it so `escape` gives the reader back what they were writing.
    editing_message: Option<Ts>,
    held_compose: Option<String>,
    /// Messages drawn before their picture had finished downloading, by the
    /// file the message is waiting on. Nothing in the update log speaks for
    /// a download, so the surface remembers this itself.
    awaiting_images: Vec<(Ts, String)>,
    _subscriptions: Vec<gpui::Subscription>,
}

/// What the surface asks its host for. Showing a picture is the host's
/// business: this crate knows which file was asked for, not what the frame
/// around it can draw.
pub enum Event {
    OpenFile(FileSummary),
    /// A dropped path that could not be read. The host says so; this crate
    /// has no notice line of its own.
    AttachFailed(String),
}

/// A picture waiting to go with the next message: what the chip shows and
/// what `enter` uploads.
#[derive(Clone)]
pub struct Attached {
    pub name: String,
    pub bytes: std::sync::Arc<Vec<u8>>,
}

impl Attached {
    /// The chip, which reads like a file line in the transcript because it
    /// is about to become one.
    fn line(&self) -> String {
        format!(
            "{} · {}",
            self.name,
            crate::types::human_size(self.bytes.len() as u64)
        )
    }
}

/// What asking for an edit did. A message that is not the reader's own is
/// the one case worth telling them about: nothing happens, and silence
/// would read as a broken key.
pub enum EditStart {
    Started(Ts),
    NotYours,
    Nothing,
}

impl EventEmitter<Event> for ConversationView {}

/// One page per user action, and the two cases where nobody has asked yet:
/// opening a conversation whose mirrored run is short, and a gap line on a
/// view that has not asked for anything.
#[derive(Default)]
struct Fill {
    asked: bool,
    /// Whether the reader has moved at all. A conversation opening onto a
    /// gap asks once for history, but a run that has not caught up with the
    /// live end waits to be scrolled onto: opening is not a request to walk
    /// forward through everything missed.
    moved: bool,
}

impl Fill {
    /// The reader scrolled or moved the cursor: the next fill is theirs to
    /// buy again.
    fn user_moved(&mut self) {
        self.asked = false;
        self.moved = true;
    }

    /// The one page this action buys, if the view is sitting on something
    /// worth asking for.
    fn wants(&mut self, want: Option<Want>) -> Option<Want> {
        if self.asked {
            return None;
        }
        let want = match want? {
            Want::Newer(_) if !self.moved => return None,
            want => want,
        };
        self.asked = true;
        Some(want)
    }
}

/// Which side of the loaded run a page is wanted on. Paging back happens at
/// the top; a hole is filled forward from the message it sits over, because
/// that is the end the reader has read up to.
#[derive(Clone, Debug, PartialEq)]
enum Want {
    Older,
    Newer(Ts),
}

/// What the surface moved on its own, so the resulting events can be told
/// apart from the reader's own scrolling and cursor motion.
#[derive(Default)]
struct Moved {
    scroll: Option<f64>,
    cursor: Option<u32>,
    /// Whether the reader has just moved the cursor. The scroll that brings
    /// the view to it is part of that motion, not a second action: without
    /// this, one `G` at the bottom of a hole buys two pages.
    after_selection: bool,
}

/// What the transcript keys on. Day rules and the gap notice are items like
/// any other, so they insert and disappear through the same path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Row {
    /// The loading or failure line above everything.
    Notice,
    /// "older messages not loaded", while the run does not reach the start.
    Gap,
    /// "newer messages not loaded", under the message a hole sits over.
    Newer(Ts),
    Day(String),
    /// `── new ──`: everything under it arrived since the reader last read
    /// the conversation.
    Unread,
    Message(Ts),
    /// The picture waiting to go with the next message, under everything
    /// and over the composer.
    Chip,
}

/// What a line offers the cursor.
#[derive(Clone, Debug, Default, PartialEq)]
struct LineMeta {
    /// The thread the line belongs to, so `enter` opens the right one.
    thread: Option<Ts>,
    /// The file the line names, which `enter` opens instead.
    file: Option<FileSummary>,
    /// The URL the line stands for. The text shows a link's label alone, so
    /// this is the only place the address survives for `enter` to open.
    link: Option<String>,
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
                    EditorEvent::ScrollPositionChanged { local, autoscroll } => {
                        let position = editor.update(cx, |editor, cx| editor.scroll_position(cx).y);
                        // The surface's own re-anchoring comes back as a
                        // scroll event too, and so does the view following
                        // the content when a page lands under the cursor.
                        // Only the reader's own counts, or one keypress at
                        // the bottom of a hole would walk the whole way down
                        // it a page at a time.
                        if this.moved.scroll == Some(position) {
                            this.moved.scroll = None;
                        } else if *local && !*autoscroll && !this.moved.after_selection {
                            this.fill.user_moved();
                        }
                        this.moved.after_selection = false;
                        cx.notify();
                    }
                    // A reader already at the top who presses `gg` moves the
                    // cursor without moving the view, so no scroll event
                    // comes: the motion is the action.
                    EditorEvent::SelectionsChanged { local: true } => {
                        // An edit under the cursor shifts it: the transcript
                        // growing is not the reader moving.
                        if this.editing {
                            return;
                        }
                        let row = this.cursor_row(cx) as u32;
                        if this.moved.cursor == Some(row) {
                            this.moved.cursor = None;
                            return;
                        }
                        this.moved.after_selection = true;
                        this.fill.user_moved();
                        cx.notify();
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
            editing: false,
            fill: Fill::default(),
            moved: Moved::default(),
            dealt: None,
            dealt_placed: false,
            unread_from: None,
            unread_at: None,
            unread_placed: false,
            editing_message: None,
            held_compose: None,
            attached: None,
            awaiting_images: Vec::new(),
            _subscriptions: subscriptions,
        };
        view.transcript.attach(&view.editor.clone(), cx);
        session.update(cx, |session, cx| session.open(&source, cx));
        // Read before the open's own mark can land, and only for a
        // conversation: a thread's read cursor is Slack's per-thread one,
        // which is a different fact.
        if matches!(source, Source::Conversation(_)) {
            view.unread_from = session
                .read(cx)
                .model()
                .last_read(source.channel())
                .cloned();
        }
        view.refresh(window, cx);
        // A conversation with nothing new opens on the composer, which is
        // what the reader came to do. One with unread messages opens on the
        // first of them instead; `refresh` has already put the cursor there.
        if !view.unread_placed {
            view.select_compose(window, cx);
        }
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

    /// The URL the cursor's line stands for: a link's label shows no address,
    /// so `enter` reads it from here.
    pub fn cursor_link(&self, cx: &mut Context<Self>) -> Option<String> {
        let row = self.cursor_row(cx) as u32;
        self.transcript
            .line_meta(row, cx)
            .and_then(|meta| meta.link.clone())
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

    /// Sends the compose region, or posts the rewrite if an edit is open.
    /// The message appears when Slack accepts it, so nothing is shown that
    /// was not actually sent.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text();
        // A picture is a message on its own; words are not required.
        if text.trim().is_empty() && self.attached.is_none() {
            return;
        }
        let source = self.source.clone();
        if let Some(file) = self.attached.take() {
            self.send_attached(file, text, cx);
            return;
        }
        if let Some(ts) = self.editing_message.take() {
            // The composer goes back to whatever the reader had put aside to
            // make the edit, the same as cancelling: the rewrite is done
            // with, and the half-written message is not.
            let held = self.held_compose.take().unwrap_or_default();
            self.set_compose(held, cx);
            self.retint(&ts, cx);
            self.session.update(cx, |session, cx| {
                session.edit_message(&source, ts, text, cx)
            });
            return;
        }
        self.set_compose(String::new(), cx);
        let sending = self
            .session
            .update(cx, |session, cx| session.send(&source, text.clone(), cx));
        cx.spawn(async move |this, cx| {
            if sending.await.is_err() {
                let _ = this.update(cx, |this, cx| this.restore_compose(text, cx));
            }
        })
        .detach();
    }

    /// Puts a refused message back where it was typed. Whatever the reader
    /// has written since goes under it rather than over it: text that was
    /// typed is never dropped on the floor, and which of the two they want
    /// is theirs to decide.
    fn restore_compose(&mut self, text: String, cx: &mut Context<Self>) {
        let held = self.input.read(cx).text();
        self.set_compose(restored_compose(text, &held), cx);
        cx.notify();
    }

    /// The message the cursor is on, if the transcript has one there.
    fn cursor_message(&self, cx: &mut Context<Self>) -> Option<Message> {
        let row = self.cursor_row(cx) as u32;
        let Row::Message(ts) = self.transcript.key_at_row(row, cx)?.clone() else {
            return None;
        };
        self.shown_messages(cx)
            .into_iter()
            .find(|message| message.ts == ts)
    }

    /// `e`: rewrite the message under the cursor. Only the reader's own can
    /// be rewritten, and saying so is the host's job, so a refusal is
    /// reported rather than swallowed.
    pub fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> EditStart {
        let Some(message) = self.cursor_message(cx) else {
            return EditStart::Nothing;
        };
        if message.user.as_ref() != Some(self.session.read(cx).model().self_id()) {
            return EditStart::NotYours;
        }
        self.begin_edit(&message, window, cx);
        EditStart::Started(message.ts)
    }

    /// `up` in an empty composer: rewrite the last thing the reader said,
    /// which is the habit Slack teaches. Nothing of theirs on screen, or a
    /// composer with something in it, and this is not the reader asking.
    pub fn edit_last_own(&mut self, window: &mut Window, cx: &mut Context<Self>) -> EditStart {
        if !self.input.read(cx).text().trim().is_empty() || self.editing_message.is_some() {
            return EditStart::Nothing;
        }
        let self_id = self.session.read(cx).model().self_id().clone();
        let Some(message) = self
            .shown_messages(cx)
            .into_iter()
            .filter(|message| message.user.as_ref() == Some(&self_id))
            .next_back()
        else {
            return EditStart::Nothing;
        };
        self.begin_edit(&message, window, cx);
        EditStart::Started(message.ts)
    }

    fn begin_edit(&mut self, message: &Message, window: &mut Window, cx: &mut Context<Self>) {
        if self.held_compose.is_none() {
            self.held_compose = Some(self.input.read(cx).text());
        }
        let previous = self.editing_message.replace(message.ts.clone());
        // The composer holds what was sent, not what was drawn: an edit
        // starts from the reader's own words.
        self.set_compose(message.text.clone(), cx);
        if let Some(previous) = previous.filter(|previous| previous != &message.ts) {
            self.retint(&previous, cx);
        }
        self.retint(&message.ts, cx);
        self.select_compose(window, cx);
    }

    /// `escape` with an edit open: the message stands as it was and the
    /// composer holds what it held before.
    pub fn cancel_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ts) = self.editing_message.take() else {
            return false;
        };
        let held = self.held_compose.take().unwrap_or_default();
        self.set_compose(held, cx);
        self.retint(&ts, cx);
        true
    }

    /// Attaches a picture to the next message. One at a time: a second
    /// replaces the first, and the answer says so, because a chip that
    /// quietly changed under the reader would send the wrong file.
    pub fn attach(&mut self, name: String, bytes: Vec<u8>, cx: &mut Context<Self>) -> bool {
        let replaced = self.attached.is_some();
        self.attached = Some(Attached {
            name,
            bytes: std::sync::Arc::new(bytes),
        });
        self.refresh_chip(cx);
        cx.notify();
        replaced
    }

    /// Reads a file from disk and attaches it: the path a drop or a prompt
    /// gave. The bytes are read here so the failure is one the reader hears
    /// about before they press enter.
    pub fn attach_path(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<bool> {
        let bytes = std::fs::read(path)
            .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_owned());
        Ok(self.attach(name, bytes, cx))
    }

    /// Drops the attachment without sending it.
    pub fn clear_attachment(&mut self, cx: &mut Context<Self>) -> bool {
        let had = self.attached.take().is_some();
        if had {
            self.refresh_chip(cx);
            cx.notify();
        }
        had
    }

    /// How big the waiting picture is, for the host's journal.
    pub fn attached_size(&self) -> Option<u64> {
        self.attached.as_ref().map(|file| file.bytes.len() as u64)
    }

    /// Uploads the picture with the message. Nothing is drawn from the
    /// local bytes: the message arrives from Slack like any other. A
    /// refusal puts the chip and the words back, so the reader can try
    /// again without retyping.
    fn send_attached(&mut self, file: Attached, text: String, cx: &mut Context<Self>) {
        self.set_compose(String::new(), cx);
        self.refresh_chip(cx);
        let source = self.source.clone();
        let sending = self.session.update(cx, |session, cx| {
            session.send_file(
                &source,
                file.name.clone(),
                file.bytes.as_ref().clone(),
                text.clone(),
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            if sending.await.is_err() {
                let _ = this.update(cx, |this, cx| {
                    this.attached = Some(file);
                    this.restore_compose(text, cx);
                    this.refresh_chip(cx);
                });
            }
        })
        .detach();
    }

    /// The chip line, kept last: it belongs between the transcript and the
    /// composer, and a message arriving appends itself after whatever is
    /// at the end.
    fn refresh_chip(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.attached.clone() else {
            self.transcript.remove(&Row::Chip, cx);
            return;
        };
        let item = muted_item(Row::Chip, format!("{}\n", file.line()), Class::Muted);
        let last = self.transcript.keys().last().cloned();
        if last.as_ref() == Some(&Row::Chip) {
            self.transcript.replace(&Row::Chip, item, cx);
            return;
        }
        self.transcript.remove(&Row::Chip, cx);
        self.transcript.insert_before(None, vec![item], cx);
    }

    /// Which message an open edit is about, for the host's journal.
    pub fn editing_message(&self) -> Option<&Ts> {
        self.editing_message.as_ref()
    }

    fn set_compose(&mut self, text: String, cx: &mut Context<Self>) {
        self.input.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
    }

    /// Redraws one message, which is how the tint goes on and comes off.
    fn retint(&mut self, ts: &Ts, cx: &mut Context<Self>) {
        let key = Row::Message(ts.clone());
        let messages = self.shown_messages(cx);
        let Some(item) = self.item_for_key(&key, &messages, cx) else {
            return;
        };
        self.editing = true;
        self.transcript.replace(&key, item, cx);
        self.editing = false;
    }

    /// Where the file's bytes are, fetched if the cache lacks them: a host
    /// showing a picture itself needs the path, not the desktop's opener.
    pub fn file_path(
        &mut self,
        file: &FileSummary,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<std::path::PathBuf>> {
        self.session
            .update(cx, |session, cx| session.file_path(file, cx))
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

    /// Fills the hole under `after` with one page forward.
    fn load_newer(&mut self, after: Ts, cx: &mut Context<Self>) {
        let source = self.source.clone();
        self.session
            .update(cx, |session, cx| session.load_newer(&source, after, cx));
    }

    /// What the view is sitting on, if it is sitting on something worth a
    /// page: the top of the run, or a hole in it.
    fn wanted(&self, position: f64, screen: f64, cursor: f64, cx: &App) -> Option<Want> {
        let snapshot = self.transcript.buffer().read(cx).snapshot();
        let holes = self
            .transcript
            .keys()
            .filter_map(|key| {
                let Row::Newer(ts) = key else {
                    return None;
                };
                let start = self.transcript.range_of(key)?.start;
                let row = text::ToPoint::to_point(&start, &snapshot).row as f64;
                Some((row, ts.clone()))
            })
            .collect::<Vec<_>>();
        wanted_at(position, screen, cursor, &holes)
    }

    /// Buys the page the view is sitting on, if this action has not spent
    /// one already.
    fn fill(&mut self, position: f64, screen: f64, cx: &mut Context<Self>) {
        let cursor = self.cursor_row(cx) as f64;
        let want = self.wanted(position, screen, cursor, cx);
        match self.fill.wants(want) {
            Some(Want::Older) => self.load_older(cx),
            Some(Want::Newer(after)) => self.load_newer(after, cx),
            None => {}
        }
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

    /// Shows the message a deal is about: tinted for the life of the
    /// surface, with the cursor on it and the view centred on it. A deal is
    /// one message to answer, and this is that message.
    pub fn reveal(&mut self, ts: Ts, window: &mut Window, cx: &mut Context<Self>) {
        if self.dealt.as_ref() == Some(&ts) && self.dealt_placed {
            return;
        }
        self.dealt = Some(ts.clone());
        self.dealt_placed = false;
        // The message may be in an older chunk than the one the surface
        // opened on, which is the ordinary case on a long history.
        let source = self.source.clone();
        self.session
            .update(cx, |session, cx| session.open_at(&source, &ts, cx));
        let key = Row::Message(ts);
        let messages = self.shown_messages(cx);
        if let Some(item) = self.item_for_key(&key, &messages, cx) {
            self.transcript.replace(&key, item, cx);
        }
        self.refresh(window, cx);
    }

    /// Puts the cursor on the dealt message once it is in the transcript.
    /// Keeps `── new ──` above the first message the reader has not seen.
    /// The anchor is recomputed rather than remembered because a page of
    /// older messages can land above it, and the rule belongs over the
    /// oldest unread one, not over whichever was first on screen.
    fn refresh_unread(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.unread_from.clone() else {
            return;
        };
        let first = first_unread(&self.shown_messages(cx), &from);
        if first == self.unread_at {
            return;
        }
        self.editing = true;
        if self.unread_at.is_some() {
            self.transcript.remove(&Row::Unread, cx);
        }
        self.unread_at = first.clone();
        if let Some(ts) = first {
            self.transcript
                .insert_before(Some(&Row::Message(ts)), vec![unread_rule()], cx);
        }
        self.editing = false;
    }

    /// Opens the conversation on the first thing the reader has not read.
    /// Not the composer, which is where a conversation with nothing new
    /// opens, and not the top, which is last week. Once only: after that
    /// the cursor is the reader's own.
    fn place_unread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unread_placed || self.dealt.is_some() {
            return;
        }
        // The reader started writing while the page was in flight. Their
        // cursor is theirs now; the rule is still on screen to scroll to.
        if !self.input.read(cx).is_empty() {
            self.unread_placed = true;
            return;
        }
        let Some(ts) = self.unread_at.clone() else {
            return;
        };
        let Some(start) = self
            .transcript
            .range_of(&Row::Message(ts))
            .map(|range| range.start)
        else {
            return;
        };
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(start)
        else {
            return;
        };
        self.unread_placed = true;
        self.editor.update(cx, |editor, cx| {
            // Near the top rather than centred: what the reader wants in
            // front of them is everything under the rule.
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::focused()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([anchor..anchor]),
            );
        });
        self.moved.cursor = Some(self.cursor_row(cx) as u32);
    }

    fn place_dealt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dealt_placed {
            return;
        }
        let Some(ts) = self.dealt.clone() else {
            return;
        };
        let Some(start) = self
            .transcript
            .range_of(&Row::Message(ts))
            .map(|range| range.start)
        else {
            return;
        };
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(start)
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            // Centred once, not pinned: a pin re-asserts itself every frame,
            // so in a conversation long enough to scroll it would drag the
            // reader back onto the message the moment they moved off it.
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([anchor..anchor]),
            );
        });
        // The surface moved the cursor, not the reader: that must not spend
        // the reader's one page of history.
        self.moved.cursor = Some(self.cursor_row(cx) as u32);
        self.dealt_placed = true;
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
        // A deal may open on a message the tail does not hold yet: the page
        // that brings it in is where the cursor goes.
        self.refresh_unread(cx);
        self.place_dealt(window, cx);
        self.place_unread(window, cx);
        self.settle_images(cx);
        self.refresh_chrome(cx);
        self.refresh_holes(cx);
        self.refresh_chip(cx);
        cx.notify();
    }

    /// A picture whose bytes were still arriving when its message was
    /// drawn. The download changes nothing about the message and nothing
    /// about its box, which was already the picture's size: the block reads
    /// the cache when it draws, so all this owes the reader is a redraw.
    /// Rewriting the item would move every row under it for one frame.
    fn settle_images(&mut self, cx: &mut Context<Self>) {
        let arrived = {
            let session = self.session.read(cx);
            self.awaiting_images
                .iter()
                .any(|(_, id)| session.cached_file(id).is_some_and(|path| path.exists()))
        };
        if !arrived {
            return;
        }
        let session = self.session.read(cx);
        self.awaiting_images
            .retain(|(_, id)| !session.cached_file(id).is_some_and(|path| path.exists()));
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.editing = true;
        self.transcript.clear(cx);
        // The rule went with everything else; where it belongs is worked
        // out again from the run that replaces it.
        self.unread_at = None;
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
        self.editing = false;
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
        self.editing = true;
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
        self.editing = false;
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
            Row::Unread => Some(unread_rule()),
            Row::Notice | Row::Gap | Row::Newer(_) | Row::Chip => None,
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

    /// The rows that say history is missing in the middle: one under each
    /// chunk that does not run into what sits over it. A hole between two
    /// loaded chunks and a run that has not caught up with the live end are
    /// the same thing to a reader, so they read the same and fill the same
    /// way.
    fn refresh_holes(&mut self, cx: &mut Context<Self>) {
        let Some((mut holes, behind_live)) = self
            .session
            .read(cx)
            .loaded(&self.source)
            .map(|loaded| (loaded.holes.clone(), loaded.behind_live))
        else {
            return;
        };
        let shown = self.shown_messages(cx);
        if behind_live && let Some(last) = shown.last() {
            holes.push(last.ts.clone());
        }
        // A hole over a message this surface does not show — a reply in a
        // channel — has no line to sit under.
        holes.retain(|ts| self.transcript.contains(&Row::Message(ts.clone())));
        let wanted = holes
            .iter()
            .cloned()
            .map(Row::Newer)
            .collect::<HashSet<_>>();
        for key in self.transcript.keys().cloned().collect::<Vec<_>>() {
            if matches!(key, Row::Newer(_)) && !wanted.contains(&key) {
                self.transcript.remove(&key, cx);
            }
        }
        for ts in holes {
            let key = Row::Newer(ts.clone());
            if self.transcript.contains(&key) {
                continue;
            }
            // Above the day rule that heads the chunk over it: the rule
            // belongs to the messages under it, not to the hole.
            let before = self.transcript.key_after(&Row::Message(ts)).cloned();
            let item = muted_item(key, "newer messages not loaded\n", Class::Muted);
            self.transcript
                .insert_before(before.as_ref(), vec![item], cx);
        }
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
        let images = image_boxes(&item)
            .into_iter()
            .map(|(line, file)| (line, file.clone()))
            .collect::<Vec<_>>();
        if self.dealt.as_ref() == Some(&message.ts)
            || self.editing_message.as_ref() == Some(&message.ts)
        {
            mark_dealt(&mut item);
        }
        if self
            .session
            .read(cx)
            .loaded(&self.source)
            .is_some_and(|loaded| loaded.is_pending(&message.ts))
        {
            mark_pending(&mut item);
        }
        self.awaiting_images
            .retain(|(waiting, _)| waiting != &message.ts);
        for (line, file) in images {
            let ready = self.session.update(cx, |session, cx| {
                // The thumbnail is asked for alongside the picture: it is
                // what stands in the box until the picture lands.
                if let Some(thumb) = file.thumbnail() {
                    session.cache_file(&thumb, cx);
                }
                session.cache_file(&file, cx);
                session
                    .cached_file(&file.id)
                    .is_some_and(|path| path.exists())
            });
            if !ready {
                self.awaiting_images
                    .push((message.ts.clone(), file.id.clone()));
            }
            item.blocks.push(image_block(
                line,
                file,
                self.session.downgrade(),
                cx.entity().downgrade(),
            ));
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

/// What the view is sitting on, if it is sitting on something worth a page.
///
/// The cursor decides, not the scroll: a motion moves it a frame before the
/// view follows, so asking on the scroll alone would fetch the top of the
/// buffer while the reader is standing at the bottom of it. A hole the
/// reader has read down to, or one on screen, is asked forward; only a
/// reader who is at the top with the cursor asks backwards.
fn wanted_at(position: f64, screen: f64, cursor: f64, holes: &[(f64, Ts)]) -> Option<Want> {
    let hole = holes.iter().find_map(|(row, ts)| {
        let read_up_to = cursor + 1.0 >= *row;
        let on_screen = row + 1.0 >= position && *row <= position + screen;
        (read_up_to || on_screen).then(|| Want::Newer(ts.clone()))
    });
    hole.or_else(|| {
        (near_top(position, screen) && cursor <= screen.max(1.0)).then_some(Want::Older)
    })
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
        if !matches!(key, Row::Notice | Row::Gap | Row::Day(_) | Row::Unread) {
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

/// The pictures a message hangs under itself: which line each sits on, and
/// which file it is. What is cached is not an input, which is the whole of
/// the no-jump rule — the same boxes, of the same height, are asked for
/// whether the bytes have arrived or not.
fn image_boxes(item: &Rendered) -> Vec<(u32, &FileSummary)> {
    item.lines
        .iter()
        .enumerate()
        .filter_map(|(line, meta)| {
            let file = meta.file.as_ref().filter(|file| file.is_image())?;
            Some((line as u32, file))
        })
        .collect()
}

/// How big the picture is drawn inside its box: its own shape, scaled to
/// fill the box the rows were measured for. The thumbnail standing in for
/// it is drawn at exactly the same size, so the swap changes the pixels and
/// nothing else — no row moves, on any screen or at any font size.
fn drawn_size(
    file: &FileSummary,
    box_width: gpui::Pixels,
    box_height: gpui::Pixels,
) -> (gpui::Pixels, gpui::Pixels) {
    let (width, height) = (file.original_w as f32, file.original_h as f32);
    if width <= 0.0 || height <= 0.0 {
        return (box_width, box_height);
    }
    let scale = (f32::from(box_width) / width).min(f32::from(box_height) / height);
    (gpui::px(width * scale), gpui::px(height * scale))
}

/// A picture under the line that names it, indented to the body column.
/// Clicking it asks for the full-size view, the same thing `enter` on the
/// file line asks for.
///
/// The box exists from the first draw, sized from what Slack says the
/// picture measures, and holds its size for the picture's whole journey:
/// Slack's smallest thumbnail blown up to fill it, then the picture itself.
/// Nothing under it moves, and the arrival costs no item — the block reads
/// the cache each time it draws, so the surface only asks for a redraw.
fn image_block(
    line: u32,
    file: FileSummary,
    session: gpui::WeakEntity<Session>,
    view: gpui::WeakEntity<ConversationView>,
) -> BlockSpec {
    let rows = file.image_rows();
    BlockSpec {
        line,
        height: rows,
        render: std::sync::Arc::new(move |cx| {
            let (file, view) = (file.clone(), view.clone());
            let cached = |id: &str| {
                session
                    .read_with(cx, |session, _| {
                        session.cached_file(id).map(std::path::Path::to_owned)
                    })
                    .ok()
                    .flatten()
                    .filter(|path| path.exists())
            };
            let thumb = file.thumbnail();
            let picture =
                cached(&file.id).or_else(|| thumb.as_ref().and_then(|thumb| cached(&thumb.id)));
            // The spacer is real text in the transcript's own font, which is
            // the only way to land the picture exactly under the body column
            // whatever font the reader has set.
            let style = cx.editor_style.text.clone();
            let box_height = cx.line_height * rows as f32;
            let box_width = cx.line_height * (IMAGE_COLUMNS as f32 * CELL_ASPECT);
            // The picture's own size inside the box, spelled out rather than
            // left to the element: the editor measures a block and resizes
            // it to what it drew, so a thumbnail allowed to be its own tiny
            // self would shrink the box and move every row under it.
            let (width, height) = drawn_size(&file, box_width, box_height);
            let inside = match picture {
                Some(path) => gpui::img(path).w(width).h(height).into_any_element(),
                // Nothing cached yet, not even the thumbnail: an empty box
                // of the right size, which still holds the rows below it.
                None => div()
                    .w(width)
                    .h(height)
                    .bg(cx.app.theme().colors().element_background)
                    .into_any_element(),
            };
            div()
                .flex()
                .items_start()
                .h(box_height)
                .font_family(style.font_family.clone())
                .text_size(style.font_size)
                .child(" ".repeat(BODY_INDENT))
                .child(
                    div()
                        .id(("slack-image", line))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let file = file.clone();
                            let _ = view.update(cx, |_, cx| cx.emit(Event::OpenFile(file)));
                        })
                        .child(inside),
                )
                .into_any_element()
        }),
        priority: 0,
    }
}

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
        lines.push(LineMeta {
            thread,
            file: None,
            link: None,
        });
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

    let (said, chrome) = model.render_parts(message);
    let said = said.trim_end().replace('\n', &format!("\n{indent}"));
    let links = crate::block::links(&message.blocks, &message.text, &message.attachments);
    push_body(&mut spans, &said, model, &links, &message.files);
    if message.edited {
        // The reader is told what they are looking at is not what was sent.
        spans.push(Span::styled(" (edited)", Class::Muted));
    }
    // The time trails the words rather than heading them: it is the least
    // of what the reader came for. It trails the words only: what the
    // renderer hangs under a message (a card, a file, a picture) was not
    // said at a time of its own.
    spans.push(Span::styled(format!("  {}", clock_time(at)), Class::Time));
    spans.push(Span::plain("\n"));
    // A file's line is the one that reads as the file: `enter` there opens
    // it rather than the thread.
    let meta = |line: &str| LineMeta {
        thread: thread.clone(),
        file: message
            .files
            .iter()
            .find(|file| line.trim() == file.line())
            .cloned(),
        link: link_on(line, &links),
    };
    lines.extend(said.split('\n').map(&meta));
    if !chrome.is_empty() {
        let text = chrome
            .iter()
            .map(|line| format!("{indent}{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        push_body(&mut spans, &text, model, &links, &message.files);
        spans.push(Span::plain("\n"));
        lines.extend(text.split('\n').map(&meta));
    }

    if !message.reactions.is_empty() {
        spans.push(Span::plain(indent.clone()));
        push_reactions(&mut spans, message, model);
        lines.push(LineMeta {
            thread: thread.clone(),
            file: None,
            link: None,
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
            link: None,
        });
    }
    // The thread under a message is one line, not a fold-out: the reader
    // sees that it exists and opens it with `enter`.
    if !in_thread && message.reply_count > 0 {
        spans.push(Span::styled(
            format!("{indent}{}\n", replies_line(message)),
            Class::Topic,
        ));
        lines.push(LineMeta {
            thread,
            file: None,
            link: None,
        });
    }
    item(Row::Message(message.ts.clone()), spans, lines)
}

/// The composer after a refused send: the words that did not go out, and
/// under them whatever the reader has typed since. Which of the two they
/// want is theirs to decide; neither is thrown away to make room.
fn restored_compose(refused: String, held: &str) -> String {
    match held.trim().is_empty() {
        true => refused,
        false => format!("{refused}\n{held}"),
    }
}

/// A message still on its way out: the whole line goes muted, so the reader
/// can tell what has landed from what has not without a marker to decode.
fn mark_pending(item: &mut Rendered) {
    item.styles = vec![(Class::Muted, 0..item.text.trim_end_matches('\n').len())];
}

/// Tints the message a deal is about. The trailing newline is left out: it
/// is the gap to the next message, and tinting it would draw an empty band
/// under the card.
fn mark_dealt(item: &mut Rendered) {
    item.backgrounds
        .push((Class::Dealt, 0..item.text.trim_end_matches('\n').len()));
}

/// The break between days, which is the only separator the transcript has.
fn day_item(at: i64) -> Rendered {
    day_rule(day_label(at))
}

/// The oldest message the reader has not seen, out of the run on screen.
/// Recomputed on every refresh: a page of older messages landing above can
/// carry unread ones with it, and the rule belongs over the oldest of them.
fn first_unread(messages: &[Message], from: &Ts) -> Option<Ts> {
    messages
        .iter()
        .find(|message| message.ts.is_newer_than(from))
        .map(|message| message.ts.clone())
}

/// The unread rule. It reads as unread rather than as chrome: a day break
/// is where the reader is in the week, this is where they stopped.
fn unread_rule() -> Rendered {
    muted_item(Row::Unread, "── new ──\n", Class::Unread)
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
    let backgrounds = unfurl_ranges(&text);
    Item::new(key, text)
        .with_styles(styles)
        .with_backgrounds(backgrounds)
        .with_lines(lines)
}

/// The runs of lines an unfurl covers, each one tinted so the card reads as
/// a box rather than as a bar beside loose lines.
fn unfurl_ranges(text: &str) -> Vec<(Class, Range<usize>)> {
    let mut ranges: Vec<(Class, Range<usize>)> = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line
            .trim_start()
            .starts_with(crate::block::UNFURL_BAR.trim_end())
        {
            // One tint per row, each starting at the bar: a range spanning
            // the newline between two rows would tint the indent of the
            // second and leave the first starting a column further in, so
            // the card's left edge came out ragged.
            let start = offset + (line.len() - line.trim_start().len());
            ranges.push((
                Class::Unfurl,
                start..offset + line.trim_end_matches('\n').len(),
            ));
        }
        offset += line.len();
    }
    ranges
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
/// The link a line stands for: the first whose label the line carries, so
/// `enter` opens what the reader is looking at.
fn link_on(line: &str, links: &[crate::block::Link]) -> Option<String> {
    links
        .iter()
        .find(|link| line.contains(&link.label))
        .map(|link| link.url.clone())
}

fn push_body(
    spans: &mut Vec<Span>,
    body: &str,
    model: &Model,
    links: &[crate::block::Link],
    files: &[FileSummary],
) {
    let mut marked: Vec<(Range<usize>, Class)> = Vec::new();
    // Link labels, in the order they were rendered, so the same word used
    // twice colours the occurrence it belongs to.
    let mut from = 0;
    for link in links {
        let Some(at) = body[from..].find(&link.label) else {
            continue;
        };
        let start = from + at;
        from = start + link.label.len();
        marked.push((start..from, Class::Link));
    }
    // Lines the renderer added rather than the author: an attachment's card,
    // preview or app card alike. They read as chrome, not as speech.
    let mut offset = 0;
    let mut in_unfurl = false;
    for line in body.split('\n') {
        let trimmed = line.trim_start();
        let start = offset + (line.len() - trimmed.len());
        // A file's name and size are a caption, not something anyone said:
        // muted like a timestamp, so the picture under it is what the eye
        // lands on.
        if files.iter().any(|file| trimmed == file.line()) {
            marked.push((start..offset + line.len(), Class::Muted));
        }
        if trimmed.starts_with(crate::block::UNFURL_BAR.trim_end()) {
            // The bar is the card's edge, the first line names the page:
            // both read as the link. What follows is the page's own words.
            let bar = start + crate::block::UNFURL_BAR.len();
            marked.push((
                start..match in_unfurl {
                    true => bar.min(offset + line.len()),
                    false => offset + line.len(),
                },
                Class::Link,
            ));
            in_unfurl = true;
        } else {
            in_unfurl = false;
        }
        offset += line.len() + 1;
    }
    // Slack's emphasis keeps its markers in the text, so the style is the
    // only thing that tells the reader it is emphasis.
    for (kind, range) in crate::block::emphasis(body) {
        marked.push((
            range,
            match kind {
                crate::block::Emphasis::Bold => Class::Bold,
                crate::block::Emphasis::Italic => Class::Italic,
                crate::block::Emphasis::Struck => Class::Struck,
            },
        ));
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
        // One page per frame, which is what makes it one page per keypress:
        // a motion raises a scroll event and a selection event, and a
        // conversation opening onto a gap raises neither.
        let (position, screen) = self.editor.update(cx, |editor, cx| {
            (
                editor.scroll_position(cx).y,
                editor.visible_line_count().unwrap_or(0.0),
            )
        });
        self.fill(position, screen, cx);
        div()
            .id("rho-slack-conversation")
            .key_context("RhoSlackConversation")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            // A file dropped on the conversation is an attachment for the
            // next message, the same as pasting one.
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                    let Some(path) = paths.paths().first().cloned() else {
                        return;
                    };
                    match this.attach_path(&path, cx) {
                        Ok(_) => {}
                        Err(error) => cx.emit(Event::AttachFailed(format!("{error:#}"))),
                    }
                }),
            )
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
    #[test]
    fn a_chip_reads_like_the_file_line_it_becomes() {
        let waiting = Attached {
            name: "image.png".to_owned(),
            bytes: std::sync::Arc::new(vec![0; 327_680]),
        };
        assert_eq!(waiting.line(), "image.png · 320 KB");
    }

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
        assert_eq!(
            fill.wants(Some(Want::Older)),
            Some(Want::Older),
            "opening onto a gap asks once"
        );
        assert_eq!(
            fill.wants(Some(Want::Older)),
            None,
            "the page landing does not buy another"
        );
        fill.user_moved();
        assert_eq!(
            fill.wants(Some(Want::Older)),
            Some(Want::Older),
            "the reader scrolling buys one more"
        );
        assert_eq!(fill.wants(Some(Want::Older)), None, "and only one");
        let mut fill = Fill::default();
        assert_eq!(
            fill.wants(None),
            None,
            "a reader away from the top asks for nothing"
        );
        assert_eq!(
            fill.wants(Some(Want::Older)),
            Some(Want::Older),
            "and has spent nothing"
        );
    }

    #[test]
    fn a_hole_is_filled_forward_and_costs_the_same_one_page() {
        let mut fill = Fill::default();
        let hole = Want::Newer(Ts("2.0".into()));
        assert_eq!(
            fill.wants(Some(hole.clone())),
            None,
            "opening at the bottom of a run is not a request to walk forward"
        );
        fill.user_moved();
        assert_eq!(
            fill.wants(Some(hole.clone())),
            Some(hole.clone()),
            "a reader sitting on the hole asks for the page under it"
        );
        assert_eq!(
            fill.wants(Some(Want::Older)),
            None,
            "and that spends the action, whichever end the next one wants"
        );
        fill.user_moved();
        assert_eq!(
            fill.wants(Some(hole.clone())),
            Some(hole),
            "moving buys one"
        );
        let mut fill = Fill::default();
        assert_eq!(
            fill.wants(None),
            None,
            "a view sitting on neither end asks for nothing"
        );
    }

    #[test]
    fn the_cursor_decides_which_end_is_asked_about() {
        let hole = [(93.0, Ts("2.0".into()))];
        assert_eq!(
            wanted_at(3.0, 38.0, 95.0, &hole),
            Some(Want::Newer(Ts("2.0".into()))),
            "a reader who pressed `G` is at the bottom, whatever the view has \
             caught up to yet"
        );
        assert_eq!(
            wanted_at(60.0, 38.0, 61.0, &hole),
            Some(Want::Newer(Ts("2.0".into()))),
            "and a hole scrolled onto is asked about too"
        );
        assert_eq!(
            wanted_at(0.0, 38.0, 0.0, &hole),
            Some(Want::Older),
            "at the top it is history that is missing"
        );
        assert_eq!(
            wanted_at(60.0, 38.0, 61.0, &[]),
            None,
            "and in the middle of a run with no hole, nothing is"
        );
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
        // An app card is the same quote box as a preview: no stray dash in
        // the middle of the conversation.
        assert!(
            text.lines().any(|line| {
                line.trim_start() == format!("{}pipeline", crate::block::UNFURL_BAR)
            }),
            "{text}"
        );
        assert!(!text.contains("— pipeline"), "{text}");
        let _ = &styles;
        assert_eq!(lines.len(), text.matches('\n').count());
    }

    #[test]
    fn an_unfurl_reads_as_a_quote_box_and_enter_opens_it() {
        let preview = parsed(json!({
            "ts": "1700000120.0",
            "user": "U1",
            "text": "worth a read <https://example.com/post|the post>",
            "attachments": [{
                "is_msg_unfurl": true,
                "title": "Worth a read",
                "text": "the first line\n\nthe second line\nthe third line",
                "service_name": "example.com",
                "title_link": "https://example.com/post",
            }],
        }));
        let (text, styles, lines) = render_messages(&[preview.clone()], &model(), false);
        assert!(
            text.contains("\u{258e} Worth a read · example.com"),
            "the title names the page and the site says where it is: {text}"
        );
        assert!(
            text.contains("\u{258e} the second line") && !text.contains("the third line"),
            "two lines of someone else's page, no more: {text}"
        );
        assert!(
            !text.contains("\u{258e} \n"),
            "no blank lines inside the box: {text}"
        );
        let linked = classed(&text, &styles, Class::Link);
        assert!(
            linked.iter().any(|span| span.contains("Worth a read")),
            "the card's first line reads as the link: {linked:?}"
        );
        assert!(
            linked.iter().any(|span| span.contains("the post")),
            "so does the label in the body: {linked:?}"
        );
        assert!(
            lines
                .iter()
                .filter(|meta| meta.link.as_deref() == Some("https://example.com/post"))
                .count()
                >= 2,
            "every line of the card opens the page it stands for"
        );
        let item = message_item(&preview, &model(), false);
        let tints = item
            .backgrounds
            .iter()
            .filter(|(class, _)| *class == Class::Unfurl)
            .map(|(_, range)| item.text[range.clone()].to_owned())
            .collect::<Vec<_>>();
        assert_eq!(tints.len(), 3, "every row of the card is tinted: {tints:?}");
        assert!(
            tints
                .iter()
                .all(|row| row.starts_with(crate::block::UNFURL_BAR.trim_end())),
            "the card's left edge is the bar on every row: {tints:?}"
        );
    }

    #[test]
    fn the_time_trails_the_words_and_never_the_chrome_under_them() {
        let with_file = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "text": "here is the mock",
            "files": [{
                "id": "F1",
                "name": "image.png",
                "title": "image.png",
                "filetype": "png",
                "size": 225_280,
                "url_private": "https://files.example.com/image.png",
            }],
        }));
        let (text, _, _) = render_messages(&[with_file], &model(), false);
        let timed = text
            .lines()
            .filter(|line| line.contains(&clock_time(1_700_000_000)))
            .collect::<Vec<_>>();
        assert_eq!(timed.len(), 1, "one time, on one line: {text}");
        assert!(
            timed[0].contains("here is the mock"),
            "the time trails what was said: {text}"
        );
        assert!(
            text.lines().any(|line| line.trim() == "image.png · 220 KB"),
            "the file line carries no time of its own: {text}"
        );
    }

    #[test]
    fn a_pictures_box_is_asked_for_before_its_bytes_arrive() {
        let with_file = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "text": "here is the mock",
            "files": [{
                "id": "F1",
                "name": "image.png",
                "title": "image.png",
                "filetype": "png",
                "size": 225_280,
                "url_private": "https://files.example.com/image.png",
                "original_w": 1200,
                "original_h": 200,
            }],
        }));
        let item = message_item(&with_file, &model(), false);
        let boxes = image_boxes(&item);
        assert_eq!(boxes.len(), 1, "one box, under the line that names it");
        let (line, file) = boxes[0];
        assert_eq!(
            item.text.lines().nth(line as usize).map(str::trim),
            Some("image.png · 220 KB")
        );
        // Nothing here consulted the cache, which is why the arrival of the
        // bytes cannot change the height and cannot move the rows below.
        assert_eq!(file.image_rows(), 4);
    }

    #[test]
    fn a_file_line_is_a_muted_caption() {
        let with_file = parsed(json!({
            "ts": "1700000000.0",
            "user": "U1",
            "text": "here is the mock",
            "files": [{
                "id": "F1",
                "name": "image.png",
                "title": "image.png",
                "filetype": "png",
                "size": 225_280,
                "url_private": "https://files.example.com/image.png",
            }],
        }));
        let (text, styles, _) = render_messages(&[with_file], &model(), false);
        assert!(
            classed(&text, &styles, Class::Muted)
                .iter()
                .any(|span| span.trim() == "image.png · 220 KB"),
            "the name and size read as chrome, not as words someone said: {text}"
        );
    }

    #[test]
    fn mrkdwn_emphasis_keeps_its_markers_and_is_styled() {
        let (text, styles, _) = render_messages(
            &[message(
                "1700000000.0",
                None,
                "*bold*, _italic_, ~struck~, `inline code`",
            )],
            &model(),
            false,
        );
        assert!(text.contains("*bold*"), "the markers stay: {text}");
        assert_eq!(classed(&text, &styles, Class::Bold), vec!["*bold*"]);
        assert_eq!(classed(&text, &styles, Class::Italic), vec!["_italic_"]);
        assert_eq!(classed(&text, &styles, Class::Struck), vec!["~struck~"]);
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

    #[test]
    fn the_dealt_message_is_tinted_but_the_gap_under_it_is_not() {
        let mut item = message_item(
            &message("1700000000.0", None, "look at this"),
            &model(),
            false,
        );
        mark_dealt(&mut item);
        let (class, range) = item.backgrounds.last().cloned().unwrap();
        assert_eq!(class, Class::Dealt);
        assert_eq!(&item.text[range.clone()], item.text.trim_end_matches('\n'));
        assert!(
            item.text[range.end..]
                .chars()
                .all(|character| character == '\n'),
            "the tint must stop before the gap to the next message"
        );
    }

    #[test]
    fn a_message_on_its_way_out_reads_muted_from_end_to_end() {
        let sending = parsed(json!({
            "ts": "1700000000.0",
            "user": "ME",
            "text": "on its way",
        }));
        let mut item = message_item(&sending, &model(), false);
        assert!(
            item.styles.iter().any(|(class, _)| *class == Class::You),
            "a landed message names its sender"
        );
        mark_pending(&mut item);
        assert_eq!(
            item.styles
                .iter()
                .map(|(class, range)| (*class, item.text[range.clone()].to_owned()))
                .collect::<Vec<_>>(),
            vec![(Class::Muted, item.text.trim_end().to_owned())],
            "nothing about it reads as landed, not even the name"
        );
    }

    #[test]
    fn a_refused_message_goes_back_above_whatever_was_typed_since() {
        assert_eq!(restored_compose("refused".into(), "   "), "refused");
        assert_eq!(
            restored_compose("refused".into(), "typed since"),
            "refused\ntyped since",
            "neither the refused message nor the new one is dropped"
        );
    }

    #[test]
    fn the_unread_rule_sits_over_the_oldest_message_the_reader_has_not_seen() {
        let held = |timestamps: &[&str]| {
            timestamps
                .iter()
                .map(|ts| parsed(json!({"ts": ts, "user": "UD", "text": "said"})))
                .collect::<Vec<_>>()
        };
        let read = Ts("300.0".into());
        assert_eq!(
            first_unread(&held(&["100.0", "400.0", "500.0"]), &read),
            Some(Ts("400.0".into()))
        );
        // A page of older messages landing above carries unread ones with
        // it: the rule moves up to the oldest of them, not the one that
        // happened to be first on screen before.
        assert_eq!(
            first_unread(&held(&["100.0", "350.0", "400.0"]), &read),
            Some(Ts("350.0".into()))
        );
        assert_eq!(first_unread(&held(&["100.0", "200.0"]), &read), None);

        let rule = unread_rule();
        assert_eq!(rule.text, "── new ──\n");
        assert_eq!(
            rule.styles
                .iter()
                .map(|(class, _)| *class)
                .collect::<Vec<_>>(),
            vec![Class::Unread],
            "it reads as unread, not as chrome"
        );
    }
}
