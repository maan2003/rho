//! Home: the dealer's own ranking, read at a glance.
//!
//! Nothing here scores anything. The rows are the dealer's cards, in the
//! dealer's order, wearing the words the deal bar already uses; Home only
//! decides where the line falls and how many rows either side of it a
//! reader is shown.
//!
//! The surface is one keyed transcript: a row is an item keyed by the card
//! it stands for or the agent it watches, so a score change, an agent's new
//! output line, or a card crossing the line edits that row and leaves the
//! cursor, the scroll, and every other row alone.

use std::collections::HashSet;
use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, HighlightStyle, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_agent_types::AgentId;
use rho_dealer::curve::age_label;
use rho_dealer::{Card, CardKind, NodeId};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

/// Home's own highlight-key space, clear of the transcript's semantic slots
/// at zero, Slack's at the top, and the shell's ANSI
/// block at half. Buckets grow upwards from here.
const HOME_KEY_BASE: usize = usize::MAX / 3;

/// How many rows a section shows. Hard: Home is a glance, and a list you
/// can groom is an inbox.
pub(crate) const HOME_CAP: usize = 5;

/// One card as a line of Home: what it is, and the same state words the
/// deal bar says for it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HomeRow {
    pub title: String,
    pub label: String,
    pub card: NodeId,
    /// Passed over by a pull. The card is still open and still ranked here;
    /// the word is how Home says the reader has already seen it.
    pub skipped: bool,
}

/// One live agent: name, what it is on, how long it has been on it, and the
/// last thing it said.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunningRow {
    pub agent_id: AgentId,
    pub name: String,
    pub topic: String,
    pub elapsed: String,
    pub last_line: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HomeRows {
    /// The top of the queue above the cutoff: a preview, not the queue.
    pub next: Vec<HomeRow>,
    pub running: Vec<RunningRow>,
}

impl HomeRows {
    pub fn is_empty(&self) -> bool {
        self.next.is_empty() && self.running.is_empty()
    }
}

/// The top of the dealer's hand, in its order.
pub(crate) fn split_hand(cards: &[Card], title: impl Fn(&Card) -> String) -> HomeRows {
    let row = |card: &Card| HomeRow {
        title: title(card),
        label: card.label.clone(),
        card: card.node.clone(),
        skipped: card.skipped,
    };
    HomeRows {
        next: cards.iter().take(HOME_CAP).map(row).collect(),
        running: Vec::new(),
    }
}

/// What a card is called on a Home row. An agent is its tag, a Slack card
/// is its conversation and what it is about, and anything else is its
/// title.
pub(crate) fn card_title(card: &Card, agent_tag: impl Fn(AgentId) -> String) -> String {
    match (card.kind, &card.node) {
        (CardKind::Agent, NodeId::Agent(agent_id)) => agent_tag(*agent_id),
        (CardKind::Slack, _) if card.context.is_empty() => card.title.clone(),
        (CardKind::Slack, _) if card.title.is_empty() => card.context.clone(),
        (CardKind::Slack, _) => format!("{} › {}", card.context, card.title),
        _ => card.title.clone(),
    }
}

/// How long a running turn has been running, in the deal bar's own units.
pub(crate) fn elapsed_label(since_ms: i64, now_ms: i64) -> String {
    age_label(((now_ms - since_ms).max(0)) as f64 / 86_400_000.0)
}

/// How long the running turn has run, for the row that says it is running.
/// Empty when the head says a turn runs without saying since when, which
/// is every turn that started before the client was listening: a row with
/// no start has no duration to print, and printing one from a missing
/// start is how a turn came to have been running since the epoch.
pub(crate) fn running_elapsed_label(facts: &rho_agents_client::AgentFacts, now_ms: i64) -> String {
    facts.turn_started_at.map_or_else(String::new, |started| {
        elapsed_label(started.0 as i64, now_ms)
    })
}

/// What a Home row offers when the cursor is on it. Home closes nothing
/// and opens nothing of its own: a row is a card, and a card is dealt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeTarget {
    Card(NodeId),
    Agent(AgentId),
    /// A section heading or the empty line: nothing to open.
    None,
}

/// One item of the transcript. A card keeps its key when it crosses the
/// line, so crossing moves the row rather than rewriting two sections.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HomeKey {
    Section(&'static str),
    Card(NodeId),
    Agent(AgentId),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum HomeClass {
    Section,
    Title,
    Label,
    Muted,
}

impl HomeClass {
    fn slot(self) -> usize {
        match self {
            Self::Section => 0,
            Self::Title => 1,
            Self::Label => 2,
            Self::Muted => 3,
        }
    }

    const COUNT: usize = 4;
}

impl rho_transcript::Style for HomeClass {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(HOME_KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
            // The sections are chrome, not content: they name the line and
            // then get out of the way.
            Self::Section => colors.text_muted,
            Self::Title => colors.text,
            Self::Label => cx.theme().status().warning,
            Self::Muted => colors.text_muted,
        };
        HighlightStyle {
            color: Some(color.into()),
            ..HighlightStyle::default()
        }
    }
}

/// One buffer, one surface. The editor is an ordinary rho editor, so
/// motions, search and vim come from it rather than from list chrome.
pub struct HomeView {
    transcript: Transcript<HomeKey, HomeClass, HomeTarget>,
    editor: Entity<Editor>,
    rows: HomeRows,
}

impl HomeView {
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
            rows: HomeRows::default(),
        };
        view.transcript.attach(&view.editor.clone(), cx);
        // A Home nobody has told anything yet still answers the question.
        let items = view.items();
        view.reconcile(items, cx);
        view.focus_first_row(cx);
        view
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// What the cursor is on. The row under the cursor is the one a deal
    /// opens; a heading offers nothing.
    pub(crate) fn cursor_target(&self, cx: &mut Context<Self>) -> HomeTarget {
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
            .unwrap_or(HomeTarget::None)
    }

    pub(crate) fn set_rows(&mut self, rows: HomeRows, cx: &mut Context<Self>) {
        if rows == self.rows {
            return;
        }
        self.rows = rows;
        let items = self.items();
        self.reconcile(items, cx);
        // A reader who has not moved is put on the first row, so the very
        // first Enter deals rather than landing on a heading.
        if self.cursor_target(cx) == HomeTarget::None {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    /// Puts the cursor on the first row that stands for something. There is
    /// no window here (the dealer invalidates Home from a timer), so the
    /// selection is changed without the effects a keypress would carry.
    fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let last = self.transcript.buffer().read(cx).snapshot().max_point().row;
        // With nothing to open the cursor still belongs on the one line
        // there is, not on the blank row after it, which would read as an
        // editable line.
        let row = (0..=last)
            .find(|row| {
                !matches!(
                    self.transcript.line_meta(*row, cx),
                    None | Some(HomeTarget::None)
                )
            })
            .unwrap_or(0);
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    /// The rows as transcript items. Titles are padded to a common column
    /// per section so the state words line up and the eye reads down them.
    fn items(&self) -> Vec<Item<HomeKey, HomeClass, HomeTarget>> {
        let mut items = Vec::new();
        if self.rows.is_empty() {
            return vec![
                line(HomeKey::Empty, "nothing needs attention", HomeClass::Muted)
                    .with_lines(vec![HomeTarget::None]),
            ];
        }
        if !self.rows.next.is_empty() {
            items.push(section("next"));
            let column = column_of(self.rows.next.iter().map(|row| row.title.as_str()));
            for row in &self.rows.next {
                items.push(card_line(row, column, HomeClass::Title));
            }
        }
        if !self.rows.running.is_empty() {
            items.push(section("running"));
            let column = column_of(self.rows.running.iter().map(|row| row.name.as_str()));
            let topics = column_of(self.rows.running.iter().map(|row| row.topic.as_str()));
            for row in &self.rows.running {
                items.push(running_line(row, column, topics));
            }
        }
        items
    }

    /// Walks the wanted items against what is on screen: a row that stayed
    /// put is replaced (and an identical one costs nothing), a row that
    /// moved is lifted and re-inserted, and a row nobody wants goes.
    fn reconcile(&mut self, items: Vec<Item<HomeKey, HomeClass, HomeTarget>>, cx: &mut App) {
        let wanted = items
            .iter()
            .map(|item| item.key.clone())
            .collect::<HashSet<_>>();
        for key in self.transcript.keys().cloned().collect::<Vec<_>>() {
            if !wanted.contains(&key) {
                self.transcript.remove(&key, cx);
            }
        }
        let mut previous: Option<HomeKey> = None;
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

fn column_of<'a>(titles: impl Iterator<Item = &'a str>) -> usize {
    /// Past this a title is left to run into its own state words rather
    /// than pushing every row on the screen sideways.
    const CAP: usize = 40;
    titles
        .map(str::chars)
        .map(Iterator::count)
        .max()
        .unwrap_or(0)
        .min(CAP)
}

fn section(name: &'static str) -> Item<HomeKey, HomeClass, HomeTarget> {
    line(HomeKey::Section(name), name, HomeClass::Section).with_lines(vec![HomeTarget::None])
}

fn line(key: HomeKey, text: &str, class: HomeClass) -> Item<HomeKey, HomeClass, HomeTarget> {
    Item::new(key, text.to_owned()).with_styles(vec![(class, 0..text.len())])
}

fn card_line(
    row: &HomeRow,
    column: usize,
    title_class: HomeClass,
) -> Item<HomeKey, HomeClass, HomeTarget> {
    let label = match (row.skipped, row.label.is_empty()) {
        (false, _) => row.label.clone(),
        (true, true) => "skipped".to_owned(),
        (true, false) => format!("{} · skipped", row.label),
    };
    let (text, styles) = columns(&[
        (row.title.as_str(), title_class, column),
        (label.as_str(), HomeClass::Label, 0),
    ]);
    Item::new(HomeKey::Card(row.card.clone()), text)
        .with_styles(styles)
        .with_lines(vec![HomeTarget::Card(row.card.clone())])
}

fn running_line(
    row: &RunningRow,
    name_column: usize,
    topic_column: usize,
) -> Item<HomeKey, HomeClass, HomeTarget> {
    let (text, styles) = columns(&[
        (row.name.as_str(), HomeClass::Title, name_column),
        (row.topic.as_str(), HomeClass::Muted, topic_column),
        (row.elapsed.as_str(), HomeClass::Label, 4),
        (row.last_line.as_str(), HomeClass::Muted, 0),
    ]);
    Item::new(HomeKey::Agent(row.agent_id), text)
        .with_styles(styles)
        .with_lines(vec![HomeTarget::Agent(row.agent_id)])
}

/// Lays out one row: two leading spaces, then each cell padded to its
/// column. Offsets are byte offsets into the row's own text, which is what
/// the transcript wants.
fn columns(cells: &[(&str, HomeClass, usize)]) -> (String, Vec<(HomeClass, Range<usize>)>) {
    let mut text = "  ".to_owned();
    let mut styles = Vec::new();
    for (index, (cell, class, column)) in cells.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let start = text.len();
        text.push_str(cell);
        styles.push((*class, start..text.len()));
        let width = cell.chars().count();
        for _ in width..*column {
            text.push(' ');
        }
    }
    (text.trim_end().to_owned(), styles)
}

impl gpui::Render for HomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoHome")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(title: &str, priority: f64) -> Card {
        Card {
            node: NodeId::Note(uuid::Uuid::nil()),
            kind: CardKind::Dated,
            title: title.to_owned(),
            context: String::new(),
            label: format!("needs reply · {priority}"),
            priority,
            cursor: String::new(),
            skipped: false,
        }
    }

    #[test]
    fn next_is_the_top_of_the_hand_capped() {
        let cards: Vec<Card> = (0..14)
            .map(|rank| card(&format!("card {rank}"), 2.0 - rank as f64 * 0.2))
            .collect();
        let rows = split_hand(&cards, |card| card.title.clone());
        assert_eq!(rows.next.len(), HOME_CAP);
        assert_eq!(rows.next[0].title, "card 0", "the dealer's order stands");
    }

    #[test]
    fn a_row_says_what_the_deal_bar_says() {
        let thread = Card {
            context: "#design".to_owned(),
            title: "can you look at the deploy?".to_owned(),
            kind: CardKind::Slack,
            label: "needs reply · 1.9h".to_owned(),
            skipped: false,
            ..card("", 2.0)
        };
        let rows = split_hand(std::slice::from_ref(&thread), |card| {
            card_title(card, |_| unreachable!())
        });
        assert_eq!(rows.next[0].title, "#design › can you look at the deploy?");
        assert_eq!(rows.next[0].label, "needs reply · 1.9h");

        let agent = Card {
            node: NodeId::Agent(
                AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap(),
            ),
            kind: CardKind::Agent,
            title: "slack polish".to_owned(),
            ..card("", 1.0)
        };
        assert_eq!(
            card_title(&agent, |_| "eng-b8os".to_owned()),
            "eng-b8os",
            "an agent row is the agent, not where it is filed"
        );
    }

    #[test]
    fn elapsed_reads_in_the_deal_bars_units() {
        let now = 10 * 86_400_000;
        assert_eq!(elapsed_label(now - 12 * 60_000, now), "12m");
        assert_eq!(elapsed_label(now - 3 * 3_600_000, now), "3.0h");
        // A clock that ran backwards is nobody's business but ours.
        assert_eq!(elapsed_label(now + 60_000, now), "0m");
    }
}
