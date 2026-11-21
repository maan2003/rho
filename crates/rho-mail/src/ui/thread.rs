//! A thread: read-only mail and an editable draft in one editor.
//!
//! A project is the same surface over a root: it lists its threads, each
//! with its replies folded under it.

use std::collections::HashSet;
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
use jiff::{SignedDuration, Timestamp};
use language::{Buffer, BufferEvent, Capability, Point, ToPoint as _};
use multi_buffer::{MultiBuffer, ToPoint as _};
use rho_agent_types::{
    Address as Party, ChatSeq, EndpointId, Mention, Message, MessageId, Revision, UnixMs,
};
use rho_dealer::facts::Fact;
use rho_dealer::{Address, Addressed as _, Card, Marks};
use rho_frame::surface::PhoneSend;
use rho_frame::thread::{self, OpenRow, QuoteSelection, SendMessage, SendOnly};
use rho_frame::{Candidate, Layout, Row, Subject, Surface, draft};
use rho_notes::NoteId;
use rho_transcript::{BlockSpec, Item, Transcript};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use super::{Event, MailSystem, ago};
use crate::reading::{body, open_anchor};
use crate::{AgentRef, HostMail, ThreadId, cards};

actions!(rho_private_reply, [ReplyPrivately, KeepPrivateReply]);

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
    /// A thread under a root, on one line.
    Thread(MessageId),
    /// The user's private reply under a message: a note about it, which
    /// only their devices hold.
    Private(NoteId),
    Pending(u64),
    Gap(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Class {
    Crumb,
    User,
    Recap,
    Proof,
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
    const COUNT: usize = 13;

    fn slot(self) -> usize {
        match self {
            Self::Crumb => 0,
            Self::User => 1,
            Self::Recap => 11,
            Self::Proof => 12,
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
            Self::User => (gpui::rgb(0xafa851).into(), None),
            Self::Recap => (gpui::rgb(0x97af9b).into(), None),
            Self::Fold => (cx.theme().status().info, None),
            Self::Quote => (colors.text_muted, None),
            Self::Name => (colors.text, None),
            Self::Proof => (colors.text, Some(gpui::FontWeight::BOLD)),
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
    Mail(MessageId, Option<usize>),
    /// One quoted block, by first line in its mail's body.
    Quote(QuoteId, usize),
    /// A project row or a tangent below a mail.
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

/// A thread the user just started in a project, which the cursor follows
/// until it stands in the list: by the id this client gave the message
/// while it is on its way, then by its own once the host logged it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Follow {
    Sending(u64),
    Logged(MessageId),
}

pub struct ThreadView {
    system: Entity<MailSystem>,
    thread: ThreadId,
    transcript: Transcript<Key, Class, Meta>,
    layout: Layout,
    editor: Entity<Editor>,
    draft: Entity<Buffer>,
    /// The last caret in the draft, even while the user selects mail.
    draft_point: Option<Anchor>,
    private_edit: Option<PrivateEdit>,
    draft_subscription: Subscription,
    follow: Option<Follow>,
    initial_scroll: bool,
    latest_mail: Option<(Party, ChatSeq)>,
    hint_anchor: Option<Anchor>,
    /// Resolve against the new transcript before inferring motion direction.
    last_point: Anchor,
    /// The mail dealt to this view, also the target when the cursor is
    /// between mails rather than on a different writer's mail.
    dealt_mail: Option<MessageId>,
    reply_to: Option<MessageId>,
    expanded_quotes: HashSet<QuoteId>,
    _subscriptions: Vec<Subscription>,
    /// Reads a project's list again when a thread's age next changes.
    _ages: Option<Task<()>>,
}

/// The latest posted mail by position, and the version now shown there.
fn latest_mail(system: &MailSystem, thread: ThreadId) -> Option<(Party, ChatSeq)> {
    let chat = system.host_mail(thread.host)?;
    let message = chat
        .reading_order(thread.message)
        .filter(|mail| {
            !matches!(
                mail.revision,
                Revision::Deleted | Revision::TakenBack { .. }
            )
        })
        .last()?;
    Some((message.from, message.version().0))
}

/// How often the ages in a project's list move: their unit is the minute.
const LIST_AGES_EVERY: SignedDuration = SignedDuration::from_secs(60);

impl ThreadView {
    /// The view over `thread` on `system`, which is `entity` as the frame
    /// holds it. The system is leased by the caller, so it is read here
    /// directly and never through its entity.
    pub fn new(
        system: &MailSystem,
        entity: Entity<MailSystem>,
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
        let list = is_project(system, thread);
        let this = cx.weak_entity();
        let history = buffer.clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: if list {
                        SizingBehavior::FullOverscrollMargin
                    } else {
                        SizingBehavior::ExcludeOverscrollMargin
                    },
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            if !list {
                draft::stick(&mut editor, "↓ new mail".into(), cx);
            }
            editor.disable_headers_for_buffers(
                [buffer.read(cx).remote_id(), draft.read(cx).remote_id()],
                cx,
            );
            editor.set_prepare_for_insert(
                Some(
                    move |editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>| {
                        let snapshot = editor.display_snapshot(cx);
                        let row = editor.selections.newest::<Point>(&snapshot).head().row;
                        let tail = draft::start(&history, cx);
                        if row < tail.row {
                            let _ = this.update(cx, |view, cx| {
                                if let Some(mail) = view.selected_reply_target(row, cx) {
                                    view.reply_to = Some(mail);
                                }
                            });
                            editor.change_selections(
                                Default::default(),
                                window,
                                cx,
                                |selections| {
                                    selections.select_ranges([tail..tail]);
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
                    let tail = draft::start(this.transcript.buffer(), cx).row;
                    if selection.start == selection.end
                        && row < tail
                        && matches!(this.transcript.line_meta(row, cx), Some(Meta::None))
                        && this.transcript.buffer().read(cx).line_len(row) == 0
                    {
                        let target = if row < previous {
                            (0..row)
                                .rev()
                                .find(|&row| this.transcript.buffer().read(cx).line_len(row) > 0)
                                .unwrap_or(tail)
                        } else {
                            (row + 1..tail)
                                .find(|&row| this.transcript.buffer().read(cx).line_len(row) > 0)
                                .unwrap_or(tail)
                        };
                        this.cursor_to_row(target, Autoscroll::fit(), cx);
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
                } else if matches!(event, EditorEvent::EditBlockedByReadOnly) {
                    this.system.read(cx).frame.clone().echo(
                        "mail cannot be edited · write in the draft",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }),
            cx.observe(&entity, |this, _, cx| this.refresh(cx)),
            cx.subscribe(&entity, |this, _, event: &Event, cx| match event {
                Event::Delivered {
                    endpoint,
                    client_id,
                    message,
                } if *endpoint == this.thread.host
                    && this.follow == Some(Follow::Sending(*client_id)) =>
                {
                    this.follow = Some(Follow::Logged(*message));
                    this.refresh(cx);
                }
                Event::DraftSent(project) if *project == this.thread => {
                    this.clear_draft(cx);
                    this.refresh(cx);
                    this.cursor_to_latest_mail(cx);
                }
                _ => {}
            }),
        ];
        let draft_subscription = Self::subscribe_draft(&draft, cx);
        let mut view = Self {
            system: entity,
            thread,
            transcript: Transcript::new(buffer),
            layout: Layout::of(window.viewport_size().width),
            editor,
            draft,
            draft_point: None,
            private_edit: None,
            draft_subscription,
            follow: None,
            initial_scroll: !list,
            latest_mail: latest_mail(system, thread),
            hint_anchor: None,
            last_point: Anchor::Min,
            dealt_mail: None,
            reply_to: None,
            expanded_quotes: HashSet::new(),
            _subscriptions: subscriptions,
            _ages: None,
        };
        view.transcript.attach(&view.editor, cx);
        let items = items(
            system,
            thread,
            view.is_list(system),
            view.layout,
            &view.expanded_quotes,
            None,
            cx,
        );
        view.transcript.reconcile(items, cx);
        if list {
            // The list reads from the top.
            view.cursor_to_first_thread(cx);
        } else {
            let end = view.editor.update(cx, |editor, cx| {
                let display = editor.display_snapshot(cx);
                let end = display.buffer_snapshot().max_point();
                editor.selections.change_with(&display, |selections| {
                    selections.select_ranges([end..end]);
                });
                editor.selections.newest_anchor().head()
            });
            view.last_point = end;
            view.draft_point = Some(end);
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

    /// The rows as they stand, reconciled into the read-only excerpt.
    /// Never reset a point (or a selection) in the draft as mail arrives.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let list = self.is_list(self.system.read(cx));
        let items = items(
            self.system.read(cx),
            self.thread,
            list,
            self.layout,
            &self.expanded_quotes,
            self.private_edit.as_ref(),
            cx,
        );
        let newest = latest_mail(self.system.read(cx), self.thread);
        let arrived = !list
            && self
                .latest_mail
                .zip(newest)
                .is_some_and(|((_, old), (from, new))| from != Party::User && new > old);
        self.latest_mail = newest;
        let (anchor, notice) = if arrived {
            let editor = self.editor.read(cx);
            if editor.bottom_stuck_excerpt_is_visible() {
                (None, true)
            } else {
                let tail = draft::start(self.transcript.buffer(), cx);
                let keep = self.editor.update(cx, |editor, cx| {
                    let viewport = editor.visible_line_count()?;
                    let scroll = editor.scroll_position(cx);
                    let display = editor.display_snapshot(cx);
                    let y = display.row_y(tail.to_display_point(&display).row().0 as f64)
                        - display.row_y(scroll.y);
                    // A known inline viewport, even if its first draft line
                    // is now above the viewport in a tall draft.
                    (y < viewport).then_some(y)
                });
                (keep, false)
            }
        } else {
            (None, false)
        };
        let writing = list
            .then(|| draft::WritingCaret::capture(&self.editor, self.transcript.buffer(), cx))
            .flatten();
        self.transcript.reconcile(items, cx);
        if let Some(writing) = writing {
            writing.restore(&self.editor, cx);
        }
        // SPEC-mail-follow-tail: the layout moves independently of point.
        if notice || anchor.is_some() {
            let tail = draft::start(self.transcript.buffer(), cx);
            self.editor.update(cx, |editor, cx| {
                if notice {
                    editor.mark_bottom_stuck_excerpt(cx);
                }
                if let Some(screen_y) = anchor {
                    let display = editor.display_snapshot(cx);
                    let tail = display.buffer_snapshot().anchor_after(tail);
                    editor.request_autoscroll(
                        Autoscroll::top_relative(screen_y).for_anchor(tail),
                        cx,
                    );
                }
            });
        }
        if let Some(Follow::Logged(message)) = self.follow
            && self.cursor_to_item(&Key::Thread(message), Autoscroll::fit(), cx)
        {
            self.follow = None;
        }
        self.schedule_ages(cx);
        cx.notify();
    }

    /// Reads the thread again when an age it shows next says another.
    fn schedule_ages(&mut self, cx: &mut Context<Self>) {
        let system = self.system.read(cx);
        let list = self.is_list(system)
            && system
                .host_mail(self.thread.host)
                .is_some_and(|chat| !chat.children(Some(self.thread.message)).is_empty());
        self._ages = list.then(|| {
            let wait = std::time::Duration::try_from(LIST_AGES_EVERY).expect("positive interval");
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
        self.reply_to = None;
    }

    fn cursor_row(&self, cx: &App) -> u32 {
        self.cursor(cx).row
    }

    fn cursor(&self, cx: &App) -> Point {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.selections.newest_anchor().head().to_point(&snapshot)
    }

    fn is_list(&self, system: &MailSystem) -> bool {
        is_project(system, self.thread)
    }

    fn selected_reply_target(&self, row: u32, cx: &App) -> Option<MessageId> {
        let chat = self.system.read(cx).host_mail(self.thread.host)?;
        let id = self.transcript.line_meta(row, cx)?.mail()?;
        incoming(chat.message(id)).then_some(id)
    }

    fn current_reply_target(&self, cx: &App) -> Option<MessageId> {
        let system = self.system.read(cx);
        if self.is_list(system) {
            return None;
        }
        let chat = system.host_mail(self.thread.host)?;
        let point = self.transcript.line_meta(self.cursor_row(cx), cx);
        self.reply_to
            .into_iter()
            .chain(point.and_then(Meta::mail))
            .chain(self.dealt_mail)
            .chain(
                chat.children(Some(self.thread.message))
                    .iter()
                    .rev()
                    .copied(),
            )
            .chain([self.thread.message])
            .find(|id| incoming(chat.message(*id)))
    }

    /// The web link at a point in the transcript, if there is one.
    fn link_at(&self, cursor: Point, cx: &App) -> Option<url::Url> {
        let buffer = self.transcript.buffer().read(cx);
        if cursor.row >= draft::start(self.transcript.buffer(), cx).row {
            return None;
        }
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
        self.editor.update(cx, |editor, cx| {
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
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    fn cursor_to_latest_mail(&mut self, cx: &mut Context<Self>) {
        let row = self.transcript.buffer().read(cx).max_point().row;
        self.cursor_to_row(row.saturating_sub(1), Autoscroll::fit(), cx);
    }
}

/// The rows of `thread` as `system` holds it.
fn items(
    system: &MailSystem,
    thread: ThreadId,
    list: bool,
    layout: Layout,
    expanded_quotes: &HashSet<QuoteId>,
    private_edit: Option<&PrivateEdit>,
    cx: &App,
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = history_items(system, thread, list, expanded_quotes, private_edit, cx);
    if !list && system.work(thread) == Some(rho_agent_types::ThreadWork::StoppedByYou) {
        items.push(line(Key::Status, "stopped by you", Class::Crumb));
    }
    if list {
        items.push(separator_item(&system.project_of(thread), list, layout));
    } else {
        items.push(Item::new(Key::Separator, "").with_lines(vec![Meta::None]));
    }
    items
}

fn separator_item(project: &str, list: bool, layout: Layout) -> Item<Key, Class, Meta> {
    let label = match (layout, list) {
        (Layout::Phone, false) => format!("── reply to {project}"),
        (Layout::Phone, true) => format!("── idea to {project}"),
        (Layout::Desktop, false) => format!("── reply to {project} · ctrl-enter sends"),
        (Layout::Desktop, true) => format!(
            "── idea to {project} · ctrl-enter starts an agent · ctrl-shift-enter only sends"
        ),
    };
    line(Key::Separator, &label, Class::Crumb)
}

fn history_items(
    system: &MailSystem,
    thread: ThreadId,
    list: bool,
    expanded_quotes: &HashSet<QuoteId>,
    private_edit: Option<&PrivateEdit>,
    cx: &App,
) -> Vec<Item<Key, Class, Meta>> {
    let host = thread.host;
    let Some(chat) = system.host_mail(host) else {
        return vec![line(
            Key::Gap(0),
            "this host's mail is not held here",
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
    // The user's private replies and verdicts both come from the ledger.
    // The frame is leased while it opens this surface; the first refresh
    // after the lease reads both before drawing.
    let (mut private, chosen_recap) = system
        .frame
        .read(cx, |frame, _| {
            let marks = frame.marks();
            let verdicts = marks
                .get(&thread.address())
                .facts()
                .0
                .iter()
                .filter_map(|entry| {
                    matches!(
                        entry.fact,
                        Fact::Settled { .. } | Fact::Snooze { .. } | Fact::Mute { .. }
                    )
                    .then(|| {
                        u64::try_from(entry.at.timestamp().as_millisecond())
                            .ok()
                            .map(UnixMs)
                    })
                    .flatten()
                });
            let chosen = (!list)
                .then(|| chat.recap(thread.message, verdicts))
                .flatten();
            (
                private_items(marks, chat, host, thread.message, private_edit),
                chosen,
            )
        })
        .unwrap_or_default();
    let mail_item = |message: &Message| {
        let voice = match message.from {
            Party::Agent(agent)
                if message
                    .mentions
                    .iter()
                    .any(|m| matches!(m, Mention::Direct(_))) =>
            {
                system
                    .agent_label(host, agent, cx)
                    .or_else(|| Some(agent.encoded()))
            }
            _ => None,
        };
        let recap = chosen_recap
            .filter(|(id, _)| *id == message.id)
            .map(|(_, text)| text);
        message_item(message, voice.as_deref(), recap, expanded_quotes)
    };
    if let Some(message) = chat.message(thread.message)
        && !list
    {
        items.extend(mail_item(message));
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
            items.push(line(Key::Gap(u32::MAX), "no threads yet", Class::Crumb));
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
        let start = items.len();
        items.extend(mail_item(message));
        items.extend(private.remove(id).unwrap_or_default());
        let replies = chat.children(Some(*id));
        if !replies.is_empty() {
            let (_, at) = chat.newest_in(*id).unwrap_or_else(|| message.version());
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
        if items.len() != start {
            gap_item(&mut items);
        }
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

/// The user's own notes under each mail. The open one uses the same
/// key and header but puts the private editor below its separator.
fn private_items(
    marks: &Marks,
    chat: &HostMail,
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

fn is_project(system: &MailSystem, thread: ThreadId) -> bool {
    system
        .host_mail(thread.host)
        .is_some_and(|chat| chat.is_project(thread.message))
}

/// The user's messages into `parent` that no host has logged yet.
fn pending_items(
    system: &MailSystem,
    host: EndpointId,
    parent: MessageId,
    gap_item: &mut impl FnMut(&mut Vec<Item<Key, Class, Meta>>),
) -> Vec<Item<Key, Class, Meta>> {
    let mut items = Vec::new();
    for outgoing in system.waiting_under(host, Some(parent)) {
        let id = outgoing.at.0;
        let mut text = body(&outgoing.content);
        let first = text.find('\n').unwrap_or(text.len());
        text.insert_str(first, " · sending…");
        let tag = first..first + " · sending…".len();
        let lines = text.lines().count().max(1);
        let len = text.len();
        items.push(
            Item::new(Key::Pending(id), text)
                .with_styles(vec![
                    (Class::User, 0..tag.start),
                    (Class::Pending, tag.clone()),
                    (Class::User, tag.end..len),
                ])
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
    system: &MailSystem,
    chat: &HostMail,
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
                    .is_some_and(|message| message.pings_user())
            })
            .count();
        let (_, at) = chat.newest_in(*id).unwrap_or_else(|| message.version());
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
    fn on_draft(&self, cx: &App) -> bool {
        self.cursor_row(cx) >= draft::start(self.transcript.buffer(), cx).row
    }

    fn has_draft(&self, cx: &App) -> bool {
        !self.draft.read(cx).text().trim().is_empty()
    }

    fn echo_empty_draft(&self, cx: &mut App) {
        self.system.read(cx).frame.clone().echo(
            "send: nothing written",
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// In a project, the idea goes to the agents system, which sends it
    /// and starts an agent from it; in a thread, the reply goes out.
    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_list(self.system.read(cx)) {
            return self.send_only(&SendOnly, window, cx);
        }
        if !self.has_draft(cx) {
            self.echo_empty_draft(cx);
            return;
        }
        thread::enter_normal_mode(&self.editor, window, cx);
        let thread = self.thread;
        self.system.update(cx, |_, cx| {
            cx.emit(Event::Idea {
                endpoint: thread.host,
                parent: thread.message,
                source: super::IdeaSource::ProjectDraft,
            });
        });
    }

    fn send_only(&mut self, _: &SendOnly, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        let text = self.draft.read(cx).text();
        if text.trim().is_empty() {
            self.echo_empty_draft(cx);
            return;
        }
        let thread = self.thread;
        let list = self.is_list(self.system.read(cx));
        let reply_to = self.current_reply_target(cx);
        let client_id = self.system.update(cx, |system, cx| {
            let client_id = match reply_to {
                Some(mail) => system.reply(thread, mail, text, cx),
                None => system.post(thread.host, Some(thread.message), text, Vec::new(), cx),
            };
            system.set_draft(thread.host, Some(thread.message), String::new());
            client_id
        });
        thread::enter_normal_mode(&self.editor, window, cx);
        self.clear_draft(cx);
        self.dealt_mail = None;
        self.refresh(cx);
        let tail = draft::start(self.transcript.buffer(), cx);
        self.cursor_to_row(tail.row, Autoscroll::fit(), cx);
        // In a project the message is a new thread: the cursor goes to it in
        // the list, on its way and then where the list puts it, so `enter`
        // opens it.
        if list {
            self.follow = Some(Follow::Sending(client_id));
        }
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
            None if self.layout == Layout::Phone
                && self.below_draft(event.position, window, cx) =>
            {
                None
            }
            None => return,
        };
        self.focus_draft(position, window, cx);
        window.prevent_default();
        cx.stop_propagation();
    }

    fn below_draft(
        &self,
        at: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let snapshot = self
            .editor
            .update(cx, |editor, cx| editor.snapshot(window, cx));
        let last = snapshot.max_point();
        self.editor.update(cx, |editor, cx| {
            editor
                .window_position_for_display_point(last, &snapshot, window, cx)
                .is_some_and(|point| at.y > point.y)
        })
    }

    fn focus_draft(&self, position: Option<Anchor>, window: &mut Window, cx: &mut Context<Self>) {
        draft::focus(&self.editor, self.transcript.buffer(), position, window, cx);
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
        if self.is_list(self.system.read(cx)) {
            return None;
        }
        let lines = selected_lines(range);
        let mail = self.transcript.line_meta(*lines.start(), cx)?.mail()?;
        lines
            .clone()
            .all(|row| self.transcript.line_meta(row, cx).and_then(Meta::mail) == Some(mail))
            .then_some(lines)
    }

    fn quote_selection(&mut self, _: &QuoteSelection, window: &mut Window, cx: &mut Context<Self>) {
        if self.private_edit.is_some() {
            return;
        }
        let Some(lines) = self.mail_selection(self.selection_range(cx), cx) else {
            self.system.read(cx).frame.clone().echo(
                "select lines in one mail to quote",
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
        if let Some(mail) = self.selected_reply_target(*lines.start(), cx) {
            self.reply_to = Some(mail);
        }
        let history = self.transcript.buffer().read(cx);
        let start = history.point_to_offset(Point::new(*lines.start(), 0));
        let end = history.point_to_offset(Point::new(*lines.end(), history.line_len(*lines.end())));
        let selected = history.text_for_range(start..end).collect::<String>();
        let quote = quote_lines(&selected);
        let line_count = *lines.end() - *lines.start() + 1;
        let draft_start = draft::start(self.transcript.buffer(), cx);

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
        if self.is_list(self.system.read(cx)) {
            return None;
        }
        let meta = self.transcript.line_meta(self.cursor_row(cx), cx)?;
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
            line: 2,
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

    fn agent_mail(&self, row: u32, cx: &App) -> Option<(rho_agent_types::AgentId, Message)> {
        let system = self.system.read(cx);
        if self.is_list(system) {
            return None;
        }
        let id = self.transcript.line_meta(row, cx)?.mail()?;
        let mail = system.host_mail(self.thread.host)?.message(id)?;
        let agent = match mail.from {
            Party::Agent(agent) | Party::System { agent, .. } => agent,
            Party::User | Party::StartFailed => return None,
        };
        Some((agent, mail.clone()))
    }

    fn inspect(&self, agent: rho_agent_types::AgentId, mail: Message, cx: &mut Context<Self>) {
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
        if self
            .transcript
            .line_meta(row, cx)
            .and_then(Meta::mail)
            .is_some()
        {
            self.quote(row..=row, window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn tap_mail_header(
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
        let row = anchor.to_point(&editor.buffer().read(cx).snapshot(cx)).row;
        if let Some(Meta::Private { note, mail }) = self.transcript.line_meta(row, cx) {
            self.open_private(*mail, Some(*note), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if let Some((agent, mail)) = self.agent_mail(row, cx)
            && self.item_row(&Key::Message(mail.id), cx) == Some(row)
        {
            self.inspect(agent, mail, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    /// A folded quote opens before links; an open quote closes only if
    /// there is no link under the point. Both take precedence over the mail.
    fn inline_open(&self, cursor: Point, cx: &App) -> Option<InlineOpen> {
        let quote = match self.transcript.line_meta(cursor.row, cx) {
            Some(Meta::Quote(quote, _)) => Some(*quote),
            _ => None,
        };
        if let Some(quote) = quote
            && !self.expanded_quotes.contains(&quote)
        {
            return Some(InlineOpen::Quote(quote));
        }
        self.link_at(cursor, cx)
            .map(InlineOpen::Link)
            .or_else(|| quote.map(InlineOpen::Quote))
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
        if let Some(Meta::Private { note, mail }) = self.transcript.line_meta(cursor.row, cx) {
            self.open_private(*mail, Some(*note), cx);
            return;
        }
        if let Some(target) = self.inline_open(cursor, cx) {
            match target {
                InlineOpen::Quote(quote) => self.toggle_quote(quote, cx),
                InlineOpen::Link(url) => self.system.read(cx).frame.clone().open_link(url, cx),
            }
            return;
        }
        if let Some((agent, mail)) = self.agent_mail(cursor.row, cx) {
            self.inspect(agent, mail, cx);
            return;
        }
        let meta = self.transcript.line_meta(cursor.row, cx).cloned();
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
        if !self.expanded_quotes.remove(&quote) {
            self.expanded_quotes.insert(quote);
        }
        self.refresh(cx);
        if let Some(start) = self.item_row(&Key::Message(quote.mail), cx)
            && let Some(row) = (start..)
                .map_while(|row| self.transcript.line_meta(row, cx).map(|meta| (row, meta)))
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

fn incoming(mail: Option<&Message>) -> bool {
    mail.is_some_and(|mail| {
        mail.from != Party::User
            && !matches!(
                mail.revision,
                Revision::Deleted | Revision::TakenBack { .. }
            )
    })
}

fn message_item(
    message: &Message,
    third_voice: Option<&str>,
    shown_recap: Option<&str>,
    expanded_quotes: &HashSet<QuoteId>,
) -> Option<Item<Key, Class, Meta>> {
    let edited = match message.revision {
        Revision::Original => "",
        Revision::Edited { .. } => " · edited",
        Revision::TakenBack { .. } => " · taken back",
        Revision::Deleted => return None,
    };
    let body = body(&message.content);
    let recap = rho_agent_types::recap_section(&body);
    let proof_heading = message
        .pings_user()
        .then(|| rho_agent_types::proof_section(&body))
        .flatten()
        .map(|range| range.start);
    let mut text = String::new();
    let mut styles = Vec::new();
    let mut lines = Vec::new();
    if let Some(recap) = shown_recap {
        text.push_str(recap);
        styles.push((Class::Recap, 0..text.len()));
        let source = rho_agent_types::recap_section(&body)
            .filter(|range| body[range.clone()].trim_end() == recap)
            .map(|range| body[..range.start].lines().count());
        lines.extend(
            recap
                .lines()
                .enumerate()
                .map(|(offset, _)| Meta::Mail(message.id, source.map(|start| start + offset))),
        );
    }
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
    } else {
        Class::Name
    };
    let mut offset = 0;
    let mut body_lines = body.lines().enumerate().peekable();
    let mut first_body = true;
    while let Some((first_line, line)) = body_lines.next() {
        let in_recap = recap.as_ref().is_some_and(|range| range.contains(&offset));
        let starts_proof = proof_heading == Some(offset);
        offset += line.len() + 1;
        if in_recap {
            // The preceding separator belongs to the hidden section.
            if !first_body && text.ends_with('\n') {
                text.pop();
                lines.pop();
            }
            continue;
        }
        if !lines.is_empty() {
            text.push('\n');
        }
        let start = text.len();
        text.push_str(if starts_proof { "Proof" } else { line });
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
                offset += quoted.len() + 1;
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
            styles.push((
                if starts_proof { Class::Proof } else { voice },
                start..text.len(),
            ));
            lines.push(Meta::Mail(message.id, Some(first_line)));
        }
        if first_body {
            let first_end = text[start..].find('\n').map_or(text.len(), |n| start + n);
            text.insert_str(first_end, edited);
            // An expanded quote is still quiet after its tag.
            if let Some((_, range)) = styles.last_mut() {
                if first_end != range.end {
                    range.end += edited.len();
                }
            }
            styles.push((Class::Crumb, first_end..first_end + edited.len()));
            first_body = false;
        }
    }
    // A mail containing only a hidden recap has no visible rows.
    if lines.is_empty() {
        return None;
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
            .host_mail(self.thread.host)
            .and_then(|chat| chat.name(self.thread.message))
            .unwrap_or_default();
        match subject.is_empty() {
            true => project,
            false => format!("{project} › {subject}"),
        }
    }

    fn show_card(&mut self, card: &Card, _window: &mut Window, _cx: &mut Context<Self>) {
        let Some(mail) = cards::attention_mail(&card.cursor) else {
            return;
        };
        self.dealt_mail = Some(mail);
    }

    fn show_search_match(
        &mut self,
        point: &rho_frame::SearchPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = match point {
            rho_frame::SearchPoint::Mail { message, line } => {
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
            }
            rho_frame::SearchPoint::Draft(line) => draft::start(self.transcript.buffer(), cx)
                .row
                .saturating_add(*line),
            _ => return,
        };
        self.initial_scroll = false;
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
    fn subject(&self, cx: &App) -> Subject {
        if self.on_draft(cx) {
            return Subject::Own;
        }
        line_subject(
            self.transcript.line_meta(self.cursor_row(cx), cx),
            self.thread,
        )
    }

    /// The thread the subject is in, as the dealer counts threads: in a
    /// thread, the thread wherever the point is in it; in a project, the
    /// thread under the point, as a row is on Home.
    fn card(&self, cx: &App) -> Subject {
        let system = self.system.read(cx);
        if !self.is_list(system) {
            return Subject::Own;
        }
        self.subject(cx)
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
            this.update(cx, |view, cx| view.send_only(&SendOnly, window, cx));
        }))
    }

    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        if self.private_edit.is_some() {
            return Vec::new();
        }
        let this = cx.entity();
        let has_draft = self.has_draft(cx);
        if !self.is_list(self.system.read(cx)) {
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
                .selected_reply_target(self.cursor_row(cx), cx)
                .is_some()
            {
                let view = this.clone();
                rows.push(Row::run("", "quote mail line", move |_, window, cx| {
                    view.update(cx, |view, cx| {
                        let row = view.cursor_row(cx);
                        let lines = view
                            .mail_selection(view.selection_range(cx), cx)
                            .unwrap_or(row..=row);
                        view.quote(lines, window, cx);
                    });
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
            let system = self.system.read(cx);
            let members = system
                .host_mail(self.thread.host)
                .map(|chat| chat.members_under(Some(self.thread.message)))
                .unwrap_or_default();
            let cursor = self.cursor(cx);
            let mail = (!self.on_draft(cx))
                .then(|| self.agent_mail(cursor.row, cx))
                .flatten();
            if let Some((agent, mail)) = mail {
                let inspector = this.clone();
                let look = Row::run("enter", "look into the work", move |_, _, cx| {
                    inspector.update(cx, |view, cx| view.inspect(agent, mail.clone(), cx));
                });
                rows.push(if self.inline_open(cursor, cx).is_none() {
                    look.bound_to(OpenRow)
                } else {
                    look
                });
            } else {
                let mut agents: Vec<_> = members
                    .into_iter()
                    .filter_map(|id| Some((id, system.participant(self.thread.host, id, cx)?)))
                    .collect();
                agents.sort_by_key(|(id, participant)| {
                    (std::cmp::Reverse(participant.created_at), *id)
                });
                if !agents.is_empty() {
                    let choices: Vec<_> = agents
                        .iter()
                        .map(|(id, participant)| {
                            let name = &participant.label;
                            let value = if agents
                                .iter()
                                .filter(|(_, other)| other.label == *name)
                                .count()
                                > 1
                            {
                                format!("{name} · {}", id.encoded())
                            } else {
                                name.clone()
                            };
                            let description = participant
                                .runtime
                                .iter()
                                .map(String::as_str)
                                .chain(participant.doing.map(|doing| doing.activity.word()))
                                .collect::<Vec<_>>()
                                .join(" · ");
                            (value, description, AgentRef(*id).address())
                        })
                        .collect();
                    rows.push(Row::run(
                        "",
                        "look into the work",
                        move |frame, window, cx| {
                            let suggestions = choices.clone();
                            let choices = choices.clone();
                            frame.open_prompt(
                                "look into the work:",
                                Rc::new(move |_, query, _| {
                                    let query = query.to_lowercase();
                                    suggestions
                                        .iter()
                                        .filter(|(name, description, _)| {
                                            name.to_lowercase().contains(&query)
                                                || description.contains(&query)
                                        })
                                        .map(|(name, description, _)| Candidate {
                                            value: name.clone(),
                                            description: description.clone(),
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
                    ));
                }
            }
            // The thumb's Open action mirrors Enter, including on a quote.
            if matches!(
                self.transcript.line_meta(self.cursor_row(cx), cx),
                Some(Meta::Quote(..) | Meta::Thread(_) | Meta::Open(_))
            ) {
                rows.push(
                    Row::run("enter", "open", move |_, window, cx| {
                        this.update(cx, |view, cx| view.open_row(&OpenRow, window, cx));
                    })
                    .bound_to(OpenRow),
                );
            }
            return rows;
        }
        if !has_draft {
            return Vec::new();
        }
        let only = this.clone();
        let send_only = Row::run("ctrl-shift-enter", "send", move |_, window, cx| {
            only.update(cx, |view, cx| view.send_only(&SendOnly, window, cx));
        })
        .bound_to(SendOnly);
        vec![
            Row::run(
                "ctrl-enter",
                "send and start agent",
                move |_, window, cx| {
                    this.update(cx, |view, cx| view.send(&SendMessage, window, cx));
                },
            )
            .bound_to(SendMessage),
            send_only,
        ]
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
        let system = self.system.read(cx);
        let list = self.is_list(system);
        if self.initial_scroll
            && let Some(chat) = system.host_mail(self.thread.host)
        {
            // SPEC-mail-thread-entry: point and viewport have different
            // destinations. The editor clamps the requested top to the tail.
            let visible = chat
                .reading_order(self.thread.message)
                .filter(|mail| self.transcript.range_of(&Key::Message(mail.id)).is_some());
            if let Some(mail) = open_anchor(visible)
                && let Some(row) = self.item_row(&Key::Message(mail), cx)
            {
                self.initial_scroll = false;
                self.editor.update(cx, |editor, cx| {
                    let display = editor.display_snapshot(cx);
                    let from = display.buffer_snapshot().anchor_after(Point::new(row, 0));
                    editor.request_autoscroll(Autoscroll::top().for_anchor(from), cx);
                });
            }
        }
        let hint = (!list && self.draft.read(cx).is_empty()).then(|| {
            self.editor
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_after(draft::start(self.transcript.buffer(), cx))
        });
        if self.hint_anchor != hint {
            self.hint_anchor = hint;
            self.editor.update(cx, |editor, cx| {
                editor.set_eol_hints(
                    hint.map(|anchor| {
                        let render: editor::EolHintRenderer = Arc::new(|_, cx| {
                            div()
                                .text_color(cx.theme().colors().text_muted)
                                .child("reply")
                                .into_any_element()
                        });
                        (anchor, render)
                    })
                    .into_iter()
                    .collect(),
                    cx,
                );
            });
        }
        let layout = Layout::of(window.viewport_size().width);
        if layout != self.layout {
            self.layout = layout;
            if list {
                let label =
                    separator_item(&self.system.read(cx).project_of(self.thread), list, layout);
                self.transcript.replace(&Key::Separator, label, cx);
            }
        }
        let context = if list {
            thread::PROJECT_SCREEN
        } else {
            thread::SCREEN
        };
        let view = cx.weak_entity();
        let gesture = (layout == Layout::Phone && !list).then(move || {
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
            .on_action(cx.listener(Self::send_only))
            .on_action(cx.listener(Self::open_row))
            .on_action(cx.listener(Self::quote_selection))
            .on_action(cx.listener(Self::reply_privately))
            .on_action(cx.listener(Self::keep_private))
            // The editor consumes text mouse-down even when its native
            // selection is disabled. Capture the draft's pointer down
            // before it; GPUI also routes a completed tap this way.
            .capture_any_mouse_down(cx.listener(Self::tap_tail))
            .capture_any_mouse_down(cx.listener(Self::tap_mail_header))
            .child(self.editor.clone())
            .children(gesture)
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentId, ChatSeq, ContentPart, UnixMs};

    use super::*;

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
                line: 2,
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
        assert_eq!(editing.blocks[0].line, 2);
        assert_eq!(editing.lines[2], Meta::Private { note, mail });
        assert_eq!(saved.lines[3], Meta::Private { note, mail });
    }

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
            ping_started: None,
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
            let item = message_item(mail, None, None, &HashSet::new()).unwrap();
            assert_eq!(item.text, format!("{first}\n"));
        }
        let third_voice = message_item(&original, Some("eng-ab12"), None, &HashSet::new()).unwrap();
        assert_eq!(third_voice.text, "eng-ab12\nReady?\n");
        let deleted = Message {
            revision: Revision::Deleted,
            ..original
        };
        assert!(message_item(&deleted, None, None, &HashSet::new()).is_none());
    }

    #[test]
    fn a_ping_keeps_proof_visible_when_its_recap_is_hidden() {
        let writer = AgentId::from_counter(11, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let mut result = mail(
            Party::Agent(writer),
            Revision::Original,
            "Done.\n\n## recap\nEarlier plan.\n\n## proof\n42 tests passed.",
        );
        result.mentions = vec![rho_agent_types::Mention::User { ping: true }];
        result.ping_started = Some((result.id.0, result.at));
        let warm = message_item(&result, None, None, &HashSet::new()).unwrap();
        assert_eq!(warm.text, "Done.\nProof\n42 tests passed.\n");
        assert!(
            warm.styles.iter().any(
                |(class, bytes)| *class == Class::Proof && &warm.text[bytes.clone()] == "Proof"
            )
        );
        let recap = "## recap\nEarlier version's plan.";
        let cold = message_item(&result, None, Some(recap), &HashSet::new()).unwrap();
        assert_eq!(
            cold.text,
            format!("{recap}\nDone.\nProof\n42 tests passed.\n")
        );
        assert!(
            cold.styles
                .iter()
                .any(|(class, bytes)| *class == Class::Recap && &cold.text[bytes.clone()] == recap)
        );
        assert_eq!(
            cold.lines[0].source_line(),
            None,
            "previous version's recap"
        );
        assert_eq!(
            cold.lines[2].source_line(),
            Some(0),
            "the body remains indexed"
        );
        let current = message_item(
            &result,
            None,
            Some("## recap\nEarlier plan."),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            current.lines[0].source_line(),
            Some(2),
            "current recap is searchable in its new place"
        );
    }

    #[test]
    fn only_incoming_nondeleted_mail_can_receive_a_reply() {
        let agent = AgentId::from_counter(19, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let incoming_mail = mail(
            Party::Agent(agent),
            Revision::Edited {
                seq: ChatSeq(2),
                at: UnixMs(2),
            },
            "Use the release branch.",
        );
        let sent = mail(Party::User, Revision::Original, "Not yet.");
        let removed = mail(Party::Agent(agent), Revision::Deleted, "Removed.");
        assert!(incoming(Some(&incoming_mail)));
        assert!(!incoming(Some(&sent)));
        assert!(!incoming(Some(&removed)));
        assert!(!incoming(None));
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
        let folded = message_item(&message, None, None, &HashSet::new()).unwrap();
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
        let expanded = message_item(&message, None, None, &HashSet::from([first])).unwrap();
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
