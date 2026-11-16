//! A thread: the message it hangs under, what was said under it, where it
//! is, who is in it, and a composer.
//!
//! A channel is the same surface over a root: its messages are the threads
//! under it, each with its replies folded under it.

use std::ops::Range;

use editor::{Editor, EditorEvent, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Subscription, Window, div,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_agent_types::{Address as Party, ContentPart, MessageId};
use rho_dealer::{Address, Addressed as _};
use rho_frame::{Row, Subject, Surface};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use super::{ChatSystem, FocusComposer, FocusLog, OpenRow, SendMessage, when};
use crate::{AgentRef, ThreadId};

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
}

impl Class {
    const COUNT: usize = 6;

    fn slot(self) -> usize {
        match self {
            Self::Crumb => 0,
            Self::From => 1,
            Self::Time => 2,
            Self::Fold => 3,
            Self::Pending => 4,
            Self::Participants => 5,
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
        };
        HighlightStyle {
            color: Some(color.into()),
            font_weight: weight,
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
            editor.set_placeholder_text("message · ctrl-enter sends", window, cx);
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
        };
        view.transcript.attach(&view.log.clone(), cx);
        let items = items(system, thread, cx);
        view.transcript.reconcile(items, cx);
        view.cursor_to_end(cx);
        view
    }

    /// The rows as they stand, reconciled into the buffer. A reader at the
    /// end stays at the end as messages arrive.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let at_end = {
            let last = self.transcript.buffer().read(cx).max_point().row;
            self.cursor_row(cx) + 1 >= last
        };
        let items = items(self.system.read(cx), self.thread, cx);
        self.transcript.reconcile(items, cx);
        if at_end {
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
        let editor = self.log.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row
    }

    fn cursor_to_end(&mut self, cx: &mut Context<Self>) {
        let last = self.transcript.buffer().read(cx).max_point();
        let row = last.row.saturating_sub(1);
        self.log.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
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
    if let Some(message) = chat.message(thread.message) {
        items.push(message_item(message, Meta::None));
    }
    if !head.members.is_empty() {
        // A gone agent keeps its place under its id: the thread was its.
        let names = head
            .members
            .iter()
            .map(|agent| {
                system
                    .agent_label(host, *agent, cx)
                    .unwrap_or_else(|| agent.encoded())
            })
            .collect::<Vec<_>>()
            .join(", ");
        items.push(
            line(
                Key::Participants,
                &format!("with {names}"),
                Class::Participants,
            )
            .with_lines(vec![
                head.members
                    .first()
                    .map_or(Meta::None, |agent| Meta::Open(AgentRef(*agent).address())),
            ]),
        );
    }
    gap_item(&mut items);
    for id in chat.children(Some(thread.message)) {
        let Some(message) = chat.message(*id) else {
            continue;
        };
        items.push(message_item(message, Meta::Message(*id)));
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
    for outgoing in system.waiting_under(host, Some(thread.message)) {
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

    /// Enter on a message opens the thread under it; on a crumb or a
    /// participant, that.
    fn open_row(&mut self, _: &OpenRow, _window: &mut Window, cx: &mut Context<Self>) {
        let host = self.thread.host;
        let address = match self.transcript.line_meta(self.cursor_row(cx), cx) {
            Some(Meta::Message(id)) => ThreadId { host, message: *id }.address(),
            Some(Meta::Open(address)) => address.clone(),
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
    let styles: Vec<(Class, Range<usize>)> = vec![
        (Class::From, 0..from.len()),
        (Class::Time, from.len()..header.len()),
    ];
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
    /// is what an agent starts from or a name goes on. The thread itself
    /// from its head, the composer, or nothing in particular.
    fn subject(&self, cx: &App) -> Subject {
        if self.focused == Focused::Composer {
            return Subject::Own;
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
        vec![Row::run("s", "send", move |_, window, cx| {
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
            .key_context("RhoThread")
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
                    .key_context("RhoThreadLog")
                    .flex_1()
                    .min_h_0()
                    .child(self.log.clone()),
            )
            .child(
                div()
                    .key_context("RhoThreadComposer")
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
}
