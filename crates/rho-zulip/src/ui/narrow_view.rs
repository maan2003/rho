//! One conversation: a Comint transcript with a compose region at its end.
//!
//! The transcript is a read-only buffer carrying the host's Markdown
//! pipeline — message content is raw Markdown, so it needs no renderer of
//! its own — and the compose buffer is an ordinary writable excerpt below
//! it, exactly as Rho's shell surface arranges a prompt.
//!
//! Only a narrow that names a single conversation composes. A stream-wide
//! or search narrow has no unambiguous destination, so it is read-only and
//! you reply from the topic you entered, the way a newsreader has you
//! follow up inside a thread.

use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Window, div};
use theme::ActiveTheme as _;
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use std::ops::Range;
use text::Anchor;

use crate::session::Session;
use crate::types::Message;
use crate::ui::{Class, Hooks, Span, apply_highlights, clock_time, crosses_day, day_label, lay_out};
use crate::{Destination, Narrow};

pub struct NarrowView {
    session: Entity<Session>,
    narrow: Narrow,
    destination: Option<Destination>,
    transcript: Entity<Buffer>,
    input: Option<Entity<Buffer>>,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// The messages currently in the transcript, in order. An arriving
    /// message that extends this list is appended; anything else (an edit,
    /// a backfill) rebuilds the buffer.
    rendered: Vec<u64>,
    /// Every classed range in the transcript. Anchors survive appends, so
    /// an appended run extends this rather than reapplying the whole
    /// transcript's highlights.
    styles: Vec<(Class, Vec<Range<Anchor>>)>,
    _observe: gpui::Subscription,
}

impl NarrowView {
    pub fn new(
        session: Entity<Session>,
        narrow: Narrow,
        hooks: Hooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let destination = narrow.destination();
        let transcript = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            (hooks.configure_markdown)(&mut buffer, cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let input = destination
            .is_some()
            .then(|| cx.new(|cx| Buffer::local("", cx)));
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                transcript.clone(),
                [Point::zero()..transcript.read(cx).max_point()],
                0,
                cx,
            );
            if let Some(input) = &input {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(1),
                    input.clone(),
                    [Point::zero()..input.read(cx).max_point()],
                    0,
                    cx,
                );
            }
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
            if let Some(input) = &input {
                editor.disable_header_for_buffer(input.read(cx).remote_id(), cx);
            }
            editor
        });

        let observe = cx.observe_in(&session, window, |this, _, window, cx| {
            this.refresh(window, cx);
        });
        let mut view = Self {
            session: session.clone(),
            narrow: narrow.clone(),
            destination,
            transcript,
            input,
            multi_buffer,
            editor,
            rendered: Vec::new(),
            styles: Vec::new(),
            _observe: observe,
        };
        session.update(cx, |session, cx| session.open(&narrow, cx));
        view.refresh(window, cx);
        view.select_compose(window, cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn narrow(&self) -> &Narrow {
        &self.narrow
    }

    /// Whether this surface can be composed into.
    pub fn composes(&self) -> bool {
        self.input.is_some()
    }

    /// Sends the compose region's contents. The message is not shown until
    /// the server echoes it back, so nothing appears that was not accepted.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let (Some(input), Some(destination)) = (&self.input, self.destination.clone()) else {
            return;
        };
        let content = input.read(cx).text();
        if content.trim().is_empty() {
            return;
        }
        input.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, "")], None, cx);
        });
        self.session
            .update(cx, |session, cx| session.send(destination, content, cx));
    }

    /// Marks the conversation read, the way leaving a Gnus summary buffer
    /// marks its articles read.
    pub fn mark_read(&mut self, cx: &mut Context<Self>) {
        let narrow = self.narrow.clone();
        self.session
            .update(cx, |session, cx| session.mark_read(&narrow, cx));
    }

    pub fn load_older(&mut self, cx: &mut Context<Self>) {
        let narrow = self.narrow.clone();
        self.session
            .update(cx, |session, cx| session.load_older(&narrow, cx));
    }

    fn select_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.input else {
            return;
        };
        let end = {
            let buffer = input.read(cx);
            buffer.anchor_after(buffer.len())
        };
        let Some(anchor) = self.multi_buffer.read(cx).snapshot(cx).anchor_in_excerpt(end) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_autoscroll_pin(anchor, AutoscrollStrategy::Bottom, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    fn refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let Some(conversation) = session.conversation(&self.narrow) else {
            return;
        };
        let messages = conversation.messages.clone();
        let notice = match (&conversation.error, conversation.loading) {
            (Some(error), _) => Some(Span::styled(format!("{error}\n\n"), Class::Error)),
            (None, true) => Some(Span::styled("loading…\n\n", Class::Muted)),
            (None, false) => None,
        };
        let me = session.model().me();
        let ids = messages.iter().map(|message| message.id).collect::<Vec<_>>();

        // The common case is a message arriving at the end of a
        // conversation already on screen, and a whole-buffer rewrite there
        // would drop the reader's selection and folds for no reason.
        let appending = notice.is_none()
            && !self.rendered.is_empty()
            && ids.len() > self.rendered.len()
            && ids.starts_with(&self.rendered);
        let (spans, replace_from) = if appending {
            let tail = &messages[self.rendered.len()..];
            let previous = messages[self.rendered.len() - 1].timestamp;
            (render_messages(tail, me, Some(previous)), None)
        } else {
            let mut spans = notice.into_iter().collect::<Vec<_>>();
            spans.extend(render_messages(&messages, me, None));
            (spans, Some(()))
        };

        let (text, styles) = lay_out(&spans);
        let anchored = self.transcript.update(cx, |buffer, cx| {
            let base = match replace_from {
                Some(()) => {
                    let len = buffer.len();
                    buffer.edit([(0..len, text)], None, cx);
                    0
                }
                None => {
                    let len = buffer.len();
                    buffer.edit([(len..len, text)], None, cx);
                    len
                }
            };
            let snapshot = buffer.snapshot();
            styles
                .into_iter()
                .map(|(class, range)| {
                    let clamp = |offset: usize| (base + offset).min(snapshot.len());
                    (
                        class,
                        vec![snapshot.anchor_before(clamp(range.start))
                            ..snapshot.anchor_after(clamp(range.end))],
                    )
                })
                .collect::<Vec<(Class, Vec<Range<Anchor>>)>>()
        });
        self.rendered = ids;
        if replace_from.is_some() {
            self.styles = anchored;
        } else {
            self.styles.extend(anchored);
        }
        // Highlights are applied from the full set: `apply_highlights`
        // clears a class that has no ranges, so passing only the appended
        // run would wipe the transcript above it.
        let styles = self.styles.clone();
        apply_highlights(&self.editor, &self.multi_buffer, &styles, cx);
        cx.notify();
    }
}

/// Renders messages into spans. `previous` is the timestamp of the message
/// before the run, so an appended run can still open with a day separator.
fn render_messages(messages: &[Message], me: Option<u64>, previous: Option<i64>) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut last = previous;
    for message in messages {
        if last.is_none_or(|last| crosses_day(last, message.timestamp)) {
            spans.push(Span::styled(
                format!("── {} ──\n", day_label(message.timestamp)),
                Class::Muted,
            ));
        }
        let sender_class = if me == Some(message.sender_id) {
            Class::You
        } else {
            Class::Sender
        };
        let name = if message.sender_full_name.is_empty() {
            message.sender_id.to_string()
        } else {
            message.sender_full_name.clone()
        };
        spans.push(Span::styled(name, sender_class));
        spans.push(Span::styled(
            format!("  {}", clock_time(message.timestamp)),
            Class::Time,
        ));
        if message.mentions_you() {
            spans.push(Span::styled("  @you", Class::Mention));
        }
        spans.push(Span::plain("\n"));
        // Message bodies start at column zero: indenting them would turn
        // every message into a Markdown code block.
        spans.push(Span::plain(format!("{}\n", message.content.trim_end())));
        if !message.reactions.is_empty() {
            let mut names = message
                .reactions
                .iter()
                .map(|reaction| reaction.emoji_name.as_str())
                .collect::<Vec<_>>();
            names.sort_unstable();
            names.dedup();
            spans.push(Span::styled(format!(":{}:\n", names.join(": :")), Class::Time));
        }
        spans.push(Span::plain("\n"));
        last = Some(message.timestamp);
    }
    spans
}

impl gpui::Render for NarrowView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoZulipNarrow")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u64, sender: u64, content: &str, timestamp: i64) -> Message {
        Message {
            id,
            sender_id: sender,
            sender_full_name: format!("user{sender}"),
            content: content.to_owned(),
            timestamp,
            topic: "colors".to_owned(),
            kind: "stream".to_owned(),
            stream_id: Some(7),
            flags: Vec::new(),
            reactions: Vec::new(),
            display_recipient: serde_json::Value::Null,
        }
    }

    fn rendered(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn bodies_are_not_indented() {
        let spans = render_messages(&[message(1, 2, "# heading", 0)], None, Some(0));
        assert!(
            rendered(&spans).contains("\n# heading\n"),
            "an indented body would parse as a code block"
        );
    }

    #[test]
    fn your_own_messages_carry_their_own_class() {
        let spans = render_messages(&[message(1, 2, "hi", 0)], Some(2), Some(0));
        assert!(spans.iter().any(|span| span.class == Some(Class::You)));
    }

    #[test]
    fn a_day_separator_opens_the_first_run() {
        let spans = render_messages(&[message(1, 2, "hi", 0)], None, None);
        assert!(rendered(&spans).starts_with("── "));
    }
}
