//! A thread: the message it hangs under, what was said under it, where it
//! is, who is in it, and a composer.
//!
//! A channel is the same surface over a root: its messages are the threads
//! under it, each with its replies folded under it.

use std::ops::Range;

use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Subscription, Task, Window, div,
};
use jiff::Timestamp;
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_agent_types::{Address as Party, AgentId, ContentPart, EndpointId, MessageId};
use rho_dealer::{Address, Addressed as _, Marks};
use rho_frame::thread::{self, FocusComposer, FocusLog, OpenRow, SendMessage};
use rho_frame::{Row, Subject, Surface};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use super::{ChatSystem, ago, when};
use crate::{AgentRef, HostChat, ThreadId};

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
    Participants,
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
    Participants,
    /// A thread's name in a root's list.
    Name,
    /// A name with something unread under it, and the count.
    Unread,
    /// Unread mentions of the user.
    Mention,
    /// A private reply: the user's own, seen by no agent.
    Private,
    /// What an agent in the `with` line is doing when it is a stop it did
    /// not choose.
    Stopped,
    /// A web link in a message: Enter on it opens it as a page.
    Link,
}

impl Class {
    const COUNT: usize = 12;

    fn slot(self) -> usize {
        match self {
            Self::Crumb => 0,
            Self::From => 1,
            Self::Time => 2,
            Self::Fold => 3,
            Self::Pending => 4,
            Self::Participants => 5,
            Self::Name => 6,
            Self::Unread => 7,
            Self::Mention => 8,
            Self::Private => 9,
            Self::Stopped => 10,
            Self::Link => 11,
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
            Self::Participants => (colors.text_muted, None),
            Self::Name => (colors.text, None),
            Self::Unread => (colors.text, Some(gpui::FontWeight::BOLD)),
            Self::Mention => (cx.theme().status().warning, Some(gpui::FontWeight::BOLD)),
            Self::Private => (colors.text_muted, None),
            Self::Stopped => (cx.theme().status().warning, None),
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
    /// A message under the thread: Enter opens the thread under it.
    Message(MessageId),
    Open(Address),
    /// The `with` line: where on it each agent is named, in order.
    Agents(Vec<(Range<usize>, AgentId)>),
}

/// The agent of the `with` line under `column`: the one named there, or
/// the nearest before it, or the first.
fn agent_at(agents: &[(Range<usize>, AgentId)], column: usize) -> Option<AgentId> {
    agents
        .iter()
        .rev()
        .find(|(span, _)| span.start <= column)
        .or(agents.first())
        .map(|(_, agent)| *agent)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focused {
    Log,
    Composer,
}

pub struct ThreadView {
    system: Entity<ChatSystem>,
    thread: ThreadId,
    transcript: Transcript<Key, Class, Meta>,
    log: Entity<Editor>,
    composer: Entity<Editor>,
    focused: Focused,
    _subscriptions: Vec<Subscription>,
    /// Reads the thread again each minute while agents are in it: the
    /// `with` line says for how long each has been doing what it does.
    _ages: Task<()>,
}

/// How often the ages on the `with` line move: their unit is the minute.
const AGES_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

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
            editor.set_placeholder_text(thread::PLACEHOLDER, window, cx);
            if !draft.is_empty() {
                editor.set_text(draft, window, cx);
            }
            editor
        });
        let subscriptions = vec![
            cx.observe(&entity, |this, _, cx| this.refresh(cx)),
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
            _subscriptions: subscriptions,
            _ages: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AGES_EVERY).await;
                    let alive = this.update(cx, |view, cx| {
                        let thread = view.thread;
                        let with_agents =
                            view.system.read(cx).chat(thread.host).is_some_and(|chat| {
                                !chat.members_under(Some(thread.message)).is_empty()
                            });
                        if with_agents {
                            view.refresh(cx);
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }),
        };
        view.transcript.attach(&view.log.clone(), cx);
        let items = items(system, thread, cx);
        view.transcript.reconcile(items, cx);
        if is_root(system, thread) {
            // The list reads from the top. What is unread in it comes from
            // the ledger, which is the frame's and is leased while it opens
            // this: the first rows go without it, and the next draw has it.
            view.cursor_to_first_thread(cx);
            cx.defer_in(window, |this, _, cx| this.refresh(cx));
        } else {
            view.cursor_to_end(cx);
        }
        view
    }

    /// The rows as they stand, reconciled into the buffer. A reader at the
    /// end stays at the end as messages arrive.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let list = is_root(self.system.read(cx), self.thread);
        let at_end = {
            let last = self.transcript.buffer().read(cx).max_point().row;
            self.cursor_row(cx) >= last
        };
        let items = items(self.system.read(cx), self.thread, cx);
        self.transcript.reconcile(items, cx);
        if at_end && !list {
            self.cursor_to_end(cx);
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
        cx.notify();
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

    /// The agent of the `with` line under the cursor, when it is there.
    fn agent_under_cursor(&self, cx: &App) -> Option<AgentId> {
        let cursor = self.cursor(cx);
        match self.transcript.line_meta(cursor.row, cx) {
            Some(Meta::Agents(agents)) => agent_at(agents, cursor.column as usize),
            _ => None,
        }
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

    /// The first thread of a root's list, if it has one.
    fn cursor_to_first_thread(&mut self, cx: &mut Context<Self>) {
        let rows = self.transcript.buffer().read(cx).max_point().row;
        let Some(row) = (0..=rows)
            .find(|row| matches!(self.transcript.line_meta(*row, cx), Some(Meta::Message(_))))
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
        self.log.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            editor.request_autoscroll(Autoscroll::fit(), cx);
            cx.notify();
        });
    }
}

/// The rows of `thread` as `system` holds it.
fn items(system: &ChatSystem, thread: ThreadId, cx: &App) -> Vec<Item<Key, Class, Meta>> {
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
    if head.path.len() > 1 {
        let above = &head.path[..head.path.len() - 1];
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
    let list = is_root(system, thread);
    // The user's private replies, by the message each is under. Read
    // once: the frame is leased while it opens this surface, and then the
    // next draw has them.
    let mut private = system
        .frame
        .read(cx, |frame, _| {
            private_items(frame.marks(), chat, host, thread.message)
        })
        .unwrap_or_default();
    if let Some(message) = chat.message(thread.message)
        && !list
    {
        items.push(message_item(message, Meta::None));
        items.extend(private.remove(&thread.message).unwrap_or_default());
    }
    if !head.members.is_empty() {
        items.push(with_line(system, host, &head.members, cx));
    }
    // A root's list starts at its first thread: nothing stands above it
    // to keep a gap from.
    if !items.is_empty() {
        gap_item(&mut items);
    }
    if list {
        let rows = system.frame.read(cx, |frame, cx| {
            thread_rows(system, chat, host, thread.message, Some(frame.marks()), cx)
        });
        let rows =
            rows.unwrap_or_else(|| thread_rows(system, chat, host, thread.message, None, cx));
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
        items.push(message_item(message, Meta::Message(*id)));
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
                when(at.0),
            );
            items.push(
                line(Key::Fold(*id), &text, Class::Fold).with_lines(vec![Meta::Message(*id)]),
            );
        }
        gap_item(&mut items);
    }
    items.extend(pending_items(system, host, thread.message, &mut gap_item));
    without_trailing_gap(items)
}

/// `with` and each agent of the thread: what to call it, and what it is
/// doing and for how long, as `eng-q9tz · working 12m`. A gone agent keeps
/// its place under its id: the thread was its.
fn with_line(
    system: &ChatSystem,
    host: EndpointId,
    members: &[AgentId],
    cx: &App,
) -> Item<Key, Class, Meta> {
    let now = Timestamp::now();
    let now_ms = rho_agent_types::UnixMs(now.as_millisecond().max(0) as u64);
    let mut text = "with ".to_owned();
    let mut styles = vec![(Class::Participants, 0..text.len())];
    let mut agents = Vec::new();
    for (index, agent) in members.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        let start = text.len();
        let participant = system.participant(host, *agent, cx);
        let label = participant
            .as_ref()
            .map_or_else(|| agent.encoded(), |participant| participant.label.clone());
        text.push_str(&label);
        styles.push((Class::Participants, start..text.len()));
        if let Some(doing) = participant.and_then(|participant| participant.doing) {
            text.push_str(" · ");
            let state = text.len();
            text.push_str(&format!(
                "{} {}",
                doing.activity.word(),
                ago(doing.since.0, now)
            ));
            let class = match doing.stop(now_ms) {
                Some(_) => Class::Stopped,
                None => Class::Participants,
            };
            styles.push((class, state..text.len()));
        }
        agents.push((start..text.len(), *agent));
    }
    Item::new(Key::Participants, text)
        .with_styles(styles)
        .with_lines(vec![Meta::Agents(agents)])
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
                let time = held
                    .written_ms
                    .map(|ms| when(ms.max(0) as u64))
                    .unwrap_or_default();
                let header = format!("🔒 you · {time}");
                let body = held.body.as_deref().unwrap_or_default().trim_end();
                let text = format!("{header}\n{body}");
                let lines = text.lines().count().max(1);
                Item::new(Key::Private(note.clone()), text)
                    .with_styles(vec![(Class::Private, 0..header.len())])
                    .with_lines(vec![Meta::Open(note.clone()); lines])
            })
            .collect();
        if !replies.is_empty() {
            by_message.insert(*id, replies);
        }
    }
    by_message
}

/// Whether `thread` is a root: a stream, whose surface lists the threads
/// under it rather than reading them.
fn is_root(system: &ChatSystem, thread: ThreadId) -> bool {
    system
        .chat(thread.host)
        .and_then(|chat| chat.message(thread.message))
        .is_some_and(|message| message.parent.is_none())
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

/// A root's threads, one line each, the way the Slack list shows
/// conversations: the name and what waits in it, with the unread ones
/// first, a mention of the user before a plain unread, and each group
/// newest first. The words are in the thread; the list is for choosing one.
/// Without `marks` nothing is known to be unread.
fn thread_rows(
    system: &ChatSystem,
    chat: &HostChat,
    host: EndpointId,
    root: MessageId,
    marks: Option<&Marks>,
    cx: &App,
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
        let agents = chat.members_under(Some(*id));
        if !agents.is_empty() {
            let mut names: Vec<String> = agents
                .iter()
                .take(2)
                .map(|agent| {
                    system
                        .agent_label(host, *agent, cx)
                        .unwrap_or_else(|| agent.encoded())
                })
                .collect();
            if agents.len() > 2 {
                names.push(format!("+{}", agents.len() - 2));
            }
            waiting.push((names.join(", "), Class::Participants));
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
                .with_lines(vec![Meta::Message(line.id)])
        })
        .collect()
}

impl ThreadView {
    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        let thread = self.thread;
        self.system.update(cx, |system, cx| {
            system.post(thread.host, Some(thread.message), text, Vec::new(), cx);
            system.set_draft(thread.host, Some(thread.message), String::new());
        });
        self.composer
            .update(cx, |composer, cx| composer.clear(window, cx));
        self.refresh(cx);
        self.cursor_to_end(cx);
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        self.focused = Focused::Composer;
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    fn focus_log(&mut self, _: &FocusLog, window: &mut Window, cx: &mut Context<Self>) {
        self.focused = Focused::Log;
        window.focus(&self.log.focus_handle(cx), cx);
        cx.notify();
    }

    /// Enter on a web link opens it as a page; elsewhere on a message,
    /// the thread under it; on a crumb, that; on the `with` line, the
    /// agent named under the cursor.
    fn open_row(&mut self, _: &OpenRow, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(url) = self.cursor_link(cx) {
            let frame = self.system.read(cx).frame.clone();
            frame.open_link(url, cx);
            return;
        }
        let host = self.thread.host;
        let address = match self.transcript.line_meta(self.cursor_row(cx), cx) {
            Some(Meta::Message(id)) => ThreadId { host, message: *id }.address(),
            Some(Meta::Open(address)) => address.clone(),
            Some(Meta::Agents(_)) => match self.agent_under_cursor(cx) {
                Some(agent) => AgentRef(agent).address(),
                None => return,
            },
            Some(Meta::None) | None => return,
        };
        let frame = self.system.read(cx).frame.clone();
        frame.open(address, cx);
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
            .find(|character: char| character.is_whitespace() || character == '<' || character == '>')
            .unwrap_or(rest.len());
        let link = rest[..length].trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"', ')', ']', '}']);
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

fn message_item(message: &rho_agent_types::Message, meta: Meta) -> Item<Key, Class, Meta> {
    let from = match message.from {
        Party::User => "you".to_owned(),
        Party::Agent(agent) => agent.encoded(),
    };
    let time = when(message.at.0);
    let header = format!("{from} · {time}");
    let mut text = header.clone();
    text.push('\n');
    text.push_str(&body(&message.content));
    let lines = text.lines().count().max(1);
    let mut styles: Vec<(Class, Range<usize>)> = vec![
        (Class::From, 0..from.len()),
        (Class::Time, from.len()..header.len()),
    ];
    styles.extend(links(&text).into_iter().map(|link| (Class::Link, link)));
    Item::new(Key::Message(message.id), text)
        .with_styles(styles)
        .with_lines(vec![meta; lines])
}

impl Surface for ThreadView {
    fn title(&self, cx: &App) -> String {
        let system = self.system.read(cx);
        let name = system
            .chat(self.thread.host)
            .and_then(|chat| chat.name(self.thread.message))
            .unwrap_or_default();
        match system.hosts().len() > 1 {
            true => format!("{} › {name}", system.host_name(self.thread.host)),
            false => name,
        }
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(match self.focused {
            Focused::Log => self.log.clone(),
            Focused::Composer => self.composer.clone(),
        })
    }

    /// The message under the point, as the thread it anchors: any message
    /// is what an agent starts from or a name goes on. On the `with` line,
    /// the agent named under the point, which the palette steers. The
    /// thread itself from its head, the composer, or nothing in particular.
    fn subject(&self, cx: &App) -> Subject {
        if self.focused == Focused::Composer {
            return Subject::Own;
        }
        if let Some(agent) = self.agent_under_cursor(cx) {
            return Subject::Address(AgentRef(agent).address());
        }
        match self.transcript.line_meta(self.cursor_row(cx), cx) {
            Some(Meta::Message(id)) if *id != self.thread.message => Subject::Address(
                ThreadId {
                    host: self.thread.host,
                    message: *id,
                }
                .address(),
            ),
            _ => Subject::Own,
        }
    }

    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        vec![Row::run("ctrl-enter", "send", move |_, window, cx| {
            this.update(cx, |view, cx| view.send(&SendMessage, window, cx));
        })]
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
            self.focused = Focused::Composer;
        } else if self.log.focus_handle(cx).is_focused(window) {
            self.focused = Focused::Log;
        }
        let colors = cx.theme().colors();
        div()
            .key_context(thread::SCREEN)
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .on_action(cx.listener(Self::send))
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
    use super::*;

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
