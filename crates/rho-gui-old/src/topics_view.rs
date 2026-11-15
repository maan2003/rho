//! The streams and their topics, the way Zulip's left sidebar lists them:
//! every stream a heading, its topics under it newest first, each with how
//! many messages are unread. A resolved topic is hidden until asked for.
//!
//! Like Home, the surface is one keyed transcript: a row is an item keyed
//! by the topic it lists, so a new message edits its row and leaves the
//! cursor and every other row alone.

use std::collections::HashSet;
use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, HighlightStyle, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_agents_client::HostTopic;
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

/// The browser's highlight-key space, clear of Home's at a third.
const TOPICS_KEY_BASE: usize = usize::MAX / 3 + usize::MAX / 12;

/// One topic as a row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TopicRow {
    pub topic: HostTopic,
    pub name: String,
    /// Messages not from the user after where the user read.
    pub unread: usize,
    pub resolved: bool,
    /// How long ago it moved, in the deal bar's units.
    pub age: String,
}

/// A stream and its topics, newest first.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StreamRows {
    pub name: String,
    pub topics: Vec<TopicRow>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BrowserRows {
    pub streams: Vec<StreamRows>,
}

/// What a row offers the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BrowserTarget {
    Topic(HostTopic),
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum BrowserKey {
    Stream(String),
    Topic(HostTopic),
    Hidden,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BrowserClass {
    Stream,
    Title,
    Unread,
    Muted,
}

impl BrowserClass {
    fn slot(self) -> usize {
        match self {
            Self::Stream => 0,
            Self::Title => 1,
            Self::Unread => 2,
            Self::Muted => 3,
        }
    }

    const COUNT: usize = 4;
}

impl rho_transcript::Style for BrowserClass {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(TOPICS_KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let (color, weight) = match self {
            Self::Stream => (colors.text, Some(gpui::FontWeight::BOLD)),
            Self::Title => (colors.text, None),
            Self::Unread => (cx.theme().status().warning, Some(gpui::FontWeight::BOLD)),
            Self::Muted => (colors.text_muted, None),
        };
        HighlightStyle {
            color: Some(color.into()),
            font_weight: weight,
            ..HighlightStyle::default()
        }
    }
}

pub struct TopicsView {
    transcript: Transcript<BrowserKey, BrowserClass, BrowserTarget>,
    editor: Entity<Editor>,
    rows: BrowserRows,
    show_resolved: bool,
}

impl TopicsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
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
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            editor
        });
        let mut view = Self {
            transcript: Transcript::new(buffer),
            editor,
            rows: BrowserRows::default(),
            show_resolved: false,
        };
        view.transcript.attach(&view.editor.clone(), cx);
        let items = view.items();
        view.reconcile(items, cx);
        view.focus_first_row(cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The topic the cursor is on.
    pub(crate) fn cursor_target(&self, cx: &mut Context<Self>) -> BrowserTarget {
        let row = self.editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
                .row
        });
        self.transcript
            .line_meta(row, cx)
            .cloned()
            .unwrap_or(BrowserTarget::None)
    }

    pub(crate) fn set_rows(&mut self, rows: BrowserRows, cx: &mut Context<Self>) {
        if rows == self.rows {
            return;
        }
        self.rows = rows;
        self.relayout(cx);
    }

    pub(crate) fn toggle_resolved(&mut self, cx: &mut Context<Self>) {
        self.show_resolved = !self.show_resolved;
        self.relayout(cx);
    }

    /// Lays the rows out again with the cursor on the topic it was on, or
    /// on the first topic if that one is gone.
    fn relayout(&mut self, cx: &mut Context<Self>) {
        let held = self.cursor_target(cx);
        let items = self.items();
        self.reconcile(items, cx);
        match held {
            BrowserTarget::Topic(_) if self.row_of(&held, cx).is_some() => {
                let row = self.row_of(&held, cx).unwrap_or(0);
                self.put_cursor(row, cx);
            }
            _ => self.focus_first_row(cx),
        }
        cx.notify();
    }

    fn row_of(&self, target: &BrowserTarget, cx: &App) -> Option<u32> {
        let last = self.transcript.buffer().read(cx).snapshot().max_point().row;
        (0..=last).find(|row| self.transcript.line_meta(*row, cx) == Some(target))
    }

    fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let last = self.transcript.buffer().read(cx).snapshot().max_point().row;
        let row = (0..=last)
            .find(|row| {
                matches!(
                    self.transcript.line_meta(*row, cx),
                    Some(BrowserTarget::Topic(_))
                )
            })
            .unwrap_or(0);
        self.put_cursor(row, cx);
    }

    fn put_cursor(&mut self, row: u32, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    fn items(&self) -> Vec<Item<BrowserKey, BrowserClass, BrowserTarget>> {
        browser_items(&self.rows, self.show_resolved)
    }

    fn reconcile(
        &mut self,
        items: Vec<Item<BrowserKey, BrowserClass, BrowserTarget>>,
        cx: &mut App,
    ) {
        let wanted = items
            .iter()
            .map(|item| item.key.clone())
            .collect::<HashSet<_>>();
        for key in self.transcript.keys().cloned().collect::<Vec<_>>() {
            if !wanted.contains(&key) {
                self.transcript.remove(&key, cx);
            }
        }
        let mut previous: Option<BrowserKey> = None;
        for item in items {
            let key = item.key.clone();
            if !self.transcript.contains(&key) {
                self.transcript
                    .insert_after(previous.as_ref(), vec![item], cx);
            } else if self.transcript.key_before(&key).cloned() == previous {
                self.transcript.replace(&key, item, cx);
            } else {
                self.transcript.remove(&key, cx);
                self.transcript
                    .insert_after(previous.as_ref(), vec![item], cx);
            }
            previous = Some(key);
        }
    }
}

/// The rows as items: each stream with the topics it shows, and how many
/// resolved topics are hidden.
fn browser_items(
    rows: &BrowserRows,
    show_resolved: bool,
) -> Vec<Item<BrowserKey, BrowserClass, BrowserTarget>> {
    let mut items = Vec::new();
    let mut hidden = 0;
    let shown = |topic: &&TopicRow| show_resolved || !topic.resolved;
    let column = rows
        .streams
        .iter()
        .flat_map(|stream| stream.topics.iter().filter(shown))
        .map(|topic| title(topic).chars().count())
        .max()
        .unwrap_or(0)
        .min(48);
    for stream in &rows.streams {
        hidden += stream.topics.iter().filter(|topic| topic.resolved).count();
        let topics = stream.topics.iter().filter(shown).collect::<Vec<_>>();
        // A stream whose every topic is hidden says nothing a heading
        // could; one with no topics yet is still a stream to know of.
        if topics.is_empty() && !stream.topics.is_empty() {
            continue;
        }
        let unread = topics.iter().map(|topic| topic.unread).sum::<usize>();
        items.push(heading(
            BrowserKey::Stream(stream.name.clone()),
            &stream.name,
            unread,
        ));
        for topic in topics {
            items.push(topic_line(topic, column));
        }
    }
    if show_resolved {
        hidden = 0;
    }
    if items.is_empty() && hidden == 0 {
        let text = "no streams yet";
        items.push(
            Item::new(BrowserKey::Empty, text.to_owned())
                .with_styles(vec![(BrowserClass::Muted, 0..text.len())])
                .with_lines(vec![BrowserTarget::None]),
        );
    }
    if hidden > 0 {
        let text = format!("{hidden} resolved hidden · t shows them");
        items.push(
            Item::new(BrowserKey::Hidden, text.clone())
                .with_styles(vec![(BrowserClass::Muted, 0..text.len())])
                .with_lines(vec![BrowserTarget::None]),
        );
    }
    items
}

/// A topic's name as its row shows it: `✔` before a resolved one.
fn title(topic: &TopicRow) -> String {
    match topic.resolved {
        true => format!("✔ {}", topic.name),
        false => topic.name.clone(),
    }
}

fn heading(
    key: BrowserKey,
    name: &str,
    unread: usize,
) -> Item<BrowserKey, BrowserClass, BrowserTarget> {
    let mut text = name.to_owned();
    let mut styles = vec![(BrowserClass::Stream, 0..text.len())];
    if unread > 0 {
        text.push_str("  ");
        let start = text.len();
        text.push_str(&unread.to_string());
        styles.push((BrowserClass::Unread, start..text.len()));
    }
    Item::new(key, text)
        .with_styles(styles)
        .with_lines(vec![BrowserTarget::None])
}

fn topic_line(topic: &TopicRow, column: usize) -> Item<BrowserKey, BrowserClass, BrowserTarget> {
    let name = title(topic);
    let name = match name.chars().count() > column {
        true => format!(
            "{}…",
            name.chars()
                .take(column.saturating_sub(1))
                .collect::<String>()
        ),
        false => name,
    };
    let mut text = "  ".to_owned();
    let mut styles: Vec<(BrowserClass, Range<usize>)> = Vec::new();
    let start = text.len();
    text.push_str(&name);
    let class = match topic.resolved {
        true => BrowserClass::Muted,
        false => BrowserClass::Title,
    };
    styles.push((class, start..text.len()));
    for _ in name.chars().count()..column {
        text.push(' ');
    }
    text.push_str("  ");
    if topic.unread > 0 {
        let start = text.len();
        text.push_str(&topic.unread.to_string());
        styles.push((BrowserClass::Unread, start..text.len()));
        text.push_str(" · ");
    }
    let start = text.len();
    text.push_str(&topic.age);
    styles.push((BrowserClass::Muted, start..text.len()));
    Item::new(BrowserKey::Topic(topic.topic), text.trim_end().to_owned())
        .with_styles(styles)
        .with_lines(vec![BrowserTarget::Topic(topic.topic)])
}

impl gpui::Render for TopicsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoTopics")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{ChatSeq, TopicId};

    use super::*;

    fn row(topic: u64, name: &str, unread: usize, resolved: bool) -> TopicRow {
        TopicRow {
            topic: HostTopic {
                machine_seed: 0,
                topic: TopicId(ChatSeq(topic)),
            },
            name: name.to_owned(),
            unread,
            resolved,
            age: "2m".to_owned(),
        }
    }

    fn texts(rows: &BrowserRows, show_resolved: bool) -> Vec<String> {
        browser_items(rows, show_resolved)
            .into_iter()
            .map(|item| item.text.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn resolved_topics_hide_until_asked_for_and_count_nothing_meanwhile() {
        let rows = BrowserRows {
            streams: vec![StreamRows {
                name: "rho".to_owned(),
                topics: vec![row(2, "deploy", 3, false), row(4, "flaky test", 5, true)],
            }],
        };
        assert_eq!(
            texts(&rows, false),
            vec![
                "rho  3",
                "  deploy  3 · 2m",
                "1 resolved hidden · t shows them"
            ],
            "a hidden topic's unread is not the stream's"
        );
        assert_eq!(
            texts(&rows, true),
            vec!["rho  8", "  deploy        3 · 2m", "  ✔ flaky test  5 · 2m"]
        );
    }

    #[test]
    fn a_stream_with_every_topic_hidden_shows_no_heading() {
        let rows = BrowserRows {
            streams: vec![
                StreamRows {
                    name: "done".to_owned(),
                    topics: vec![row(2, "shipped", 1, true)],
                },
                StreamRows {
                    name: "new".to_owned(),
                    topics: Vec::new(),
                },
            ],
        };
        assert_eq!(
            texts(&rows, false),
            vec!["new", "1 resolved hidden · t shows them"],
            "a stream with no topics yet keeps its heading"
        );
        assert_eq!(
            texts(&rows, true),
            vec!["done  1", "  ✔ shipped  1 · 2m", "new"]
        );
    }
}
