//! One conversation: a transcript with a compose region at its end.
//!
//! A channel, a group, a DM, and a thread are the same surface. What differs
//! is where a composed message goes, and that is the source's business, not
//! the view's.
//!
//! Block Kit is rendered to text by the model, so the transcript is plain
//! prose carrying the host's Markdown pipeline, exactly like the agent
//! transcript beside it.

use std::ops::Range;

use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorEvent, EditorMode, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use text::Anchor;
use theme::ActiveTheme as _;

use crate::model::Model;
use crate::session::{Session, Source};
use crate::types::{Message, ThreadKey, Ts};
use crate::ui::{
    Class, Hooks, Span, apply_highlights, clock_time, crosses_day, day_label, lay_out,
};

pub struct ConversationView {
    session: Entity<Session>,
    source: Source,
    transcript: Entity<Buffer>,
    input: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// The thread each transcript line belongs to, so `enter` on a message
    /// knows which thread to open without any of it reaching the screen.
    line_threads: Vec<Option<Ts>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl ConversationView {
    pub fn new(
        session: Entity<Session>,
        source: Source,
        hooks: Hooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let transcript = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            (hooks.configure_markdown)(&mut buffer, cx);
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
        // Reaching the top of the transcript is the ask for older history:
        // the same gesture as scrolling back in any other reader.
        subscriptions.push(
            cx.subscribe(&editor, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::ScrollPositionChanged { .. }) {
                    let at_top =
                        editor.update(cx, |editor, cx| editor.scroll_position(cx).y <= 0.5);
                    if at_top {
                        this.load_older(cx);
                    }
                }
            }),
        );

        let mut view = Self {
            session: session.clone(),
            source: source.clone(),
            transcript,
            input,
            multi_buffer,
            editor,
            line_threads: Vec::new(),
            _subscriptions: subscriptions,
        };
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

    /// The thread the cursor is in, for opening a thread from a channel. A
    /// thread surface has none: it is already the thread.
    pub fn cursor_thread(&self, cx: &mut Context<Self>) -> Option<ThreadKey> {
        if matches!(self.source, Source::Thread(_)) {
            return None;
        }
        let row = self.editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
                .row as usize
        });
        let thread_ts = self.line_threads.get(row).cloned().flatten()?;
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

    fn refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let Some(loaded) = session.loaded(&self.source) else {
            return;
        };
        let notice = match (&loaded.error, loaded.loading) {
            (Some(error), _) => Some(Span::styled(format!("{error}\n\n"), Class::Error)),
            (None, true) => Some(Span::styled("loading…\n\n", Class::Muted)),
            (None, false) => None,
        };
        let mut spans = notice.iter().cloned().collect::<Vec<_>>();
        let (body, line_threads) = render_messages(&loaded.messages, session.model());
        spans.extend(body);
        // The notice occupies its own lines above the transcript, so the
        // line map has to start where the transcript does.
        let leading = notice
            .map(|notice| notice.text.matches('\n').count())
            .unwrap_or(0);
        let mut lines = vec![None; leading];
        lines.extend(line_threads);
        self.line_threads = lines;

        let (text, styles) = lay_out(&spans);
        let anchored = self.transcript.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
            let snapshot = buffer.snapshot();
            styles
                .into_iter()
                .map(|(class, range)| {
                    let clamp = |offset: usize| offset.min(snapshot.len());
                    (
                        class,
                        vec![
                            snapshot.anchor_before(clamp(range.start))
                                ..snapshot.anchor_after(clamp(range.end)),
                        ],
                    )
                })
                .collect::<Vec<(Class, Vec<Range<Anchor>>)>>()
        });
        apply_highlights(&self.editor, &self.multi_buffer, &anchored, cx);
        cx.notify();
    }
}

/// Renders messages into spans, and the thread each produced line belongs
/// to. Names, not ids; wall-clock times, not Slack timestamps.
fn render_messages(messages: &[Message], model: &Model) -> (Vec<Span>, Vec<Option<Ts>>) {
    let mut spans = Vec::new();
    let mut lines = Vec::new();
    let mut last: Option<i64> = None;
    for message in messages {
        let at = message.ts.epoch_seconds() as i64;
        if last.is_none_or(|last| crosses_day(last, at)) {
            spans.push(Span::styled(
                format!("── {} ──\n", day_label(at)),
                Class::Muted,
            ));
            lines.push(None);
        }
        let thread = Some(message.thread_root());
        let you = message.user.as_ref() == Some(model.self_id());
        spans.push(Span::styled(
            model.author(message),
            match you {
                true => Class::You,
                false => Class::Sender,
            },
        ));
        spans.push(Span::styled(format!("  {}", clock_time(at)), Class::Time));
        if message.thread_ts.is_some() {
            spans.push(Span::styled("  in thread", Class::Topic));
        }
        spans.push(Span::plain("\n"));
        lines.push(thread.clone());
        // Bodies start at column zero: indenting them would turn every
        // message into a Markdown code block.
        let body = model.render(message);
        let body = body.trim_end();
        spans.push(Span::plain(format!("{body}\n\n")));
        lines.extend(std::iter::repeat_n(thread, body.matches('\n').count() + 2));
        last = Some(at);
    }
    (spans, lines)
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

    fn rendered(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn a_message_reads_as_a_name_a_time_and_prose() {
        let (spans, _) = render_messages(&[message("1700000000.0", None, "hello")], &model());
        let text = rendered(&spans);
        assert!(text.contains("ada"), "{text}");
        assert!(!text.contains("1700000000"), "no raw timestamps: {text}");
        assert!(text.contains("\nhello\n"), "bodies are not indented");
    }

    #[test]
    fn every_line_of_a_message_opens_its_thread() {
        let messages = [
            message("1700000000.0", None, "first line\nsecond line"),
            message("1700000001.0", Some("1700000000.0"), "a reply"),
        ];
        let (spans, lines) = render_messages(&messages, &model());
        assert_eq!(
            lines.len(),
            rendered(&spans).matches('\n').count(),
            "the line map must cover the transcript exactly"
        );
        let root = Ts("1700000000.0".into());
        assert!(
            lines
                .iter()
                .filter(|line| line.as_ref() == Some(&root))
                .count()
                >= 5,
            "both messages belong to the same thread"
        );
    }
}
