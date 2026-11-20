//! The desk shows the dealer's hand, recent status changes, and the idea box.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Font, HighlightStyle, Pixels, TextRun, Window,
    WindowTextSystem, div, px,
};
use jiff::{SignedDuration, Timestamp};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_dealer::Address;
use rho_dealer::rank::Hand;
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use crate::phone::Layout;
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
    // The hand, not just the visible seven cards, tells us what is waiting.
    let waiting_threads = hand
        .cards
        .iter()
        .map(|card| &card.address)
        .collect::<HashSet<_>>();
    moving.retain(|thread| !waiting_threads.contains(&thread.thread));
    moving.sort_by(|a, b| (Reverse(a.changed), &a.thread).cmp(&(Reverse(b.changed), &b.thread)));
    let more = moving.len().saturating_sub(MOVING_LIMIT);
    let mut next_age = None;
    let moving = moving
        .into_iter()
        .take(MOVING_LIMIT)
        .map(|thread| {
            let (age, next) = age(thread.changed, now);
            if let Some(next) = next {
                next_age = Some(next_age.map_or(next, |other: Timestamp| other.min(next)));
            }
            DeskRow {
                target: DeskTarget::Thread(thread.thread.clone()),
                key: DeskKey::Moving(thread.thread),
                subject: thread.subject,
                words: format!("{} · {age}", thread.status),
                project: thread.project,
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
    layout: DeskLayout,
    text_system: Arc<WindowTextSystem>,
}

#[derive(PartialEq, Eq)]
struct DeskLayout {
    viewport: Pixels,
    font: Font,
    font_size: Pixels,
}

impl DeskLayout {
    fn read(editor: &Entity<Editor>, window: &Window, cx: &mut App) -> Self {
        editor.update(cx, |editor, cx| {
            let text = &editor.style(cx).text;
            Self {
                viewport: window.viewport_size().width,
                font: text.font(),
                font_size: text.font_size.to_pixels(window.rem_size()),
            }
        })
    }

    fn measure(&self, text_system: &WindowTextSystem, text: &str) -> Pixels {
        text_system
            .layout_line(
                text,
                self.font_size,
                &[TextRun {
                    len: text.len(),
                    font: self.font.clone(),
                    ..Default::default()
                }],
                None,
            )
            .width
    }
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
        let layout = DeskLayout::read(&editor, window, cx);
        let mut view = Self {
            transcript: Transcript::new(buffer),
            editor,
            rows: DeskRows::default(),
            layout,
            text_system: window.text_system().clone(),
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
        let rows = || self.rows.waiting.iter().chain(&self.rows.moving);
        let measure = |text: &str| self.layout.measure(&self.text_system, text);
        let ch = measure("0");
        // Leave room for the editor's gutter.
        let room = (self.layout.viewport - ch * 4.).max(px(0.));
        let row_item: Box<dyn Fn(&DeskRow) -> _> = match Layout::of(self.layout.viewport) {
            Layout::Phone => {
                let width = (room / ch) as usize;
                let project_width = rows()
                    .map(|row| first_line(&row.project).chars().count())
                    .max()
                    .unwrap_or(0)
                    .min(30);
                Box::new(move |row| row.compact(width, project_width))
            }
            Layout::Desktop => {
                let wide = WideRow::new(room, ch, rows().map(|row| row.words.as_str()), measure);
                Box::new(move |row| wide.item(row))
            }
        };
        items.extend(self.rows.waiting.iter().map(&row_item));
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

/// Laptop rows share a fixed subject column even when the hand changes. An
/// editor line is plain text: shape the cells in its actual font before using
/// spaces to position the next cell.
struct WideRow<M> {
    measure: M,
    subject: Pixels,
    words: Pixels,
    project: Pixels,
    space: Pixels,
}

impl<M: Fn(&str) -> Pixels> WideRow<M> {
    fn new<'a>(room: Pixels, ch: Pixels, words: impl Iterator<Item = &'a str>, measure: M) -> Self {
        let space = measure(" ");
        let subject = (room * (1. / 3.)).min(ch * 30.);
        let words = (words
            .map(|text| measure(first_line(text)))
            .max()
            .unwrap_or_default()
            + space * 2.)
            .min(ch * 42.)
            .min((room - subject) * (2. / 3.));
        Self {
            subject,
            words,
            project: (room - subject - words).min(ch * 30.),
            space,
            measure,
        }
    }

    fn item(&self, row: &DeskRow) -> Item<DeskKey, DeskClass, DeskTarget> {
        let room = |cell: Pixels| (cell - self.space * 2.).max(px(0.));
        let subject = self.cut(&row.subject, room(self.subject), "");
        let mut text = subject;
        let title_end = text.len();
        self.pad(&mut text, self.subject);

        let words_start = text.len();
        let words = first_line(&row.words);
        if (self.measure)(words) <= room(self.words) {
            text.push_str(words);
        } else if let DeskTarget::Thread(_) = row.target
            && let Some((status, age)) = words.rsplit_once(" · ")
        {
            text.push_str(&self.cut(status, room(self.words), &format!(" · {age}")));
        } else {
            text.push_str(&self.cut(words, room(self.words), ""));
        }
        let words_end = text.len();
        let project = self.cut(&row.project, self.project, "");
        let mut styles = vec![
            (DeskClass::Title, 0..title_end),
            (row.words_class(), words_start..words_end),
        ];
        if !project.is_empty() {
            self.pad(&mut text, self.subject + self.words);
            let start = text.len();
            text.push_str(&project);
            styles.push((DeskClass::Muted, start..text.len()));
        }
        text.truncate(text.trim_end().len());
        styles.retain(|(_, range)| !range.is_empty() && range.end <= text.len());
        Item::new(row.key.clone(), text)
            .with_styles(styles)
            .with_lines(vec![row.target.clone()])
    }

    fn cut(&self, text: &str, room: Pixels, suffix: &str) -> String {
        let text = first_line(text);
        let mut shown = format!("{text}{suffix}");
        if (self.measure)(&shown) <= room {
            return shown;
        }
        shown = format!("…{suffix}");
        if (self.measure)(&shown) > room {
            return String::new();
        }
        for end in text.char_indices().skip(1).map(|(end, _)| end) {
            let prefix = text[..end].trim_end();
            let candidate = format!("{prefix}…{suffix}");
            if (self.measure)(&candidate) > room {
                break;
            }
            shown = candidate;
        }
        shown
    }

    fn pad(&self, text: &mut String, column: Pixels) {
        let remaining = (column - (self.measure)(text)).max(px(0.));
        // The editor shows non-ASCII spaces as invisible characters, so
        // round to the nearest position plain spaces can represent.
        text.extend(std::iter::repeat_n(
            ' ',
            (remaining / self.space).round() as usize,
        ));
    }
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
        let layout = DeskLayout::read(&self.editor, window, cx);
        if layout != self.layout {
            self.layout = layout;
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
    fn moving_excludes_the_whole_hand_before_taking_and_counting_rows() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let moving = [28, 7, 32, 3, 14, 26, 4, 12, 6, 18, 11, 5, 9, 35]
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
            cards: [3, 42, 45, 61, 80, 19, 99, 6].map(card).to_vec(),
            next_change: None,
        };
        let rows = desk_rows(&hand, moving, |_| None, now);
        assert_eq!(rows.wait_count, 8);
        assert_eq!(rows.waiting[0].target, DeskTarget::Card(address(3)));
        assert_eq!(rows.moving[0].words, "running 4 tests · 4m");
        assert_eq!(rows.moving[0].target, DeskTarget::Thread(address(4)));
        assert_eq!(rows.more, 2);
        assert_eq!(
            rows.moving
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            [
                "thread 4",
                "thread 5",
                "thread 7",
                "thread 9",
                "thread 11",
                "thread 12",
                "thread 14",
                "thread 18",
                "thread 26",
                "thread 28"
            ]
        );
    }

    #[test]
    fn laptop_subjects_share_a_pixel_column_and_ellipsize_before_it() {
        // Equal character counts have quite different advances; one title
        // fits as-is, while the other has to keep a visible ellipsis.
        let measure = |text: &str| {
            px(text
                .chars()
                .map(|ch| match ch {
                    'W' => 9.,
                    'i' => 2.,
                    '…' => 6.,
                    ' ' => 4.,
                    _ => 5.,
                })
                .sum())
        };
        let waiting = DeskRow {
            key: DeskKey::Waiting(address(3)),
            subject: "WiWi".into(),
            words: "asks".into(),
            project: "mini".into(),
            target: DeskTarget::Card(address(3)),
        };
        let moving = DeskRow {
            key: DeskKey::Moving(address(4)),
            subject: "WWWW".into(),
            words: "fixing".into(),
            project: "mini".into(),
            target: DeskTarget::Thread(address(4)),
        };
        for (room, moving_title, waiting_offset, moving_offset) in [
            // At 100 px the cell is 33⅓ px; ASCII spaces put rows on the
            // nearest representable offset to that column.
            (100., "WW…", 34., 32.),
            (117., "WW…", 38., 40.),
            (135., "WWWW", 46., 44.),
        ] {
            let wide = WideRow::new(
                px(room),
                px(5.),
                [&waiting, &moving]
                    .map(|row| row.words.as_str())
                    .into_iter(),
                measure,
            );
            for (row, title, offset) in [
                (&waiting, "WiWi", waiting_offset),
                (&moving, moving_title, moving_offset),
            ] {
                let item = wide.item(row);
                let (class, range) = &item.styles[1];
                assert_eq!(*class, row.words_class());
                assert_eq!(&item.text[range.clone()], row.words);
                assert_eq!(&item.text[item.styles[0].1.clone()], title);
                assert_eq!(measure(&item.text[..range.start]), px(offset));
                assert_eq!(item.lines, std::slice::from_ref(&row.target));
                assert_eq!(item.text.lines().count(), 1);
            }
        }
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
