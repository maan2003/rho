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
use crate::types::{FileSummary, Message, ThreadKey, Ts};
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
    /// The file each line offers, for the same reason: `enter` on a file
    /// line opens the file rather than the thread it hangs under.
    line_files: Vec<Option<FileSummary>>,
    /// The preview blocks currently under image lines, so a redraw can take
    /// them down before it puts the new ones up.
    image_blocks: Vec<editor::display_map::CustomBlockId>,
    /// How far the body column is indented, so a preview lines up under the
    /// prose rather than under the clock.
    body_indent: usize,
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
            line_files: Vec::new(),
            image_blocks: Vec::new(),
            body_indent: 0,
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

    /// The file on the cursor's line, if the line is one. A file is opened
    /// before a thread, because the reader who put the cursor on a file line
    /// asked for the file.
    pub fn cursor_file(&self, cx: &mut Context<Self>) -> Option<FileSummary> {
        self.line_files.get(self.cursor_row(cx)).cloned().flatten()
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
        let thread_ts = self
            .line_threads
            .get(self.cursor_row(cx))
            .cloned()
            .flatten()?;
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
        let transcript = render_messages(
            &loaded.messages,
            session.model(),
            matches!(self.source, Source::Thread(_)),
        );
        let (body, line_threads, line_files) =
            (transcript.spans, transcript.threads, transcript.files);
        spans.extend(body);
        // The notice occupies its own lines above the transcript, so the
        // line map has to start where the transcript does.
        let leading = notice
            .map(|notice| notice.text.matches('\n').count())
            .unwrap_or(0);
        let mut lines = vec![None; leading];
        lines.extend(line_threads);
        self.line_threads = lines;
        let mut files = vec![None; leading];
        files.extend(line_files);
        self.line_files = files;
        self.body_indent = transcript.indent;

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
        self.refresh_images(cx);
        cx.notify();
    }

    /// Shows an image under the line that names it. The bytes are fetched
    /// when the line is on screen for the first time and read from the state
    /// cache after that, so scrolling costs nothing and nothing is
    /// downloaded that the reader never opened.
    fn refresh_images(&mut self, cx: &mut Context<Self>) {
        let taken = std::mem::take(&mut self.image_blocks);
        if !taken.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(taken.into_iter().collect(), None, cx);
            });
        }
        let images = self
            .line_files
            .iter()
            .enumerate()
            .filter_map(|(row, file)| {
                let file = file.as_ref().filter(|file| file.is_image())?;
                Some((row as u32, file.clone()))
            })
            .collect::<Vec<_>>();
        if images.is_empty() {
            return;
        }
        let mut blocks = Vec::new();
        for (row, file) in images {
            let path = self.session.update(cx, |session, cx| {
                session.cache_file(&file, cx);
                session.cached_file(&file.id).map(std::path::Path::to_owned)
            });
            let Some(path) = path.filter(|path| path.exists()) else {
                continue;
            };
            let indent = self.body_indent;
            let point = Point::new(row, 0);
            let anchor = {
                let transcript = self.transcript.read(cx);
                if point > transcript.max_point() {
                    continue;
                }
                transcript.anchor_after(point)
            };
            let Some(anchor) = self
                .multi_buffer
                .read(cx)
                .snapshot(cx)
                .anchor_in_excerpt(anchor)
            else {
                continue;
            };
            blocks.push(editor::display_map::BlockProperties {
                placement: editor::display_map::BlockPlacement::Below(anchor),
                // Tall enough to read, short enough that a picture never
                // pushes the conversation off the screen.
                height: Some(IMAGE_ROWS),
                style: editor::display_map::BlockStyle::Fixed,
                render: std::sync::Arc::new(move |cx| {
                    // The spacer is real text in the transcript's own font,
                    // which is the only way to land the picture exactly under
                    // the body column whatever font the reader has set.
                    let style = cx.editor_style.text.clone();
                    div()
                        .flex()
                        .items_start()
                        .font_family(style.font_family.clone())
                        .text_size(style.font_size)
                        .child(" ".repeat(indent))
                        .child(
                            gpui::img(path.clone())
                                .max_h(cx.line_height * IMAGE_ROWS as f32)
                                .max_w_full(),
                        )
                        .into_any_element()
                }),
                priority: 0,
            });
        }
        if blocks.is_empty() {
            return;
        }
        self.image_blocks = self
            .editor
            .update(cx, |editor, cx| editor.insert_blocks(blocks, None, cx));
    }
}

/// How many lines tall an inline image preview is allowed to be.
const IMAGE_ROWS: u32 = 10;

/// Renders messages into spans, and the thread each produced line belongs
/// to. Names, not ids; wall-clock times, not Slack timestamps.
///
/// The shape is IRC's, which is the one people have read in a terminal for
/// thirty years: time, author, and the message on one line, authors padded
/// to a column so the bodies line up and the eye can run down them. No blank
/// line between messages; a day is the only break.
#[derive(Default)]
struct Transcript {
    spans: Vec<Span>,
    /// The thread each line belongs to, so `enter` opens the right one.
    threads: Vec<Option<Ts>>,
    /// The file each line offers, so `enter` opens that instead.
    files: Vec<Option<FileSummary>>,
    /// Columns the body starts at, which chrome under a line matches.
    indent: usize,
}

fn render_messages(messages: &[Message], model: &Model, in_thread: bool) -> Transcript {
    // Replies live in their thread and nowhere else, which is what Slack
    // does and what makes a channel readable. The exception is a reply the
    // sender also sent to the channel, which was addressed to the room.
    let shown = messages
        .iter()
        .filter(|message| in_thread || message.is_top_level())
        .collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut lines = Vec::new();
    let mut files: Vec<Option<FileSummary>> = Vec::new();
    let mut last: Option<i64> = None;
    let width = author_column(&shown, model);
    // Continuation lines start under the body, so a wrapped sentence reads
    // as one message rather than as a new one.
    let indent = " ".repeat(TIME_WIDTH + 2 + width + 2);
    for message in shown {
        let at = message.ts.epoch_seconds() as i64;
        if last.is_none_or(|last| crosses_day(last, at)) {
            spans.push(Span::styled(
                format!("── {} ──\n", day_label(at)),
                Class::Muted,
            ));
            lines.push(None);
            files.push(None);
        }
        let thread = Some(message.thread_root());
        // Slack shows joins, leaves, and topic changes, and so does rho:
        // leaving them out makes a channel read as if nothing happened.
        // They are one muted line, without an author column of their own.
        if let Some(line) = system_line(message, model) {
            spans.push(Span::styled(format!("{indent}{line}\n"), Class::Muted));
            lines.push(thread);
            files.push(None);
            last = Some(at);
            continue;
        }
        let you = message.user.as_ref() == Some(model.self_id());
        let author = fit(&model.author(message), width);
        spans.push(Span::styled(format!("{}  ", clock_time(at)), Class::Time));
        spans.push(Span::styled(
            author.clone(),
            match you {
                true => Class::You,
                false => Class::Sender,
            },
        ));
        spans.push(Span::plain(format!(
            "{}  ",
            " ".repeat(width.saturating_sub(author.chars().count()))
        )));

        let body = model.render(message);
        let body = body.trim_end().replace('\n', &format!("\n{indent}"));
        push_body(&mut spans, &body, model);
        if message.edited {
            // The reader is told what they are looking at is not what was
            // sent. It rides at the end of the body rather than after the
            // time, which would push every other line's columns across.
            spans.push(Span::styled(" (edited)", Class::Muted));
        }
        spans.push(Span::plain("\n"));
        lines.extend(std::iter::repeat_n(
            thread.clone(),
            body.matches('\n').count() + 1,
        ));
        // A file's line is the one that reads as the file: `enter` there
        // opens it rather than the thread.
        files.extend(body.split('\n').map(|line| {
            message
                .files
                .iter()
                .find(|file| line.trim() == file.line())
                .cloned()
        }));

        if !message.reactions.is_empty() {
            spans.push(Span::plain(indent.clone()));
            push_reactions(&mut spans, message, model);
            lines.push(thread.clone());
            files.push(None);
        }
        if !in_thread && message.is_broadcast() {
            // It was said in a thread and sent here too: the reader should
            // know which, or the thread reads as two conversations.
            spans.push(Span::styled(
                format!("{indent}also sent to the channel\n"),
                Class::Muted,
            ));
            lines.push(thread.clone());
            files.push(None);
        }
        // The thread under a message is one line, not a fold-out: the reader
        // sees that it exists and opens it with `enter`.
        if !in_thread && message.reply_count > 0 {
            spans.push(Span::styled(
                format!("{indent}{}\n", replies_line(message)),
                Class::Topic,
            ));
            lines.push(thread);
            files.push(None);
        }
        last = Some(at);
    }
    Transcript {
        spans,
        threads: lines,
        files,
        indent: indent.len(),
    }
}

/// `14:27`, which every line starts with.
const TIME_WIDTH: usize = 5;
/// Longer than this and one name would push every body across the screen, so
/// it is cut instead. Long enough for "Ada Lovelace".
const AUTHOR_LIMIT: usize = 14;

/// The width the author column takes in this conversation: as wide as the
/// longest name in it, so the bodies line up and no wider.
fn author_column(messages: &[&Message], model: &Model) -> usize {
    messages
        .iter()
        .map(|message| model.author(message).chars().count().min(AUTHOR_LIMIT))
        .max()
        .unwrap_or(0)
}

fn fit(name: &str, width: usize) -> String {
    match name.chars().count() > width {
        true => {
            name.chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
                + "…"
        }
        false => name.to_owned(),
    }
}

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

    fn rendered(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn a_message_is_one_line_of_time_author_and_prose() {
        let spans = render_messages(
            &[
                message("1700000000.0", None, "hello"),
                message("1700000060.0", None, "over\ntwo lines"),
            ],
            &model(),
            false,
        )
        .spans;
        let text = rendered(&spans);
        assert!(!text.contains("1700000000"), "no raw timestamps: {text}");
        let body = text
            .lines()
            .find(|line| line.contains("hello"))
            .expect("the message is on a line");
        assert!(
            body.ends_with("  ada  hello") && body.len() == "00:00  ada  hello".len(),
            "time, author, body, one line: {body:?}"
        );
        assert!(
            !text.contains("\n\n"),
            "no blank line between messages: {text:?}"
        );
        let continuation = text
            .lines()
            .find(|line| line.contains("two lines"))
            .expect("the second line of a body");
        assert_eq!(
            continuation, "            two lines",
            "continuation lines align under the body column"
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
        let spans = render_messages(&[own, mention], &model, false).spans;
        let text = rendered(&spans);

        assert!(!text.contains("you"), "the word never appears: {text}");
        assert!(text.contains("Manmeet"), "{text}");
        assert!(text.contains("can @Manmeet take this?"), "{text}");
        let yours = spans
            .iter()
            .filter(|span| span.class == Some(Class::You))
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            yours,
            vec!["Manmeet", "@Manmeet"],
            "own author line and a mention of the reader, class only"
        );
    }

    #[test]
    fn standard_emoji_are_glyphs_and_workspace_emoji_stay_muted_shortcodes() {
        let mut model = model();
        model.set_custom_emoji(["forrest_gump_wave".to_owned()]);
        let spans = render_messages(
            &[message(
                "1700000000.0",
                None,
                "morning :wave: :forrest_gump_wave:",
            )],
            &model,
            false,
        )
        .spans;
        let text = rendered(&spans);
        assert!(text.contains("morning 👋"), "{text}");
        assert!(
            text.contains(":forrest_gump_wave:"),
            "a workspace emoji has no glyph to become: {text}"
        );
        let muted = spans
            .iter()
            .find(|span| span.text == ":forrest_gump_wave:")
            .expect("the custom shortcode is its own span");
        assert_eq!(muted.class, Some(Class::Muted));
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

        let transcript = render_messages(&messages, &model(), false);
        let (spans, lines) = (transcript.spans, transcript.threads);
        let text = rendered(&spans);
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
            lines.last().and_then(|line| line.clone()),
            Some(root),
            "enter on the count line opens the thread"
        );

        // The thread surface is where every reply renders.
        let spans = render_messages(&messages, &model(), true).spans;
        let text = rendered(&spans);
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
        let transcript = render_messages(&[message], &model, false);
        let (spans, lines) = (transcript.spans, transcript.threads);
        let text = rendered(&spans);
        assert!(text.contains("👍 2 · 🎉 1"), "{text}");
        assert!(!text.contains("you"), "no word for it: {text}");
        assert_eq!(
            spans
                .iter()
                .find(|span| span.text == "👍 2")
                .map(|span| span.class),
            Some(Some(Class::You)),
            "one the reader added is theirs"
        );
        assert_eq!(
            spans
                .iter()
                .find(|span| span.text == "🎉 1")
                .map(|span| span.class),
            Some(Some(Class::Muted))
        );
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
        let transcript = render_messages(&[message], &model(), false);
        let (spans, lines) = (transcript.spans, transcript.threads);
        let text = rendered(&spans);
        assert!(text.contains("friday it is (edited)\n"), "{text}");
        assert_eq!(
            spans
                .iter()
                .find(|span| span.text == " (edited)")
                .map(|span| span.class),
            Some(Some(Class::Muted))
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
        let transcript = render_messages(&[join, bot, preview], &model(), false);
        let (spans, lines) = (transcript.spans, transcript.threads);
        let text = rendered(&spans);

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
        let muted = spans
            .iter()
            .filter(|span| span.class == Some(Class::Muted))
            .map(|span| span.text.trim().to_owned())
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
        let transcript = render_messages(&messages, &model(), true);
        let (spans, lines) = (transcript.spans, transcript.threads);
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
                >= 3,
            "both messages belong to the same thread"
        );
    }
}
