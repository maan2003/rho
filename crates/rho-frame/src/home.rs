//! Home: what is next, what is unread and where things are, read at a
//! glance.
//!
//! Nothing here scores anything. The cards are the dealer's, in the
//! dealer's order, wearing the words the deal bar uses; the sections under
//! them are the systems'. The surface is one keyed transcript: a row is
//! an item keyed by what it stands for, so a change edits that row and
//! leaves the cursor, the scroll, and every other row alone.

use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_dealer::{Address, Card};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use crate::surface::{Subject, Surface};
use crate::system::{ChannelRow, HomeSection};

/// Home's own highlight-key space, clear of the transcript's semantic slots
/// at zero and Slack's at the top. Buckets grow upwards from here.
const HOME_KEY_BASE: usize = usize::MAX / 3;

/// How many cards Home shows. Hard: Home is a glance, and a list you can
/// groom is an inbox.
pub const HOME_CAP: usize = 5;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HomeRows {
    /// The top of the hand: a preview, not the queue.
    pub next: Vec<Card>,
    /// What the systems list under the cards: unread threads, say.
    pub sections: Vec<HomeSection>,
    /// Every channel across the systems, with where it is.
    pub channels: Vec<ChannelRow>,
}

impl HomeRows {
    pub fn is_empty(&self) -> bool {
        self.next.is_empty()
            && self.sections.iter().all(|section| section.rows.is_empty())
            && self.channels.is_empty()
    }
}

/// What a Home row opens when the cursor is on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeTarget {
    Address(Address),
    /// A section heading or the empty line: nothing to open.
    None,
}

/// One item of the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HomeKey {
    Section(String),
    Card(Address),
    Row(String, Address),
    Channel(Address),
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
            width,
        };
        view.transcript.attach(&view.editor.clone(), cx);
        // A Home nobody has told anything yet still answers the question.
        let items = view.items();
        view.transcript.reconcile(items, cx);
        view.focus_first_row(cx);
        view
    }

    /// What the cursor is on. The row under the cursor is the one Enter
    /// opens; a heading offers nothing.
    pub fn cursor_target(&self, cx: &App) -> HomeTarget {
        self.transcript
            .line_meta(self.cursor_row(cx), cx)
            .cloned()
            .unwrap_or(HomeTarget::None)
    }

    fn cursor_row(&self, cx: &App) -> u32 {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row
    }

    pub fn set_rows(&mut self, rows: HomeRows, cx: &mut Context<Self>) {
        if rows == self.rows {
            return;
        }
        self.rows = rows;
        let items = self.items();
        self.transcript.reconcile(items, cx);
        // A reader who has not moved is put on the first row, so the very
        // first Enter opens rather than landing on a heading.
        if self.cursor_target(cx) == HomeTarget::None {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    /// Puts the cursor on the first row that stands for something.
    fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let last = self.transcript.buffer().read(cx).snapshot().max_point().row;
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
            let column = column_of(self.rows.next.iter().map(|card| card.heading()).collect());
            for card in &self.rows.next {
                items.push(card_line(card, column, self.width));
            }
        }
        for section_rows in &self.rows.sections {
            if section_rows.rows.is_empty() {
                continue;
            }
            items.push(section(&section_rows.title));
            let column = column_of(
                section_rows
                    .rows
                    .iter()
                    .map(|row| row.title.clone())
                    .collect(),
            );
            for row in &section_rows.rows {
                let label = match row.unread {
                    true if row.label.is_empty() => "unread".to_owned(),
                    true => format!("unread · {}", row.label),
                    false => row.label.clone(),
                };
                let room = self.width.saturating_sub(4 + label.chars().count());
                let title = cut(&row.title, room);
                let (text, styles) = columns(&[
                    (title.as_str(), HomeClass::Title, column.min(room)),
                    (label.as_str(), HomeClass::Muted, 0),
                ]);
                items.push(
                    Item::new(
                        HomeKey::Row(section_rows.title.clone(), row.address.clone()),
                        text,
                    )
                    .with_styles(styles)
                    .with_lines(vec![HomeTarget::Address(row.address.clone())]),
                );
            }
        }
        if !self.rows.channels.is_empty() {
            items.push(section("channels"));
            let column = column_of(
                self.rows
                    .channels
                    .iter()
                    .map(|row| row.name.clone())
                    .collect(),
            );
            for row in &self.rows.channels {
                let label = match row.unread {
                    true => format!("{} · unread", row.tag),
                    false => row.tag.clone(),
                };
                let (text, styles) = columns(&[
                    (row.name.as_str(), HomeClass::Title, column),
                    (label.as_str(), HomeClass::Muted, 0),
                ]);
                items.push(
                    Item::new(HomeKey::Channel(row.address.clone()), text)
                        .with_styles(styles)
                        .with_lines(vec![HomeTarget::Address(row.address.clone())]),
                );
            }
        }
        items
    }
}

impl Surface for HomeView {
    fn title(&self, _cx: &App) -> String {
        "home".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn subject(&self, cx: &App) -> Subject {
        match self.transcript.line_meta(self.cursor_row(cx), cx) {
            Some(HomeTarget::Address(address)) => Subject::Address(address.clone()),
            _ => Subject::None,
        }
    }
}

impl Focusable for HomeView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
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

fn column_of(titles: Vec<String>) -> usize {
    /// Past this a title is left to run into its own state words rather
    /// than pushing every row on the screen sideways.
    const CAP: usize = 40;
    titles
        .iter()
        .map(|title| title.chars().count())
        .max()
        .unwrap_or(0)
        .min(CAP)
}

fn section(name: &str) -> Item<HomeKey, HomeClass, HomeTarget> {
    line(HomeKey::Section(name.to_owned()), name, HomeClass::Section)
        .with_lines(vec![HomeTarget::None])
}

fn line(key: HomeKey, text: &str, class: HomeClass) -> Item<HomeKey, HomeClass, HomeTarget> {
    Item::new(key, text.to_owned()).with_styles(vec![(class, 0..text.len())])
}

fn card_line(card: &Card, column: usize, width: usize) -> Item<HomeKey, HomeClass, HomeTarget> {
    let label = match (card.skipped, card.label.is_empty()) {
        (false, _) => card.label.clone(),
        (true, true) => "skipped".to_owned(),
        (true, false) => format!("{} · skipped", card.label),
    };
    // The lead, the gap before the label and the label are what the row
    // keeps; the title has the rest.
    let room = width.saturating_sub(4 + label.chars().count());
    let title = cut(&card.heading(), room);
    let (text, styles) = columns(&[
        (title.as_str(), HomeClass::Title, column.min(room)),
        (label.as_str(), HomeClass::Label, 0),
    ]);
    Item::new(HomeKey::Card(card.address.clone()), text)
        .with_styles(styles)
        .with_lines(vec![HomeTarget::Address(card.address.clone())])
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

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One measure per frame, and a relayout only when it changes: a
        // narrower window cuts the titles shorter.
        let width = row_width(&self.editor, window, cx);
        if width != self.width {
            self.width = width;
            let items = self.items();
            self.transcript.reconcile(items, cx);
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

    #[test]
    fn a_row_pads_its_cells_to_the_column() {
        let (text, styles) = columns(&[("ab", HomeClass::Title, 4), ("x", HomeClass::Label, 0)]);
        assert_eq!(text, "  ab    x");
        assert_eq!(styles[1].1, 8..9);
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("abc", 4), "abc");
    }
}
