//! A thread: an editable draft above read-only mail, newest first, in one
//! editor.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use editor::display_map::ToDisplayPoint as _;
use editor::scroll::Autoscroll;
use editor::{
    Anchor, Editor, EditorEvent, EditorMode, HighlightKey, SelectionEffects, SizingBehavior,
};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, HighlightStyle, KeyBinding, MouseButton,
    MouseDownEvent, Subscription, Task, TextStyleRefinement, Window, actions, div,
};
use language::{Buffer, BufferEvent, Capability, Point, ToPoint as _};
use multi_buffer::{MultiBuffer, ToPoint as _};
use rho_agent_types::{
    Address as Party, AgentId, ChatSeq, EndpointId, Message, MessageId, Revision, UnixMs, Voice,
};
use rho_dealer::facts::Seen;
use rho_dealer::{Address, Addressed as _, Card, Marks};
use rho_frame::surface::PhoneSend;
use rho_frame::thread::{self, OpenRow, QuoteSelection, SendMessage};
use rho_frame::{CardHere, Command, Layout, Spelling, Subject, Surface};
use rho_notes::NoteId;
use rho_transcript::{BlockLine, BlockSpec, Item, Transcript};
use rho_window::draft;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use super::sheet::{self, Heading, token};
use super::{ChatSystem, Event, Participant};
use crate::reading::body;
use crate::{HostChat, ThreadId, cards};

actions!(rho_private_reply, [ReplyPrivately, KeepPrivateReply]);
actions!(rho_chat, [GoToDraft, GoToOldestMessage]);

const PRIVATE_SCREEN: &str = "RhoPrivateReply";
const PRIVATE_KEY: &str = "alt-r";

pub static REPLY_PRIVATELY: Spelling = Spelling {
    name: "reply privately",
    aliases: &[
        ("private reply", 9),
        ("note on message", 7),
        ("annotate message", 5),
    ],
    argument: None,
};

pub static QUOTE_MESSAGE: Spelling = Spelling {
    name: "quote message",
    aliases: &[("quote", 9), ("cite", 6)],
    argument: None,
};

pub static WRITE_REPLY_AGAIN: Spelling = Spelling {
    name: "write reply again",
    aliases: &[
        ("retry reply", 8),
        ("resend reply", 8),
        ("edit refused reply", 6),
    ],
    argument: None,
};

pub static SEND_MESSAGE: Spelling = Spelling {
    name: "send message",
    aliases: &[("send", 9), ("reply", 6), ("post", 6)],
    argument: None,
};

pub static FORK_THREAD: Spelling = Spelling {
    name: "fork thread",
    aliases: &[
        ("tangent", 9),
        ("new tangent", 8),
        ("branch thread", 6),
        ("fork", 7),
    ],
    argument: None,
};

pub static OPEN_UNDER_CURSOR: Spelling = Spelling {
    name: "open under cursor",
    aliases: &[("follow link", 7), ("open link", 7), ("open", 4)],
    argument: None,
};
const PRIVATE_SEPARATOR: &str = "── private · only you see this · escape keeps it";

pub(super) fn bind(cx: &mut App) {
    let mut bindings = rho_frame::normal_contexts(thread::SCREEN, " && vim_operator == none")
        .map(|context| KeyBinding::new(PRIVATE_KEY, ReplyPrivately, Some(&context)))
        .to_vec();
    bindings.push(KeyBinding::new(
        "escape",
        KeepPrivateReply,
        Some(&format!("{PRIVATE_SCREEN} > Editor")),
    ));
    for context in rho_frame::normal_contexts(
        thread::SCREEN,
        " && !RhoPrivateReply && vim_operator == none && !showing_completions",
    ) {
        bindings.push(KeyBinding::new("g g", GoToDraft, Some(&context)));
        bindings.push(KeyBinding::new(
            "shift-g",
            GoToOldestMessage,
            Some(&context),
        ));
    }
    cx.bind_keys(bindings);
}

/// The one open private tail; its text lives only in the note editor,
/// never in the public draft or a host's outbox.
struct PrivateEdit {
    note: NoteId,
    mail: MessageId,
    editor: Entity<Editor>,
    block: BlockSpec,
    _edited: Subscription,
    _blur: Subscription,
}

/// This surface's highlight-key space, clear of the others'.
const KEY_BASE: usize = usize::MAX / 5;

/// What a row is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Crumb,
    Separator,
    /// The message the thread hangs under, and each under it.
    Message(MessageId),
    /// The replies under a message, folded.
    Fold(MessageId),
    EarlierFlashes(AgentId),
    /// The user's private reply under a message: a note about it, which
    /// only their devices hold.
    Private(NoteId),
    Pending(u64),
    PendingGap(u64),
    /// A push or fetch that waits for the user's word, by its request.
    Ask(u64),
    AskGap(u64),
    MailGap(MessageId),
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Class {
    Crumb,
    User,
    Time,
    Fold,
    Pending,
    Quote,
    /// The project's words, read or not: one calm voice.
    Agent,
    /// A private reply: the user's own, seen by no agent.
    Private,
    /// A web link in a message: Enter on it opens it as a page.
    Link,
}

impl Class {
    const COUNT: usize = 9;

    fn slot(self) -> usize {
        match self {
            Self::Crumb => 0,
            Self::User => 1,
            Self::Time => 2,
            Self::Fold => 3,
            Self::Pending => 4,
            Self::Quote => 5,
            // A later slot paints over an earlier one where both apply: a
            // link stays a link in any voice.
            Self::Agent => 6,
            Self::Private => 7,
            Self::Link => 8,
        }
    }
}

impl rho_transcript::Style for Class {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let _ = cx;
        let color = match self {
            Self::Crumb | Self::Time | Self::Pending | Self::Quote | Self::Private | Self::Fold => {
                token::quiet()
            }
            Self::User => token::marked(),
            Self::Link => token::answer(),
            Self::Agent => token::reading_text(),
        };
        HighlightStyle {
            color: Some(color),
            font_style: matches!(self, Self::Private).then_some(gpui::FontStyle::Italic),
            underline: matches!(self, Self::Link).then(|| gpui::UnderlineStyle {
                thickness: gpui::px(1.),
                color: Some(color.into()),
                wavy: false,
            }),
            ..HighlightStyle::default()
        }
    }
}

/// What a line opens.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Meta {
    None,
    /// A mail in this thread, including its opening mail.
    Mail(MessageId, Option<usize>),
    /// One quoted block, by first line in its mail's body.
    Quote(QuoteId, usize),
    EarlierFlashes(AgentId),
    /// A tangent below a mail.
    Thread(MessageId),
    Private {
        note: NoteId,
        mail: MessageId,
    },
    Open(Address),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct QuoteId {
    mail: MessageId,
    first_line: usize,
}

#[derive(Debug)]
enum InlineOpen {
    Flashes(AgentId),
    Quote(QuoteId),
    Link(url::Url),
}

/// What `enter` does, as [`ThreadView::enter_action`] decides it.
#[derive(Debug)]
enum Enter {
    Inline(InlineOpen),
    Private(MessageId, NoteId),
    Inspect(AgentId, Message),
    Open(Address),
}

impl Meta {
    fn mail(&self) -> Option<MessageId> {
        match *self {
            Self::Mail(id, _) => Some(id),
            Self::Quote(quote, _) => Some(quote.mail),
            _ => None,
        }
    }

    fn source_line(&self) -> Option<usize> {
        match self {
            Self::Mail(_, line) => *line,
            Self::Quote(_, line) => Some(*line),
            _ => None,
        }
    }
}

struct UserMailGutter;

pub struct ThreadView {
    system: Entity<ChatSystem>,
    thread: ThreadId,
    transcript: Transcript<Key, Class, Meta, UserMailGutter>,
    editor: Entity<Editor>,
    draft: Entity<Buffer>,
    /// The last caret in the draft, even while the user selects mail.
    draft_point: Option<Anchor>,
    private_edit: Option<PrivateEdit>,
    draft_subscription: Subscription,
    latest_project_activity: Option<ChatSeq>,
    /// How far the reader has read the thread: the newest log position of
    /// its messages the last time its top was in view. A verdict or a
    /// reply settles through this, never through what came while the
    /// reader was in the history below.
    seen_in_view: Option<ChatSeq>,
    /// The pushes and fetches that were waiting the last time the top of
    /// the thread was in view.
    seen_asks: BTreeSet<u64>,
    /// Resolve against the new transcript before inferring motion direction.
    last_point: Anchor,
    expanded: Expanded,
    /// Looks again when words on their way turn `quiet`.
    _slow_send: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

const DRAFT_TEXT: HighlightKey =
    HighlightKey::SyntaxTreeView(rho_window::style::PROMPT_DRAFT_HIGHLIGHT_KEY);

/// The draft's own bar, in the user's colour.
struct DraftGutter;

/// A draft in the user's voice: muted teal words and the teal bar beside
/// every line, with no placeholder when it is empty. In a thread row 1 is
/// the reply, and an idea's `to:`, `in:` and `on:` lines say what its
/// words are, so neither needs a word to say so.
pub(super) fn style_draft_in(editor: &mut Editor, range: Range<Anchor>, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let empty = range.start.to_point(&snapshot) == range.end.to_point(&snapshot);
    editor.highlight_text(
        DRAFT_TEXT,
        if empty {
            Vec::new()
        } else {
            vec![range.clone()]
        },
        HighlightStyle {
            color: Some(token::marked()),
            ..HighlightStyle::default()
        },
        cx,
    );
    editor.highlight_gutter::<DraftGutter>(vec![range], token::marked_bar_in, cx);
}

/// Where the reader left a thread whose screen closed, so that the next
/// open shows it as it was left when nothing new came (`chat.Thread.open`).
pub(super) struct Left {
    seen_in_view: Option<ChatSeq>,
    seen_asks: BTreeSet<u64>,
    /// The item the point was in, `None` for the draft; the line within it
    /// and the column.
    item: Option<Key>,
    line: u32,
    column: u32,
    /// How many rows below the top of the window the point was.
    rows_above: f64,
}

#[derive(Default)]
struct Expanded {
    quotes: HashSet<QuoteId>,
    flashes: HashSet<AgentId>,
}

/// New project posts count even when a sync batch ends with the user's
/// mail. A revision counts only if it is on the newest visible mail.
fn latest_project_activity(system: &ChatSystem, thread: ThreadId) -> Option<ChatSeq> {
    let chat = system.host_chat(thread.host)?;
    let mut newest = None;
    let mut project_post = None;
    for mail in chat
        .reading_order(thread.message)
        .filter(|mail| mail.voice != Voice::Flash)
    {
        if matches!(
            mail.revision,
            Revision::Deleted | Revision::TakenBack { .. }
        ) {
            continue;
        }
        newest = Some(mail);
        if mail.from != Party::User {
            project_post = Some(mail.id.0);
        }
    }
    newest
        .filter(|mail| mail.from != Party::User)
        .map(|mail| mail.version().0)
        .or(project_post)
}

/// The newest log position among the thread's messages: every message
/// and edit at or before it is in the document.
fn newest_version(system: &ChatSystem, thread: ThreadId) -> Option<ChatSeq> {
    system
        .host_chat(thread.host)?
        .reading_order(thread.message)
        .map(|mail| mail.version().0)
        .max()
}

/// The newest log position of a message or an edit in the thread: what
/// opens it at the top when the reader has not had it in view. A flash is
/// the now, not news.
fn newest_news(system: &ChatSystem, thread: ThreadId) -> Option<ChatSeq> {
    system
        .host_chat(thread.host)?
        .reading_order(thread.message)
        .filter(|mail| mail.voice != Voice::Flash)
        .map(|mail| mail.version().0)
        .max()
}

/// The pushes and fetches that wait in the thread now.
fn waiting_asks(system: &ChatSystem, thread: ThreadId, cx: &App) -> BTreeSet<u64> {
    system
        .asks_in(thread, cx)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

impl ThreadView {
    /// The view over `thread` on `system`, which is `entity` as the frame
    /// holds it. The system is leased by the caller, so it is read here
    /// directly and never through its entity.
    pub fn new(
        system: &ChatSystem,
        entity: Entity<ChatSystem>,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The messages are a document of Markdown blocks read by the same
        // pipeline the agent transcript uses.
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            // The whole thread is one document: its parse holds the frame
            // that made the text, so new words never draw the markup they
            // conceal and then wrap again without it.
            rho_window::markdown::configure_document(&mut buffer, cx);
            buffer
        });
        let draft = cx.new(|cx| Buffer::local(system.draft(thread.host, Some(thread.message)), cx));
        // The editor treats these excerpts as one document for navigation,
        // search and registers, but only the draft accepts edits.
        let multi_buffer = draft::document(&buffer, &draft, cx);
        let history = buffer.clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            rho_notes::sheet::take_gutter(&mut editor, cx);
            // A list item or a quote that wraps keeps its words on one left
            // edge, in the draft and the history alike.
            editor.set_hanging_indent(Some(rho_notes::sheet::hanging_indent), cx);
            draft::stick(&mut editor, "↑ new".into(), cx);
            let doc = editor.buffer().read(cx).snapshot(cx);
            style_draft_in(&mut editor, draft::range(&doc), cx);
            editor.disable_headers_for_buffers(
                [buffer.read(cx).remote_id(), draft.read(cx).remote_id()],
                cx,
            );
            editor.set_prepare_for_insert(
                Some(
                    move |editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>| {
                        let snapshot = editor.display_snapshot(cx);
                        let point = editor.selections.newest::<Point>(&snapshot).head();
                        let doc = snapshot.buffer_snapshot();
                        if let Some((buffer, _)) = doc.point_to_buffer_point(point)
                            && buffer.remote_id() == history.read(cx).remote_id()
                        {
                            let start = draft::range(doc).start;
                            editor.change_selections(
                                Default::default(),
                                window,
                                cx,
                                |selections| {
                                    selections.select_anchor_ranges([start..start]);
                                },
                            );
                        }
                    },
                ),
                cx,
            );
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&editor, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::SelectionsChanged { local: true }) {
                    let editor = editor.read(cx);
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let selection = editor.selections.newest_anchor();
                    let head = selection.head();
                    let row = head.to_point(&snapshot).row;
                    let previous = std::mem::replace(&mut this.last_point, head)
                        .to_point(&snapshot)
                        .row;
                    let between = |row| {
                        matches!(this.line_meta(row, cx), Some(Meta::None))
                            && snapshot.line_len(multi_buffer::MultiBufferRow(row)) == 0
                    };
                    if selection.start == selection.end && between(row) {
                        let forward =
                            || (row + 1..=snapshot.max_point().row).find(|&row| !between(row));
                        let backward = || (0..row).rev().find(|&row| !between(row));
                        let target = if row < previous {
                            backward().or_else(forward)
                        } else {
                            forward().or_else(backward)
                        };
                        if let Some(target) = target {
                            this.cursor_to_row(target, Autoscroll::fit(), cx);
                        }
                        return;
                    }
                    if snapshot
                        .anchor_to_buffer_anchor(head)
                        .is_some_and(|(_, buffer)| {
                            buffer.remote_id() == this.draft.read(cx).remote_id()
                        })
                    {
                        this.draft_point = Some(snapshot.anchor_before(head.to_point(&snapshot)));
                    }
                    if row != previous {
                        cx.notify();
                    }
                } else if matches!(event, EditorEvent::ScrollPositionChanged { .. }) {
                    this.note_top(cx);
                } else if matches!(event, EditorEvent::EditBlockedByReadOnly) {
                    this.system.read(cx).frame.clone().echo(
                        "this message cannot be edited · write in the draft",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }),
            cx.observe(&entity, |this, _, cx| this.refresh_mail(cx)),
            cx.subscribe(&entity, |this, _, event: &Event, cx| match event {
                Event::DraftSent(thread) if *thread == this.thread => this.finish_send(cx),
                _ => {}
            }),
        ];
        let draft_subscription = Self::subscribe_draft(&draft, cx);
        let left = system.left.get(&thread);
        let mut view = Self {
            system: entity,
            thread,
            transcript: Transcript::new(buffer),
            editor,
            draft,
            draft_point: None,
            private_edit: None,
            draft_subscription,
            latest_project_activity: latest_project_activity(system, thread),
            // What the reader had in view when they left the thread, or
            // nothing: its open decides where it shows.
            seen_in_view: left.map_or(Some(ChatSeq(0)), |left| left.seen_in_view),
            seen_asks: left.map(|left| left.seen_asks.clone()).unwrap_or_default(),
            last_point: Anchor::Min,
            expanded: Expanded::default(),
            _slow_send: None,
            _subscriptions: subscriptions,
        };
        view.transcript.set_gutter(token::marked_bar_in);
        view.transcript.attach(&view.editor, cx);
        let items = items(system, thread, &view.expanded, None, cx);
        view.transcript.reconcile(items, cx);
        let end = view.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let end = draft::range(display.buffer_snapshot()).end;
            editor.selections.change_with(&display, |selections| {
                selections.select_anchor_ranges([end..end]);
            });
            editor.selections.newest_anchor().head()
        });
        view.last_point = end;
        view.draft_point = Some(end);
        if let Some(left) = left {
            view.restore(left, window, cx);
        }
        // The next open of the thread finds it where the reader leaves it.
        cx.on_release(|view, cx| {
            let left = view.left(cx);
            let (system, thread) = (view.system.clone(), view.thread);
            cx.defer(move |cx| {
                system.update(cx, |system, _| {
                    system.left.insert(thread, left);
                });
            });
        })
        .detach();
        // The user's private replies come from the ledger, which is the
        // frame's and is leased while it opens this: the rows are read
        // again as soon as that is over, before the first draw. Not
        // `defer_in`: that runs inside the window's update, where the frame
        // cannot be read.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| this.refresh(cx));
        });
        view
    }

    /// When the draft is projected, new project mail is outside the
    /// ordinary viewport. The notice never changes the document or point.
    fn refresh_mail(&mut self, cx: &mut Context<Self>) {
        let newest = latest_project_activity(self.system.read(cx), self.thread);
        let arrived = newest > self.latest_project_activity;
        self.latest_project_activity = newest;
        if arrived && !self.top_in_view(cx) {
            self.editor
                .update(cx, |editor, cx| editor.mark_stuck_excerpt(cx));
        }
        self.refresh(cx);
        self.note_top(cx);
    }

    /// Whether the top of the thread is in view: the draft inline rather
    /// than stuck to the top edge, so new mail lands where the reader sees
    /// it. Read from the scroll position, which moves before the next
    /// layout says whether the draft is stuck.
    fn top_in_view(&self, cx: &mut App) -> bool {
        self.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let last = draft::range(display.buffer_snapshot())
                .end
                .to_display_point(&display)
                .row();
            editor.scroll_position(cx).y < f64::from(last.0 + 1)
        })
    }

    /// With the top in view, the reader has seen everything in the thread.
    fn note_top(&mut self, cx: &mut Context<Self>) {
        if self.top_in_view(cx) {
            self.seen_top(cx);
        }
    }

    fn seen_top(&mut self, cx: &App) {
        let system = self.system.read(cx);
        self.seen_in_view = newest_version(system, self.thread);
        self.seen_asks = waiting_asks(system, self.thread, cx);
    }

    /// Whether a message, an edit, or a push or fetch came that the reader
    /// has not had in view at the top of the thread.
    fn news(&self, cx: &App) -> bool {
        let system = self.system.read(cx);
        newest_news(system, self.thread) > self.seen_in_view
            || !waiting_asks(system, self.thread, cx).is_subset(&self.seen_asks)
    }

    /// `chat.Thread.open`, from the deal or by name: news opens the top,
    /// the draft and the newest item; otherwise the thread is as the
    /// reader left it. A remembered line opens on that line instead
    /// ([`Surface::show_search_match`]).
    fn open(&mut self, cx: &mut Context<Self>) {
        if self.news(cx) {
            self.open_at_draft(cx);
        }
    }

    /// Where the reader is, for the next open after the screen goes.
    fn left(&self, cx: &mut App) -> Left {
        let cursor = self.cursor(cx);
        let (item, start) = match self.on_draft(cx) {
            true => (None, Some(self.draft_start(cx).row)),
            false => {
                let item = self
                    .history_row(cursor.row, cx)
                    .and_then(|row| self.transcript.key_at_row(row, cx))
                    .cloned();
                let start = item.as_ref().and_then(|key| self.item_row(key, cx));
                (item, start)
            }
        };
        let rows_above = self.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let row = cursor.to_display_point(&display).row().0 as f64;
            row - editor.scroll_position(cx).y
        });
        let (item, line, column) = match start {
            Some(start) => (item, cursor.row - start, cursor.column),
            None => (None, 0, 0),
        };
        Left {
            seen_in_view: self.seen_in_view,
            seen_asks: self.seen_asks.clone(),
            item,
            line,
            column,
            rows_above,
        }
    }

    /// The point and the scroll where the reader left them, when the item
    /// they were in is still here; otherwise the view stays at the draft.
    fn restore(&mut self, left: &Left, window: &mut Window, cx: &mut Context<Self>) {
        let start = match &left.item {
            None => self.draft_start(cx).row,
            Some(key) => match self.item_row(key, cx) {
                Some(row) => row,
                None => return,
            },
        };
        let point = self.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let point = display.buffer_snapshot().clip_point(
                Point::new(start + left.line, left.column),
                editor::Bias::Left,
            );
            editor.selections.change_with(&display, |selections| {
                selections.select_ranges([point..point]);
            });
            let row = point.to_display_point(&display).row().0 as f64;
            editor.set_scroll_position(
                gpui::point(0., (row - left.rows_above).max(0.)),
                window,
                cx,
            );
            editor.selections.newest_anchor().head()
        });
        self.last_point = point;
        if self.on_draft(cx) {
            self.draft_point = Some(point);
        }
    }

    /// The draft is the first excerpt; a history edit cannot move it.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let items = items(
            self.system.read(cx),
            self.thread,
            &self.expanded,
            self.private_edit.as_ref(),
            cx,
        );
        self.transcript.reconcile(items, cx);
        self.schedule_slow_send(cx);
        cx.notify();
    }

    /// Words on their way turn `quiet` once they are `SLOW_SEND_MS` old:
    /// look again then.
    fn schedule_slow_send(&mut self, cx: &mut Context<Self>) {
        let now = UnixMs::now().0;
        let system = self.system.read(cx);
        let wait = system
            .local_under(self.thread.host, Some(self.thread.message))
            .into_iter()
            .filter(|outgoing| outgoing.state == crate::outbox::OutgoingState::Queued)
            .map(|outgoing| (outgoing.at.0 + SLOW_SEND_MS).saturating_sub(now))
            .filter(|wait| *wait > 0)
            .min();
        self._slow_send = wait.map(|wait| {
            let wait = std::time::Duration::from_millis(wait);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |view, cx| view.refresh(cx));
            })
        });
    }

    fn subscribe_draft(draft: &Entity<Buffer>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(draft, |view, draft, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                let text = draft.read(cx).text();
                view.style_draft(cx);
                let thread = view.thread;
                view.system.update(cx, |system, _| {
                    system.set_draft(thread.host, Some(thread.message), text);
                });
            }
        })
    }

    /// A sent draft is not the next draft's undo history.
    fn clear_draft(&mut self, cx: &mut Context<Self>) {
        let draft = cx.new(|cx| Buffer::local("", cx));
        draft::replace(&self.editor, &draft, cx);
        self.draft_subscription = Self::subscribe_draft(&draft, cx);
        self.draft = draft;
        self.draft_point = None;
        self.style_draft(cx);
    }

    fn style_draft(&self, cx: &mut App) {
        let range = draft::range(&self.document(cx));
        self.editor
            .update(cx, |editor, cx| style_draft_in(editor, range, cx));
    }

    fn cursor_row(&self, cx: &App) -> u32 {
        self.cursor(cx).row
    }

    fn cursor(&self, cx: &App) -> Point {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.selections.newest_anchor().head().to_point(&snapshot)
    }

    fn document(&self, cx: &App) -> multi_buffer::MultiBufferSnapshot {
        self.editor.read(cx).buffer().read(cx).snapshot(cx)
    }

    fn draft_start(&self, cx: &App) -> Point {
        let doc = self.document(cx);
        draft::range(&doc).start.to_point(&doc)
    }

    /// Translate the document's rows to the independent transcript buffer.
    fn history_row(&self, row: u32, cx: &App) -> Option<u32> {
        let doc = self.document(cx);
        let (buffer, point) = doc.point_to_buffer_point(Point::new(row, 0))?;
        (buffer.remote_id() == self.transcript.buffer().read(cx).remote_id()).then_some(point.row)
    }

    fn history_start_row(&self, cx: &App) -> u32 {
        draft::history_start(&self.document(cx)).row
    }

    fn line_meta(&self, row: u32, cx: &App) -> Option<&Meta> {
        self.transcript.line_meta(self.history_row(row, cx)?, cx)
    }

    /// The web link at a point in the transcript, if there is one.
    fn link_at(&self, cursor: Point, cx: &App) -> Option<url::Url> {
        let buffer = self.transcript.buffer().read(cx);
        let row = self.history_row(cursor.row, cx)?;
        let start = buffer.point_to_offset(Point::new(row, 0));
        let end = buffer.point_to_offset(Point::new(row, buffer.line_len(row)));
        let line: String = buffer.text_for_range(start..end).collect();
        let column = cursor.column as usize;
        links(&line)
            .into_iter()
            .find(|link| link.start <= column && column < link.end)
            .and_then(|link| url::Url::parse(&line[link]).ok())
    }

    fn item_row(&self, key: &Key, cx: &App) -> Option<u32> {
        let start = self.transcript.range_of(key)?.start;
        let buffer = self.transcript.buffer().read(cx);
        let offset = buffer.snapshot().offset_for_anchor(&start);
        Some(self.history_start_row(cx) + buffer.offset_to_point(offset).row)
    }

    fn cursor_to_item(&mut self, key: &Key, scroll: Autoscroll, cx: &mut Context<Self>) -> bool {
        let Some(row) = self.item_row(key, cx) else {
            return false;
        };
        self.cursor_to_row(row, scroll, cx);
        true
    }

    fn cursor_to_row(&mut self, row: u32, scroll: Autoscroll, cx: &mut Context<Self>) {
        let point = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            editor.request_autoscroll(scroll, cx);
            cx.notify();
            editor.selections.newest_anchor().head()
        });
        self.last_point = point;
        if self.on_draft(cx) {
            self.draft_point = Some(point);
        }
    }

    fn go_to_draft(&mut self, _: &GoToDraft, _: &mut Window, cx: &mut Context<Self>) {
        let start = self.draft_start(cx);
        self.cursor_to_row(start.row, Autoscroll::top().for_anchor(Anchor::Min), cx);
    }

    fn go_to_oldest_mail(&mut self, _: &GoToOldestMessage, _: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.system.read(cx).host_chat(self.thread.host) else {
            return;
        };
        let row = chat
            .reading_order(self.thread.message)
            .find_map(|mail| self.item_row(&Key::Message(mail.id), cx));
        if let Some(row) = row {
            self.cursor_to_row(row, Autoscroll::fit(), cx);
        }
    }

    /// The sent words leave the draft. A draft that was inline, even with
    /// its first lines above the window, shrinks to one line: the view
    /// stays at the top of the thread, with the empty draft and the sent
    /// message below it, rather than wherever the shrinking left the
    /// scroll. A stuck draft keeps the history row the reader was on.
    fn finish_send(&mut self, cx: &mut Context<Self>) {
        let inline = self.top_in_view(cx);
        self.clear_draft(cx);
        self.refresh(cx);

        let point = self.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let start = draft::range(display.buffer_snapshot()).start;
            editor.selections.change_with(&display, |selections| {
                selections.select_anchor_ranges([start..start]);
            });
            if inline {
                editor.request_autoscroll(Autoscroll::top().for_anchor(Anchor::Min), cx);
            }
            let point = editor.selections.newest_anchor().head();
            cx.notify();
            point
        });
        self.last_point = point;
        self.draft_point = Some(point);
    }

    /// An open of the thread, from the deal or by name, shows its top: the
    /// draft with the point where the user left it, then the newest mail.
    /// A private reply being written keeps the point.
    fn open_at_draft(&mut self, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        let left = self.draft_point;
        let point = self.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let point = left.unwrap_or_else(|| draft::range(display.buffer_snapshot()).start);
            editor.selections.change_with(&display, |selections| {
                selections.select_anchor_ranges([point..point]);
            });
            editor.request_autoscroll(Autoscroll::top().for_anchor(Anchor::Min), cx);
            cx.notify();
            editor.selections.newest_anchor().head()
        });
        self.last_point = point;
        self.draft_point = Some(point);
        self.seen_top(cx);
    }
}

/// The rows of `thread` as `system` holds it, below the draft, the age
/// each turn among them shows, and which of them are unanswered. What the
/// thread's agents do is the heading's to say ([`ThreadView::heading`]), not a
/// row's.
fn items(
    system: &ChatSystem,
    thread: ThreadId,
    expanded: &Expanded,
    private_edit: Option<&PrivateEdit>,
    cx: &App,
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = vec![separator()];
    items.extend(ask_items(system, thread, cx));
    items.extend(history_items(system, thread, expanded, private_edit, cx));
    items
}

/// The blank line between the draft and the newest item. Space alone sets
/// the draft apart: no line is drawn.
fn separator() -> Item<Key, Class, Meta> {
    gap(Key::Separator)
}

/// What the thread calls an agent on a line of its own: its short code,
/// `eng-q9tz`. An agent the host no longer describes, or one of a host
/// not attached, is called by the same short form of its id rather than
/// the whole of it.
fn agent_tag(system: &ChatSystem, host: EndpointId, agent: AgentId, cx: &App) -> String {
    let id = agent.encoded();
    system
        .participant(host, agent, cx)
        .map(|participant| participant.code)
        .filter(|code| *code != id)
        .unwrap_or_else(|| short_tag(&id))
}

/// An agent's name for a third voice: its label, or its tag when the
/// label is nothing but its id.
fn agent_name(system: &ChatSystem, host: EndpointId, agent: AgentId, cx: &App) -> String {
    let id = agent.encoded();
    system
        .agent_label(host, agent, cx)
        .filter(|label| *label != id && !label.trim().is_empty())
        .unwrap_or_else(|| agent_tag(system, host, agent, cx))
}

/// The short form the agents system gives an agent it knows nothing more
/// of: the default prefix and the first four characters of its id.
fn short_tag(id: &str) -> String {
    let end = id
        .char_indices()
        .nth(4)
        .map_or(id.len(), |(index, _)| index);
    format!("eng-{}", &id[..end])
}

/// Each push or fetch that waits in the thread, the oldest first, right
/// below the draft: what it would do, and the keys that answer it.
fn ask_items(system: &ChatSystem, thread: ThreadId, cx: &App) -> Vec<Item<Key, Class, Meta>> {
    let mut items = Vec::new();
    for (id, waiting) in system.asks_in(thread, cx) {
        let asker = waiting.ask.request.agent.map_or_else(String::new, |agent| {
            agent_name(system, thread.host, agent, cx)
        });
        let asked = thread::when(waiting.since().as_millisecond());
        let text = waiting.text(&asker, &asked);
        let mut styles = Vec::new();
        let mut start = 0;
        for line in text.split('\n') {
            let end = start + line.len();
            let class = match line == super::git::HINT {
                true => Class::Crumb,
                false => Class::Agent,
            };
            styles.push((class, start..end));
            start = end + 1;
        }
        let lines = vec![Meta::None; text.split('\n').count()];
        items.push(
            Item::new(Key::Ask(id), text)
                .with_styles(styles)
                .with_lines(lines),
        );
        items.push(gap(Key::AskGap(id)));
    }
    items
}

fn gap(key: Key) -> Item<Key, Class, Meta> {
    Item::new(key, "").with_lines(vec![Meta::None])
}

fn history_items(
    system: &ChatSystem,
    thread: ThreadId,
    expanded: &Expanded,
    private_edit: Option<&PrivateEdit>,
    cx: &App,
) -> Vec<Item<Key, Class, Meta>> {
    let host = thread.host;
    let Some(chat) = system.host_chat(host) else {
        let line = line(
            Key::Unavailable,
            "this host's messages are not held here",
            Class::Crumb,
        );
        return vec![line];
    };
    let Some(head) = chat.thread(thread.message) else {
        let line = line(Key::Unavailable, "this thread is gone", Class::Crumb);
        return vec![line];
    };
    let mut items = Vec::new();
    // The heading already names the project; the breadcrumb leads back to
    // the nearest real ancestor of a tangent.
    let first = usize::from(chat.is_project(head.path[0].0));
    let crumb = (head.path.len() > first + 1).then(|| {
        let above = &head.path[first..head.path.len() - 1];
        let text = above
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(" › ");
        let parent = ThreadId {
            host,
            message: above[above.len() - 1].0,
        };
        line(Key::Crumb, &text, Class::Crumb).with_lines(vec![Meta::Open(parent.address())])
    });
    // The frame is leased when it opens the surface. The first refresh
    // after the lease reads its ledger before drawing.
    let mut private = system
        .frame
        .read(cx, |frame, _| {
            private_items(frame.marks(), chat, host, thread.message, private_edit)
        })
        .unwrap_or_default();
    let order: Vec<&Message> = chat.reading_order(thread.message).collect();
    let voices = voices(
        &order,
        |agent| agent_tag(system, host, agent, cx),
        |agent| agent_name(system, host, agent, cx),
    );
    let mail_item = |message: &Message| {
        message_item(
            message,
            voices.get(&message.id).map(String::as_str),
            &expanded.quotes,
        )
    };
    items.extend(pending_items(system, host, thread.message, UnixMs::now()));
    let flashes = flash_groups(chat.reading_order(thread.message));
    let mut history: Vec<_> = chat.reading_order(thread.message).collect();
    history.reverse();
    for message in history {
        let id = message.id;
        let start = items.len();
        if let Some(agent) = flash_author(message) {
            let group = &flashes[&agent];
            let newest = group.latest == id;
            let open = expanded.flashes.contains(&agent);
            if newest || open {
                items.extend(mail_item(message));
            }
            if newest && group.earlier > 0 {
                let prefix = if open { "−" } else { "+" };
                items.push(
                    line(
                        Key::EarlierFlashes(agent),
                        &format!("{prefix}{} earlier", group.earlier),
                        Class::Time,
                    )
                    .with_lines(vec![Meta::EarlierFlashes(agent)]),
                );
            }
        } else {
            items.extend(mail_item(message));
        }
        items.extend(private.remove(&id).unwrap_or_default());
        // The opening mail's children are this history, not a tangent. A
        // mail with messages under it is the opening of one tangent: the
        // thread `fork thread` (or an agent's `chat.fork`) starts there.
        let replies = chat.children(Some(id));
        if id != thread.message && !replies.is_empty() {
            let (_, at) = chat.newest_in(id).unwrap_or_else(|| message.version());
            let text = format!(
                "  ↳ 1 tangent · {} · {} {} · {}",
                chat.name(id).unwrap_or_default(),
                replies.len(),
                if replies.len() == 1 {
                    "message"
                } else {
                    "messages"
                },
                thread::when(at.0 as i64),
            );
            items.push(line(Key::Fold(id), &text, Class::Fold).with_lines(vec![Meta::Thread(id)]));
        }
        if items.len() != start {
            items.push(gap(Key::MailGap(id)));
        }
    }
    items.extend(crumb);
    without_trailing_gap(items)
}

/// The live messages (`chat.the last message stays live`), each with its
/// agent: one an agent sent since the user's last message, and did not
/// take back.
fn live_messages<'a>(
    messages: impl Iterator<Item = &'a Message>,
) -> std::collections::BTreeMap<MessageId, AgentId> {
    let mut live = std::collections::BTreeMap::new();
    for message in messages.filter(|message| message.voice == Voice::Message) {
        match message.from {
            Party::User => live.clear(),
            Party::Agent(agent)
                if !matches!(
                    message.revision,
                    Revision::Deleted | Revision::TakenBack { .. }
                ) =>
            {
                live.insert(message.id, agent);
            }
            _ => {}
        }
    }
    live
}

/// The agents with a live message.
#[cfg(test)]
fn live_writers<'a>(messages: impl Iterator<Item = &'a Message>) -> BTreeSet<AgentId> {
    live_messages(messages).into_values().collect()
}

/// The quiet name line above each message that has one, by message. The
/// project speaks with one voice, so a message of its own has none, with
/// one exception: while more than one agent holds a live message, each of
/// those live messages, and only those, carries its agent's `tag`, as
/// two open asks would otherwise read as one. A message from an agent to
/// another is a third voice and is `named`.
fn voices(
    order: &[&Message],
    tag: impl Fn(AgentId) -> String,
    named: impl Fn(AgentId) -> String,
) -> HashMap<MessageId, String> {
    let live = live_messages(order.iter().copied());
    let several = live.values().collect::<BTreeSet<_>>().len() > 1;
    let mut voices = HashMap::new();
    for message in order {
        let voice = match (message.from, message.voice) {
            (Party::Agent(agent), Voice::ToAgent(_)) => named(agent),
            (Party::Agent(agent), Voice::Message) if several && live.contains_key(&message.id) => {
                tag(agent)
            }
            _ => continue,
        };
        voices.insert(message.id, voice);
    }
    voices
}

/// One group per author, independent of edit versions and display state.
struct FlashGroup {
    latest: MessageId,
    earlier: usize,
}

fn flash_author(message: &Message) -> Option<AgentId> {
    match message.from {
        Party::Agent(agent) if message.flash_deadline().is_some() => Some(agent),
        _ => None,
    }
}

fn flash_groups<'a>(messages: impl Iterator<Item = &'a Message>) -> HashMap<AgentId, FlashGroup> {
    let mut groups = HashMap::<AgentId, FlashGroup>::new();
    for message in messages {
        if let Some(agent) = flash_author(message) {
            groups
                .entry(agent)
                .and_modify(|group| {
                    group.latest = group.latest.max(message.id);
                    group.earlier += 1;
                })
                .or_insert(FlashGroup {
                    latest: message.id,
                    earlier: 0,
                });
        }
    }
    groups
}

fn without_trailing_gap(mut items: Vec<Item<Key, Class, Meta>>) -> Vec<Item<Key, Class, Meta>> {
    if items.last().is_some_and(|item| {
        matches!(
            item.key,
            Key::PendingGap(_) | Key::MailGap(_) | Key::AskGap(_)
        )
    }) {
        items.pop();
    }
    items
}

fn private_items(
    marks: &Marks,
    chat: &HostChat,
    host: EndpointId,
    head: MessageId,
    editing: Option<&PrivateEdit>,
) -> std::collections::HashMap<MessageId, Vec<Item<Key, Class, Meta>>> {
    let mut by_message = std::collections::HashMap::new();
    for &mail in std::iter::once(&head).chain(chat.children(Some(head))) {
        let address = ThreadId {
            host,
            message: mail,
        }
        .address();
        let mut replies: Vec<_> = marks
            .replies_to(&address)
            .into_iter()
            .filter_map(|(address, held)| {
                let note = NoteId::from_address(address)?;
                let time = held.written_ms.map(thread::when);
                let body = held.body.as_deref().unwrap_or_default();
                let block = editing
                    .filter(|open| open.note == note)
                    .map(|open| open.block.clone());
                Some(private_item(note, mail, time.as_deref(), body, block))
            })
            .collect();
        if let Some(open) = editing.filter(|open| open.mail == mail)
            && !replies
                .iter()
                .any(|item| item.key == Key::Private(open.note))
        {
            replies.push(private_item(
                open.note,
                mail,
                None,
                "",
                Some(open.block.clone()),
            ));
        }
        if !replies.is_empty() {
            by_message.insert(mail, replies);
        }
    }
    by_message
}

fn private_item(
    note: NoteId,
    mail: MessageId,
    time: Option<&str>,
    body: &str,
    block: Option<BlockSpec>,
) -> Item<Key, Class, Meta> {
    let (time, body) = if block.is_some() {
        ("now", PRIVATE_SEPARATOR)
    } else {
        (time.unwrap_or("now"), body.trim_end())
    };
    let text = format!("\n🔒 you · {time}\n{body}");
    let aside = 1..text.len();
    let lines = std::iter::once(Meta::None)
        .chain(text.lines().skip(1).map(|_| Meta::Private { note, mail }))
        .collect();
    Item::new(Key::Private(note), text)
        .with_styles(vec![(Class::Private, aside)])
        .with_lines(lines)
        .with_blocks(block.into_iter().collect())
}

/// How long words on their way read as sent before they turn `quiet`
/// (`design.slow send`): a host that takes them in time never shows the
/// state.
const SLOW_SEND_MS: u64 = 2_000;

/// The user's messages into `parent` that no host has logged yet, newest
/// first.
fn pending_items(
    system: &ChatSystem,
    host: EndpointId,
    parent: MessageId,
    now: UnixMs,
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = Vec::new();
    let mut outgoing = system.local_under(host, Some(parent));
    outgoing.reverse();
    for outgoing in outgoing {
        let id = outgoing.at.0;
        let mut text = body(&outgoing.content);
        let len = text.len();
        // Words on their way read as sent, laid out as they will be once
        // the host has them, so their landing changes nothing. Only words
        // the host has not taken in `SLOW_SEND_MS` turn `quiet`. A refusal
        // is an answer, told in words.
        let styles = match &outgoing.state {
            crate::outbox::OutgoingState::Queued => {
                let slow = now.0.saturating_sub(outgoing.at.0) >= SLOW_SEND_MS;
                vec![(if slow { Class::Pending } else { Class::User }, 0..len)]
            }
            crate::outbox::OutgoingState::Refused { reason } => {
                let first = text.find('\n').unwrap_or(len);
                let tag = format!(" · not sent: {}", rho_agent_types::one_line(reason));
                text.insert_str(first, &tag);
                let end = first + tag.len();
                vec![
                    (Class::User, 0..first),
                    (Class::Pending, first..end),
                    (Class::User, end..text.len()),
                ]
            }
        };
        let lines = text.lines().count().max(1);
        let len = text.len();
        items.push(
            Item::new(Key::Pending(id), text)
                .with_styles(styles)
                .with_lines(vec![Meta::None; lines])
                .with_gutter(0..len),
        );
        items.push(gap(Key::PendingGap(id)));
    }
    items
}

impl ThreadView {
    fn on_draft(&self, cx: &App) -> bool {
        let doc = self.document(cx);
        let head = self.editor.read(cx).selections.newest_anchor().head();
        doc.anchor_to_buffer_anchor(head)
            .is_some_and(|(_, buffer)| buffer.remote_id() == self.draft.read(cx).remote_id())
    }

    fn has_draft(&self, cx: &App) -> bool {
        !self.draft.read(cx).text().trim().is_empty()
    }

    /// Takes the reply the host refused back into the empty draft, to
    /// change and send again, as `write this idea again` does for an idea.
    /// A draft being written stays, and the refused reply with it.
    fn write_reply_again(&mut self, cx: &mut Context<Self>) {
        let Some((id, text)) = self.system.read(cx).refused_reply(self.thread) else {
            return;
        };
        if self.has_draft(cx) {
            self.system.read(cx).frame.clone().echo(
                "reply: send or clear the draft first",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        self.draft.update(cx, |draft, cx| draft.set_text(text, cx));
        self.system
            .update(cx, |system, cx| system.withdraw_refused(id, cx));
        self.style_draft(cx);
        let end = self.editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let end = draft::range(display.buffer_snapshot()).end;
            editor.selections.change_with(&display, |selections| {
                selections.select_anchor_ranges([end..end]);
            });
            editor.request_autoscroll(Autoscroll::top().for_anchor(Anchor::Min), cx);
            cx.notify();
            editor.selections.newest_anchor().head()
        });
        self.last_point = end;
        self.draft_point = Some(end);
    }

    fn echo_empty_draft(&self, cx: &mut App) {
        self.system.read(cx).frame.clone().echo(
            "send: nothing written",
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// The draft becomes the thread's newest message.
    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        let text = self.draft.read(cx).text();
        if text.trim().is_empty() {
            self.echo_empty_draft(cx);
            return;
        }
        let thread = self.thread;
        thread::enter_normal_mode(&self.editor, window, cx);
        let system = self.system.clone();
        self.system
            .read(cx)
            .frame
            .clone()
            .with_seen(thread.address(), cx, move |seen, cx| {
                system.update(cx, |system, cx| {
                    let settles = seen.and_then(|seen| {
                        cards::answered(system.host_chat(thread.host)?, thread.message, seen)
                    });
                    system.reply(thread, settles, text, cx);
                    system.set_draft(thread.host, Some(thread.message), String::new());
                    cx.emit(Event::DraftSent(thread));
                });
            });
    }

    /// Place the point at the tapped column, including at the end of a
    /// line. A tap on the separator starts at the beginning of the draft.
    fn tap_tail(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        if event.button != MouseButton::Left || event.modifiers.modified() {
            return;
        }
        let position = match draft::hit(
            self.editor.read(cx),
            self.transcript.buffer(),
            &self.draft,
            event.position,
            cx,
        ) {
            Some(draft::Hit::Text(anchor)) => Some(anchor),
            Some(draft::Hit::Separator) => None,
            None => return,
        };
        self.focus_draft(position, window, cx);
        window.prevent_default();
        cx.stop_propagation();
    }

    fn focus_draft(&self, position: Option<Anchor>, window: &mut Window, cx: &mut Context<Self>) {
        draft::focus(&self.editor, position, window, cx);
    }

    fn selection_range(&self, cx: &App) -> Range<Point> {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        selection.start.to_point(&snapshot)..selection.end.to_point(&snapshot)
    }

    /// The complete lines touched by the main selection, if all are in one
    /// mail.
    fn mail_selection(
        &self,
        range: Range<Point>,
        cx: &App,
    ) -> Option<std::ops::RangeInclusive<u32>> {
        let lines = selected_lines(range);
        let mail = self.line_meta(*lines.start(), cx)?.mail()?;
        lines
            .clone()
            .all(|row| self.line_meta(row, cx).and_then(Meta::mail) == Some(mail))
            .then_some(lines)
    }

    /// `>`: the selected lines of one read-only message, quoted into the
    /// draft. Where the user writes, in the draft or a private reply, `>`
    /// stays the editor's indent.
    fn quote_selection(&mut self, _: &QuoteSelection, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() || self.on_draft(cx) {
            cx.propagate();
            return;
        }
        let Some(lines) = self.mail_selection(self.selection_range(cx), cx) else {
            self.system.read(cx).frame.clone().echo(
                "select lines in one message to quote",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.quote(lines, window, cx);
    }

    fn quote(
        &mut self,
        lines: std::ops::RangeInclusive<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first) = self.history_row(*lines.start(), cx) else {
            return;
        };
        let Some(last) = self.history_row(*lines.end(), cx) else {
            return;
        };
        let history = self.transcript.buffer().read(cx);
        let start = history.point_to_offset(Point::new(first, 0));
        let end = history.point_to_offset(Point::new(last, history.line_len(last)));
        let selected = history.text_for_range(start..end).collect::<String>();
        let quote = quote_lines(&selected);
        let line_count = *lines.end() - *lines.start() + 1;
        let draft_start = self.draft_start(cx);

        thread::enter_normal_mode(&self.editor, window, cx);
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let caret = self
                .draft_point
                .map_or(draft_start, |point| point.to_point(&snapshot));
            let insert = Point::new(caret.row, 0);
            let below = Point::new(caret.row + line_count, caret.column);
            editor.finalize_last_transaction(cx);
            editor.transact(window, cx, |editor, window, cx| {
                editor.edit([(insert..insert, quote)], cx);
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::fit()),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([below..below]);
                    },
                );
            });
            editor.finalize_last_transaction(cx);
        });
        let confirmation = if line_count == 1 {
            "quoted 1 line".to_string()
        } else {
            format!("quoted {line_count} lines")
        };
        self.system
            .read(cx)
            .frame
            .clone()
            .echo(confirmation, StyleClass::SystemInfo, cx);
    }

    fn private_target(&self, cx: &App) -> Option<(MessageId, Option<NoteId>)> {
        let meta = self.line_meta(self.cursor_row(cx), cx)?;
        match *meta {
            Meta::Private { note, mail } => Some((mail, Some(note))),
            _ => meta.mail().map(|mail| (mail, None)),
        }
    }

    fn reply_privately(&mut self, _: &ReplyPrivately, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((mail, note)) = self.private_target(cx) {
            self.open_private(mail, note, cx);
        }
    }

    fn open_private(&mut self, mail: MessageId, note: Option<NoteId>, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        let subject = ThreadId {
            host: self.thread.host,
            message: mail,
        }
        .address();
        let this = cx.weak_entity();
        self.system
            .read(cx)
            .frame
            .clone()
            .with(cx, move |frame, window, cx| {
                let Some(this) = this.upgrade() else { return };
                let note = note
                    .or_else(|| {
                        frame
                            .marks()
                            .replies_to(&subject)
                            .into_iter()
                            .rev()
                            .find_map(|(address, _)| NoteId::from_address(address))
                    })
                    .unwrap_or_else(|| rho_notes::create_private(frame, &subject, cx));
                let notes = this.read(cx).system.read(cx).notes.clone();
                let text = notes.read(cx).text(frame.marks(), note);
                this.update(cx, |this, cx| {
                    this.start_private(mail, note, notes, text, window, cx)
                });
            });
    }

    fn start_private(
        &mut self,
        mail: MessageId,
        note: NoteId,
        notes: Entity<rho_notes::NoteSystem>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.private_edit.is_some() {
            return;
        }
        let body = cx.new(|cx| Buffer::local(&text, cx));
        let document = cx.new(|cx| MultiBuffer::singleton(body.clone(), cx));
        let input = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::SizeByContent,
                },
                document,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_mouse_click_selection_enabled(true, cx);
            editor.set_text_style_refinement(TextStyleRefinement {
                font_style: Some(gpui::FontStyle::Italic),
                color: Some(cx.theme().colors().text_muted.into()),
                ..Default::default()
            });
            editor
        });
        let saves = notes.clone();
        let edited = cx.subscribe(&body, move |_, body, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                let text = body.read(cx).text();
                saves.update(cx, |notes, cx| notes.edited(note, text, cx));
            }
        });
        let focus = input.focus_handle(cx);
        let blur = cx.on_blur(&focus, window, move |_, _, cx| {
            notes.update(cx, |notes, cx| notes.save(note, cx));
        });
        let block_input = input.clone();
        let block = BlockSpec {
            line: BlockLine::Below(2),
            height: 1,
            priority: 0,
            render: Arc::new(move |block| {
                div()
                    .key_context(PRIVATE_SCREEN)
                    .block_mouse_except_scroll()
                    .w(block.max_width)
                    .child(block_input.clone())
                    .into_any_element()
            }),
        };
        self.private_edit = Some(PrivateEdit {
            note,
            mail,
            editor: input.clone(),
            block,
            _edited: edited,
            _blur: blur,
        });

        // The frame was leased to read/create the note. Read the thread anew
        // outside that window update, then focus the editor at the end.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if let Some(row) = this.item_row(&Key::Private(note), cx) {
                    this.cursor_to_row(row + 3, Autoscroll::fit(), cx);
                }
                cx.with_window(this.editor.entity_id(), |window, cx| {
                    let snapshot = input.read(cx).buffer().read(cx).snapshot(cx);
                    let start = snapshot.anchor_after(Point::zero());
                    let end = snapshot.anchor_after(snapshot.max_point());
                    draft::focus_writing_at(&input, start, end, window, cx);
                });
            });
        });
    }

    fn keep_private(&mut self, _: &KeepPrivateReply, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.private_edit.take() else {
            return;
        };
        let note = edit.note;
        let notes = self.system.read(cx).notes.clone();
        notes.update(cx, |notes, cx| notes.save(note, cx));
        window.focus(&self.editor.focus_handle(cx), cx);
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if let Some(row) = this.item_row(&Key::Private(note), cx) {
                    this.cursor_to_row(row + 1, Autoscroll::fit(), cx);
                }
            });
        });
    }

    fn agent_mail<'a>(&self, row: u32, cx: &'a App) -> Option<(AgentId, &'a Message)> {
        self.authored_mail(self.line_meta(row, cx)?.mail()?, cx)
    }

    fn authored_mail<'a>(&self, id: MessageId, cx: &'a App) -> Option<(AgentId, &'a Message)> {
        let mail = self
            .system
            .read(cx)
            .host_chat(self.thread.host)?
            .message(id)?;
        let agent = match mail.from {
            Party::Agent(agent) | Party::System { agent, .. } => agent,
            Party::User | Party::StartFailed => return None,
        };
        Some((agent, mail))
    }

    fn chosen_work<'a>(&self, dealt: Option<Seen>, cx: &'a App) -> Option<(AgentId, &'a Message)> {
        let chat = self.system.read(cx).host_chat(self.thread.host)?;
        self.authored_mail(
            cards::answered(chat, self.thread.message, dealt?)?.message,
            cx,
        )
    }

    fn work_participants<'a>(
        &self,
        cx: &'a App,
    ) -> impl Iterator<Item = (AgentId, Participant)> + 'a {
        let system = self.system.read(cx);
        let thread = self.thread;
        system
            .host_chat(thread.host)
            .map(|chat| chat.members_under(Some(thread.message)))
            .unwrap_or_default()
            .into_iter()
            .filter_map(move |agent| Some((agent, system.participant(thread.host, agent, cx)?)))
    }

    fn draft_worker(&self, dealt: Option<Seen>, cx: &App) -> Option<AgentId> {
        if let Some((agent, _)) = self.chosen_work(dealt, cx) {
            return Some(agent);
        }
        let mut participants = self.work_participants(cx);
        let (agent, _) = participants.next()?;
        participants.next().is_none().then_some(agent)
    }

    fn inspect(&self, agent: AgentId, mail: Message, cx: &mut Context<Self>) {
        self.system
            .update(cx, |_, cx| cx.emit(Event::Inspect { mail, agent }));
    }

    fn long_press(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.private_edit.is_some() {
            return;
        }
        let editor = self.editor.read(cx);
        let Some((anchor, _, _)) =
            editor.buffer_location_for_window_position(position, editor::Bias::Left)
        else {
            return;
        };
        let row = anchor.to_point(&editor.buffer().read(cx).snapshot(cx)).row;
        if self.line_meta(row, cx).and_then(Meta::mail).is_some() {
            self.quote(row..=row, window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn tap_mail(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.private_edit.is_some() {
            return;
        }
        let editor = self.editor.read(cx);
        let Some(anchor) = rho_frame::phone_editor_tap(editor, event, window) else {
            return;
        };
        let cursor = anchor.to_point(&editor.buffer().read(cx).snapshot(cx));
        if let Some(Meta::Private { note, mail }) = self.line_meta(cursor.row, cx) {
            self.open_private(*mail, Some(*note), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if let Some(open) = self.inline_open(cursor, cx) {
            self.open_inline(open, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if let Some((agent, mail)) = self.agent_mail(cursor.row, cx)
            && self.item_row(&Key::Message(mail.id), cx) == Some(cursor.row)
        {
            self.inspect(agent, mail.clone(), cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    /// The fold under the point, or the link: a link first, as `enter`
    /// takes it. A folded quote or flash opens; an open one closes.
    fn inline_open(&self, cursor: Point, cx: &App) -> Option<InlineOpen> {
        if let Some(url) = self.link_at(cursor, cx) {
            return Some(InlineOpen::Link(url));
        }
        match self.line_meta(cursor.row, cx)? {
            Meta::Quote(quote, _) => Some(InlineOpen::Quote(*quote)),
            Meta::EarlierFlashes(agent) => Some(InlineOpen::Flashes(*agent)),
            _ => None,
        }
    }

    fn open_inline(&mut self, open: InlineOpen, cx: &mut Context<Self>) {
        match open {
            InlineOpen::Flashes(agent) => {
                if !self.expanded.flashes.remove(&agent) {
                    self.expanded.flashes.insert(agent);
                }
                self.refresh(cx);
                self.cursor_to_item(&Key::EarlierFlashes(agent), Autoscroll::fit(), cx);
            }
            InlineOpen::Quote(quote) => self.toggle_quote(quote, cx),
            InlineOpen::Link(url) => self.system.read(cx).frame.clone().open_link(url, cx),
        }
    }

    /// The character the cursor is on: Helix draws the cursor of a
    /// selection on its last character, and a bare point on the
    /// character after it.
    fn cursor_char(&self, cx: &App) -> Point {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let head = selection.head().to_point(&snapshot);
        if head <= selection.tail().to_point(&snapshot) {
            return head;
        }
        match head.column {
            0 => Point::new(
                head.row - 1,
                snapshot.line_len(multi_buffer::MultiBufferRow(head.row - 1)),
            ),
            column => snapshot.clip_point(Point::new(head.row, column - 1), editor::Bias::Left),
        }
    }

    /// What `enter` does on the character at `cursor` (`chat.Thread`'s
    /// keys), whatever the selection around it: a word that `w` or `/`
    /// selected still opens what it is in. In strict order: a private
    /// reply edits in place; a tangent line opens the tangent; a link
    /// opens as a page; a project's message or flash opens its work.
    /// Folded quotes and flashes open themselves.
    fn enter_action(&self, cursor: Point, cx: &App) -> Option<Enter> {
        let host = self.thread.host;
        let meta = self.line_meta(cursor.row, cx);
        match meta {
            Some(Meta::Private { note, mail }) => return Some(Enter::Private(*mail, *note)),
            Some(Meta::Thread(id)) if *id != self.thread.message => {
                return Some(Enter::Open(ThreadId { host, message: *id }.address()));
            }
            _ => {}
        }
        if let Some(open) = self.inline_open(cursor, cx) {
            return Some(Enter::Inline(open));
        }
        if let Some((agent, mail)) = self.agent_mail(cursor.row, cx) {
            return Some(Enter::Inspect(agent, mail.clone()));
        }
        match meta? {
            Meta::Mail(id, _) if *id != self.thread.message => {
                Some(Enter::Open(ThreadId { host, message: *id }.address()))
            }
            Meta::Open(address) => Some(Enter::Open(address.clone())),
            _ => None,
        }
    }

    fn open_row(&mut self, _: &OpenRow, _: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        match self.enter_action(self.cursor_char(cx), cx) {
            Some(Enter::Inline(open)) => self.open_inline(open, cx),
            Some(Enter::Private(mail, note)) => self.open_private(mail, Some(note), cx),
            Some(Enter::Inspect(agent, mail)) => self.inspect(agent, mail, cx),
            Some(Enter::Open(address)) => self.system.read(cx).frame.clone().open(address, cx),
            None => {}
        }
    }

    fn toggle_quote(&mut self, quote: QuoteId, cx: &mut Context<Self>) {
        if !self.expanded.quotes.remove(&quote) {
            self.expanded.quotes.insert(quote);
        }
        self.refresh(cx);
        if let Some(start) = self.item_row(&Key::Message(quote.mail), cx)
            && let Some(row) = (start..)
                .map_while(|row| self.line_meta(row, cx).map(|meta| (row, meta)))
                .take_while(|(_, meta)| meta.mail() == Some(quote.mail))
                .find_map(|(row, meta)| {
                    matches!(meta, Meta::Quote(id, _) if *id == quote).then_some(row)
                })
        {
            self.cursor_to_row(row, Autoscroll::fit(), cx);
        }
    }
}

/// Visual line selections end at the start of the next line; that line
/// isn't selected. A single empty line still counts as one selected line.
fn selected_lines(range: Range<Point>) -> std::ops::RangeInclusive<u32> {
    let end = if range.end.column == 0 && range.start.row < range.end.row {
        range.end.row - 1
    } else {
        range.end.row
    };
    range.start.row..=end
}

pub(super) fn quote_lines(text: &str) -> String {
    let mut quote = String::new();
    for line in text.split('\n') {
        quote.push_str("> ");
        quote.push_str(line);
        quote.push('\n');
    }
    quote
}

/// What a private reply or a tangent lands on.
fn line_subject(meta: Option<&Meta>, thread: ThreadId) -> Subject {
    let id = match meta {
        Some(Meta::Thread(id)) => Some(*id),
        _ => meta.and_then(Meta::mail),
    };
    match id {
        Some(id) if id != thread.message => Subject::Address(
            ThreadId {
                host: thread.host,
                message: id,
            }
            .address(),
        ),
        Some(_) => Subject::Own,
        None => Subject::None,
    }
}

fn line(key: Key, text: &str, class: Class) -> Item<Key, Class, Meta> {
    Item::new(key, text.to_owned())
        .with_styles(vec![(class, 0..text.len())])
        .with_lines(vec![Meta::None])
}

/// Where `text` holds a web link: each `http://` or `https://` up to the
/// next space, without the punctuation that closes a sentence around it.
fn links(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..].find("http") {
        let start = at + offset;
        let rest = &text[start..];
        let scheme = ["https://", "http://"]
            .into_iter()
            .find(|scheme| rest.starts_with(scheme));
        let word_start = text[..start]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric());
        let Some(scheme) = scheme.filter(|_| word_start) else {
            at = start + "http".len();
            continue;
        };
        let length = rest
            .find(|character: char| {
                character.is_whitespace() || character == '<' || character == '>'
            })
            .unwrap_or(rest.len());
        let link = rest[..length]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"', ')', ']', '}']);
        if link.len() > scheme.len() {
            found.push(start..start + link.len());
        }
        at = start + length.max(scheme.len());
    }
    found
}

fn message_item(
    message: &Message,
    third_voice: Option<&str>,
    expanded_quotes: &HashSet<QuoteId>,
) -> Option<Item<Key, Class, Meta>> {
    let tag = match message.revision {
        Revision::Original => "",
        Revision::Edited { .. } | Revision::Recovered { .. } => " · edited",
        Revision::TakenBack { .. } => " · taken back",
        Revision::Deleted => return None,
    };
    let body = body(&message.content);
    let mut text = String::new();
    let mut styles = Vec::new();
    let mut lines = Vec::new();
    if let Some(voice) = third_voice {
        text.push_str(voice);
        styles.push((Class::Crumb, 0..text.len()));
        lines.push(Meta::Mail(message.id, None));
    }
    // The project's words, in its one voice.
    let voice = match (message.from, message.voice) {
        (Party::User, _) => Class::User,
        (_, Voice::Flash) => Class::Time,
        _ => Class::Agent,
    };
    // The words as written: the editor wraps them at the measure.
    let source: Vec<&str> = body.lines().collect();
    let push = |text: &mut String, lines: &mut Vec<Meta>, line: &str, meta: Meta| {
        if !lines.is_empty() {
            text.push('\n');
        }
        text.push_str(line);
        lines.push(meta);
    };
    let mut at = 0;
    while at < source.len() {
        let line = source[at];
        let first_line = at;
        at += 1;
        // Where this line's words start, after the newline that ends the
        // line before.
        let start = text.len() + usize::from(!lines.is_empty());
        if line.starts_with("> ") {
            let quote = QuoteId {
                mail: message.id,
                first_line,
            };
            let expanded = expanded_quotes.contains(&quote);
            let mut quoted = vec![(first_line, line)];
            while source.get(at).is_some_and(|line| line.starts_with("> ")) {
                quoted.push((at, source[at]));
                at += 1;
            }
            let more = quoted.len() > 1;
            if !expanded {
                quoted.truncate(1);
            }
            for (source_line, quoted) in quoted {
                push(
                    &mut text,
                    &mut lines,
                    quoted,
                    Meta::Quote(quote, source_line),
                );
            }
            if more && !expanded {
                text.push_str(" …");
            }
            styles.push((Class::Quote, start..text.len()));
        } else {
            push(
                &mut text,
                &mut lines,
                line,
                Meta::Mail(message.id, Some(first_line)),
            );
            styles.push((voice, start..text.len()));
        }
    }
    if lines.is_empty() {
        return None;
    }
    // `· edited` or `· taken back` closes the prose, quietly.
    let end = text.len();
    text.push_str(tag);
    styles.push((Class::Crumb, end..text.len()));
    styles.extend(links(&text).into_iter().map(|link| (Class::Link, link)));
    let len = text.len();
    let item = Item::new(Key::Message(message.id), text)
        .with_styles(styles)
        .with_lines(lines);
    Some(if message.from == Party::User {
        item.with_gutter(0..len)
    } else {
        item
    })
}

impl Surface for ThreadView {
    fn title(&self, cx: &App) -> String {
        self.page_heading(cx).text()
    }

    fn draws_top_left(&self, _cx: &App) -> bool {
        true
    }

    fn show_search_match(
        &mut self,
        point: &rho_frame::SearchPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = match point {
            rho_frame::SearchPoint::Message { message, line } => {
                let Some(mail) = ThreadId::from_address(message) else {
                    return;
                };
                let Some(range) = self.transcript.range_of(&Key::Message(mail.message)) else {
                    return;
                };
                let buffer = self.transcript.buffer().read(cx).snapshot();
                let start = range.start.to_point(&buffer).row;
                let end = range.end.to_point(&buffer).row.saturating_sub(1);
                (start..=end)
                    .filter_map(|row| {
                        let source = self.transcript.line_meta(row, cx)?.source_line()?;
                        // Exact source lines win; for collapsed text prefer
                        // the preceding line if distances tie.
                        Some((source.abs_diff(*line as usize), source, row))
                    })
                    .min()
                    .map_or(start, |(_, _, row)| row)
                    + self.history_start_row(cx)
            }
            rho_frame::SearchPoint::Draft(line) => self.draft_start(cx).row.saturating_add(*line),
            _ => return,
        };
        thread::enter_normal_mode(&self.editor, window, cx);
        self.cursor_to_row(row, Autoscroll::center(), cx);
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(
            self.private_edit
                .as_ref()
                .map_or(&self.editor, |edit| &edit.editor)
                .clone(),
        )
    }

    /// A mail under the point anchors a tangent; the draft belongs to the
    /// whole thread rather than to any particular row.
    fn subject(&self, dealt: Option<Seen>, cx: &App) -> Subject {
        if self.on_draft(cx) {
            return Subject::Draft {
                worker: if self.private_edit.is_none() {
                    self.draft_worker(dealt, cx)
                } else {
                    None
                },
            };
        }
        line_subject(self.line_meta(self.cursor_row(cx), cx), self.thread)
    }

    /// The thread is the card wherever the point is in it.
    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Itself
    }

    /// The card as far as the reader has had the top of the thread in
    /// view: mail that came while they read the history below waits on.
    fn seen(&self, card: Seen, _cx: &App) -> Seen {
        match (card, self.seen_in_view) {
            (Seen::Topic(through), Some(seen)) => Seen::Topic(through.min(seen.0)),
            (card, _) => card,
        }
    }

    /// The page's marker: a push or fetch that waits, or what the
    /// thread's agents do. How long the thread has waited is not one: the
    /// newest turn says it right below.
    fn status(&self, cx: &App) -> Option<String> {
        self.page_heading(cx).marker.map(|(words, _)| words)
    }

    fn opened(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open(cx);
    }

    fn revisited(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open(cx);
    }

    fn show_card(&mut self, _card: &Card, _window: &mut Window, cx: &mut Context<Self>) {
        self.open(cx);
    }

    fn edits_at_point(&self, cx: &App) -> bool {
        self.private_edit.is_some() || self.on_draft(cx)
    }

    fn phone_tools(&self, _: bool, _: &App) -> Vec<rho_frame::PhoneTool> {
        if self.private_edit.is_none() {
            return Vec::new();
        }
        vec![rho_frame::PhoneTool::new("✓", "keep", |window, cx| {
            window.dispatch_action(Box::new(KeepPrivateReply), cx);
        })]
    }

    fn phone_send(&mut self, cx: &mut Context<Self>) -> Option<PhoneSend> {
        if self.private_edit.is_some() || !self.has_draft(cx) {
            return None;
        }
        let this = cx.entity();
        Some(Rc::new(move |window, cx| {
            this.update(cx, |view, cx| view.send(&SendMessage, window, cx));
        }))
    }

    fn draft(&self, cx: &App) -> Option<String> {
        let text = self.draft.read(cx).text();
        (!text.trim().is_empty()).then_some(text)
    }

    fn commands(
        &mut self,
        _: Option<Seen>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Command> {
        if self.private_edit.is_some() {
            return Vec::new();
        }
        let this = cx.entity();
        let has_draft = self.has_draft(cx);
        let send = this.clone();
        let mut commands = Vec::new();
        if self.private_target(cx).is_some() {
            let view = this.clone();
            commands.push(
                Command::simple(&REPLY_PRIVATELY, move |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.reply_privately(&ReplyPrivately, window, cx)
                    });
                })
                .bound_to(ReplyPrivately),
            );
        }
        if self
            .line_meta(self.cursor_row(cx), cx)
            .and_then(Meta::mail)
            .is_some()
        {
            let view = this.clone();
            commands.push(
                Command::simple(&QUOTE_MESSAGE, move |_, window, cx| {
                    view.update(cx, |view, cx| {
                        let row = view.cursor_row(cx);
                        let lines = view
                            .mail_selection(view.selection_range(cx), cx)
                            .unwrap_or(row..=row);
                        view.quote(lines, window, cx);
                    });
                })
                .bound_to(QuoteSelection),
            );
        }
        // A message under the point, other than the one this thread opens
        // with, can open a tangent of its own.
        if let Some(mail) = self
            .line_meta(self.cursor_row(cx), cx)
            .and_then(Meta::mail)
            .filter(|mail| *mail != self.thread.message)
        {
            let system = self.system.clone();
            let tangent = ThreadId {
                host: self.thread.host,
                message: mail,
            };
            commands.push(Command::simple(&FORK_THREAD, move |frame, window, cx| {
                system.update(cx, |system, _| system.start_tangent(tangent));
                frame.open(tangent.address(), window, cx);
            }));
        }
        if self.system.read(cx).refused_reply(self.thread).is_some() {
            let view = this.clone();
            commands.push(Command::simple(&WRITE_REPLY_AGAIN, move |_, _, cx| {
                view.update(cx, |view, cx| view.write_reply_again(cx));
            }));
        }
        if has_draft {
            commands.push(
                Command::simple(&SEND_MESSAGE, move |_, window, cx| {
                    send.update(cx, |view, cx| view.send(&SendMessage, window, cx));
                })
                .bound_to(SendMessage),
            );
        }
        // `open transcript` is the agents system's, on every screen.
        // The thumb's Open action follows Enter at the point.
        if matches!(
            self.enter_action(self.cursor_char(cx), cx),
            Some(Enter::Inline(_) | Enter::Open(_))
        ) {
            commands.push(
                Command::simple(&OPEN_UNDER_CURSOR, move |_, window, cx| {
                    this.update(cx, |view, cx| view.open_row(&OpenRow, window, cx));
                })
                .bound_to(OpenRow),
            );
        }
        commands
    }
}

impl ThreadView {
    /// What the top-left of the page says: the subject, the project, and
    /// the page's marker, with no breadcrumb, no model and no wait.
    fn page_heading(&self, cx: &App) -> Heading {
        let system = self.system.read(cx);
        let project = system.project_of(self.thread, cx);
        let subject = system
            .host_chat(self.thread.host)
            .and_then(|chat| chat.name(self.thread.message))
            .filter(|subject| !subject.trim().is_empty());
        let heading = match subject {
            Some(subject) => Heading::new(subject).context(project),
            None => Heading::new(project),
        };
        if !system.asks_in(self.thread, cx).is_empty() {
            return heading.marker("approve push", token::attention());
        }
        match system.work(self.thread) {
            Some(work) => heading.marker(work.words(), token::quiet()),
            None => heading,
        }
    }
}

impl Focusable for ThreadView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.private_edit
            .as_ref()
            .map_or(&self.editor, |edit| &edit.editor)
            .focus_handle(cx)
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = Layout::of(window.viewport_size().width);
        let context = thread::SCREEN;
        let view = cx.weak_entity();
        let gesture = (layout == Layout::Phone).then(move || {
            gpui::canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    window.on_mouse_event(
                        move |event: &gpui::LongPressEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Capture
                                && event.phase == gpui::TouchPhase::Started
                                && bounds.contains(&event.start_position)
                            {
                                let _ = view.update(cx, |view, cx| {
                                    view.long_press(event.start_position, window, cx)
                                });
                            }
                        },
                    );
                },
            )
            .absolute()
            .size_full()
        });
        let text = self
            .editor
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let heading = self.page_heading(cx);
        // Everything wraps at the prose measure (`design.prose measure`),
        // or at the editor's width on a narrower window.
        let prose = sheet::prose_width(&text, window);
        self.editor
            .update(cx, |editor, cx| editor.set_soft_wrap_limit(Some(prose), cx));
        let editor = div().size_full().child(self.editor.clone());
        let page = sheet::page_with_canopy(&heading, &text, editor, &self.editor, window, cx);
        div()
            .id("thread")
            .relative()
            .key_context(context)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::open_row))
            .on_action(cx.listener(Self::go_to_draft))
            .on_action(cx.listener(Self::go_to_oldest_mail))
            .on_action(cx.listener(Self::quote_selection))
            .on_action(cx.listener(Self::reply_privately))
            .on_action(cx.listener(Self::keep_private))
            // The editor consumes text mouse-down even when its native
            // selection is disabled. Capture the draft's pointer down
            // before it; GPUI also routes a completed tap this way.
            .capture_any_mouse_down(cx.listener(Self::tap_tail))
            .capture_any_mouse_down(cx.listener(Self::tap_mail))
            .child(page)
            .children(gesture)
    }
}

#[cfg(test)]
mod tests {
    use editor::Bias;
    use editor::display_map::{DisplayPoint, DisplayRow};
    use rho_agent_types::{AgentId, ChatSeq, ContentPart, UnixMs};

    use super::*;

    /// A long thread where agent `1` asked 3 to `last` in turn, opened by
    /// name in a frame, with `draft` written.
    fn long_thread(
        cx: &mut gpui::TestAppContext,
        last: u64,
        draft: &str,
    ) -> (
        gpui::WindowHandle<rho_frame::Frame>,
        Entity<ChatSystem>,
        tempfile::TempDir,
        ThreadId,
    ) {
        use rho_agent_types::{ChatEvent, ChatRecord};

        let host = EndpointId([3; 32]);
        let agent =
            Party::Agent(AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap());
        let thread = ThreadId {
            host,
            message: MessageId(ChatSeq(2)),
        };
        let mut log = HostChat::default();
        for seq in 1..=last {
            let (parent, from, text) = match seq {
                1 => (None, Party::User, "Clock".to_owned()),
                2 => (Some(1), Party::User, "Check the clocks.".to_owned()),
                n => (
                    Some(2),
                    agent,
                    format!("Reading {n}.\n\nThe clock drifted."),
                ),
            };
            log.tell(&ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(seq),
                event: ChatEvent::Posted {
                    parent: parent.map(|id| MessageId(ChatSeq(id))),
                    retry_of: None,
                    thread_reply: None,
                    from,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text { text }],
                },
            });
        }
        let (window, mail, db) = super::super::search_bench::fixture(cx, log);
        cx.update(|cx| {
            mail.update(cx, |system, _| {
                system.set_draft(host, Some(thread.message), draft.into());
            });
        });
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        cx.draw_window(window.into());
        (window, mail, db, thread)
    }

    /// A thread where agent `a` answered at length before the user's last
    /// reply, then `a` and `b` each sent a message: two live asks from two
    /// agents. Neither agent is described by the agents system, as with
    /// an agent its host no longer runs.
    fn two_voices(
        cx: &mut gpui::TestAppContext,
        second: bool,
    ) -> (
        gpui::WindowHandle<rho_frame::Frame>,
        tempfile::TempDir,
        [AgentId; 2],
    ) {
        use rho_agent_types::{ChatEvent, ChatRecord};

        let domain = rho_agent_types::AgentIdDomain(0);
        let a = AgentId::from_counter(1, &domain).unwrap();
        let b = AgentId::from_counter(2, &domain).unwrap();
        let thread = ThreadId {
            host: EndpointId([3; 32]),
            message: MessageId(ChatSeq(2)),
        };
        let mut posts = vec![
            (None, Party::User, "tide"),
            (Some(1), Party::User, "Fix the flaky clock test."),
            (Some(2), Party::Agent(a), "The clock drifts under load."),
            (Some(2), Party::Agent(a), "A per-test clock fixes it."),
            (Some(2), Party::User, "Run the suite once more."),
            (Some(2), Party::Agent(a), "Both clock tests pass 500/500."),
        ];
        if second {
            posts.push((Some(2), Party::Agent(b), "I pushed the legacy-clock flag."));
        }
        let mut log = HostChat::default();
        for (seq, (parent, from, text)) in (1..).zip(posts) {
            log.tell(&ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(1_760_000_000_000 + seq * 60_000),
                event: ChatEvent::Posted {
                    parent: parent.map(|id| MessageId(ChatSeq(id))),
                    retry_of: None,
                    thread_reply: None,
                    from,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text { text: text.into() }],
                },
            });
        }
        let (window, _mail, db) = super::super::search_bench::fixture(cx, log);
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        cx.draw_window(window.into());
        (window, db, [a, b])
    }

    /// The document's lines as drawn.
    fn display_lines(
        cx: &mut gpui::TestAppContext,
        window: gpui::WindowHandle<rho_frame::Frame>,
    ) -> Vec<String> {
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                let editor = frame.current_surface().editor(cx).unwrap();
                editor.update(cx, |editor, cx| {
                    let display = editor.display_snapshot(cx);
                    display.text().lines().map(str::to_owned).collect()
                })
            })
            .unwrap()
    }

    /// The WF4 capture showed `0lv8s73v8b56` over every project message of
    /// a long thread once a second agent spoke. The project speaks with
    /// one voice: an ordinary thread has no name line at all, and while
    /// two agents hold live asks only those two messages say which agent
    /// wrote them, by a short tag, never by the agent's whole id.
    #[gpui::test]
    fn messages_never_carry_an_agents_raw_id(cx: &mut gpui::TestAppContext) {
        let (window, _db, [a, b]) = two_voices(cx, false);
        let one = display_lines(cx, window);
        let ids = [a.encoded(), b.encoded()];
        let tags = [short_tag(&ids[0]), short_tag(&ids[1])];
        assert!(
            one.iter().all(|line| ids
                .iter()
                .chain(&tags)
                .all(|id| !line.contains(id.as_str()))),
            "one agent, no name lines: {one:#?}"
        );
        assert_eq!(one[0], "", "the empty draft has no placeholder: {one:#?}");

        let (window, _db, _) = two_voices(cx, true);
        let two = display_lines(cx, window);
        assert!(
            two.iter()
                .all(|line| ids.iter().all(|id| !line.contains(id.as_str()))),
            "no raw id anywhere: {two:#?}"
        );
        let named: Vec<_> = two
            .iter()
            .filter(|line| {
                tags.iter()
                    .any(|tag| line.trim_end().starts_with(tag.as_str()))
            })
            .map(|line| line.split_whitespace().next().unwrap().to_owned())
            .collect();
        assert_eq!(
            named,
            [tags[1].clone(), tags[0].clone()],
            "only the two live messages, newest first, are named: {two:#?}"
        );
        let older = two
            .iter()
            .position(|line| line.starts_with("A per-test clock fixes it."))
            .unwrap();
        assert!(
            !tags
                .iter()
                .any(|tag| two[older - 1].starts_with(tag.as_str())),
            "an answered message is the project's: {two:#?}"
        );
        // A thread shows no ages: the words are all there is.
        assert!(
            two.iter().any(|line| line == "Run the suite once more."),
            "{two:#?}"
        );
    }

    #[test]
    fn only_live_messages_of_several_agents_are_named() {
        let domain = rho_agent_types::AgentIdDomain(0);
        let a = AgentId::from_counter(1, &domain).unwrap();
        let b = AgentId::from_counter(2, &domain).unwrap();
        let said = |seq, from: Party| Message {
            id: MessageId(ChatSeq(seq)),
            ..mail(from, Revision::Original, "words")
        };
        let history = [
            said(3, Party::Agent(a)),
            said(4, Party::Agent(b)),
            said(5, Party::User),
            said(6, Party::Agent(a)),
            said(7, Party::Agent(b)),
            said(8, Party::Agent(a)),
        ];
        let order: Vec<_> = history.iter().collect();
        let tag = |agent: AgentId| short_tag(&agent.encoded());
        let named_lines = voices(&order, tag, |agent| format!("named {}", tag(agent)));
        let mut named: Vec<_> = named_lines.keys().map(|id| id.0.0).collect();
        named.sort();
        assert_eq!(
            named,
            [6, 7, 8],
            "messages before the user's reply are the project's"
        );
        assert_eq!(named_lines[&MessageId(ChatSeq(7))], tag(b));

        let alone = [
            said(5, Party::User),
            said(6, Party::Agent(a)),
            said(8, Party::Agent(a)),
        ];
        let order: Vec<_> = alone.iter().collect();
        assert!(
            voices_of(&order).is_empty(),
            "one live agent speaks as the project"
        );

        let id = a.encoded();
        assert_eq!(short_tag(&id), format!("eng-{}", &id[..4]));
        assert_ne!(short_tag(&id), id);

        fn voices_of(order: &[&Message]) -> HashMap<MessageId, String> {
            voices(order, |agent| agent.encoded(), |agent| agent.encoded())
        }
    }

    #[test]
    fn a_message_keeps_its_lines_as_written_for_the_editor_to_wrap() {
        let long = "word ".repeat(30);
        let code = format!("let x = \"{}\";", "c".repeat(100));
        let body = format!("{}\n```rust\n{code}\n```\nDone.", long.trim_end());
        let message = mail(
            Party::Agent(AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap()),
            Revision::Original,
            &body,
        );
        let item = message_item(&message, None, &HashSet::new()).unwrap();
        let lines: Vec<_> = item.text.lines().collect();
        // No newline is added: a long line stays one line, which the
        // editor soft-wraps at the measure the window gives it.
        assert_eq!(lines, body.lines().collect::<Vec<_>>());
        assert_eq!(
            item.lines.iter().map(Meta::source_line).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2), Some(3), Some(4)],
            "each line maps to the line it came from"
        );
        let link = "https://example.com/x";
        let ask = Message {
            content: vec![ContentPart::Text {
                text: format!("See {link} now."),
            }],
            ..message.clone()
        };
        let ask = message_item(&ask, None, &HashSet::new()).unwrap();
        let at = ask.text.find(link).unwrap();
        assert!(
            ask.styles
                .iter()
                .any(|(class, range)| *class == Class::Link && *range == (at..at + link.len())),
            "a link in the project's voice stays a link: {:?}",
            ask.styles
        );
    }

    /// Where the point is, how far the thread is scrolled, and whether
    /// the draft is stuck to the top edge, as drawn.
    fn drawn(
        cx: &mut gpui::TestAppContext,
        window: gpui::WindowHandle<rho_frame::Frame>,
    ) -> (Point, f64, bool) {
        cx.run_until_parked();
        cx.draw_window(window.into());
        window
            .update(cx, |frame, _, cx| {
                let editor = frame.current_surface().editor(cx).unwrap();
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let head = editor.selections.newest_anchor().head().to_point(&snapshot);
                    (
                        head,
                        editor.scroll_position(cx).y,
                        editor.stuck_excerpt_is_visible(),
                    )
                })
            })
            .unwrap()
    }

    fn scroll_to(
        cx: &mut gpui::TestAppContext,
        window: gpui::WindowHandle<rho_frame::Frame>,
        row: f64,
    ) {
        window
            .update(cx, |frame, window, cx| {
                let editor = frame.current_surface().editor(cx).unwrap();
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_position(gpui::point(0., row), window, cx)
                });
            })
            .unwrap();
    }

    /// The thread's card in the hand, by the mail it waits from.
    fn waits_from(
        cx: &mut gpui::TestAppContext,
        window: gpui::WindowHandle<rho_frame::Frame>,
        thread: ThreadId,
    ) -> Option<String> {
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .read(cx)
                .unwrap()
                .hand()
                .cards
                .iter()
                .find(|card| card.address == thread.address())
                .map(|card| card.cursor.clone())
        })
    }

    fn done(
        cx: &mut gpui::TestAppContext,
        window: gpui::WindowHandle<rho_frame::Frame>,
        thread: ThreadId,
    ) {
        window
            .update(cx, |frame, window, cx| {
                frame.verdict(
                    &thread.address(),
                    rho_frame::attention::Verdict::Done,
                    "done".to_owned(),
                    window,
                    cx,
                )
            })
            .unwrap();
    }

    /// A draft taller than the scroll it was written at shrinks to one
    /// line when sent. The view stays at the top, with the sent words
    /// right below the empty draft, rather than wherever the shrink left
    /// the scroll, where the stuck draft covered the sent words.
    #[gpui::test]
    fn sending_a_tall_draft_in_a_long_thread_keeps_the_top_in_view(cx: &mut gpui::TestAppContext) {
        use rho_agent_types::{ChatEvent, ChatRecord};

        let draft = (1..=8)
            .map(|n| format!("Line {n} of the answer."))
            .collect::<Vec<_>>()
            .join("\n");
        let (window, mail, _db, thread) = long_thread(cx, 40, &draft);
        // Writing scrolled the window into the draft, part way into a
        // line; it is still inline.
        scroll_to(cx, window, 5.5);
        let (_, scroll, stuck) = drawn(cx, window);
        assert_eq!((scroll, stuck), (5.5, false));

        cx.dispatch_action(window.into(), SendMessage);
        let sending = drawn(cx, window);
        assert_eq!(sending, (Point::new(0, 0), 0., false), "while sending");
        let doc = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, _, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.read(cx).buffer().read(cx).snapshot(cx).text()
                })
                .unwrap()
        };
        assert!(
            doc(cx).starts_with("\n\nLine 1 of the answer.\n"),
            "{}",
            doc(cx)
        );
        let while_sending = doc(cx);
        // Words on their way read as sent; only a slow host turns them
        // quiet, and neither changes the words.
        let pending = |cx: &mut gpui::TestAppContext, late: u64| {
            cx.update(|cx| {
                let system = mail.read(cx);
                let at = system.local_under(thread.host, Some(thread.message))[0].at;
                pending_items(system, thread.host, thread.message, UnixMs(at.0 + late))
            })
        };
        let classes = |items: Vec<Item<Key, Class, Meta>>| {
            let item = items.into_iter().next().unwrap();
            assert_eq!(item.text.trim_end(), draft);
            item.styles
                .into_iter()
                .map(|(class, range)| (class, range.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(classes(pending(cx, 0)), [(Class::User, draft.len())]);
        assert_eq!(
            classes(pending(cx, SLOW_SEND_MS - 1)),
            [(Class::User, draft.len())]
        );
        assert_eq!(
            classes(pending(cx, SLOW_SEND_MS)),
            [(Class::Pending, draft.len())]
        );

        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                system.outbox.next(thread.host).unwrap();
                system.outbox.answered(thread.host).unwrap();
                system
                    .logs
                    .get_mut(&thread.host)
                    .unwrap()
                    .tell(&ChatRecord {
                        seq: ChatSeq(41),
                        at: UnixMs(41),
                        event: ChatEvent::Posted {
                            parent: Some(thread.message),
                            retry_of: None,
                            thread_reply: None,
                            from: Party::User,
                            voice: Voice::Message,
                            content: vec![ContentPart::Text {
                                text: draft.clone(),
                            }],
                        },
                    });
                system.changed(thread.host, cx);
            })
        });
        assert_eq!(drawn(cx, window), sending, "when the sent mail lands");
        // The words do not change when they land, so no line wraps again.
        assert_eq!(doc(cx), while_sending);
    }

    /// A list item hangs under its words wherever it is, and neither a
    /// key in the draft nor a send rewraps the whole thread: a long
    /// thread's full rewrap runs in the background, and until it lands
    /// the new lines show unwrapped.
    #[gpui::test]
    fn writing_and_sending_rewrap_only_the_lines_they_touch(cx: &mut gpui::TestAppContext) {
        let item = format!("- {}", "words that run on ".repeat(8).trim_end());
        let (window, _mail, _db, _thread) = long_thread(cx, 40, &item);
        drawn(cx, window);
        let editor = window
            .update(cx, |frame, _, cx| {
                frame.current_surface().editor(cx).unwrap()
            })
            .unwrap();
        let rows = |cx: &mut gpui::TestAppContext| {
            editor.update(cx, |editor, cx| {
                editor.display_snapshot(cx).max_point().row().0 + 1
            })
        };
        // How far down the thread a wrap pass reached since the last ask.
        // The draft and the newest message are at the top, so a pass that
        // reaches the end laid out the whole thread again.
        let seen = std::cell::Cell::new(0);
        let rewrapped = |cx: &mut gpui::TestAppContext| {
            editor.update(cx, |editor, cx| {
                let records = editor.display_map.read(cx).wrap_sync_records(cx);
                let reach = records[seen.get()..]
                    .iter()
                    .map(|record| record.new_end)
                    .max()
                    .unwrap_or(0);
                seen.set(records.len());
                reach
            })
        };
        let draft_rows = |cx: &mut gpui::TestAppContext| {
            editor.update(cx, |editor, cx| {
                let display = editor.display_snapshot(cx);
                display
                    .text()
                    .lines()
                    .take_while(|line| !line.contains("sending"))
                    .skip_while(|line| !line.starts_with("- "))
                    .take(3)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
        };
        let total = rows(cx);
        assert!(
            total > 100,
            "a thread long enough to wrap in the background: {total}"
        );
        let hung = draft_rows(cx);
        assert!(hung[0].starts_with("- words"), "{hung:?}");
        assert!(
            hung[1].starts_with("  words") && hung[1].trim_start().len() + 2 == hung[1].len(),
            "the item's second row hangs under its words: {hung:?}"
        );
        rewrapped(cx);

        window
            .update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| editor.insert(" more", window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let typed = rewrapped(cx);
        assert!(typed < 20, "a key rewrapped down to row {typed} of {total}");

        cx.dispatch_action(window.into(), SendMessage);
        drawn(cx, window);
        let sent = rewrapped(cx);
        assert!(sent < 20, "a send rewrapped down to row {sent} of {total}");
        let doc = editor.update(cx, |editor, cx| editor.display_snapshot(cx).text());
        let at = doc
            .lines()
            .position(|line| line.starts_with("- words"))
            .unwrap();
        let next = doc.lines().nth(at + 1).unwrap();
        assert!(
            next.starts_with("  words"),
            "the sent item still hangs: {doc:?}"
        );
    }

    /// Mail that lands at the top while the reader is in the history
    /// below, a new ask or an edit of the newest, is out of sight: done
    /// settles what the reader had in view, and the arrival deals again.
    #[gpui::test]
    fn done_while_reading_history_leaves_what_came_meanwhile(cx: &mut gpui::TestAppContext) {
        use rho_agent_types::{ChatEvent, ChatRecord};

        let (window, mail, _db, thread) = long_thread(cx, 30, "");
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        assert!(waits_from(cx, window, thread).is_some());
        let arrive = |cx: &mut gpui::TestAppContext, event: ChatEvent, seq: u64| {
            cx.update(|cx| {
                mail.update(cx, |system, cx| {
                    system
                        .logs
                        .get_mut(&thread.host)
                        .unwrap()
                        .tell(&ChatRecord {
                            seq: ChatSeq(seq),
                            at: UnixMs(seq),
                            event,
                        });
                    system.changed(thread.host, cx);
                })
            });
        };
        let ask = |text: &str| ChatEvent::Posted {
            parent: Some(thread.message),
            retry_of: None,
            thread_reply: None,
            from: Party::Agent(agent),
            voice: Voice::Message,
            content: vec![ContentPart::Text { text: text.into() }],
        };

        // A new ask while the reader is below.
        scroll_to(cx, window, 30.);
        assert!(drawn(cx, window).2, "the draft is stuck while reading");
        arrive(cx, ask("Widen the retry window?"), 31);
        assert!(drawn(cx, window).2);
        done(cx, window, thread);
        let waiting = waits_from(cx, window, thread).expect("the new ask still waits");
        assert!(waiting.ends_with("/31"), "{waiting}");
        // The deal opens it again, at the top: now it is seen.
        assert_eq!(
            cx.update(|cx| window.read(cx).unwrap().current_address().clone()),
            thread.address()
        );
        assert_eq!(drawn(cx, window).1, 0.);
        done(cx, window, thread);
        assert_eq!(waits_from(cx, window, thread), None);

        // An edit of the newest ask while the reader is below.
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        drawn(cx, window);
        scroll_to(cx, window, 30.);
        assert!(drawn(cx, window).2);
        arrive(
            cx,
            ChatEvent::Revised {
                message: MessageId(ChatSeq(31)),
                author: agent,
                content: vec![ContentPart::Text {
                    text: "Widen the retry window to a minute?".into(),
                }],
            },
            32,
        );
        assert!(
            waits_from(cx, window, thread).is_some(),
            "changed words deal"
        );
        done(cx, window, thread);
        assert!(
            waits_from(cx, window, thread).is_some(),
            "the edit came out of sight"
        );

        // Back at the top, the reader has seen it, and done settles it.
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        scroll_to(cx, window, 30.);
        drawn(cx, window);
        cx.dispatch_action(window.into(), GoToDraft);
        assert_eq!(drawn(cx, window).1, 0.);
        scroll_to(cx, window, 30.);
        drawn(cx, window);
        done(cx, window, thread);
        assert_eq!(waits_from(cx, window, thread), None);
    }

    /// A reply the host refused goes back into the empty draft to change
    /// and send again, as a refused idea does, and leaves the outbox. A
    /// draft being written is never overwritten.
    #[gpui::test]
    fn a_refused_reply_can_be_written_again(cx: &mut gpui::TestAppContext) {
        let (window, mail, _db, thread) = answered_then_asked_again(cx, true);
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        sent_settles(cx, window, &mail, thread);
        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                system.refuse_reply(thread.host, "the thread moved".into(), cx)
            })
        });
        cx.run_until_parked();
        const AGAIN: &str = "write reply again";
        let doc = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, _, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.read(cx).buffer().read(cx).snapshot(cx).text()
                })
                .unwrap()
        };
        let write_draft = |cx: &mut gpui::TestAppContext, text: &str| {
            window
                .update(cx, |frame, _, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.update(cx, |editor, cx| {
                        let doc = editor.buffer().read(cx).snapshot(cx);
                        let draft = draft::range(&doc);
                        let draft = draft.start.to_point(&doc)..draft.end.to_point(&doc);
                        editor
                            .buffer()
                            .update(cx, |doc, cx| doc.edit([(draft, text.to_owned())], None, cx));
                    });
                })
                .unwrap();
            cx.run_until_parked();
        };
        // Runs the command, when the thread offers it.
        let again = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    let commands = frame.current_surface().commands(None, window, cx);
                    let command = commands
                        .into_iter()
                        .find(|command| command.name() == AGAIN)?;
                    command.run(None, frame, window, cx);
                    Some(())
                })
                .unwrap()
        };
        let refused = |cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                mail.read(cx)
                    .local_under(thread.host, Some(thread.message))
                    .iter()
                    .map(|outgoing| rho_agent_types::content_text(&outgoing.content))
                    .collect::<Vec<_>>()
            })
        };
        let saved = |cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                mail.read(cx)
                    .draft(thread.host, Some(thread.message))
                    .to_owned()
            })
        };
        assert!(
            doc(cx).starts_with("\n\nWiden it. · not sent: the thread moved\n"),
            "{}",
            doc(cx)
        );

        write_draft(cx, "Another thought.");
        assert_eq!(again(cx), Some(()));
        assert_eq!(
            saved(cx),
            "Another thought.",
            "the draft being written stays"
        );
        assert_eq!(refused(cx), ["Widen it."], "and so does the refused reply");

        write_draft(cx, "");
        assert_eq!(again(cx), Some(()));
        assert_eq!(saved(cx), "Widen it.");
        assert_eq!(refused(cx), Vec::<String>::new(), "it left the outbox");
        assert!(
            doc(cx).starts_with("Widen it.\n\nWiden the retry window?\n"),
            "{}",
            doc(cx)
        );
        assert_eq!(drawn(cx, window).0, Point::new(0, "Widen it.".len() as u32));
        assert_eq!(again(cx), None, "nothing else to write again");
    }

    #[gpui::test]
    fn sending_an_inline_reply_keeps_point_at_the_top_and_shows_the_newest_mail(
        cx: &mut gpui::TestAppContext,
    ) {
        check_thread(cx, CheckThread::Inline);
    }

    #[gpui::test]
    fn sending_a_stuck_draft_preserves_the_first_visible_history_row(
        cx: &mut gpui::TestAppContext,
    ) {
        check_thread(cx, CheckThread::Stuck);
    }

    #[gpui::test]
    fn a_thread_reply_has_no_addressee_or_retarget_command(cx: &mut gpui::TestAppContext) {
        check_thread(cx, CheckThread::ThreadReply);
    }

    /// A thread where agent `a` asked (3), the user answered it (4), and
    /// `a` asked again (5), with a draft ready to send.
    fn answered_then_asked_again(
        cx: &mut gpui::TestAppContext,
        asked_again: bool,
    ) -> (
        gpui::WindowHandle<rho_frame::Frame>,
        Entity<ChatSystem>,
        tempfile::TempDir,
        ThreadId,
    ) {
        use rho_agent_types::{ChatEvent, ChatRecord, MessageVersion, ThreadReply};

        let domain = rho_agent_types::AgentIdDomain(0);
        let a = AgentId::from_counter(1, &domain).unwrap();
        let root = MessageId(ChatSeq(1));
        let thread = ThreadId {
            host: EndpointId([3; 32]),
            message: MessageId(ChatSeq(2)),
        };
        let mut log = HostChat::default();
        let ask = |words: &'static str| (Party::Agent(a), words, Voice::Message, None);
        let mut posts = vec![
            (1, None, (Party::User, "Clock", Voice::Message, None)),
            (
                2,
                Some(root),
                (Party::User, "Check the clocks.", Voice::Message, None),
            ),
            (3, Some(thread.message), ask("Use clock A?")),
            (
                4,
                Some(thread.message),
                (
                    Party::User,
                    "Yes, clock A.",
                    Voice::Message,
                    Some(ThreadReply {
                        settles: Some(MessageVersion {
                            message: MessageId(ChatSeq(3)),
                            version: ChatSeq(3),
                        }),
                    }),
                ),
            ),
        ];
        if asked_again {
            posts.push((5, Some(thread.message), ask("Widen the retry window?")));
        }
        for (id, parent, (from, words, voice, thread_reply)) in posts {
            log.tell(&ChatRecord {
                seq: ChatSeq(id),
                at: UnixMs(id),
                event: ChatEvent::Posted {
                    parent,
                    from,
                    voice,
                    retry_of: None,
                    thread_reply,
                    content: vec![ContentPart::Text { text: words.into() }],
                },
            });
        }
        let (window, mail, db) = super::super::search_bench::fixture(cx, log);
        cx.update(|cx| {
            mail.update(cx, |system, _| {
                system.set_draft(thread.host, Some(thread.message), "Widen it.".into());
            });
        });
        (window, mail, db, thread)
    }

    fn sent_settles(
        cx: &mut gpui::TestAppContext,
        window: gpui::WindowHandle<rho_frame::Frame>,
        mail: &Entity<ChatSystem>,
        thread: ThreadId,
    ) -> Vec<rho_agent_types::MessageVersion> {
        cx.run_until_parked();
        cx.draw_window(window.into());
        cx.dispatch_action(window.into(), SendMessage);
        cx.run_until_parked();
        cx.update(|cx| {
            mail.update(cx, |system, _| {
                let (_, outgoing) = system.outbox.next(thread.host).expect("the draft was sent");
                assert_eq!(outgoing.parent, Some(thread.message));
                assert_eq!(
                    rho_agent_types::content_text(&outgoing.content),
                    "Widen it."
                );
                outgoing.settles.into_iter().collect()
            })
        })
    }

    #[gpui::test]
    fn a_reply_settles_the_waiting_card_of_a_thread_opened_by_name(cx: &mut gpui::TestAppContext) {
        let (window, mail, _db, thread) = answered_then_asked_again(cx, true);
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        assert_eq!(
            sent_settles(cx, window, &mail, thread),
            [rho_agent_types::MessageVersion {
                message: MessageId(ChatSeq(5)),
                version: ChatSeq(5),
            }],
            "the ask on screen goes, however the thread was reached"
        );
    }

    #[gpui::test]
    fn a_reply_settles_the_ask_on_screen_rather_than_the_one_first_dealt(
        cx: &mut gpui::TestAppContext,
    ) {
        use rho_agent_types::{ChatEvent, ChatRecord};
        use rho_dealer::Card;
        use rho_dealer::facts::Seen;
        use rho_dealer::rank::CardKind;

        // Dealt on the first ask; the answer to it and the next ask came
        // while the thread stayed on screen.
        let (window, mail, _db, thread) = answered_then_asked_again(cx, false);
        let card = Card {
            address: thread.address(),
            title: "Check the clocks.".into(),
            kind: CardKind::Waits,
            context: "clock@lab".into(),
            label: "waits".into(),
            priority: 1.,
            cursor: String::new(),
            seen: Seen::Mail {
                message: 3,
                version: 3,
            },
            skipped: false,
        };
        window
            .update(cx, |frame, window, cx| frame.open_card(card, window, cx))
            .unwrap();
        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
                system
                    .logs
                    .get_mut(&thread.host)
                    .unwrap()
                    .tell(&ChatRecord {
                        seq: ChatSeq(5),
                        at: UnixMs(5),
                        event: ChatEvent::Posted {
                            parent: Some(thread.message),
                            from: Party::Agent(agent),
                            voice: Voice::Message,
                            retry_of: None,
                            thread_reply: None,
                            content: vec![ContentPart::Text {
                                text: "Widen the retry window?".into(),
                            }],
                        },
                    });
                system.changed(thread.host, cx);
            })
        });
        assert_eq!(
            sent_settles(cx, window, &mail, thread),
            [rho_agent_types::MessageVersion {
                message: MessageId(ChatSeq(5)),
                version: ChatSeq(5),
            }],
            "the follow-up ask goes, not the one already answered"
        );
    }

    /// `chat.Thread.open`, one rule however the thread is reached: a
    /// remembered line from `Places` opens on that line; a message, an
    /// edit, or a push the reader has not had in view opens the top, the
    /// draft and the newest item; otherwise the thread is as the reader
    /// left it, by name, from the recent places or the deal, and after its
    /// screen closed too.
    #[gpui::test]
    fn a_thread_opens_on_a_match_on_news_or_as_left(cx: &mut gpui::TestAppContext) {
        use editor::SelectionEffects;
        use rho_agent_types::{ChatEvent, ChatRecord, ThreadWork};
        use rho_dealer::Card;
        use rho_dealer::facts::Seen;
        use rho_dealer::rank::CardKind;
        use rho_frame::System as _;

        let host = EndpointId([3; 32]);
        let worker = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let agent = Party::Agent(worker);
        let long = ThreadId {
            host,
            message: MessageId(ChatSeq(2)),
        };
        let other = ThreadId {
            host,
            message: MessageId(ChatSeq(3)),
        };
        let posted = |seq: u64, parent: u64, from: Party, voice: Voice, text: String| ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs::now(),
            event: ChatEvent::Posted {
                parent: Some(MessageId(ChatSeq(parent))),
                retry_of: None,
                thread_reply: None,
                from,
                voice,
                content: vec![ContentPart::Text { text }],
            },
        };
        let mut log = HostChat::default();
        log.tell(&ChatRecord {
            seq: ChatSeq(1),
            at: UnixMs(1),
            event: ChatEvent::Posted {
                parent: None,
                retry_of: None,
                thread_reply: None,
                from: Party::User,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: "Clock".into(),
                }],
            },
        });
        log.tell(&posted(
            2,
            1,
            Party::User,
            Voice::Message,
            "Check the clocks.".into(),
        ));
        log.tell(&posted(
            3,
            1,
            Party::User,
            Voice::Message,
            "Another thread.".into(),
        ));
        for n in 4..44 {
            log.tell(&posted(
                n,
                2,
                agent,
                Voice::Message,
                format!("Reading {n}.\n\nThe clock drifted.\n\nAgain."),
            ));
        }
        log.tell(&ChatRecord {
            seq: ChatSeq(44),
            at: UnixMs(44),
            event: ChatEvent::Joined {
                thread: long.message,
                agent: worker,
            },
        });
        let (window, mail, _db) = super::super::search_bench::fixture(cx, log);
        let lab = rho_agent_hosts::HostId(7);
        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                system.set_draft(host, Some(long.message), "Keep clock A.".into());
                system
                    .logs
                    .get_mut(&host)
                    .unwrap()
                    .set_work(vec![(long.message, ThreadWork::Working)]);
                system.streams(
                    lab,
                    "lab",
                    Some(host),
                    &rho_agent_hosts::Link::detached(),
                    cx,
                );
            });
        });
        let tell = |cx: &mut gpui::TestAppContext, record: ChatRecord| {
            cx.update(|cx| {
                mail.update(cx, |system, cx| {
                    system.logs.get_mut(&host).unwrap().tell(&record);
                    system.changed(host, cx);
                })
            });
        };
        // The line the point is on, its column, and how many rows below
        // the top of the window it is: where the reader is, whatever came
        // above it meanwhile.
        let view = |cx: &mut gpui::TestAppContext| {
            cx.run_until_parked();
            cx.draw_window(window.into());
            window
                .update(cx, |frame, _, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.update(cx, |editor, cx| {
                        let display = editor.display_snapshot(cx);
                        let text = display.buffer_snapshot().text();
                        let head = editor
                            .selections
                            .newest_anchor()
                            .head()
                            .to_point(display.buffer_snapshot());
                        let row = head.to_display_point(&display).row().0 as f64;
                        (
                            text.lines().nth(head.row as usize).unwrap_or("").to_owned(),
                            head.column,
                            row - editor.scroll_position(cx).y,
                        )
                    })
                })
                .unwrap()
        };
        let go_deep = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.update(cx, |editor, cx| {
                        let deep = Point::new(150, 3);
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([deep..deep]),
                        );
                    });
                })
                .unwrap();
            view(cx)
        };
        let open = |address: Address, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| frame.open(address, window, cx))
                .unwrap();
        };
        let close = |address: Address, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| frame.close(&address, window, cx))
                .unwrap();
            cx.run_until_parked();
        };
        let deal = |cx: &mut gpui::TestAppContext| {
            let card = Card {
                address: long.address(),
                title: "Check the clocks.".into(),
                kind: CardKind::Waits,
                context: "clock@lab".into(),
                label: "waits".into(),
                priority: 1.,
                cursor: String::new(),
                seen: Seen::Mail {
                    message: 43,
                    version: 43,
                },
                skipped: false,
            };
            window
                .update(cx, |frame, window, cx| frame.open_card(card, window, cx))
                .unwrap();
        };
        let top = ("Keep clock A.".to_owned(), "Keep clock A.".len() as u32, 0.);

        open(long.address(), cx);
        assert_eq!(view(cx), top);
        let status = cx.update(|cx| window.read(cx).unwrap().current_surface().status(cx));
        assert_eq!(status.as_deref(), Some("working"));

        // (3) Nothing new: as left, however the thread comes back.
        let deep = go_deep(cx);
        assert_eq!(deep.1, 3);
        assert!(deep.2 > 0., "the point is below the top of the window");
        assert_ne!(deep.0, top.0);
        open(other.address(), cx);
        open(long.address(), cx);
        assert_eq!(view(cx), deep, "by name");
        open(other.address(), cx);
        window
            .update(cx, |frame, window, cx| frame.previous_place(window, cx))
            .unwrap();
        assert_eq!(view(cx), deep, "from the recent places");
        deal(cx);
        assert_eq!(view(cx), deep, "from the deal");
        close(long.address(), cx);
        open(long.address(), cx);
        assert_eq!(view(cx), deep, "after its screen closed");

        // A flash is the now, not news: it lands above, and the reader
        // stays where they were.
        open(other.address(), cx);
        tell(cx, posted(45, 2, agent, Voice::Flash, "Rerunning.".into()));
        open(long.address(), cx);
        assert_eq!(view(cx), deep, "a flash");

        // (2) A message the reader has not had in view: the top, with it
        // right below the draft.
        open(other.address(), cx);
        tell(
            cx,
            posted(46, 2, agent, Voice::Message, "Reading 46.".into()),
        );
        open(long.address(), cx);
        assert_eq!(view(cx), top, "a new message, the screen held");
        let below_draft = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, _, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    let text = editor.read(cx).text(cx);
                    text.lines().nth(2).unwrap_or("").to_owned()
                })
                .unwrap()
        };
        assert_eq!(below_draft(cx), "Reading 46.");
        // Its screen closed, and the newest message edited.
        let deep = go_deep(cx);
        close(long.address(), cx);
        tell(
            cx,
            ChatRecord {
                seq: ChatSeq(47),
                at: UnixMs::now(),
                event: ChatEvent::Revised {
                    message: MessageId(ChatSeq(46)),
                    author: worker,
                    content: vec![ContentPart::Text {
                        text: "Reading 46, again.".into(),
                    }],
                },
            },
        );
        deal(cx);
        assert_ne!(view(cx), deep);
        assert_eq!(view(cx), top, "an edit, the screen closed");

        // A push the agent's git asks for is news too.
        let deep = go_deep(cx);
        open(other.address(), cx);
        let (ask, _answered) = rho_agent_hosts::git::GitAsk::for_test(
            0xa1,
            super::super::git::tests::push_to(&["refs/heads/main"], Some(worker)),
            UnixMs::now().0,
        );
        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                system.git_heard(
                    rho_agent_hosts::git::GitEvent {
                        host: lab,
                        change: rho_agent_hosts::git::GitChange::Asked(ask),
                    },
                    cx,
                )
            })
        });
        open(long.address(), cx);
        assert_ne!(view(cx), deep);
        assert_eq!(view(cx), top, "a push");
        assert_eq!(
            below_draft(cx),
            format!(
                "{} asks to push with your ssh login",
                short_tag(&worker.encoded())
            )
        );

        // (1) A remembered line: on it, news or not.
        go_deep(cx);
        open(other.address(), cx);
        tell(
            cx,
            posted(48, 2, agent, Voice::Message, "Reading 48.".into()),
        );
        for _ in 0..2 {
            window
                .update(cx, |frame, window, cx| {
                    frame.open_text_match(
                        &rho_frame::FoundText {
                            address: long.address(),
                            title: String::new(),
                            line: String::new(),
                            point: rho_frame::SearchPoint::Message {
                                message: ThreadId {
                                    host,
                                    message: MessageId(ChatSeq(10)),
                                }
                                .address(),
                                line: 0,
                            },
                        },
                        window,
                        cx,
                    )
                })
                .unwrap();
            assert_eq!(view(cx).0, "Reading 10.");
            // And again with the screen closed.
            close(long.address(), cx);
        }
    }

    /// A thread that `enter` and `>` act on, opened in a window that reads
    /// keys as rho does: Helix, the bundled keymaps, and the screen's own
    /// keys over them. Agent `1` works in it; its newest items are a
    /// flash, an ask with a tangent under it, and a proof with a link.
    struct Keyed {
        window: gpui::WindowHandle<rho_frame::Frame>,
        thread: ThreadId,
        links: Rc<std::cell::RefCell<Vec<String>>>,
        inspected: Rc<std::cell::RefCell<Vec<MessageId>>>,
        _db: tempfile::TempDir,
    }

    const PROOF: MessageId = MessageId(ChatSeq(5));
    const ASK: MessageId = MessageId(ChatSeq(6));
    const FLASH: MessageId = MessageId(ChatSeq(8));
    const URL: &str = "https://proof.example.invalid/clock";
    const DRAFT: &str = "Keep clock A.";

    /// Takes the links the user opens, as the browser would.
    struct Links(Rc<std::cell::RefCell<Vec<String>>>);

    impl rho_frame::System for Links {
        fn kinds(&self) -> &'static [&'static str] {
            &[]
        }

        fn open(
            &mut self,
            _: &Address,
            _: &mut Window,
            _: &mut Context<Self>,
        ) -> rho_frame::Opening {
            rho_frame::Opening::Gone
        }

        fn open_link(&mut self, url: &url::Url, _: &mut Context<Self>) -> bool {
            self.0.borrow_mut().push(url.to_string());
            true
        }
    }

    fn keyed(cx: &mut gpui::TestAppContext) -> Keyed {
        use rho_agent_types::{ChatEvent, ChatRecord};
        use rho_frame::{Frame, FrameLink};

        let host = EndpointId([3; 32]);
        let worker = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let thread = ThreadId {
            host,
            message: MessageId(ChatSeq(3)),
        };
        let posted =
            |seq: u64, parent: Option<u64>, from: Party, voice: Voice, text: &str| ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(UnixMs::now().0 - 60_000 + seq),
                event: ChatEvent::Posted {
                    parent: parent.map(|id| MessageId(ChatSeq(id))),
                    retry_of: None,
                    thread_reply: None,
                    from,
                    voice,
                    content: vec![ContentPart::Text { text: text.into() }],
                },
            };
        let agent = Party::Agent(worker);
        let mut log = HostChat::default();
        log.tell(&posted(1, None, Party::User, Voice::Message, "tide"));
        log.tell(&ChatRecord {
            seq: ChatSeq(2),
            at: UnixMs(2),
            event: ChatEvent::Project {
                project: rho_agent_types::ProjectId(MessageId(ChatSeq(1))),
                config: rho_agent_types::ProjectConfig {
                    place: rho_agent_types::ProjectPlace::parse("~/src/tide").unwrap(),
                    mode: rho_agent_types::ProjectMode::One,
                    runtime: rho_agent_types::RuntimeLabel::default(),
                },
            },
        });
        log.tell(&posted(
            3,
            Some(1),
            Party::User,
            Voice::Message,
            "Check the clocks.",
        ));
        log.tell(&ChatRecord {
            seq: ChatSeq(4),
            at: UnixMs(4),
            event: ChatEvent::Joined {
                thread: thread.message,
                agent: worker,
            },
        });
        log.tell(&posted(
            5,
            Some(3),
            agent,
            Voice::Message,
            &format!("Proof at {URL} today.\n\nThe clock drifted."),
        ));
        log.tell(&posted(
            6,
            Some(3),
            agent,
            Voice::Message,
            "Which clock should I keep?",
        ));
        log.tell(&posted(
            7,
            Some(6),
            Party::User,
            Voice::Message,
            "About the reference.",
        ));
        log.tell(&posted(
            8,
            Some(3),
            agent,
            Voice::Flash,
            "Rerunning the clock test.",
        ));

        let temp = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(temp.path().join("rho.redb"));
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            rho_window::editor_config::init_vim_mode(cx).unwrap();
        });
        let links = Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut mail = None;
        let window = cx.add_window(|window, cx| {
            let frame = FrameLink::new(window.window_handle().downcast::<Frame>().unwrap());
            let notes = cx.new(|cx| rho_notes::NoteSystem::new(frame.clone(), cx));
            let system = cx.new(|cx| {
                let mut system = ChatSystem::new(db.clone(), frame, notes, cx);
                system.logs.insert(host, log);
                system.set_draft(host, Some(thread.message), DRAFT.into());
                system
            });
            let catcher = cx.new(|_| Links(links.clone()));
            mail = Some(system.clone());
            Frame::new(
                db,
                vec![
                    rho_frame::AnySystem::new(system),
                    rho_frame::AnySystem::new(catcher),
                ],
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        });
        let mail = mail.unwrap();
        let inspected = Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = inspected.clone();
        cx.update(|cx| {
            cx.subscribe(&mail, move |_, event: &Event, _| {
                if let Event::Inspect { mail, .. } = event {
                    seen.borrow_mut().push(mail.id);
                }
            })
            .detach()
        });
        // A private reply under the proof.
        let notes = mail.read_with(cx, |system, _| system.notes.clone());
        let subject = ThreadId {
            host,
            message: PROOF,
        }
        .address();
        let note = window
            .update(cx, |frame, _, cx| {
                rho_notes::create_private(frame, &subject, cx)
            })
            .unwrap();
        notes.update(cx, |notes, cx| {
            notes.edited(note, "Ask about clock B.".into(), cx);
            notes.save(note, cx);
        });
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread.address(), window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        // The window opens editors to write in; reading is Helix's normal
        // mode.
        cx.simulate_keystrokes(window.into(), "escape");
        cx.run_until_parked();
        cx.draw_window(window.into());
        Keyed {
            window,
            thread,
            links,
            inspected,
            _db: temp,
        }
    }

    impl Keyed {
        fn view(&self, cx: &mut gpui::TestAppContext) -> Entity<ThreadView> {
            self.window
                .update(cx, |frame, _, _| {
                    frame
                        .current_surface()
                        .view()
                        .downcast::<ThreadView>()
                        .unwrap()
                })
                .unwrap()
        }

        fn text(&self, cx: &mut gpui::TestAppContext) -> String {
            let view = self.view(cx);
            view.read_with(cx, |view, cx| view.document(cx).text())
        }

        /// The row of the document's line that is exactly `line`.
        fn row(&self, cx: &mut gpui::TestAppContext, line: &str) -> u32 {
            let text = self.text(cx);
            text.lines()
                .position(|candidate| candidate == line)
                .unwrap_or_else(|| panic!("no line {line:?} in\n{text}")) as u32
        }

        /// The point on `column` of the line that is exactly `line`, in
        /// Helix's normal mode.
        fn at(&self, cx: &mut gpui::TestAppContext, line: &str, column: u32) {
            let row = self.row(cx, line);
            let view = self.view(cx);
            self.window
                .update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.editor.update(cx, |editor, cx| {
                            let point = Point::new(row, column);
                            editor.change_selections(Default::default(), window, cx, |s| {
                                s.select_ranges([point..point])
                            });
                        });
                    })
                })
                .unwrap();
            cx.run_until_parked();
            cx.draw_window(self.window.into());
        }

        fn keys(&self, cx: &mut gpui::TestAppContext, keys: &str) {
            cx.simulate_keystrokes(self.window.into(), keys);
            cx.run_until_parked();
            cx.draw_window(self.window.into());
        }

        /// The selection's width, in characters.
        fn selected(&self, cx: &mut gpui::TestAppContext) -> usize {
            let view = self.view(cx);
            view.read_with(cx, |view, cx| {
                view.document(cx)
                    .text_for_range(view.selection_range(cx))
                    .map(|chunk| chunk.chars().count())
                    .sum()
            })
        }

        fn draft(&self, cx: &mut gpui::TestAppContext) -> String {
            let view = self.view(cx);
            view.read_with(cx, |view, cx| view.draft.read(cx).text())
        }

        fn current(&self, cx: &mut gpui::TestAppContext) -> Address {
            self.window
                .update(cx, |frame, _, _| frame.current_address().clone())
                .unwrap()
        }
    }

    /// `>` quotes what is selected in one read-only message into the
    /// draft, above where the draft's caret was, a whole line or a word
    /// that `w` selected alike; the draft keeps its words.
    #[gpui::test]
    fn greater_than_quotes_a_selection_in_a_message(cx: &mut gpui::TestAppContext) {
        let thread = keyed(cx);
        thread.at(cx, "Which clock should I keep?", 0);
        thread.keys(cx, "x >");
        assert_eq!(
            thread.draft(cx),
            format!("> Which clock should I keep?\n{DRAFT}")
        );
        // A word `w` selected quotes its line.
        thread.at(cx, &format!("Proof at {URL} today."), 0);
        thread.keys(cx, "w");
        assert!(thread.selected(cx) > 1, "w selects a word");
        thread.keys(cx, ">");
        assert_eq!(
            thread.draft(cx),
            format!("> Which clock should I keep?\n> Proof at {URL} today.\n{DRAFT}")
        );
        assert!(thread.links.borrow().is_empty(), "quoting opens nothing");
        assert!(thread.inspected.borrow().is_empty());
    }

    /// Inside the draft, `>` is Helix's indent, not a quote.
    #[gpui::test]
    fn greater_than_indents_in_the_draft(cx: &mut gpui::TestAppContext) {
        let thread = keyed(cx);
        thread.at(cx, DRAFT, 0);
        thread.keys(cx, ">");
        let draft = thread.draft(cx);
        assert_ne!(draft, DRAFT, "the line is indented");
        assert_eq!(draft.trim_start(), DRAFT);
        assert!(draft.starts_with([' ', '\t']), "{draft:?}");
    }

    /// `enter` opens what is under the cursor however wide the selection
    /// Helix left: after `w` on a link, the link; after `w` on a project
    /// message or a flash, its work; after `/` found words in a message,
    /// its work; after `w` on a private reply, the reply; after `w` on a
    /// tangent's line, the tangent. It never quotes.
    #[gpui::test]
    fn enter_opens_what_is_under_the_cursor_whatever_the_selection(cx: &mut gpui::TestAppContext) {
        let thread = keyed(cx);
        let proof_line = format!("Proof at {URL} today.");

        // A link a `w` landed in.
        thread.at(cx, &proof_line, "Proof at ".len() as u32);
        thread.keys(cx, "w");
        assert!(thread.selected(cx) > 1);
        thread.keys(cx, "enter");
        assert_eq!(*thread.links.borrow(), [URL]);

        // The project's message, a word selected in it.
        thread.at(cx, "Which clock should I keep?", 0);
        thread.keys(cx, "w");
        assert!(thread.selected(cx) > 1);
        thread.keys(cx, "enter");
        assert_eq!(*thread.inspected.borrow(), [ASK]);

        // The flash.
        thread.at(cx, "Rerunning the clock test.", 0);
        thread.keys(cx, "w enter");
        assert_eq!(*thread.inspected.borrow(), [ASK, FLASH]);

        // A search's whole match.
        thread.at(cx, DRAFT, 0);
        thread.keys(cx, "/ d r i f t e d enter");
        assert_eq!(thread.selected(cx), "drifted".len());
        thread.keys(cx, "enter");
        assert_eq!(*thread.inspected.borrow(), [ASK, FLASH, PROOF]);
        assert_eq!(thread.draft(cx), DRAFT, "nothing was quoted");

        // A private reply edits in place.
        thread.at(cx, "Ask about clock B.", 0);
        thread.keys(cx, "w enter");
        let view = thread.view(cx);
        assert!(view.read_with(cx, |view, _| view.private_edit.is_some()));
        thread.keys(cx, "escape");
        assert!(view.read_with(cx, |view, _| view.private_edit.is_none()));

        // A tangent's line opens the tangent.
        let fold = thread
            .text(cx)
            .lines()
            .find(|line| line.starts_with("  ↳ 1 tangent") && line.contains("1 message"))
            .unwrap()
            .to_owned();
        thread.at(cx, &fold, "  ↳ ".len() as u32);
        thread.keys(cx, "w enter");
        assert_eq!(
            thread.current(cx),
            ThreadId {
                host: thread.thread.host,
                message: ASK,
            }
            .address()
        );
        assert_eq!(*thread.links.borrow(), [URL]);
        assert_eq!(thread.inspected.borrow().len(), 3);
    }

    #[gpui::test]
    fn a_bare_proof_url_in_mail_has_an_open_command(cx: &mut gpui::TestAppContext) {
        check_thread(cx, CheckThread::ProofRow);
    }

    #[gpui::test]
    fn the_draft_uses_a_sole_participant_and_never_guesses(cx: &mut gpui::TestAppContext) {
        check_thread(cx, CheckThread::WorkTarget);
    }

    enum CheckThread {
        Inline,
        Stuck,
        ThreadReply,
        ProofRow,
        WorkTarget,
    }

    fn check_thread(cx: &mut gpui::TestAppContext, check: CheckThread) {
        use gpui::{point, px, size};
        use rho_agent_types::{ChatEvent, ChatRecord};
        use rho_frame::{Frame, FrameLink};

        fn post(seq: u64, parent: Option<u64>, from: Party, text: &str) -> ChatRecord {
            ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(1_760_000_000_000 + seq),
                event: ChatEvent::Posted {
                    parent: parent.map(|id| MessageId(ChatSeq(id))),
                    retry_of: None,
                    thread_reply: None,
                    from,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text { text: text.into() }],
                },
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(temp.path().join("rho.redb"));
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
        let frame = FrameLink::new(cx.add_window(|window, cx| {
            Frame::new(
                db.clone(),
                Vec::new(),
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        }));
        let thread = ThreadId {
            host: EndpointId([3; 32]),
            message: MessageId(ChatSeq(2)),
        };
        let worker = AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let agent = Party::Agent(worker);
        let other = AgentId::from_counter(9, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let reply = "Use each counter's own reference.";
        let mail = cx.update(|cx| {
            let notes = cx.new(|cx| rho_notes::NoteSystem::new(frame.clone(), cx));
            cx.new(|cx| {
                let mut system = ChatSystem::new(db, frame.clone(), notes, cx);
                system.set_participants(Rc::new(move |host, id, _| {
                    (host == thread.host && (id == worker || id == other)).then(|| Participant {
                        label: if id == worker { "worker" } else { "other" }.into(),
                        code: if id == worker { "eng-work" } else { "eng-othr" }.into(),
                        runtime: Some("sol".into()),
                        created_at: Some(UnixMs(1)),
                        doing: None,
                        stopped_because: None,
                        parent: None,
                    })
                }));
                let log = system.logs.entry(thread.host).or_default();
                log.tell(&post(1, None, Party::User, "Clock"));
                log.tell(&post(2, Some(1), agent, &"Earlier readings\n\n".repeat(25)));
                log.tell(&post(
                    3,
                    Some(2),
                    Party::User,
                    "My preceding answer.\n\n> The earlier question.",
                ));
                log.tell(&post(4, Some(2), agent, "The latest question."));
                if matches!(check, CheckThread::WorkTarget) {
                    log.tell(&ChatRecord {
                        seq: ChatSeq(5),
                        at: UnixMs(1_760_000_000_005),
                        event: ChatEvent::Joined {
                            thread: thread.message,
                            agent: worker,
                        },
                    });
                }
                system.set_draft(thread.host, Some(thread.message), reply.into());
                system
            })
        });
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                mail.update(cx, |system, cx| {
                    cx.new(|cx| ThreadView::new(system, mail.clone(), thread, window, cx))
                })
            })
            .unwrap()
        });
        cx.simulate_window_resize(window.into(), size(px(640.), px(340.)));
        cx.draw_window(window.into());
        window
            .update(cx, |view, _, cx| {
                let doc = view.document(cx).text();
                let first = doc.lines().take(9).collect::<Vec<_>>();
                assert_eq!(
                    first.as_slice(),
                    &[
                        reply,
                        "",
                        "The latest question.",
                        "",
                        "My preceding answer.",
                        "",
                        "> The earlier question.",
                        "",
                        "Earlier readings"
                    ],
                );
                assert_eq!(view.cursor(cx), Point::new(0, reply.len() as u32));
                assert_eq!(
                    view.editor
                        .update(cx, |editor, cx| editor.scroll_position(cx).y),
                    0.
                );
            })
            .unwrap();

        if matches!(check, CheckThread::WorkTarget) {
            // The thread names who its draft steers; the agents system's
            // commands, on every screen, act on that. The thread offers no
            // `open transcript` of its own, so `Commands` has one.
            let own_look =
                |view: &mut ThreadView, window: &mut Window, cx: &mut Context<ThreadView>| {
                    view.commands(None, window, cx)
                        .iter()
                        .any(|command| command.name() == "open transcript")
                };
            window
                .update(cx, |view, window, cx| {
                    assert_eq!(
                        view.subject(None, cx),
                        Subject::Draft {
                            worker: Some(worker)
                        }
                    );
                    assert!(!own_look(view, window, cx));
                })
                .unwrap();
            cx.update(|cx| {
                mail.update(cx, |system, cx| {
                    let log = system.logs.get_mut(&thread.host).unwrap();
                    log.tell(&ChatRecord {
                        seq: ChatSeq(6),
                        at: UnixMs(1_760_000_000_006),
                        event: ChatEvent::Joined {
                            thread: thread.message,
                            agent: other,
                        },
                    });
                    log.tell(&post(7, Some(2), Party::Agent(other), "Another question."));
                    cx.notify();
                });
            });
            window
                .update(cx, |view, _, cx| {
                    assert_eq!(
                        view.subject(None, cx),
                        Subject::Draft { worker: None },
                        "a draft with two participants must not guess a steering target"
                    );
                })
                .unwrap();
            return;
        }

        if matches!(check, CheckThread::ProofRow) {
            cx.update(|cx| {
                mail.update(cx, |system, cx| {
                    system.logs.get_mut(&thread.host).unwrap().tell(&post(
                        5,
                        Some(2),
                        agent,
                        "https://proof.example.invalid/clock",
                    ));
                    cx.notify();
                });
            });
            window
                .update(cx, |view, window, cx| {
                    let key = Key::Message(MessageId(ChatSeq(5)));
                    assert!(view.cursor_to_item(&key, Autoscroll::fit(), cx));
                    assert_eq!(
                        view.link_at(view.cursor(cx), cx).unwrap().as_str(),
                        "https://proof.example.invalid/clock"
                    );
                    let commands = view.commands(None, window, cx);
                    assert!(
                        commands
                            .iter()
                            .any(|command| command.name() == OPEN_UNDER_CURSOR.name),
                        "Commands must offer open for this ordinary mail line"
                    );
                })
                .unwrap();
            return;
        }

        if matches!(check, CheckThread::ThreadReply) {
            cx.update(|cx| {
                mail.update(cx, |system, cx| {
                    system.logs.get_mut(&thread.host).unwrap().tell(&post(
                        5,
                        Some(2),
                        agent,
                        "Some newer context, not a question.",
                    ));
                    cx.notify();
                });
            });
            window
                .update(cx, |view, host_window, cx| {
                    assert!(
                        !view
                            .document(cx)
                            .text()
                            .lines()
                            .any(|line| line.starts_with("answers "))
                    );
                    let key = Key::Message(MessageId(ChatSeq(2)));
                    assert!(view.cursor_to_item(&key, Autoscroll::fit(), cx));
                    let commands = view.commands(None, host_window, cx);
                    assert!(
                        !commands
                            .iter()
                            .any(|command| command.name().starts_with("answer"))
                    );
                    let row = view.cursor_row(cx);
                    view.quote(row..=row, host_window, cx);
                    assert!(view.on_draft(cx));
                    view.send(&SendMessage, host_window, cx);
                })
                .unwrap();
            let sent = cx.update(|cx| {
                mail.update(cx, |system, _| system.outbox.next(thread.host).unwrap().1)
            });
            assert_eq!((sent.settles, sent.failed_start), (None, None));
            return;
        }

        let mail_gutters = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |view, window, cx| {
                    view.editor.update(cx, |editor, cx| {
                        let display = editor.display_snapshot(cx);
                        let text = display.buffer_snapshot().text();
                        let lines = text.lines().collect::<Vec<_>>();
                        let mut spans = editor
                            .all_gutter_highlights(window, cx)
                            .into_iter()
                            .filter_map(|(range, colour)| {
                                let start = display.display_point_to_point(range.start, Bias::Left);
                                if start.row == 0 {
                                    return None; // the draft's own bar
                                }
                                assert_eq!(colour, token::marked_bar());
                                let end = display.display_point_to_point(range.end, Bias::Left);
                                Some((start.row, end.row, lines[start.row as usize].to_owned()))
                            })
                            .collect::<Vec<_>>();
                        spans.sort_unstable();
                        spans
                    })
                })
                .unwrap()
        };
        let preceding = "My preceding answer.".to_owned();
        assert_eq!(mail_gutters(cx), [(4, 6, preceding.clone())]);

        let stuck = matches!(check, CheckThread::Stuck);
        if stuck {
            window
                .update(cx, |view, window, cx| {
                    let latest = view
                        .item_row(&Key::Message(MessageId(ChatSeq(4))), cx)
                        .unwrap();
                    view.editor.update(cx, |editor, cx| {
                        let display = editor.display_snapshot(cx);
                        let row = Point::new(latest, 0).to_display_point(&display).row();
                        editor.set_scroll_position(point(0., row.0 as f64), window, cx);
                    });
                })
                .unwrap();
            cx.draw_window(window.into());
        }

        let draft_and_history = |cx: &mut gpui::TestAppContext, sent: bool| {
            window
                .update(cx, |view, window, cx| {
                    let draft = view.draft_start(cx);
                    if sent {
                        assert!(view.draft.read(cx).is_empty());
                        assert_eq!(view.cursor(cx), draft, "point must stay in the draft");
                    }
                    view.editor.update(cx, |editor, cx| {
                        assert_eq!(
                            editor.stuck_excerpt_is_visible(),
                            stuck,
                            "draft edge changed"
                        );
                        let scroll = editor.scroll_position(cx);
                        let display = editor.snapshot(window, cx);
                        let draft = draft.to_display_point(&display);
                        let y = editor
                            .window_position_for_display_point(draft, &display, window, cx)
                            .expect("draft on screen")
                            .y;
                        let first = DisplayPoint::new(DisplayRow(scroll.y.floor() as u32), 0);
                        let first = display.display_point_to_point(first, Bias::Left).row;
                        let text = display.buffer_snapshot().text();
                        let history =
                            stuck.then(|| text.lines().nth(first as usize).unwrap().to_owned());
                        (y, history)
                    })
                })
                .unwrap()
        };
        let before = draft_and_history(cx, false);
        if stuck {
            assert_eq!(before.1.as_deref(), Some("The latest question."));
        }

        window
            .update(cx, |view, window, cx| view.send(&SendMessage, window, cx))
            .unwrap();
        cx.draw_window(window.into());
        assert_eq!(
            draft_and_history(cx, true),
            before,
            "while the mail is sending"
        );
        window
            .update(cx, |view, _, cx| {
                assert!(
                    view.document(cx)
                        .text()
                        .starts_with(&format!("\n\n{reply}\n"))
                );
            })
            .unwrap();

        assert_eq!(
            mail_gutters(cx),
            [(2, 2, reply.to_owned()), (6, 8, preceding.clone()),],
        );

        // The receipt can clear the optimistic row before the durable mail
        // reaches the view. Test across both changes to the history.
        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                system.outbox.next(thread.host).unwrap();
                system.outbox.answered(thread.host).unwrap();
                cx.notify();
            });
        });
        cx.draw_window(window.into());
        assert_eq!(draft_and_history(cx, true), before, "after the receipt");
        assert_eq!(mail_gutters(cx), [(4, 6, preceding.clone())]);

        cx.update(|cx| {
            mail.update(cx, |system, cx| {
                system.logs.get_mut(&thread.host).unwrap().tell(&post(
                    5,
                    Some(2),
                    Party::User,
                    reply,
                ));
                cx.notify();
            });
        });
        cx.draw_window(window.into());
        window
            .update(cx, |view, _, cx| {
                assert!(
                    view.transcript
                        .contains(&Key::Message(MessageId(ChatSeq(5))))
                );
                assert!(
                    view.document(cx)
                        .text()
                        .starts_with(&format!("\n\n{reply}\n"))
                );
            })
            .unwrap();
        assert_eq!(
            draft_and_history(cx, true),
            before,
            "when the sent mail lands"
        );

        assert_eq!(
            mail_gutters(cx),
            [(2, 2, reply.to_owned()), (6, 8, preceding)],
        );

        window
            .update(cx, |view, window, cx| {
                view.go_to_oldest_mail(&GoToOldestMessage, window, cx);
                assert_eq!(
                    view.line_meta(view.cursor_row(cx), cx).and_then(Meta::mail),
                    Some(MessageId(ChatSeq(2)))
                );
                view.go_to_draft(&GoToDraft, window, cx);
                assert!(view.on_draft(cx));
            })
            .unwrap();
        cx.draw_window(window.into());
        window
            .update(cx, |view, _, cx| {
                view.editor.update(cx, |editor, cx| {
                    assert_eq!(editor.scroll_position(cx).y, 0.);
                    assert!(!editor.stuck_excerpt_is_visible());
                });
            })
            .unwrap();
    }

    #[test]
    fn the_open_private_note_has_the_same_identity_but_no_body_in_mail_history() {
        let note = NoteId::from_address(&Address::new(
            Address::NOTE,
            "0000000000000000000000000000002f",
        ))
        .unwrap();
        let mail = MessageId(ChatSeq(17));
        let body = "check the cache\nthen the lease";
        let saved = private_item(note, mail, Some("10:15"), body, None);
        let editing = private_item(
            note,
            mail,
            None,
            body,
            Some(BlockSpec {
                line: BlockLine::Below(2),
                height: 1,
                render: Arc::new(|_| gpui::Empty.into_any_element()),
                priority: 0,
            }),
        );
        assert_eq!(
            saved.text,
            "\n🔒 you · 10:15\ncheck the cache\nthen the lease\n"
        );
        assert_eq!(editing.key, saved.key);
        assert_eq!(
            editing.text,
            "\n🔒 you · now\n── private · only you see this · escape keeps it\n"
        );
        assert_eq!(editing.blocks[0].line, BlockLine::Below(2));
        assert_eq!(editing.lines[2], Meta::Private { note, mail });
        assert_eq!(saved.lines[3], Meta::Private { note, mail });
    }

    fn mail(from: Party, revision: Revision, text: &str) -> Message {
        Message {
            id: MessageId(rho_agent_types::ChatSeq(13)),
            parent: None,
            retry_of: None,
            thread_reply: None,
            at: rho_agent_types::UnixMs(1_760_000_000_000),
            from,
            voice: Voice::Message,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            revision,
        }
    }

    /// `chat.the last message stays live`: an agent holds a live message
    /// from its message since the user's last one, until it is deleted or
    /// taken back; a flash never holds one.
    #[test]
    fn only_messages_since_the_users_last_are_live() {
        let domain = rho_agent_types::AgentIdDomain(0);
        let a = AgentId::from_counter(1, &domain).unwrap();
        let b = AgentId::from_counter(2, &domain).unwrap();
        let said = |from: Party| mail(from, Revision::Original, "words");
        let flash = |agent| Message {
            voice: Voice::Flash,
            ..said(Party::Agent(agent))
        };
        let taken_back = |agent| {
            mail(
                Party::Agent(agent),
                Revision::TakenBack {
                    seq: ChatSeq(9),
                    at: UnixMs(9),
                },
                "words",
            )
        };
        let live = |messages: &[Message]| {
            let numbered: Vec<_> = (1..)
                .zip(messages)
                .map(|(seq, message)| Message {
                    id: MessageId(ChatSeq(seq)),
                    ..message.clone()
                })
                .collect();
            live_writers(numbered.iter())
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            live(&[
                said(Party::Agent(a)),
                said(Party::User),
                said(Party::Agent(b)),
                said(Party::Agent(a)),
            ]),
            [a, b]
        );
        assert_eq!(
            live(&[
                said(Party::Agent(a)),
                said(Party::Agent(b)),
                said(Party::User),
                flash(b),
            ]),
            Vec::<AgentId>::new(),
            "the user's reply answered both, and a flash is no message"
        );
        assert_eq!(
            live(&[said(Party::Agent(a)), taken_back(b)]),
            [a],
            "words taken back hold no place"
        );
    }

    #[test]
    fn mail_speaks_for_the_project_and_marks_edits() {
        let agent = AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let original = mail(Party::Agent(agent), Revision::Original, "Ready?");
        let bot = mail(
            Party::System {
                agent,
                stop: rho_agent_types::Stop::Failed,
                since: original.at,
            },
            Revision::Edited {
                seq: ChatSeq(2),
                at: UnixMs(2),
            },
            "cargo exited 101",
        );
        let user = mail(Party::User, Revision::Original, "Go ahead.");
        for (mail, first) in [
            (&original, "Ready?"),
            (&bot, "cargo exited 101 · edited"),
            (&user, "Go ahead."),
        ] {
            let item = message_item(mail, None, &HashSet::new()).unwrap();
            assert_eq!(item.text, format!("{first}\n"));
        }
        let third_voice = message_item(&original, Some("eng-ab12"), &HashSet::new()).unwrap();
        assert_eq!(third_voice.text, "eng-ab12\nReady?\n");
        let deleted = Message {
            revision: Revision::Deleted,
            ..original
        };
        assert!(message_item(&deleted, None, &HashSet::new()).is_none());
    }

    /// A result may use any headings, `## proof` among them; none is a
    /// designated section, so each line reads as written in the project's
    /// voice and still maps to its own source line.
    #[test]
    fn markdown_headings_in_a_result_are_ordinary_text() {
        let writer = AgentId::from_counter(11, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let body = "## recap\nEarlier plan.\n\n## Result\nDone.\n\n## proof\n42 tests passed.";
        let mut result = mail(
            Party::Agent(writer),
            Revision::Edited {
                seq: ChatSeq(2),
                at: UnixMs(2),
            },
            body,
        );
        // A message in a thread: words at a root never ping.
        result.parent = Some(MessageId(ChatSeq(2)));
        assert!(result.pings_user());
        let item = message_item(&result, None, &HashSet::new()).unwrap();
        assert_eq!(
            item.text,
            "## recap\nEarlier plan.\n\n## Result\nDone.\n\n## proof\n42 tests passed. · edited\n"
        );
        let proof = item.text.find("## proof").unwrap();
        assert!(
            item.styles
                .iter()
                .any(|(class, bytes)| *class == Class::Agent && bytes.contains(&proof)),
            "{:?}",
            item.styles
        );
        let sources: Vec<_> = item.lines.iter().map(Meta::source_line).collect();
        assert_eq!(sources, (0..8).map(Some).collect::<Vec<_>>());
    }

    #[test]
    fn flash_groups_use_original_order_and_ignore_deleted_messages() {
        let domain = rho_agent_types::AgentIdDomain(0);
        let a = AgentId::from_counter(1, &domain).unwrap();
        let b = AgentId::from_counter(2, &domain).unwrap();
        let flash = |id, agent, revision| Message {
            id: MessageId(ChatSeq(id)),
            voice: Voice::Flash,
            at: UnixMs(100_000),
            ..mail(
                Party::Agent(agent),
                revision,
                "Running the suite.\nNo retries.",
            )
        };
        let edited = flash(
            3,
            a,
            Revision::Edited {
                seq: ChatSeq(29),
                at: UnixMs(800_000),
            },
        );
        let newest = flash(12, a, Revision::Original);
        let groups = flash_groups(
            [
                &flash(2, a, Revision::Original),
                &edited,
                &flash(5, b, Revision::Original),
                &newest,
                &flash(41, a, Revision::Deleted),
            ]
            .into_iter(),
        );
        assert_eq!((groups[&a].latest, groups[&a].earlier), (newest.id, 2));
        assert_eq!(
            (groups[&b].latest, groups[&b].earlier),
            (MessageId(ChatSeq(5)), 0)
        );
        let item = message_item(&edited, None, &HashSet::new()).unwrap();
        assert_eq!(
            item.text, "Running the suite.\nNo retries. · edited\n",
            "a flash's age is drawn beside it, not written into it"
        );
        assert_eq!(&item.text[item.styles[0].1.clone()], "Running the suite.");
        assert_eq!(item.styles[0].0, Class::Time);
        assert_eq!(item.lines[1].source_line(), Some(1));
    }

    #[test]
    fn quoting_a_linewise_selection_omits_the_next_line_and_preserves_blank_lines() {
        assert_eq!(selected_lines(Point::new(2, 0)..Point::new(5, 0)), 2..=4);
        assert_eq!(selected_lines(Point::new(2, 4)..Point::new(5, 3)), 2..=5);
        assert_eq!(selected_lines(Point::new(2, 0)..Point::new(2, 0)), 2..=2);
        assert_eq!(quote_lines("before\n\nafter"), "> before\n> \n> after\n");
        assert_eq!(quote_lines("first"), "> first\n");
    }

    #[test]
    fn opening_one_quote_leaves_the_other_folded_and_the_question_in_place() {
        let message = mail(
            Party::User,
            Revision::Original,
            "preface\n> façade first\n> second\nCan this ship?\n> another quote\n> hidden",
        );
        let first = QuoteId {
            mail: message.id,
            first_line: 1,
        };
        let second = QuoteId {
            mail: message.id,
            first_line: 4,
        };
        let folded = message_item(&message, None, &HashSet::new()).unwrap();
        assert_eq!(
            folded.text,
            "preface\n> façade first …\nCan this ship?\n> another quote …\n",
        );
        let quote_text = |item: &Item<Key, Class, Meta>| {
            item.styles
                .iter()
                .filter(|(class, _)| *class == Class::Quote)
                .map(|(_, range)| item.text[range.clone()].to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            quote_text(&folded),
            ["> façade first …", "> another quote …"]
        );
        let expanded = message_item(&message, None, &HashSet::from([first])).unwrap();
        assert_eq!(
            expanded.text,
            "preface\n> façade first\n> second\nCan this ship?\n> another quote …\n",
        );
        assert_eq!(
            quote_text(&expanded),
            ["> façade first\n> second", "> another quote …"]
        );
        assert_eq!(
            &expanded.lines[1..],
            &[
                Meta::Quote(first, 1),
                Meta::Quote(first, 2),
                Meta::Mail(message.id, Some(3)),
                Meta::Quote(second, 4)
            ]
        );
    }

    #[test]
    fn a_body_names_its_pictures() {
        let content = vec![
            ContentPart::Text {
                text: "hello".to_owned(),
            },
            ContentPart::Image {
                media_type: "image/png".to_owned(),
                data: vec![1],
            },
        ];
        assert_eq!(body(&content), "hello\n[image · image/png]");
    }

    /// `reply privately` lands on what the point is on: a message, or the
    /// thread on its head, and nothing on a blank line.
    #[test]
    fn the_point_is_on_a_message_only_on_a_message_line() {
        let host = EndpointId([7; 32]);
        let thread = ThreadId {
            host,
            message: MessageId(rho_agent_types::ChatSeq(10)),
        };
        let child = MessageId(rho_agent_types::ChatSeq(12));
        assert_eq!(
            line_subject(Some(&Meta::Mail(thread.message, None)), thread),
            Subject::Own
        );
        assert_eq!(
            line_subject(Some(&Meta::Mail(child, None)), thread),
            Subject::Address(
                ThreadId {
                    host,
                    message: child
                }
                .address()
            )
        );
        for nothing in [
            Some(Meta::None),
            Some(Meta::Open(Address::new("note", "x"))),
            None,
        ] {
            assert_eq!(line_subject(nothing.as_ref(), thread), Subject::None);
        }
    }

    #[test]
    fn a_link_runs_to_the_space_without_the_sentence_around_it() {
        let text = "see https://github.com/o/r/pull/7. and (http://a.b/c?d=1), not xhttps://no or https:// alone";
        let found: Vec<&str> = links(text).into_iter().map(|link| &text[link]).collect();
        assert_eq!(found, ["https://github.com/o/r/pull/7", "http://a.b/c?d=1"]);
        // A link that ends the text, and two on one line.
        let text = "https://x.y/a https://x.y/b";
        let found: Vec<&str> = links(text).into_iter().map(|link| &text[link]).collect();
        assert_eq!(found, ["https://x.y/a", "https://x.y/b"]);
    }
}
