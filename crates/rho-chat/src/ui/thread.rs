//! A thread: the project's mail, its current status, and a reply box.
//!
//! A channel is the same surface over a root: its messages are the threads
//! under it, each with its replies folded under it.

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Subscription, Task, Window, div,
};
use jiff::{SignedDuration, Timestamp};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_agent_types::{Address as Party, ContentPart, EndpointId, Message, MessageId, Revision};
use rho_dealer::{Address, Addressed as _, Card, Marks};
use rho_frame::thread::{self, FocusComposer, FocusLog, OpenRow, SendMessage, SendOnly};
use rho_frame::{Candidate, Layout, Row, Subject, Surface};
use rho_transcript::{Item, Transcript};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use super::{ChatSystem, Event, ago};
use crate::{AgentRef, HostChat, ThreadId, cards};

/// This surface's highlight-key space, clear of the others'.
const KEY_BASE: usize = usize::MAX / 5;

/// What a row is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Crumb,
    /// The message the thread hangs under, and each under it.
    Message(MessageId),
    /// The replies under a message, folded.
    Fold(MessageId),
    /// A thread under a root, on one line.
    Thread(MessageId),
    /// The user's private reply under a message: a note about it, which
    /// only their devices hold.
    Private(Address),
    Pending(u64),
    Gap(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Class {
    Crumb,
    From,
    Time,
    Fold,
    Pending,
    Quote,
    /// A thread's name in a root's list.
    Name,
    /// A name with something unread under it, and the count.
    Unread,
    /// Unread mentions of the user.
    Mention,
    /// A private reply: the user's own, seen by no agent.
    Private,
    /// A web link in a message: Enter on it opens it as a page.
    Link,
}

impl Class {
    const COUNT: usize = 11;

    fn slot(self) -> usize {
        match self {
            Self::Crumb => 0,
            Self::From => 1,
            Self::Time => 2,
            Self::Fold => 3,
            Self::Pending => 4,
            Self::Quote => 5,
            Self::Name => 6,
            Self::Unread => 7,
            Self::Mention => 8,
            Self::Private => 9,
            Self::Link => 10,
        }
    }
}

impl rho_transcript::Style for Class {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let (color, weight) = match self {
            Self::Crumb | Self::Time | Self::Pending => (colors.text_muted, None),
            Self::From => (colors.text_accent, Some(gpui::FontWeight::BOLD)),
            Self::Fold => (cx.theme().status().info, None),
            Self::Quote => (colors.text_muted, None),
            Self::Name => (colors.text, None),
            Self::Unread => (colors.text, Some(gpui::FontWeight::BOLD)),
            Self::Mention => (cx.theme().status().warning, Some(gpui::FontWeight::BOLD)),
            Self::Private => (colors.text_muted, None),
            Self::Link => (colors.text_accent, None),
        };
        HighlightStyle {
            color: Some(color.into()),
            font_weight: weight,
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
    Mail(MessageId),
    /// One quoted block, by first line in its mail's body.
    Quote(QuoteId),
    /// A project row or a tangent below a mail.
    Thread(MessageId),
    Open(Address),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct QuoteId {
    mail: MessageId,
    first_line: usize,
}

impl Meta {
    fn mail(&self) -> Option<MessageId> {
        match *self {
            Self::Mail(id) => Some(id),
            Self::Quote(quote) => Some(quote.mail),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focused {
    Log,
    Composer,
}

/// A thread the user just started in a channel, which the cursor follows
/// until it stands in the list: by the id this client gave the message
/// while it is on its way, then by its own once the host logged it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Follow {
    Sending(u64),
    Logged(MessageId),
}

pub struct ThreadView {
    system: Entity<ChatSystem>,
    thread: ThreadId,
    transcript: Transcript<Key, Class, Meta>,
    log: Entity<Editor>,
    composer: Entity<Editor>,
    focused: Focused,
    follow: Option<Follow>,
    /// The mail dealt to this view, also the target when the cursor is
    /// between mails rather than on a different writer's mail.
    dealt_mail: Option<MessageId>,
    reply_to: Option<MessageId>,
    expanded_quotes: HashSet<QuoteId>,
    _subscriptions: Vec<Subscription>,
    /// Reads a project's list again when a thread's age next changes.
    _ages: Option<Task<()>>,
}

/// How often the ages in a channel's list move: their unit is the minute.
const LIST_AGES_EVERY: SignedDuration = SignedDuration::from_secs(60);

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
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                buffer.clone(),
                [Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });
        let log = cx.new(|cx| {
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
            editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            editor
        });
        let draft = system.draft(thread.host, Some(thread.message)).to_owned();
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 10, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            if !draft.is_empty() {
                editor.set_text(draft, window, cx);
            }
            editor
        });
        let subscriptions = vec![
            cx.observe(&entity, |this, _, cx| this.refresh(cx)),
            cx.subscribe(&entity, |this, _, event: &Event, cx| {
                if let Event::Delivered {
                    endpoint,
                    client_id,
                    message,
                } = event
                    && *endpoint == this.thread.host
                    && this.follow == Some(Follow::Sending(*client_id))
                {
                    this.follow = Some(Follow::Logged(*message));
                    this.refresh(cx);
                }
            }),
            cx.subscribe(&composer, |this, composer, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = composer.read(cx).text(cx);
                    let thread = this.thread;
                    this.system.update(cx, |system, _| {
                        system.set_draft(thread.host, Some(thread.message), text);
                    });
                }
            }),
        ];
        let mut view = Self {
            system: entity,
            thread,
            transcript: Transcript::new(buffer),
            log,
            composer,
            focused: Focused::Log,
            follow: None,
            dealt_mail: None,
            reply_to: None,
            expanded_quotes: HashSet::new(),
            _subscriptions: subscriptions,
            _ages: None,
        };
        view.transcript.attach(&view.log.clone(), cx);
        let items = items(
            system,
            thread,
            view.is_list(system),
            &view.expanded_quotes,
            cx,
        );
        view.transcript.reconcile(items, cx);
        if is_project(system, thread) {
            // The list reads from the top.
            view.cursor_to_first_thread(cx);
        } else {
            view.cursor_to_end(cx);
        }
        // What is unread and the user's private replies come from the
        // ledger, which is the frame's and is leased while it opens this:
        // the rows are read again as soon as that is over, before the
        // first draw. Not `defer_in`: that runs inside the window's update,
        // where the frame cannot be read.
        let this = cx.weak_entity();
        let list = view.is_list(system);
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if list {
                    this.cursor_to_first_thread(cx);
                }
            });
        });
        view
    }

    /// The rows as they stand, reconciled into the buffer. A reader at the
    /// end stays at the end as messages arrive.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let list = self.is_list(self.system.read(cx));
        let at_end = {
            let last = self.transcript.buffer().read(cx).max_point().row;
            self.cursor_row(cx) >= last
        };
        let items = items(
            self.system.read(cx),
            self.thread,
            list,
            &self.expanded_quotes,
            cx,
        );
        self.transcript.reconcile(items, cx);
        if at_end && !list {
            self.cursor_to_end(cx);
        }
        if let Some(Follow::Logged(message)) = self.follow
            && self.cursor_to_item(&Key::Thread(message), Autoscroll::fit(), cx)
        {
            self.follow = None;
        }
        // A draft the system sent on the user's behalf (`send and start`)
        // leaves the composer: its words are in the thread now.
        let draft_gone = self
            .system
            .read(cx)
            .draft(self.thread.host, Some(self.thread.message))
            .is_empty();
        if draft_gone && !self.composer.read(cx).text(cx).is_empty() {
            self.clear_composer(cx);
        }
        self.schedule_ages(cx);
        cx.notify();
    }

    /// Reads the thread again when an age it shows next says another.
    fn schedule_ages(&mut self, cx: &mut Context<Self>) {
        let system = self.system.read(cx);
        let list = self.is_list(system)
            && system
                .chat(self.thread.host)
                .is_some_and(|chat| !chat.children(Some(self.thread.message)).is_empty());
        self._ages = list.then(|| {
            let wait = std::time::Duration::try_from(LIST_AGES_EVERY).expect("positive interval");
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |view, cx| view.refresh(cx));
            })
        });
    }

    /// Empties the composer without a window: the edit goes to its buffer.
    fn clear_composer(&self, cx: &mut Context<Self>) {
        let Some(buffer) = self.composer.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, "")], None, cx);
        });
    }

    fn cursor_row(&self, cx: &App) -> u32 {
        self.cursor(cx).row
    }

    fn cursor(&self, cx: &App) -> Point {
        let editor = self.log.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.selections.newest_anchor().head().to_point(&snapshot)
    }

    fn is_list(&self, system: &ChatSystem) -> bool {
        is_project(system, self.thread)
    }

    fn current_reply_target(&self, cx: &App) -> Option<MessageId> {
        if self.is_list(self.system.read(cx)) {
            return None;
        }
        let chat = self.system.read(cx).chat(self.thread.host)?;
        let point = self.transcript.line_meta(self.cursor_row(cx), cx);
        reply_target(point, self.dealt_mail, |id| chat.message(id))
    }

    /// The web link the cursor is on, if it is on one.
    fn cursor_link(&self, cx: &App) -> Option<url::Url> {
        let cursor = self.cursor(cx);
        let buffer = self.transcript.buffer().read(cx);
        let start = buffer.point_to_offset(Point::new(cursor.row, 0));
        let end = buffer.point_to_offset(Point::new(cursor.row, buffer.line_len(cursor.row)));
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
        Some(buffer.offset_to_point(offset).row)
    }

    fn cursor_to_item(&mut self, key: &Key, scroll: Autoscroll, cx: &mut Context<Self>) -> bool {
        let Some(row) = self.item_row(key, cx) else {
            return false;
        };
        self.cursor_to_row(row, scroll, cx);
        true
    }

    fn cursor_to_row(&mut self, row: u32, scroll: Autoscroll, cx: &mut Context<Self>) {
        self.log.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            editor.request_autoscroll(scroll, cx);
            cx.notify();
        });
    }

    /// The first thread of a root's list, if it has one.
    fn cursor_to_first_thread(&mut self, cx: &mut Context<Self>) {
        let rows = self.transcript.buffer().read(cx).max_point().row;
        let Some(row) = (0..=rows)
            .find(|row| matches!(self.transcript.line_meta(*row, cx), Some(Meta::Thread(_))))
        else {
            return;
        };
        self.log.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    fn cursor_to_end(&mut self, cx: &mut Context<Self>) {
        let row = self.transcript.buffer().read(cx).max_point().row;
        self.cursor_to_row(row, Autoscroll::fit(), cx);
    }
}

/// The rows of `thread` as `system` holds it.
fn items(
    system: &ChatSystem,
    thread: ThreadId,
    list: bool,
    expanded_quotes: &HashSet<QuoteId>,
    cx: &App,
) -> Vec<Item<Key, Class, Meta>> {
    let host = thread.host;
    let Some(chat) = system.chat(host) else {
        return vec![line(
            Key::Gap(0),
            "this host's chat is not held here",
            Class::Crumb,
        )];
    };
    let Some(head) = chat.thread(thread.message) else {
        return vec![line(Key::Gap(0), "this thread is gone", Class::Crumb)];
    };
    let mut items = Vec::new();
    let mut gap = 0u32;
    let mut gap_item = |items: &mut Vec<Item<Key, Class, Meta>>| {
        items.push(Item::new(Key::Gap(gap), "").with_lines(vec![Meta::None]));
        gap += 1;
    };
    // Where the thread is: every thread above it, the nearest last.
    // The heading already names the project. Keep only real ancestors,
    // such as the mail a tangent hangs under.
    let first = usize::from(chat.is_project(head.path[0].0));
    if head.path.len() > first + 1 {
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
        items.push(
            line(Key::Crumb, &text, Class::Crumb).with_lines(vec![Meta::Open(parent.address())]),
        );
    }
    // The user's private replies, by the message each is under. Read
    // once: the frame is leased while it opens this surface, and then the
    // next draw has them.
    let mut private = system
        .frame
        .read(cx, |frame, _| {
            private_items(frame.marks(), chat, host, thread.message)
        })
        .unwrap_or_default();
    let project = system.project_of(thread);
    if let Some(message) = chat.message(thread.message)
        && !list
    {
        items.extend(message_item(message, &project, expanded_quotes));
        items.extend(private.remove(&thread.message).unwrap_or_default());
    }
    // A root's list starts at its first thread: nothing stands above it
    // to keep a gap from.
    if !items.is_empty() {
        gap_item(&mut items);
    }
    if list {
        let rows = system.frame.read(cx, |frame, _| {
            thread_rows(system, chat, host, thread.message, Some(frame.marks()))
        });
        let rows = rows.unwrap_or_else(|| thread_rows(system, chat, host, thread.message, None));
        if rows.is_empty() {
            items.push(line(
                Key::Gap(u32::MAX),
                "no threads yet · the composer starts one",
                Class::Crumb,
            ));
        }
        items.extend(rows);
        gap_item(&mut items);
        items.extend(pending_items(system, host, thread.message, &mut gap_item));
        return without_trailing_gap(items);
    }
    for id in chat.children(Some(thread.message)) {
        let Some(message) = chat.message(*id) else {
            continue;
        };
        items.extend(message_item(message, &project, expanded_quotes));
        items.extend(private.remove(id).unwrap_or_default());
        let replies = chat.children(Some(*id));
        if !replies.is_empty() {
            let (_, at) = chat.newest_in(*id).unwrap_or((*id, message.at));
            let text = format!(
                "  ↳ {} {} · {} · {}",
                replies.len(),
                match replies.len() {
                    1 => "reply",
                    _ => "replies",
                },
                chat.name(*id).unwrap_or_default(),
                thread::when(at.0 as i64),
            );
            items
                .push(line(Key::Fold(*id), &text, Class::Fold).with_lines(vec![Meta::Thread(*id)]));
        }
        gap_item(&mut items);
    }
    items.extend(pending_items(system, host, thread.message, &mut gap_item));
    without_trailing_gap(items)
}

/// A gap stands between rows, never after the last: a row with nothing on
/// it is nowhere for the cursor to be.
fn without_trailing_gap(mut items: Vec<Item<Key, Class, Meta>>) -> Vec<Item<Key, Class, Meta>> {
    if items
        .last()
        .is_some_and(|item| matches!(item.key, Key::Gap(_)))
    {
        items.pop();
    }
    items
}

/// The user's private replies under the thread's head and under each
/// message in it: the notes about each, as the user's own lines, marked
/// so and dim. Enter opens the note to edit it.
fn private_items(
    marks: &Marks,
    chat: &HostChat,
    host: EndpointId,
    head: MessageId,
) -> std::collections::HashMap<MessageId, Vec<Item<Key, Class, Meta>>> {
    let mut by_message = std::collections::HashMap::new();
    for id in std::iter::once(&head).chain(chat.children(Some(head))) {
        let address = ThreadId { host, message: *id }.address();
        let replies: Vec<Item<Key, Class, Meta>> = marks
            .replies_to(&address)
            .into_iter()
            .map(|(note, held)| {
                let time = held.written_ms.map(thread::when).unwrap_or_default();
                let header = format!("🔒 you · {time}");
                let body = held.body.as_deref().unwrap_or_default().trim_end();
                let text = format!("\n{header}\n{body}");
                let aside = 1..text.len();
                let lines = std::iter::once(Meta::None)
                    .chain(text.lines().skip(1).map(|_| Meta::Open(note.clone())))
                    .collect();
                Item::new(Key::Private(note.clone()), text)
                    .with_styles(vec![(Class::Private, aside)])
                    .with_lines(lines)
            })
            .collect();
        if !replies.is_empty() {
            by_message.insert(*id, replies);
        }
    }
    by_message
}

/// Whether this place lists a project's threads rather than reading mail.
fn is_project(system: &ChatSystem, thread: ThreadId) -> bool {
    system
        .chat(thread.host)
        .is_some_and(|chat| chat.is_project(thread.message))
}

/// The user's messages into `parent` that no host has logged yet.
fn pending_items(
    system: &ChatSystem,
    host: EndpointId,
    parent: MessageId,
    gap_item: &mut impl FnMut(&mut Vec<Item<Key, Class, Meta>>),
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = Vec::new();
    for outgoing in system.waiting_under(host, Some(parent)) {
        let id = outgoing.at.0;
        let mut text = "you · sending…\n".to_owned();
        text.push_str(&body(&outgoing.content));
        let header = text.find('\n').unwrap_or(text.len());
        let lines = text.lines().count().max(1);
        items.push(
            Item::new(Key::Pending(id), text)
                .with_styles(vec![(Class::Pending, 0..header)])
                .with_lines(vec![Meta::None; lines]),
        );
        gap_item(&mut items);
    }
    items
}

/// A root's threads, one line each: the name and what waits in it, with the
/// unread ones first, a mention of the user before a plain unread, and each
/// group newest first. The words are in the thread; the list is for choosing
/// one. Without `marks` nothing is known to be unread.
fn thread_rows(
    system: &ChatSystem,
    chat: &HostChat,
    host: EndpointId,
    root: MessageId,
    marks: Option<&Marks>,
) -> Vec<Item<Key, Class, Meta>> {
    struct Line {
        id: MessageId,
        at: u64,
        mentions: usize,
        unread: usize,
        text: String,
        styles: Vec<(Class, Range<usize>)>,
    }
    let now = Timestamp::now();
    let mut lines = Vec::new();
    for id in chat.children(Some(root)) {
        let Some(message) = chat.message(*id) else {
            continue;
        };
        let thread = ThreadId { host, message: *id };
        let unread = marks.map_or_else(Vec::new, |marks| system.unread_ids_in(chat, thread, marks));
        let mentions = unread
            .iter()
            .filter(|id| {
                chat.message(**id)
                    .is_some_and(|message| message.mentions(Party::User).is_some())
            })
            .count();
        let (_, at) = chat.newest_in(*id).unwrap_or((*id, message.at));
        let mut text = chat.name(*id).unwrap_or_default();
        let name_class = match unread.is_empty() {
            true => Class::Name,
            false => Class::Unread,
        };
        let mut styles = vec![(name_class, 0..text.len())];
        styles.extend(links(&text).into_iter().map(|link| (Class::Link, link)));
        let mut waiting: Vec<(String, Class)> = Vec::new();
        if mentions > 0 {
            waiting.push((format!("@{mentions}"), Class::Mention));
        }
        if !unread.is_empty() {
            waiting.push((format!("{} new", unread.len()), Class::Unread));
        }
        let replies = chat.children(Some(*id)).len();
        if replies > 0 {
            let word = match replies {
                1 => "reply",
                _ => "replies",
            };
            waiting.push((format!("{replies} {word}"), Class::Time));
        }
        if let Some(status) = system.status(thread) {
            waiting.push((status.text.clone(), Class::Time));
        }
        waiting.push((ago(at.0, now), Class::Time));
        for (index, (word, class)) in waiting.into_iter().enumerate() {
            text.push_str(match index {
                0 => "  ",
                _ => " · ",
            });
            let start = text.len();
            text.push_str(&word);
            styles.push((class, start..text.len()));
        }
        lines.push(Line {
            id: *id,
            at: at.0,
            mentions,
            unread: unread.len(),
            text,
            styles,
        });
    }
    lines.sort_by_key(|line| {
        (
            std::cmp::Reverse(line.mentions > 0),
            std::cmp::Reverse(line.unread > 0),
            std::cmp::Reverse(line.at),
        )
    });
    lines
        .into_iter()
        .map(|line| {
            Item::new(Key::Thread(line.id), line.text)
                .with_styles(line.styles)
                .with_lines(vec![Meta::Thread(line.id)])
        })
        .collect()
}

impl ThreadView {
    /// In a channel, the idea goes to the agents system, which sends it
    /// and starts an agent from it; in a thread, the reply goes out.
    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_list(self.system.read(cx)) {
            return self.send_only(&SendOnly, window, cx);
        }
        if self.composer.read(cx).text(cx).trim().is_empty() {
            return;
        }
        // The keys go back to the messages, as `escape` takes them: the
        // next `i` writes a new message, never into this one.
        self.focus_log(&FocusLog, window, cx);
        let thread = self.thread;
        self.system.update(cx, |_, cx| {
            cx.emit(Event::Idea {
                endpoint: thread.host,
                parent: thread.message,
                source: super::IdeaSource::ChannelDraft,
            });
        });
    }

    fn send_only(&mut self, _: &SendOnly, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        let thread = self.thread;
        let list = self.is_list(self.system.read(cx));
        let reply_to = match self.focused {
            Focused::Composer => self.reply_to.take(),
            Focused::Log => self.current_reply_target(cx),
        };
        let client_id = self.system.update(cx, |system, cx| {
            let client_id = match reply_to {
                Some(mail) => system.reply(thread, mail, text, cx),
                None => system.post(thread.host, Some(thread.message), text, Vec::new(), cx),
            };
            system.set_draft(thread.host, Some(thread.message), String::new());
            client_id
        });
        self.composer
            .update(cx, |composer, cx| composer.clear(window, cx));
        self.focus_log(&FocusLog, window, cx);
        self.dealt_mail = None;
        self.refresh(cx);
        self.cursor_to_end(cx);
        // In a channel the message is a new thread: the cursor goes to it in
        // the list, on its way and then where the list puts it, so `enter`
        // opens it.
        if list {
            self.follow = Some(Follow::Sending(client_id));
        }
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        if self.focused != Focused::Composer {
            self.reply_to = self.current_reply_target(cx);
        }
        self.focused = Focused::Composer;
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    fn focus_log(&mut self, _: &FocusLog, window: &mut Window, cx: &mut Context<Self>) {
        self.focused = Focused::Log;
        window.focus(&self.log.focus_handle(cx), cx);
        cx.notify();
    }

    /// Enter opens the thing under the point: a quote, link, tangent or crumb.
    fn open_row(&mut self, _: &OpenRow, _window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_row(cx);
        let meta = self.transcript.line_meta(cursor, cx).cloned();
        if let Some(Meta::Quote(quote)) = meta
            && !self.expanded_quotes.contains(&quote)
        {
            self.toggle_quote(quote, cx);
            return;
        }
        if let Some(url) = self.cursor_link(cx) {
            self.system.read(cx).frame.clone().open_link(url, cx);
            return;
        }
        if let Some(Meta::Quote(quote)) = meta {
            self.toggle_quote(quote, cx);
            return;
        }
        let host = self.thread.host;
        let address = match meta {
            Some(Meta::Thread(id) | Meta::Mail(id)) if id != self.thread.message => {
                ThreadId { host, message: id }.address()
            }
            Some(Meta::Open(address)) => address,
            _ => return,
        };
        self.system.read(cx).frame.clone().open(address, cx);
    }

    fn toggle_quote(&mut self, quote: QuoteId, cx: &mut Context<Self>) {
        if !self.expanded_quotes.remove(&quote) {
            self.expanded_quotes.insert(quote);
        }
        self.refresh(cx);
        if let Some(start) = self.item_row(&Key::Message(quote.mail), cx)
            && let Some(row) = (start..)
                .map_while(|row| self.transcript.line_meta(row, cx).map(|meta| (row, meta)))
                .take_while(|(_, meta)| meta.mail() == Some(quote.mail))
                .find_map(|(row, meta)| (*meta == Meta::Quote(quote)).then_some(row))
        {
            self.cursor_to_row(row, Autoscroll::fit(), cx);
        }
    }
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

/// A message's text, with each picture named.
fn body(content: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: words } => text.push_str(words),
            ContentPart::Image { media_type, .. } => {
                text.push_str(&format!("[image · {media_type}]"))
            }
        }
        text.push('\n');
    }
    text.trim_end().to_owned()
}

/// An explicitly selected incoming mail overrides the dealt mail. From a
/// gap (or the user's own text), a reply still answers the dealt mail.
fn reply_target<'a>(
    point: Option<&Meta>,
    dealt: Option<MessageId>,
    mail: impl Fn(MessageId) -> Option<&'a Message>,
) -> Option<MessageId> {
    point
        .and_then(Meta::mail)
        .into_iter()
        .chain(dealt)
        .find(|id| {
            mail(*id)
                .is_some_and(|mail| mail.from != Party::User && mail.revision != Revision::Deleted)
        })
}

fn message_item(
    message: &Message,
    project: &str,
    expanded_quotes: &HashSet<QuoteId>,
) -> Option<Item<Key, Class, Meta>> {
    let edited = match message.revision {
        Revision::Original => "",
        Revision::Edited => " · edited",
        Revision::Deleted => return None,
    };
    let from = match message.from {
        Party::User => "you",
        Party::Agent(_) | Party::System { .. } => project,
    };
    let time = thread::when(message.at.0 as i64);
    let mut text = format!("{from} · {time}{edited}");
    let mut styles = vec![
        (Class::From, 0..from.len()),
        (Class::Time, from.len()..text.len()),
    ];
    let mut lines = vec![Meta::Mail(message.id)];
    let body = body(&message.content);
    let mut body_lines = body.lines().enumerate().peekable();
    while let Some((first_line, line)) = body_lines.next() {
        text.push('\n');
        let start = text.len();
        text.push_str(line);
        if line.starts_with("> ") {
            let quote = QuoteId {
                mail: message.id,
                first_line,
            };
            let expanded = expanded_quotes.contains(&quote);
            let mut more = false;
            while body_lines
                .peek()
                .is_some_and(|(_, line)| line.starts_with("> "))
            {
                let (_, quoted) = body_lines.next().expect("peeked a quote line");
                more = true;
                if expanded {
                    text.push('\n');
                    text.push_str(quoted);
                    lines.push(Meta::Quote(quote));
                }
            }
            if more && !expanded {
                text.push_str(" …");
            }
            styles.push((Class::Quote, start..text.len()));
            lines.push(Meta::Quote(quote));
        } else {
            lines.push(Meta::Mail(message.id));
        }
    }
    styles.extend(links(&text).into_iter().map(|link| (Class::Link, link)));
    Some(
        Item::new(Key::Message(message.id), text)
            .with_styles(styles)
            .with_lines(lines),
    )
}

impl Surface for ThreadView {
    fn title(&self, cx: &App) -> String {
        let system = self.system.read(cx);
        let project = system.project_of(self.thread);
        if self.is_list(system) {
            return project;
        }
        let subject = system
            .chat(self.thread.host)
            .and_then(|chat| chat.name(self.thread.message))
            .unwrap_or_default();
        match subject.is_empty() {
            true => project,
            false => format!("{project} › {subject}"),
        }
    }

    fn show_card(&mut self, card: &Card, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(mail) = cards::attention_mail(&card.cursor) else {
            return;
        };
        self.dealt_mail = Some(mail);
        self.focused = Focused::Log;
        self.cursor_to_item(&Key::Message(mail), Autoscroll::focused(), cx);
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(match self.focused {
            Focused::Log => self.log.clone(),
            Focused::Composer => self.composer.clone(),
        })
    }

    /// The message under the point, as the thread it anchors: any message
    /// is what an agent starts from or a name goes on. The thread itself
    /// from its head, the composer, or nothing in particular.
    fn subject(&self, cx: &App) -> Subject {
        if self.focused == Focused::Composer {
            return Subject::Own;
        }
        line_subject(
            self.transcript.line_meta(self.cursor_row(cx), cx),
            self.thread,
        )
    }

    /// The thread the subject is in, as the dealer counts threads: in a
    /// thread, the thread wherever the point is in it; in a channel, the
    /// thread under the point, as a row is on Home.
    fn card(&self, cx: &App) -> Subject {
        let system = self.system.read(cx);
        if !self.is_list(system) {
            return Subject::Own;
        }
        self.subject(cx)
    }

    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        if !self.is_list(self.system.read(cx)) {
            let send = this.clone();
            let mut rows = vec![Row::run("ctrl-enter", "send", move |_, window, cx| {
                send.update(cx, |view, cx| view.send_only(&SendOnly, window, cx));
            })];
            let system = self.system.read(cx);
            let members = system
                .chat(self.thread.host)
                .map(|chat| chat.members_under(Some(self.thread.message)))
                .unwrap_or_default();
            let mut agents: Vec<_> = members
                .into_iter()
                .filter_map(|id| {
                    let participant = system.participant(self.thread.host, id, cx)?;
                    let state = participant.doing.map(|doing| doing.activity.word());
                    Some((id, participant.label, state))
                })
                .collect();
            // The prompt groups the choices by their state, then by name.
            agents.sort_by(|a, b| (a.2, &a.1, a.0).cmp(&(b.2, &b.1, b.0)));
            match agents.as_slice() {
                [] => {}
                [(id, _, _)] => {
                    let agent = AgentRef(*id).address();
                    rows.push(Row::run(
                        "",
                        "look into the work",
                        move |frame, window, cx| {
                            frame.open(agent.clone(), window, cx);
                        },
                    ));
                }
                _ => rows.push(Row::run(
                    "",
                    "look into the work",
                    move |frame, window, cx| {
                        // Host labels need not be unique. Only duplicate labels
                        // gain an id in the chooser so each choice routes
                        // exactly.
                        let choices: Vec<_> = agents
                            .iter()
                            .map(|(id, name, state)| {
                                let value =
                                    if agents.iter().filter(|(_, other, _)| other == name).count()
                                        > 1
                                    {
                                        format!("{name} · {}", id.encoded())
                                    } else {
                                        name.clone()
                                    };
                                (
                                    value,
                                    state.unwrap_or("state unknown"),
                                    AgentRef(*id).address(),
                                )
                            })
                            .collect();
                        let suggestions = choices.clone();
                        frame.open_prompt(
                            "look into the work:",
                            Rc::new(move |_, query, _| {
                                let query = query.to_lowercase();
                                suggestions
                                    .iter()
                                    .filter(|(name, state, _)| {
                                        name.to_lowercase().contains(&query)
                                            || state.contains(&query)
                                    })
                                    .map(|(name, state, _)| Candidate {
                                        value: name.clone(),
                                        description: (*state).to_owned(),
                                    })
                                    .collect()
                            }),
                            Rc::new(move |frame, input, window, cx| {
                                if let Some((_, _, address)) =
                                    choices.iter().find(|(name, _, _)| name == input.trim())
                                {
                                    frame.open(address.clone(), window, cx);
                                } else {
                                    frame.echo("no such agent", StyleClass::SystemInfo, cx);
                                }
                            }),
                            window,
                            cx,
                        );
                    },
                )),
            }
            // The thumb's Open action mirrors Enter, including on a quote.
            if self.focused == Focused::Log
                && matches!(
                    self.transcript.line_meta(self.cursor_row(cx), cx),
                    Some(Meta::Quote(_) | Meta::Thread(_) | Meta::Open(_))
                )
            {
                rows.push(Row::run("enter", "open", move |_, window, cx| {
                    this.update(cx, |view, cx| view.open_row(&OpenRow, window, cx));
                }));
            }
            return rows;
        }
        let only = this.clone();
        let send_only = Row::run("ctrl-shift-enter", "send", move |_, window, cx| {
            only.update(cx, |view, cx| view.send_only(&SendOnly, window, cx));
        });
        vec![
            Row::run(
                "ctrl-enter",
                "send and start agent",
                move |_, window, cx| {
                    this.update(cx, |view, cx| view.send(&SendMessage, window, cx));
                },
            ),
            send_only,
        ]
    }
}

impl Focusable for ThreadView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.focused {
            Focused::Log => self.log.focus_handle(cx),
            Focused::Composer => self.composer.focus_handle(cx),
        }
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The keyboard moving by mouse or by the frame counts as much as
        // by key: whichever editor has it is the one the chrome reads.
        if self.composer.focus_handle(cx).is_focused(window) {
            if self.focused != Focused::Composer {
                self.reply_to = self.current_reply_target(cx);
            }
            self.focused = Focused::Composer;
        } else if self.log.focus_handle(cx).is_focused(window) {
            self.focused = Focused::Log;
        }
        let list = self.is_list(self.system.read(cx));
        let hint = match (list, Layout::of(window.viewport_size().width)) {
            (false, Layout::Phone) => "reply",
            (true, Layout::Phone) => "idea",
            (false, Layout::Desktop) => thread::PLACEHOLDER,
            (true, Layout::Desktop) => thread::CHANNEL_PLACEHOLDER,
        };
        let text_style = self.composer.update(cx, |composer, cx| {
            if composer.placeholder_text(cx).as_deref() != Some(hint) {
                composer.set_placeholder_text(hint, window, cx);
            }
            composer.style(cx).text.clone()
        });
        let colors = cx.theme().colors();
        let system = self.system.read(cx);
        let status = (!list)
            .then(|| {
                system
                    .status(self.thread)
                    .map(|status| status.text.clone())
                    .or_else(|| {
                        system
                            .failed_start(self.thread)
                            .and_then(|reason| reason.lines().next())
                            .map(|reason| format!("did not start: {reason}"))
                    })
            })
            .flatten();
        div()
            .key_context(thread::SCREEN)
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::send_only))
            .on_action(cx.listener(Self::focus_composer))
            .on_action(cx.listener(Self::focus_log))
            .on_action(cx.listener(Self::open_row))
            .child(
                div()
                    .key_context(thread::LOG)
                    .flex_1()
                    .min_h_0()
                    .child(self.log.clone()),
            )
            .children(status.map(|status| {
                div()
                    .id("thread-status")
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .truncate()
                    .font_family(text_style.font_family)
                    .font_weight(text_style.font_weight)
                    .text_size(text_style.font_size)
                    .line_height(text_style.line_height)
                    .text_color(colors.text_muted)
                    .child(status)
            }))
            .child(
                div()
                    .key_context(thread::COMPOSER)
                    .flex_none()
                    .w_full()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .px_2()
                    .py_1()
                    .child(self.composer.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::AgentId;

    use super::*;

    fn mail(from: Party, revision: Revision, text: &str) -> Message {
        Message {
            id: MessageId(rho_agent_types::ChatSeq(13)),
            parent: None,
            reply_to: None,
            at: rho_agent_types::UnixMs(1_760_000_000_000),
            from,
            mentions: Vec::new(),
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
            Revision::Edited,
            "cargo exited 101",
        );
        let user = mail(Party::User, Revision::Original, "Go ahead.");
        for (mail, from, marker) in [
            (&original, "tide@lab", false),
            (&bot, "tide@lab", true),
            (&user, "you", false),
        ] {
            let item = message_item(mail, "tide@lab", &HashSet::new()).unwrap();
            let header = item.text.lines().next().unwrap();
            assert!(header.starts_with(&format!("{from} · ")), "{header}");
            assert_eq!(header.ends_with(" · edited"), marker);
        }
        let deleted = Message {
            revision: Revision::Deleted,
            ..original
        };
        assert!(message_item(&deleted, "tide@lab", &HashSet::new()).is_none());
    }

    #[test]
    fn replies_target_the_selected_incoming_mail_or_fall_back_to_the_dealt_ask() {
        let agent = AgentId::from_counter(19, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let ask = mail(Party::Agent(agent), Revision::Original, "Ship it?");
        let next = Message {
            id: MessageId(rho_agent_types::ChatSeq(24)),
            ..mail(
                Party::Agent(agent),
                Revision::Edited,
                "Use the release branch.",
            )
        };
        let user = Message {
            id: MessageId(rho_agent_types::ChatSeq(25)),
            ..mail(Party::User, Revision::Original, "Not yet.")
        };
        let removed = Message {
            id: MessageId(rho_agent_types::ChatSeq(26)),
            ..mail(Party::Agent(agent), Revision::Deleted, "Removed.")
        };
        let lookup = |id| {
            [&ask, &next, &user, &removed]
                .into_iter()
                .find(|message| message.id == id)
        };
        assert_eq!(reply_target(None, Some(ask.id), lookup), Some(ask.id));
        assert_eq!(
            reply_target(Some(&Meta::Mail(user.id)), Some(ask.id), lookup),
            Some(ask.id)
        );
        let quote = Meta::Quote(QuoteId {
            mail: next.id,
            first_line: 2,
        });
        assert_eq!(
            reply_target(Some(&quote), Some(ask.id), lookup),
            Some(next.id)
        );
        assert_eq!(
            reply_target(Some(&Meta::Mail(removed.id)), Some(ask.id), lookup),
            Some(ask.id)
        );
        assert_eq!(reply_target(Some(&Meta::Mail(user.id)), None, lookup), None);
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
        let folded = message_item(&message, "tide@lab", &HashSet::new()).unwrap();
        let display = |text: &str| text.split_once('\n').unwrap().1.to_owned();
        assert_eq!(
            display(&folded.text),
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
        let expanded = message_item(&message, "tide@lab", &HashSet::from([first])).unwrap();
        assert_eq!(
            display(&expanded.text),
            "preface\n> façade first\n> second\nCan this ship?\n> another quote …\n",
        );
        assert_eq!(
            quote_text(&expanded),
            ["> façade first\n> second", "> another quote …"]
        );
        assert_eq!(
            &expanded.lines[2..],
            &[
                Meta::Quote(first),
                Meta::Quote(first),
                Meta::Mail(message.id),
                Meta::Quote(second)
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
            line_subject(Some(&Meta::Mail(thread.message)), thread),
            Subject::Own
        );
        assert_eq!(
            line_subject(Some(&Meta::Mail(child)), thread),
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
