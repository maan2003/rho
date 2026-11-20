//! The desk shows the dealer's hand, recent status changes, and the idea box.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Window, div};
use jiff::{SignedDuration, Timestamp};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_dealer::Address;
use rho_dealer::rank::Hand;
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use crate::surface::{Subject, Surface};

const DESK_KEY_BASE: usize = usize::MAX / 3;
const WAITING_LIMIT: usize = 7;
const MOVING_LIMIT: usize = 10;

/// A thread's last Status change. The project and subject come from the
/// same source as the thread itself.
pub struct MovingThread {
    pub thread: Address,
    pub subject: String,
    pub project: String,
    pub status: String,
    pub changed: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeskTarget {
    Card(Address),
    Thread(Address),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeskKey {
    WaitHeader,
    Waiting(Address),
    Gap,
    MovingHeader,
    Moving(Address),
    More,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DeskClass {
    Title,
    Label,
    Muted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeskRow {
    key: DeskKey,
    subject: String,
    words: String,
    project: String,
    target: DeskTarget,
}

#[derive(Default, PartialEq, Eq)]
pub(crate) struct DeskRows {
    wait_count: usize,
    waiting: Vec<DeskRow>,
    moving: Vec<DeskRow>,
    more: usize,
    pub(crate) next_age: Option<Timestamp>,
}

pub(crate) fn desk_rows(
    hand: &Hand,
    mut moving: Vec<MovingThread>,
    project_of: impl Fn(&Address) -> Option<String>,
    now: Timestamp,
) -> DeskRows {
    let waiting = hand
        .cards
        .iter()
        .take(WAITING_LIMIT)
        .map(|card| DeskRow {
            key: DeskKey::Waiting(card.address.clone()),
            subject: card.title.clone(),
            words: card.label.clone(),
            project: project_of(&card.address).unwrap_or_else(|| card.context.clone()),
            target: DeskTarget::Card(card.address.clone()),
        })
        .collect();
    // A moving row with a waiting card wears the first such card's words
    // even when that card is below the desk's seven waiting rows.
    let mut by_thread = HashMap::new();
    for card in &hand.cards {
        by_thread.entry(&card.address).or_insert(card);
    }
    moving.sort_by(|a, b| (Reverse(a.changed), &a.thread).cmp(&(Reverse(b.changed), &b.thread)));
    let more = moving.len().saturating_sub(MOVING_LIMIT);
    let mut next_age = None;
    let moving = moving
        .into_iter()
        .take(MOVING_LIMIT)
        .map(|thread| {
            let card = by_thread.get(&thread.thread);
            let words = match card {
                Some(card) => card.label.clone(),
                None => {
                    let (age, next) = age(thread.changed, now);
                    if let Some(next) = next {
                        next_age = Some(next_age.map_or(next, |other: Timestamp| other.min(next)));
                    }
                    format!("{} · {age}", thread.status)
                }
            };
            let target = match card {
                Some(card) => DeskTarget::Card(card.address.clone()),
                None => DeskTarget::Thread(thread.thread.clone()),
            };
            DeskRow {
                key: DeskKey::Moving(thread.thread),
                subject: thread.subject,
                words,
                project: thread.project,
                target,
            }
        })
        .collect();
    DeskRows {
        wait_count: hand.cards.len(),
        waiting,
        moving,
        more,
        next_age,
    }
}

fn age(at: Timestamp, now: Timestamp) -> (String, Option<Timestamp>) {
    let minutes = at.duration_until(now).as_mins();
    let (label, next_minute) = match minutes {
        ..=0 => ("now".into(), 1),
        1..60 => (format!("{minutes}m"), minutes + 1),
        60..1440 => (format!("{}h", minutes / 60), (minutes / 60 + 1) * 60),
        _ => (format!("{}d", minutes / 1440), (minutes / 1440 + 1) * 1440),
    };
    (
        label,
        at.checked_add(SignedDuration::from_mins(next_minute)).ok(),
    )
}

impl DeskClass {
    fn slot(self) -> usize {
        match self {
            Self::Title => 0,
            Self::Label => 1,
            Self::Muted => 2,
        }
    }

    const COUNT: usize = 3;
}

impl rho_transcript::Style for DeskClass {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(DESK_KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
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

/// The desk log uses a keyed transcript, so a status change updates a row
/// rather than resetting the reader's selections and scroll.
pub(crate) struct DeskView {
    transcript: Transcript<DeskKey, DeskClass, DeskTarget>,
    editor: Entity<Editor>,
    rows: DeskRows,
    width: usize,
}

impl DeskView {
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
            rows: DeskRows::default(),
            width,
        };
        view.transcript.attach(&view.editor.clone(), cx);
        // The empty desk still answers what waits.
        let items = view.items();
        view.transcript.reconcile(items, cx);
        view.focus_first_row(cx);
        view
    }

    pub(crate) fn cursor_target(&self, cx: &App) -> Option<DeskTarget> {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let row = editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row;
        self.transcript.line_meta(row, cx).cloned()
    }

    pub(crate) fn set_rows(&mut self, rows: DeskRows, cx: &mut Context<Self>) {
        if rows == self.rows {
            return;
        }
        let was_empty = self.rows.waiting.is_empty() && self.rows.moving.is_empty();
        self.rows = rows;
        self.transcript.reconcile(self.items(), cx);
        if was_empty {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    pub(crate) fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let row = if !self.rows.waiting.is_empty() {
            1
        } else if !self.rows.moving.is_empty() {
            3
        } else {
            0
        };
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    fn items(&self) -> Vec<Item<DeskKey, DeskClass, DeskTarget>> {
        let mut items = vec![line(
            DeskKey::WaitHeader,
            format!("{} wait", self.rows.wait_count),
        )];
        let field_width = |field: fn(&DeskRow) -> &str| {
            self.rows
                .waiting
                .iter()
                .chain(&self.rows.moving)
                .map(|row| first_line(field(row)).chars().count())
                .max()
                .unwrap_or(0)
        };
        let subject_width = field_width(|row| &row.subject).min(40);
        let project_width = field_width(|row| &row.project).min(30);
        let words_width = field_width(|row| &row.words).min(42);
        let compact = subject_width + project_width + words_width + 4 > self.width;
        let row_item = |row: &DeskRow| {
            if compact {
                row.compact(self.width, project_width)
            } else {
                row.wide(subject_width, words_width, project_width)
            }
        };
        items.extend(self.rows.waiting.iter().map(row_item));
        if !self.rows.moving.is_empty() {
            items.push(line(DeskKey::Gap, String::new()));
            items.push(line(DeskKey::MovingHeader, "moving".into()));
            items.extend(self.rows.moving.iter().map(row_item));
            if self.rows.more > 0 {
                items.push(line(DeskKey::More, format!("and {} more", self.rows.more)));
            }
        }
        items
    }
}

impl Surface for DeskView {
    fn title(&self, _cx: &App) -> String {
        "desk".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn subject(&self, cx: &App) -> Subject {
        match self.cursor_target(cx) {
            Some(DeskTarget::Card(address) | DeskTarget::Thread(address)) => {
                Subject::Address(address)
            }
            None => Subject::None,
        }
    }

    fn card(&self, cx: &App) -> Subject {
        match self.cursor_target(cx) {
            Some(DeskTarget::Card(address)) => Subject::Address(address),
            _ => Subject::None,
        }
    }
}

impl Focusable for DeskView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl DeskRow {
    fn words_class(&self) -> DeskClass {
        match self.target {
            DeskTarget::Card(_) => DeskClass::Label,
            DeskTarget::Thread(_) => DeskClass::Muted,
        }
    }

    fn wide(
        &self,
        subject_width: usize,
        words_width: usize,
        project_width: usize,
    ) -> Item<DeskKey, DeskClass, DeskTarget> {
        let (text, styles) = columns(&[
            (
                cut(&self.subject, subject_width),
                DeskClass::Title,
                subject_width,
            ),
            (
                cut_words(&self.words, words_width),
                self.words_class(),
                words_width,
            ),
            (cut(&self.project, project_width), DeskClass::Muted, 0),
        ]);
        Item::new(self.key.clone(), text)
            .with_styles(styles)
            .with_lines(vec![self.target.clone()])
    }

    fn compact(&self, width: usize, project_width: usize) -> Item<DeskKey, DeskClass, DeskTarget> {
        // A narrow row is two lines, with the address by the subject and
        // the card words or status below. Either line opens the same thing.
        let project_width = project_width.min(width / 2);
        let subject_width = width.saturating_sub(project_width + 2);
        let (mut text, mut styles) = columns(&[
            (
                cut(&self.subject, subject_width),
                DeskClass::Title,
                subject_width,
            ),
            (cut(&self.project, project_width), DeskClass::Muted, 0),
        ]);
        text.push_str("\n  ");
        let start = text.len();
        text.push_str(&cut_words(&self.words, width.saturating_sub(2)));
        styles.push((self.words_class(), start..text.len()));
        Item::new(self.key.clone(), text)
            .with_styles(styles)
            .with_lines(vec![self.target.clone(), self.target.clone()])
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn cut_words(text: &str, width: usize) -> String {
    // Keep the age of long statuses legible even at the narrowest width.
    let text = first_line(text);
    if text.chars().count() > width
        && let Some((words, age)) = text.rsplit_once(" · ")
        && let Some(remaining) = width.checked_sub(age.chars().count() + 3)
        && remaining > 0
    {
        format!("{} · {age}", cut(words, remaining))
    } else {
        cut(text, width)
    }
}

fn cut(text: &str, width: usize) -> String {
    let text = first_line(text);
    if text.chars().count() <= width {
        text.to_owned()
    } else if width == 0 {
        String::new()
    } else {
        format!(
            "{}…",
            text.chars().take(width - 1).collect::<String>().trim_end()
        )
    }
}

/// How many characters of the desk's text fit across the window. Measured on
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
    (columns as usize).saturating_sub(4)
}

fn line(key: DeskKey, text: String) -> Item<DeskKey, DeskClass, DeskTarget> {
    let end = text.len();
    Item::new(key, text).with_styles(vec![(DeskClass::Muted, 0..end)])
}

fn columns(cells: &[(String, DeskClass, usize)]) -> (String, Vec<(DeskClass, Range<usize>)>) {
    let mut text = String::new();
    let mut styles = Vec::new();
    for (index, (cell, class, width)) in cells.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let start = text.len();
        text.push_str(cell);
        styles.push((*class, start..text.len()));
        text.extend(std::iter::repeat_n(
            ' ',
            width.saturating_sub(cell.chars().count()),
        ));
    }
    // An unaddressed card can have no third cell; keep style byte ranges
    // valid when the trailing padding is removed.
    text.truncate(text.trim_end().len());
    styles.retain(|(_, range)| range.end <= text.len() && !range.is_empty());
    (text, styles)
}

impl Render for DeskView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = row_width(&self.editor, window, cx);
        if width != self.width {
            self.width = width;
            self.transcript.reconcile(self.items(), cx);
        }
        div()
            .key_context("RhoDesk")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use rho_dealer::Card;

    use super::*;

    fn address(n: u32) -> Address {
        Address::new("thread", format!("t{n}"))
    }

    fn card(n: u32) -> Card {
        Card {
            address: address(n),
            title: format!("idea {n}"),
            context: "unformatted channel".into(),
            label: format!("asks · {n}m"),
            priority: f64::from(n),
            cursor: format!("{n}/100"),
            skipped: false,
        }
    }

    #[test]
    fn waiting_comes_straight_from_the_hand_with_the_full_count() {
        // Ranking a second time by priority, subject, or project yields
        // different orders. Counting the displayed rows would say seven.
        let hand = Hand {
            cards: [7, 94, 2, 71, 28, 16, 48, 44, 12].map(card).to_vec(),
            next_change: None,
        };
        let rows = desk_rows(
            &hand,
            Vec::new(),
            |_| Some("shoal@lab".into()),
            Timestamp::UNIX_EPOCH,
        );
        assert_eq!(rows.wait_count, 9);
        assert_eq!(
            rows.waiting
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            [
                "idea 7", "idea 94", "idea 2", "idea 71", "idea 28", "idea 16", "idea 48"
            ]
        );
        assert_eq!(rows.waiting[1].project, "shoal@lab");
        assert_eq!(rows.waiting[1].target, DeskTarget::Card(address(94)));
    }

    #[test]
    fn a_narrow_row_keeps_the_project_and_status_age_and_opens_from_either_line() {
        let thread = address(7);
        let row = DeskRow {
            key: DeskKey::Moving(thread.clone()),
            subject: "Clock correction edge cases".into(),
            words: "Replaying the slower timezone tests · 16m".into(),
            project: "tide@lab".into(),
            target: DeskTarget::Thread(thread),
        };
        let item = row.compact(30, 8);
        assert_eq!(
            item.text,
            "Clock correction ed…  tide@lab\n  Replaying the slower… · 16m\n"
        );
        assert_eq!(item.lines, [row.target.clone(), row.target]);
    }

    #[test]
    fn moving_uses_status_order_and_wears_a_card_even_below_the_waiting_cutoff() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let moving = [28, 7, 32, 3, 14, 26, 4, 12, 6, 18, 11, 5]
            .into_iter()
            .map(|n| MovingThread {
                thread: address(n),
                subject: format!("thread {n}"),
                project: "tide@mini".into(),
                status: format!("running {n} tests"),
                changed: now - SignedDuration::from_mins(i64::from(n)),
            })
            .collect();
        let hand = Hand {
            cards: [50, 42, 45, 61, 80, 19, 99, 6].map(card).to_vec(),
            next_change: None,
        };
        let rows = desk_rows(&hand, moving, |_| None, now);
        assert_eq!(rows.more, 2);
        assert_eq!(
            rows.moving
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            [
                "thread 3",
                "thread 4",
                "thread 5",
                "thread 6",
                "thread 7",
                "thread 11",
                "thread 12",
                "thread 14",
                "thread 18",
                "thread 26"
            ]
        );
        assert_eq!(rows.moving[0].words, "running 3 tests · 3m");
        assert_eq!(rows.moving[0].target, DeskTarget::Thread(address(3)));
        assert_eq!(rows.moving[3].words, "asks · 6m");
        assert_eq!(rows.moving[3].target, DeskTarget::Card(address(6)));
    }

    #[test]
    fn a_status_age_advances_without_another_status_change() {
        let changed: Timestamp = "2026-11-19T22:45:23Z".parse().unwrap();
        let now: Timestamp = "2026-11-20T00:05:13Z".parse().unwrap();
        let (words, next) = age(changed, now);
        assert_eq!(words, "1h");
        assert_eq!(next.unwrap().to_string(), "2026-11-20T00:45:23Z");
        assert_eq!(age(changed, next.unwrap()).0, "2h");
    }
}
