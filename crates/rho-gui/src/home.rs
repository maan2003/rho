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

/// One agent that said something lately, by name and how long ago.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecentRow {
    pub agent_id: AgentId,
    pub name: String,
    pub label: String,
}

/// One pile the user put cards on: Enter deals from it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PileRow {
    /// `None` for everything snoozed, shown as `later`.
    pub name: Option<String>,
    pub count: usize,
    /// When the first of it comes back by itself, in words; empty for a
    /// named pile, which comes back only when opened.
    pub back: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HomeRows {
    /// The top of the queue above the cutoff: a preview, not the queue.
    pub next: Vec<HomeRow>,
    /// The agents that sent lately, newest first, but for those already in
    /// `next`. Folded until opened: what wants the reader is a card above.
    pub recent: Vec<RecentRow>,
    /// Every pile, all of them: a pile nobody sees is where cards go to die.
    pub piles: Vec<PileRow>,
}

impl HomeRows {
    pub fn is_empty(&self) -> bool {
        self.next.is_empty() && self.recent.is_empty() && self.piles.is_empty()
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
        recent: Vec::new(),
        piles: Vec::new(),
    }
}

/// What a card is called on a Home row. A mail or Slack thread is who it
/// is with and what it is about, and anything else is its title.
pub(crate) fn card_title(card: &Card) -> String {
    let title = card.title.lines().next().unwrap_or_default();
    match card.kind {
        CardKind::Mail | CardKind::Slack if card.context.is_empty() => title.to_owned(),
        CardKind::Mail | CardKind::Slack if title.is_empty() => card.context.clone(),
        CardKind::Mail | CardKind::Slack => format!("{} › {title}", card.context),
        CardKind::Dated => title.to_owned(),
    }
}

/// How long ago something happened, in the deal bar's own units.
pub(crate) fn elapsed_label(since_ms: i64, now_ms: i64) -> String {
    age_label(((now_ms - since_ms).max(0)) as f64 / 86_400_000.0)
}

/// What a Home row offers when the cursor is on it. Home closes nothing
/// and opens nothing of its own: a row is a card, and a card is dealt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeTarget {
    Card(NodeId),
    Agent(AgentId),
    /// The recent heading: Enter folds or unfolds it.
    Recent,
    /// A pile, dealt from on Enter; `None` is everything snoozed.
    Pile(Option<String>),
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
    Pile(Option<String>),
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
    recent_open: bool,
    /// How many characters fit on one row, as the window last measured it;
    /// a card's title is cut to leave its row one line.
    width: usize,
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
        let width = row_width(&editor, window, cx);
        let mut view = Self {
            transcript: Transcript::new(buffer),
            editor,
            rows: HomeRows::default(),
            recent_open: false,
            width,
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

    pub(crate) fn toggle_recent(&mut self, cx: &mut Context<Self>) {
        self.recent_open = !self.recent_open;
        let items = self.items();
        self.reconcile(items, cx);
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
                items.push(card_line(row, column, self.width, HomeClass::Title));
            }
        }
        if !self.rows.recent.is_empty() {
            let fold = if self.recent_open { "▾" } else { "▸" };
            let heading = format!("recent {fold} {}", self.rows.recent.len());
            items.push(
                line(HomeKey::Section("recent"), &heading, HomeClass::Section)
                    .with_lines(vec![HomeTarget::Recent]),
            );
            if self.recent_open {
                let column = column_of(self.rows.recent.iter().map(|row| row.name.as_str()));
                for row in &self.rows.recent {
                    items.push(recent_line(row, column));
                }
            }
        }
        if !self.rows.piles.is_empty() {
            items.push(section("piles"));
            let name = |row: &PileRow| row.name.clone().unwrap_or_else(|| "later".to_owned());
            let column = column_of(
                self.rows
                    .piles
                    .iter()
                    .map(|row| row.name.as_deref().unwrap_or("later")),
            );
            for row in &self.rows.piles {
                let name = name(row);
                let count = row.count.to_string();
                let (text, styles) = columns(&[
                    (name.as_str(), HomeClass::Title, column),
                    (count.as_str(), HomeClass::Label, 3),
                    (row.back.as_str(), HomeClass::Muted, 0),
                ]);
                items.push(
                    Item::new(HomeKey::Pile(row.name.clone()), text)
                        .with_styles(styles)
                        .with_lines(vec![HomeTarget::Pile(row.name.clone())]),
                );
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

/// `text` in at most `room` characters, ending in an ellipsis when cut.
fn cut(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(room.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// How many characters of Home's text fit across the window. Measured on
/// `0`, the font's average width, less two for the editor's gutter.
fn row_width(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> usize {
    let style = editor.update(cx, |editor, cx| editor.style(cx).clone());
    let font_size = style.text.font_size.to_pixels(window.rem_size());
    let font_id = window.text_system().resolve_font(&style.text.font());
    let advance = window
        .text_system()
        .ch_advance(font_id, font_size)
        .expect("the editor's font resolves");
    let columns = f32::from(window.viewport_size().width) / f32::from(advance);
    (columns as usize).saturating_sub(2)
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
    width: usize,
    title_class: HomeClass,
) -> Item<HomeKey, HomeClass, HomeTarget> {
    let label = match (row.skipped, row.label.is_empty()) {
        (false, _) => row.label.clone(),
        (true, true) => "skipped".to_owned(),
        (true, false) => format!("{} · skipped", row.label),
    };
    // The lead, the gap before the label and the label are what the row
    // keeps; the title has the rest.
    let room = width.saturating_sub(4 + label.chars().count());
    let title = cut(&row.title, room);
    let (text, styles) = columns(&[
        (title.as_str(), title_class, column.min(room)),
        (label.as_str(), HomeClass::Label, 0),
    ]);
    Item::new(HomeKey::Card(row.card.clone()), text)
        .with_styles(styles)
        .with_lines(vec![HomeTarget::Card(row.card.clone())])
}

fn recent_line(row: &RecentRow, column: usize) -> Item<HomeKey, HomeClass, HomeTarget> {
    let (text, styles) = columns(&[
        (row.name.as_str(), HomeClass::Title, column),
        (row.label.as_str(), HomeClass::Muted, 0),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One measure per frame, and a relayout only when it changes: a
        // narrower window cuts the titles shorter.
        let width = row_width(&self.editor, window, cx);
        if width != self.width {
            self.width = width;
            let items = self.items();
            self.reconcile(items, cx);
        }
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
        let rows = split_hand(std::slice::from_ref(&thread), card_title);
        assert_eq!(rows.next[0].title, "#design › can you look at the deploy?");
        assert_eq!(rows.next[0].label, "needs reply · 1.9h");

        let mail = Card {
            kind: CardKind::Mail,
            context: "eng-b8os".to_owned(),
            title: "is the deploy green?".to_owned(),
            ..card("", 1.0)
        };
        assert_eq!(
            card_title(&mail),
            "eng-b8os › is the deploy green?",
            "a mail row is the agent and the thread, as a Slack row is"
        );
    }

    #[test]
    fn a_card_row_is_one_line_cut_to_fit() {
        let mail = Card {
            kind: CardKind::Mail,
            context: "eng-b8os".to_owned(),
            title: "is the deploy green?\nsecond line".to_owned(),
            ..card("", 1.0)
        };
        assert_eq!(card_title(&mail), "eng-b8os › is the deploy green?");

        let row = |title: &str| HomeRow {
            card: NodeId::Note(uuid::Uuid::nil()),
            title: title.to_owned(),
            label: "asks · 2d".to_owned(),
            skipped: false,
        };
        // 30 across: 2 lead, 2 gap and 9 of label leave 17 for the title.
        let long = row("abcdefghijklmnopqrstuvwxyz");
        let text = card_line(&long, 40, 30, HomeClass::Title).text;
        assert_eq!(text, "  abcdefghijklmnop…  asks · 2d\n");
        assert_eq!(text.trim_end().chars().count(), 30);

        let short = row("abc");
        let text = card_line(&short, 3, 30, HomeClass::Title).text;
        assert_eq!(
            text, "  abc  asks · 2d\n",
            "a title that fits is left whole"
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
