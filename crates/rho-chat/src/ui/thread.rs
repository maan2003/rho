//! A thread: an editable draft above read-only mail, newest first, in one
//! editor.

use std::collections::{HashMap, HashSet};
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
use jiff::Timestamp;
use language::{Buffer, BufferEvent, Capability, Point, ToPoint as _};
use multi_buffer::{MultiBuffer, ToPoint as _};
use rho_agent_types::{
    Address as Party, AgentId, ChatSeq, EndpointId, Message, MessageId, Revision, UnixMs, Voice,
};
use rho_dealer::facts::Seen;
use rho_dealer::{Address, Addressed as _, Card, Marks};
use rho_frame::surface::PhoneSend;
use rho_frame::thread::{self, OpenRow, QuoteSelection, SendMessage};
use rho_frame::{CardHere, Layout, Row, Subject, Surface};
use rho_notes::NoteId;
use rho_transcript::{BlockLine, BlockSpec, Item, Transcript};
use rho_window::draft;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use super::{ChatSystem, Event, Participant, ago};
use crate::reading::body;
use crate::{HostChat, ThreadId, cards};

actions!(rho_private_reply, [ReplyPrivately, KeepPrivateReply]);
actions!(rho_chat, [GoToDraft, GoToOldestMessage]);

const PRIVATE_SCREEN: &str = "RhoPrivateReply";
const PRIVATE_KEY: &str = "alt-r";
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
    Status,
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
    /// What the project says.
    Name,
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
            Self::Name => 6,
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
        let colors = cx.theme().colors();
        let color = match self {
            Self::Crumb | Self::Time | Self::Pending | Self::Quote | Self::Private => {
                colors.text_muted
            }
            Self::User | Self::Link => colors.text_accent,
            Self::Fold => cx.theme().status().info,
            Self::Name => colors.text,
        };
        HighlightStyle {
            color: Some(color.into()),
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

enum InlineOpen {
    Flashes(AgentId),
    Quote(QuoteId),
    Link(url::Url),
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
    /// Resolve against the new transcript before inferring motion direction.
    last_point: Anchor,
    expanded: Expanded,
    _subscriptions: Vec<Subscription>,
    /// Reads the thread again when a flash's age next changes.
    _ages: Option<Task<()>>,
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

const MINUTE_MS: u64 = 60_000;

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
            rho_window::markdown::configure_buffer(&mut buffer, cx);
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
            draft::stick(&mut editor, "↑ new".into(), cx);
            let doc = editor.buffer().read(cx).snapshot(cx);
            draft::style(&mut editor, draft::range(&doc), "reply", cx);
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
            // A new view starts at the top of the thread.
            seen_in_view: newest_version(system, thread),
            last_point: Anchor::Min,
            expanded: Expanded::default(),
            _subscriptions: subscriptions,
            _ages: None,
        };
        view.transcript
            .set_gutter(rho_window::style::user_prompt_gutter_color);
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
            self.seen_in_view = newest_version(self.system.read(cx), self.thread);
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
        self.schedule_ages(cx);
        cx.notify();
    }

    /// Reads the thread again when an age it shows next says another.
    fn schedule_ages(&mut self, cx: &mut Context<Self>) {
        let system = self.system.read(cx);
        let thread = self.thread.message;
        let now = UnixMs::now().0;
        let wait = system.host_chat(self.thread.host).and_then(|chat| {
            chat.reading_order(thread)
                .filter(|message| message.flash_deadline().is_some())
                .map(|message| MINUTE_MS - now.saturating_sub(message.at.0) % MINUTE_MS)
                .min()
        });
        self._ages = wait.map(|wait| {
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
            .update(cx, |editor, cx| draft::style(editor, range, "reply", cx));
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
        self.seen_in_view = newest_version(self.system.read(cx), self.thread);
    }
}

/// The rows of `thread` as `system` holds it, below the draft.
fn items(
    system: &ChatSystem,
    thread: ThreadId,
    expanded: &Expanded,
    private_edit: Option<&PrivateEdit>,
    cx: &App,
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = vec![gap(Key::Separator)];
    if system.work(thread) == Some(rho_agent_types::ThreadWork::StoppedByYou) {
        items.push(line(Key::Status, "stopped by you", Class::Crumb));
    }
    items.extend(history_items(system, thread, expanded, private_edit, cx));
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
        return vec![line(
            Key::Unavailable,
            "this host's messages are not held here",
            Class::Crumb,
        )];
    };
    let Some(head) = chat.thread(thread.message) else {
        return vec![line(Key::Unavailable, "this thread is gone", Class::Crumb)];
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
    let now = Timestamp::now();
    let mail_item = |message: &Message| {
        let voice = match (message.from, message.voice) {
            (Party::Agent(agent), Voice::ToAgent(_)) => system
                .agent_label(host, agent, cx)
                .or_else(|| Some(agent.encoded())),
            _ => None,
        };
        message_item(message, voice.as_deref(), &expanded.quotes, now)
    };
    items.extend(pending_items(system, host, thread.message));
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
        // The opening mail's children are this history, not a tangent.
        let replies = chat.children(Some(id));
        if id != thread.message && !replies.is_empty() {
            let (_, at) = chat.newest_in(id).unwrap_or_else(|| message.version());
            let text = format!(
                "  ↳ {} {} · {} · {}",
                replies.len(),
                if replies.len() == 1 {
                    "reply"
                } else {
                    "replies"
                },
                chat.name(id).unwrap_or_default(),
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
    if items
        .last()
        .is_some_and(|item| matches!(item.key, Key::PendingGap(_) | Key::MailGap(_)))
    {
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

/// The user's messages into `parent` that no host has logged yet, newest
/// first.
fn pending_items(
    system: &ChatSystem,
    host: EndpointId,
    parent: MessageId,
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = Vec::new();
    let mut outgoing = system.local_under(host, Some(parent));
    outgoing.reverse();
    for outgoing in outgoing {
        let id = outgoing.at.0;
        let mut text = body(&outgoing.content);
        let first = text.find('\n').unwrap_or(text.len());
        let suffix = match &outgoing.state {
            crate::outbox::OutgoingState::Queued => " · sending…".to_owned(),
            crate::outbox::OutgoingState::Refused { reason } => {
                format!(" · not sent: {}", rho_agent_types::one_line(reason))
            }
        };
        text.insert_str(first, &suffix);
        let tag = first..first + suffix.len();
        let lines = text.lines().count().max(1);
        let len = text.len();
        items.push(
            Item::new(Key::Pending(id), text)
                .with_styles(vec![
                    (Class::User, 0..tag.start),
                    (Class::Pending, tag.clone()),
                    (Class::User, tag.end..len),
                ])
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

    fn quote_selection(&mut self, _: &QuoteSelection, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
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
        if self.open_inline(cursor, cx) {
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

    /// A folded quote opens before links; an open quote closes only if
    /// there is no link under the point. Both take precedence over the mail.
    fn inline_open(&self, cursor: Point, cx: &App) -> Option<InlineOpen> {
        let quote = match self.line_meta(cursor.row, cx) {
            Some(Meta::Quote(quote, _)) => Some(*quote),
            Some(Meta::EarlierFlashes(agent)) => return Some(InlineOpen::Flashes(*agent)),
            _ => None,
        };
        if let Some(quote) = quote
            && !self.expanded.quotes.contains(&quote)
        {
            return Some(InlineOpen::Quote(quote));
        }
        self.link_at(cursor, cx)
            .map(InlineOpen::Link)
            .or_else(|| quote.map(InlineOpen::Quote))
    }

    fn open_inline(&mut self, cursor: Point, cx: &mut Context<Self>) -> bool {
        match self.inline_open(cursor, cx) {
            Some(InlineOpen::Flashes(agent)) => {
                if !self.expanded.flashes.remove(&agent) {
                    self.expanded.flashes.insert(agent);
                }
                self.refresh(cx);
                self.cursor_to_item(&Key::EarlierFlashes(agent), Autoscroll::fit(), cx);
            }
            Some(InlineOpen::Quote(quote)) => self.toggle_quote(quote, cx),
            Some(InlineOpen::Link(url)) => self.system.read(cx).frame.clone().open_link(url, cx),
            None => return false,
        }
        true
    }

    /// Enter opens the thing under the point: a quote, link, agent's mail,
    /// tangent or crumb.
    fn open_row(&mut self, _: &OpenRow, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        let selection = self.selection_range(cx);
        if !selection.is_empty()
            && let Some(lines) = self.mail_selection(selection, cx)
        {
            self.quote(lines, window, cx);
            return;
        }
        let cursor = self.cursor(cx);
        if let Some(Meta::Private { note, mail }) = self.line_meta(cursor.row, cx) {
            self.open_private(*mail, Some(*note), cx);
            return;
        }
        if self.open_inline(cursor, cx) {
            return;
        }
        if let Some((agent, mail)) = self.agent_mail(cursor.row, cx) {
            self.inspect(agent, mail.clone(), cx);
            return;
        }
        let meta = self.line_meta(cursor.row, cx).cloned();
        let host = self.thread.host;
        let address = match meta {
            Some(Meta::Thread(id) | Meta::Mail(id, _)) if id != self.thread.message => {
                ThreadId { host, message: id }.address()
            }
            Some(Meta::Open(address)) => address,
            _ => return,
        };
        self.system.read(cx).frame.clone().open(address, cx);
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

fn quote_lines(text: &str) -> String {
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
    now: Timestamp,
) -> Option<Item<Key, Class, Meta>> {
    let edited = match message.revision {
        Revision::Original => "",
        Revision::Edited { .. } | Revision::Recovered { .. } => " · edited",
        Revision::TakenBack { .. } => " · taken back",
        Revision::Deleted => return None,
    };
    let flash = message.voice == Voice::Flash;
    let tag = if flash {
        format!("{edited} · {}", ago(message.at.0, now))
    } else {
        edited.to_owned()
    };
    let body = body(&message.content);
    let mut text = String::new();
    let mut styles = Vec::new();
    let mut lines = Vec::new();
    if let Some(voice) = third_voice {
        if !lines.is_empty() {
            text.push('\n');
        }
        let start = text.len();
        text.push_str(voice);
        styles.push((Class::Crumb, start..text.len()));
        lines.push(Meta::Mail(message.id, None));
    }
    let voice = if message.from == Party::User {
        Class::User
    } else if flash {
        Class::Time
    } else {
        Class::Name
    };
    let mut body_lines = body.lines().enumerate().peekable();
    let mut first_body = true;
    while let Some((first_line, line)) = body_lines.next() {
        if !lines.is_empty() {
            text.push('\n');
        }
        let start = text.len();
        text.push_str(line);
        if line.starts_with("> ") {
            let quote = QuoteId {
                mail: message.id,
                first_line,
            };
            let expanded = expanded_quotes.contains(&quote);
            let mut more = false;
            lines.push(Meta::Quote(quote, first_line));
            while body_lines
                .peek()
                .is_some_and(|(_, line)| line.starts_with("> "))
            {
                let (source_line, quoted) = body_lines.next().expect("peeked a quote line");
                more = true;
                if expanded {
                    text.push('\n');
                    text.push_str(quoted);
                    lines.push(Meta::Quote(quote, source_line));
                }
            }
            if more && !expanded {
                text.push_str(" …");
            }
            styles.push((Class::Quote, start..text.len()));
        } else {
            styles.push((voice, start..text.len()));
            lines.push(Meta::Mail(message.id, Some(first_line)));
        }
        if first_body {
            let first_end = text[start..].find('\n').map_or(text.len(), |n| start + n);
            text.insert_str(first_end, &tag);
            // An expanded quote is still quiet after its tag.
            if let Some((_, range)) = styles.last_mut() {
                if first_end != range.end {
                    range.end += tag.len();
                }
            }
            styles.push((Class::Crumb, first_end..first_end + tag.len()));
            first_body = false;
        }
    }
    if lines.is_empty() {
        return None;
    }
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
        let system = self.system.read(cx);
        let project = system.project_of(self.thread, cx);
        let subject = system
            .host_chat(self.thread.host)
            .and_then(|chat| chat.name(self.thread.message))
            .unwrap_or_default();
        match subject.is_empty() {
            true => project,
            false => format!("{project} › {subject}"),
        }
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

    /// What the thread's agents do while nothing in it waits.
    fn status(&self, cx: &App) -> Option<String> {
        let work = self.system.read(cx).work(self.thread)?;
        Some(work.words().to_owned())
    }

    fn opened(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_at_draft(cx);
    }

    fn show_card(&mut self, _card: &Card, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_at_draft(cx);
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

    fn rows(&mut self, _: Option<Seen>, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        if self.private_edit.is_some() {
            return Vec::new();
        }
        let this = cx.entity();
        let has_draft = self.has_draft(cx);
        let send = this.clone();
        let mut rows = Vec::new();
        if self.private_target(cx).is_some() {
            let view = this.clone();
            rows.push(
                Row::run(PRIVATE_KEY, "reply privately", move |_, window, cx| {
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
            rows.push(Row::run("", "quote message line", move |_, window, cx| {
                view.update(cx, |view, cx| {
                    let row = view.cursor_row(cx);
                    let lines = view
                        .mail_selection(view.selection_range(cx), cx)
                        .unwrap_or(row..=row);
                    view.quote(lines, window, cx);
                });
            }));
        }
        if self.system.read(cx).refused_reply(self.thread).is_some() {
            let view = this.clone();
            rows.push(Row::run("", "write this reply again", move |_, _, cx| {
                view.update(cx, |view, cx| view.write_reply_again(cx));
            }));
        }
        if has_draft {
            rows.push(
                Row::run("ctrl-enter", "send", move |_, window, cx| {
                    send.update(cx, |view, cx| view.send(&SendMessage, window, cx));
                })
                .bound_to(SendMessage),
            );
        }
        // `look into the work` is the agents system's, on every screen.
        let cursor = self.cursor(cx);
        let opens_inline = self.inline_open(cursor, cx).is_some();
        // The thumb's Open action follows Enter at the point.
        if opens_inline
            || matches!(
                self.line_meta(cursor.row, cx),
                Some(Meta::Thread(_) | Meta::Open(_))
            )
        {
            rows.push(
                Row::run("enter", "open", move |_, window, cx| {
                    this.update(cx, |view, cx| view.open_row(&OpenRow, window, cx));
                })
                .bound_to(OpenRow),
            );
        }
        rows
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
            .child(self.editor.clone())
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
            doc(cx).starts_with("\n\nLine 1 of the answer. · sending…\n"),
            "{}",
            doc(cx)
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
        assert!(doc(cx).starts_with("\n\nLine 1 of the answer.\n"));
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
        const AGAIN: &str = "write this reply again";
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
        // Runs the palette row, when the thread offers it.
        let again = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    let rows = frame.current_surface().rows(None, window, cx);
                    let run = rows.into_iter().find_map(|row| match row.pick {
                        rho_frame::menu::Pick::Run { label, run, .. } if label == AGAIN => {
                            Some(run)
                        }
                        _ => None,
                    })?;
                    run(frame, None, window, cx);
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

    /// A thread opened from the deal or by name shows its top: the draft
    /// with the point where it was left, then the newest mail. Returning
    /// through history keeps the place in the history below.
    #[gpui::test]
    fn an_open_starts_at_the_draft_and_back_keeps_the_view(cx: &mut gpui::TestAppContext) {
        use editor::SelectionEffects;
        use rho_agent_types::{ChatEvent, ChatRecord, ThreadWork};
        use rho_dealer::Card;
        use rho_dealer::facts::Seen;
        use rho_dealer::rank::CardKind;

        let host = EndpointId([3; 32]);
        let agent =
            Party::Agent(AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap());
        let long = ThreadId {
            host,
            message: MessageId(ChatSeq(2)),
        };
        let other = ThreadId {
            host,
            message: MessageId(ChatSeq(3)),
        };
        let mut log = HostChat::default();
        let mut post = |seq: u64, parent: Option<u64>, from: Party, text: String| {
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
        };
        post(1, None, Party::User, "Clock".into());
        post(2, Some(1), Party::User, "Check the clocks.".into());
        post(3, Some(1), Party::User, "Another thread.".into());
        for n in 4..44 {
            post(
                n,
                Some(2),
                agent,
                format!("Reading {n}.\n\nThe clock drifted.\n\nAgain."),
            );
        }
        let (window, mail, _db) = super::super::search_bench::fixture(cx, log);
        cx.update(|cx| {
            mail.update(cx, |system, _| {
                system.set_draft(host, Some(long.message), "Keep clock A.".into());
                system
                    .logs
                    .get_mut(&host)
                    .unwrap()
                    .set_work(vec![(long.message, ThreadWork::Working)]);
            });
        });
        let view = |cx: &mut gpui::TestAppContext| {
            cx.draw_window(window.into());
            window
                .update(cx, |frame, _, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.update(cx, |editor, cx| {
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let head = editor.selections.newest_anchor().head().to_point(&snapshot);
                        (head, editor.scroll_position(cx).y)
                    })
                })
                .unwrap()
        };
        let go_deep = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    let editor = frame.current_surface().editor(cx).unwrap();
                    editor.update(cx, |editor, cx| {
                        let deep = Point::new(150, 0);
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([deep..deep]),
                        );
                    });
                })
                .unwrap();
        };
        let open = |address: Address, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| frame.open(address, window, cx))
                .unwrap();
        };

        open(long.address(), cx);
        let top = view(cx);
        assert_eq!(top.0, Point::new(0, "Keep clock A.".len() as u32));
        assert_eq!(top.1, 0.);
        let status = cx.update(|cx| window.read(cx).unwrap().current_surface().status(cx));
        assert_eq!(status.as_deref(), Some("working"));

        go_deep(cx);
        let deep = view(cx);
        assert_eq!(deep.0.row, 150);
        assert!(deep.1 > 100., "the history is scrolled: {}", deep.1);

        // Back through history and forward again: the view is as left.
        open(other.address(), cx);
        view(cx);
        window
            .update(cx, |frame, window, cx| frame.back(window, cx))
            .unwrap();
        assert_eq!(view(cx), deep);
        window
            .update(cx, |frame, window, cx| frame.forward_or_deal(window, cx))
            .unwrap();
        window
            .update(cx, |frame, window, cx| frame.back(window, cx))
            .unwrap();
        assert_eq!(view(cx), deep);

        // By name: the top, with the point where the draft was left.
        open(other.address(), cx);
        open(long.address(), cx);
        assert_eq!(view(cx), top);

        // From the deal: the same.
        go_deep(cx);
        assert_eq!(view(cx), deep);
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
        assert_eq!(view(cx), top);
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
                        runtime: Some("sol".into()),
                        created_at: Some(UnixMs(1)),
                        doing: None,
                        stopped_because: None,
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
            // `look into the work` of its own, so the palette has one.
            let own_look =
                |view: &mut ThreadView, window: &mut Window, cx: &mut Context<ThreadView>| {
                    view.rows(None, window, cx).into_iter().any(|row| {
                        matches!(row.pick, rho_frame::menu::Pick::Run { label, .. }
                        if label == "look into the work")
                    })
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
            window.update(cx, |view, window, cx| {
                let key = Key::Message(MessageId(ChatSeq(5)));
                assert!(view.cursor_to_item(&key, Autoscroll::fit(), cx));
                assert_eq!(view.link_at(view.cursor(cx), cx).unwrap().as_str(), "https://proof.example.invalid/clock");
                let rows = view.rows(None, window, cx);
                assert!(
                    rows.into_iter().any(|row| matches!(row.pick, rho_frame::menu::Pick::Run { label, .. } if label == "open")),
                    "the palette must offer open for this ordinary mail line"
                );
            }).unwrap();
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
                    let rows = view.rows(None, host_window, cx);
                    assert!(!rows.iter().any(|row| matches!(&row.pick,
                        rho_frame::menu::Pick::Run { label, .. } if label.starts_with("answer this")
                    )));
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
                                assert_eq!(colour, rho_window::style::user_prompt_gutter_color(cx));
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
                        .starts_with(&format!("\n\n{reply} · sending…\n"))
                );
            })
            .unwrap();

        assert_eq!(
            mail_gutters(cx),
            [
                (2, 2, format!("{reply} · sending…")),
                (6, 8, preceding.clone()),
            ],
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
            let item = message_item(mail, None, &HashSet::new(), Timestamp::UNIX_EPOCH).unwrap();
            assert_eq!(item.text, format!("{first}\n"));
        }
        let third_voice = message_item(
            &original,
            Some("eng-ab12"),
            &HashSet::new(),
            Timestamp::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(third_voice.text, "eng-ab12\nReady?\n");
        let deleted = Message {
            revision: Revision::Deleted,
            ..original
        };
        assert!(message_item(&deleted, None, &HashSet::new(), Timestamp::UNIX_EPOCH).is_none());
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
        let item = message_item(&result, None, &HashSet::new(), Timestamp::UNIX_EPOCH).unwrap();
        assert_eq!(
            item.text,
            "## recap · edited\nEarlier plan.\n\n## Result\nDone.\n\n## proof\n42 tests passed.\n"
        );
        let proof = item.text.find("## proof").unwrap();
        assert!(
            item.styles
                .iter()
                .any(|(class, bytes)| *class == Class::Name && bytes.contains(&proof)),
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
        let now = Timestamp::from_millisecond(840_000).unwrap();
        let item = message_item(&edited, None, &HashSet::new(), now).unwrap();
        assert_eq!(
            item.text,
            "Running the suite. · edited · 12m\nNo retries.\n"
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
        let folded = message_item(&message, None, &HashSet::new(), Timestamp::UNIX_EPOCH).unwrap();
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
        let expanded = message_item(
            &message,
            None,
            &HashSet::from([first]),
            Timestamp::UNIX_EPOCH,
        )
        .unwrap();
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
