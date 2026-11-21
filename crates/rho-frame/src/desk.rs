//! The desk shows the dealer's hand and the latest words from work that moves.

use std::any::TypeId;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use editor::display_map::Crease;
use editor::scroll::Autoscroll;
use editor::{Editor, EditorMode, FoldPlaceholder, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Font, HighlightStyle, Pixels, TextRun, Window,
    WindowTextSystem, div, px,
};
use jiff::Timestamp;
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, MultiBufferOffset, ToPoint as _};
use rho_dealer::Address;
use rho_dealer::rank::{CardKind, Hand};
use rho_transcript::{Item, Transcript};
use text::{Bias, ToOffset as _, ToPoint as _};
use theme::ActiveTheme as _;

use crate::idea::IdeaProject;
use crate::keys;
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
    pub words: String,
    pub changed: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeskTarget {
    Card(Address),
    Thread(Address),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeskKey {
    Waiting(Address),
    Stops(Address),
    WaitingMore,
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
    overflow: Option<WaitingOverflow>,
    more: usize,
}

#[derive(PartialEq, Eq)]
struct WaitingOverflow {
    first: DeskTarget,
    words: String,
}

impl DeskRows {
    fn keep_order(rows: &mut [DeskRow], previous: &[DeskRow]) {
        rows.sort_by_key(|row| {
            previous
                .iter()
                .position(|old| old.key == row.key)
                .unwrap_or(usize::MAX)
        });
    }
}

pub(crate) fn desk_rows(
    hand: &Hand,
    mut moving: Vec<MovingThread>,
    project_of: impl Fn(&Address) -> Option<IdeaProject>,
) -> DeskRows {
    // A project's stops occupy its first stop's place. Every pinging mail
    // remains a row, even when another came from the same project.
    let mut waiting: Vec<(DeskRow, CardKind, usize, bool)> = Vec::new();
    let mut stops = HashMap::new();
    for card in &hand.cards {
        let project = project_of(&card.address);
        let key = match (card.kind, &project) {
            (CardKind::Stop(word), Some(project)) => {
                if let Some(&index) = stops.get(&project.address) {
                    let (_, kind, count, mixed): &mut (DeskRow, CardKind, usize, bool) =
                        &mut waiting[index];
                    *count += 1;
                    *mixed |= *kind != CardKind::Stop(word);
                    continue;
                }
                stops.insert(project.address.clone(), waiting.len());
                DeskKey::Stops(project.address.clone())
            }
            _ => DeskKey::Waiting(card.address.clone()),
        };
        waiting.push((
            DeskRow {
                key,
                subject: card.title.clone(),
                words: String::new(),
                project: project.map_or_else(|| card.context.clone(), |project| project.name),
                target: DeskTarget::Card(card.address.clone()),
            },
            card.kind,
            1,
            false,
        ));
    }
    let overflow = waiting.get(WAITING_LIMIT..).and_then(|hidden| {
        let (first, ..) = hidden.first()?;
        let total: usize = hidden.iter().map(|(_, _, count, _)| count).sum();
        let mut words = format!("and {total} more");
        let mut projects = HashSet::new();
        for (row, ..) in hidden {
            if !row.project.is_empty() && projects.insert(&row.project) {
                words.push_str(" · ");
                words.push_str(&row.project);
            }
        }
        Some(WaitingOverflow {
            first: first.target.clone(),
            words,
        })
    });
    let waiting = waiting
        .into_iter()
        .take(WAITING_LIMIT)
        .map(|(mut row, kind, count, mixed)| {
            if let CardKind::Stop(reason) = kind
                && count > 1
            {
                row.subject = std::mem::take(&mut row.project);
                let reason = if mixed { "stops" } else { reason };
                row.words = format!("{count} {reason}");
            }
            row
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
    let moving = moving
        .into_iter()
        .take(MOVING_LIMIT)
        .map(|thread| DeskRow {
            target: DeskTarget::Thread(thread.thread.clone()),
            key: DeskKey::Moving(thread.thread),
            subject: thread.subject,
            words: thread.words,
            project: thread.project,
        })
        .collect();
    DeskRows {
        wait_count: hand.cards.len(),
        waiting,
        moving,
        overflow,
        more,
    }
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
    project: Option<IdeaProject>,
    layout: DeskLayout,
    text_system: Rc<WindowTextSystem>,
}

#[derive(PartialEq, Eq)]
struct DeskLayout {
    viewport: Pixels,
    font: Font,
    font_size: Pixels,
    inset: Pixels,
    content: Pixels,
}

impl DeskLayout {
    fn read(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> Self {
        editor.update(cx, |editor, cx| {
            let style = editor.style(cx).clone();
            let font = style.text.font();
            let font_size = style.text.font_size.to_pixels(window.rem_size());
            let font_id = window.text_system().resolve_font(&font);
            let gutter = editor
                .snapshot(window, cx)
                .gutter_dimensions(font_id, font_size, &style, window, cx);
            let viewport = window.viewport_size().width;
            Self {
                viewport,
                font,
                font_size,
                inset: gutter.width,
                content: (viewport
                    - crate::frame::FRAME_PADDING * 2.
                    - gutter.width
                    - gutter.margin)
                    .max(px(0.)),
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
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    // An arrival can scroll the new row out of the way
                    // while writing, even when the whole Desk fits.
                    sizing_behavior: SizingBehavior::FullOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        let layout = DeskLayout::read(&editor, window, cx);
        let mut view = Self {
            transcript: Transcript::new(buffer),
            editor,
            rows: DeskRows::default(),
            project: None,
            layout,
            text_system: window.text_system().clone(),
        };
        view.transcript.attach(&view.editor.clone(), cx);
        view.reconcile(cx);
        view.focus_first_row(cx);
        view
    }

    pub(crate) fn cursor_target(&self, cx: &App) -> Option<DeskTarget> {
        self.transcript.line_meta(self.cursor_row(cx), cx).cloned()
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

    /// Selects the tapped row for the caller to open. Either display line
    /// on the phone maps to the same item; a heading or blank does not.
    pub(crate) fn tap(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let editor = self.editor.read(cx);
        let hit = editor
            .buffer_location_for_window_position(position, Bias::Left)
            .and_then(|(anchor, _, _)| {
                let row = anchor.to_point(&editor.buffer().read(cx).snapshot(cx)).row;
                self.transcript.line_meta(row, cx)?;
                let key = self.transcript.key_at_row(row, cx)?;
                let start = self.transcript.range_of(key)?.start;
                let row = start
                    .to_point(&self.transcript.buffer().read(cx).snapshot())
                    .row;
                Some(row)
            });
        let Some(row) = hit else { return false };
        self.select_row(row, None, cx);
        window.focus(&self.editor.focus_handle(cx), cx);
        true
    }

    pub(crate) fn set_rows(
        &mut self,
        mut rows: DeskRows,
        project: Option<IdeaProject>,
        hold: bool,
        cx: &mut Context<Self>,
    ) {
        let changed_scope =
            self.project.as_ref().map(|p| &p.address) != project.as_ref().map(|p| &p.address);
        let changed_title = self.project.as_ref().and_then(|p| p.details.as_ref())
            != project.as_ref().and_then(|p| p.details.as_ref());
        self.project = project;
        if hold && !changed_scope && Layout::of(self.layout.viewport) == Layout::Phone {
            // Update displayed words in place. A newly ranked row can't
            // take the place of one the user is about to tap.
            DeskRows::keep_order(&mut rows.waiting, &self.rows.waiting);
            DeskRows::keep_order(&mut rows.moving, &self.rows.moving);
        }
        if rows == self.rows && !changed_scope {
            if changed_title {
                cx.notify();
            }
            return;
        }
        self.rows = rows;
        self.reconcile(cx);
        if self.cursor_target(cx).is_none() {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    pub(crate) fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let row = self.selectable_rows(cx).next().unwrap_or(0);
        self.select_row(row, None, cx);
    }

    /// The first buffer line of each selectable item; on a phone the
    /// label's second line is part of the same stop, not one more keypress.
    fn selectable_rows<'a>(&'a self, cx: &'a App) -> impl DoubleEndedIterator<Item = u32> + 'a {
        let history = self.transcript.buffer().read(cx).snapshot();
        self.transcript.keys().filter_map(move |key| {
            let row = self.transcript.range_of(key)?.start.to_point(&history).row;
            self.transcript.line_meta(row, cx).map(|_| row)
        })
    }

    fn step(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.cursor_row(cx);
        let count = vim::take_count(cx).unwrap_or(1);
        let current = self
            .transcript
            .key_at_row(row, cx)
            .and_then(|key| self.transcript.range_of(key))
            .map_or(row, |range| {
                range
                    .start
                    .to_point(&self.transcript.buffer().read(cx).snapshot())
                    .row
            });
        let selected = if down {
            self.selectable_rows(cx)
                .filter(|&candidate| candidate > current)
                .take(count)
                .last()
        } else {
            self.selectable_rows(cx)
                .rev()
                .filter(|&candidate| candidate < current)
                .take(count)
                .last()
        };
        if let Some(row) = selected {
            self.select_row(row, Some(window), cx);
        }
    }

    fn select_row(&mut self, row: u32, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let point = Point::new(row, 0);
            if let Some(window) = window {
                editor.change_selections(
                    editor::SelectionEffects::scroll(Autoscroll::fit()),
                    window,
                    cx,
                    |selections| selections.select_ranges([point..point]),
                );
            } else {
                let snapshot = editor.display_snapshot(cx);
                editor.selections.change_with(&snapshot, |selections| {
                    selections.select_ranges([point..point]);
                });
                editor.request_autoscroll(Autoscroll::fit(), cx);
                cx.notify();
            }
        });
    }

    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let rows = || self.rows.waiting.iter().chain(&self.rows.moving);
        let measure = |text: &str| self.layout.measure(&self.text_system, text);
        let phone = Layout::of(self.layout.viewport) == Layout::Phone;
        // The editor and count extend to the frame edge; keep the existing
        // narrow-row inset, but spend the full text lane on desktop.
        let room = if phone {
            (self.layout.viewport - measure("0") * 4.).max(px(0.))
        } else {
            self.layout.content
        };
        let wide = RowLayout::new(measure);
        let project_width = rows()
            .map(|row| measure(first_line(&row.project)))
            .max()
            .unwrap_or_default()
            .min(room * 0.45);
        let mut offsets = Vec::new();
        let mut row_item = |row: &DeskRow| {
            let (item, gaps) = if phone {
                wide.compact(row, room, project_width)
            } else {
                wide.item(row, room)
            };
            offsets.extend(
                gaps.into_iter()
                    .map(|(offset, width)| (row.key.clone(), offset, width)),
            );
            item
        };
        let mut waiting = self
            .rows
            .waiting
            .iter()
            .map(&mut row_item)
            .collect::<Vec<_>>();
        let moving = self
            .rows
            .moving
            .iter()
            .map(&mut row_item)
            .collect::<Vec<_>>();
        if let Some(overflow) = &self.rows.overflow {
            waiting.push(
                line(DeskKey::WaitingMore, overflow.words.clone())
                    .with_lines(vec![overflow.first.clone()]),
            );
        }
        if !moving.is_empty() {
            if !waiting.is_empty() {
                waiting.push(line(DeskKey::Gap, String::new()));
            }
            waiting.push(line(DeskKey::MovingHeader, "moving".into()));
            waiting.extend(moving);
            if self.rows.more > 0 {
                waiting.push(line(DeskKey::More, format!("and {} more", self.rows.more)));
            }
        }
        self.transcript.reconcile(waiting, cx);
        let snapshot = self.transcript.buffer().read(cx).snapshot();
        // A separator is one buffer space, laid out at a measured width.
        // The editor still owns the words, selection and hit testing.
        struct ColumnGap;
        let tag = TypeId::of::<ColumnGap>();
        let columns = offsets
            .into_iter()
            .map(|(key, offset, width)| {
                let range = self.transcript.range_of(&key).expect("row was reconciled");
                let start = range.start.to_offset(&snapshot) + offset;
                Crease::simple(
                    MultiBufferOffset(start)..MultiBufferOffset(start + 1),
                    FoldPlaceholder {
                        render: Arc::new(move |_, _, _| div().w(width).into_any_element()),
                        constrain_width: false,
                        merge_adjacent: false,
                        type_tag: Some(tag),
                        ..Default::default()
                    },
                )
            })
            .collect();
        self.editor
            .read(cx)
            .display_map
            .clone()
            .update(cx, |map, cx| {
                map.replace_folds_with_type(tag, columns, cx);
            });
        cx.notify();
    }
}

impl Surface for DeskView {
    fn title(&self, _cx: &App) -> String {
        self.project.as_ref().map_or_else(
            || "desk".into(),
            |project| {
                project.details.as_ref().map_or_else(
                    || project.name.clone(),
                    |details| format!("{} · {details}", project.name),
                )
            },
        )
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
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// The same shaped font determines clipping and the widths of separators.
struct RowLayout<M> {
    measure: M,
    space: Pixels,
}

impl<M: Fn(&str) -> Pixels> RowLayout<M> {
    fn new(measure: M) -> Self {
        let space = measure(" ");
        Self { measure, space }
    }

    fn item(&self, row: &DeskRow, room: Pixels) -> LaidOutRow {
        let gap = self.space * 2.;
        let project = self.cut(&row.project, room - gap, "");
        let project_cell = if project.is_empty() {
            px(0.)
        } else {
            (self.measure)(&project) + gap
        };
        let words = self.words(row, room - project_cell - gap);
        let words_cell = if words.is_empty() {
            px(0.)
        } else {
            (self.measure)(&words) + gap
        };
        let subject = self.cut(&row.subject, room - project_cell - words_cell, "");
        let mut text = String::new();
        let mut styles = Vec::new();
        let mut gaps = Vec::new();
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &subject,
            DeskClass::Title,
            (!words.is_empty() || !project.is_empty()).then(|| (self.measure)(&subject) + gap),
        );
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &project,
            DeskClass::Muted,
            (!project.is_empty() && !words.is_empty()).then_some(project_cell),
        );
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &words,
            row.words_class(),
            None,
        );
        (
            Item::new(row.key.clone(), text)
                .with_styles(styles)
                .with_lines(vec![row.target.clone()]),
            gaps,
        )
    }

    fn compact(&self, row: &DeskRow, room: Pixels, project_width: Pixels) -> LaidOutRow {
        let project = self.cut(&row.project, project_width, "");
        let has_project = !project.is_empty();
        let column = room - project_width;
        let subject_room = if has_project {
            column - self.space * 2.
        } else {
            room
        };
        let subject = self.cut(&row.subject, subject_room, "");
        let mut text = String::new();
        let mut styles = Vec::new();
        let mut gaps = Vec::new();
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &subject,
            DeskClass::Title,
            has_project.then_some(column),
        );
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &project,
            DeskClass::Muted,
            None,
        );
        let mut lines = vec![row.target.clone()];
        let words = self.words(row, room - 2. * self.space);
        if !words.is_empty() {
            text.push_str("\n  ");
            self.cell(
                &mut text,
                &mut styles,
                &mut gaps,
                &words,
                row.words_class(),
                None,
            );
            lines.push(row.target.clone());
        } else {
            text.push('\n');
        }
        (
            Item::new(row.key.clone(), text)
                .with_styles(styles)
                .with_lines(lines),
            gaps,
        )
    }

    fn words(&self, row: &DeskRow, room: Pixels) -> String {
        self.cut(&row.words, room, "")
    }

    fn cell(
        &self,
        text: &mut String,
        styles: &mut Vec<(DeskClass, Range<usize>)>,
        gaps: &mut Vec<(usize, Pixels)>,
        cell: &str,
        class: DeskClass,
        width: Option<Pixels>,
    ) {
        let start = text.len();
        text.push_str(cell);
        if start < text.len() {
            styles.push((class, start..text.len()));
        }
        if let Some(width) = width {
            gaps.push((text.len(), (width - (self.measure)(cell)).max(px(0.))));
            text.push(' ');
        }
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
}

type LaidOutRow = (Item<DeskKey, DeskClass, DeskTarget>, Vec<(usize, Pixels)>);

fn line(key: DeskKey, text: String) -> Item<DeskKey, DeskClass, DeskTarget> {
    let end = text.len();
    Item::new(key, text).with_styles(vec![(DeskClass::Muted, 0..end)])
}

impl Render for DeskView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = DeskLayout::read(&self.editor, window, cx);
        if layout != self.layout {
            self.layout = layout;
            self.reconcile(cx);
        }
        let text = self
            .editor
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let colors = cx.theme().colors();
        div()
            .key_context("RhoDesk")
            .on_action(
                cx.listener(|this, _: &keys::DeskNext, window, cx| this.step(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &keys::DeskPrevious, window, cx| {
                    this.step(false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &keys::DeskFirst, window, cx| {
                vim::take_count(cx);
                let row = this.selectable_rows(cx).next().unwrap_or(0);
                this.select_row(row, Some(window), cx);
            }))
            .on_action(cx.listener(|this, _: &keys::DeskDraft, window, cx| {
                vim::take_count(cx);
                let row = this.selectable_rows(cx).next_back().unwrap_or(0);
                this.select_row(row, Some(window), cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(
                div()
                    .flex_none()
                    .pl(self.layout.inset)
                    .font(text.font())
                    .text_size(text.font_size)
                    .line_height(text.line_height)
                    .text_color(colors.text_muted)
                    .child(format!("{} wait", self.rows.wait_count)),
            )
            .child(div().flex_1().min_h_0().w_full().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
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
            label: format!("waits · {n}m"),
            kind: CardKind::Waits,
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
        let rows = desk_rows(&hand, Vec::new(), |address| {
            Some(IdeaProject {
                address: address.clone(),
                name: "shoal@lab".into(),
                details: None,
            })
        });
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
    fn folding_preserves_the_first_card_and_reports_hidden_cards_not_groups() {
        let mut cards = [7, 85, 2, 71, 28, 25, 48, 44, 12, 16, 9, 1].map(card);
        for index in [0, 2, 7, 9, 10] {
            cards[index].label = format!("failed · {index}m");
            cards[index].kind = CardKind::Stop("failed");
        }
        cards[10].label = "stuck · 1m".into();
        cards[10].kind = CardKind::Stop("stuck");
        cards[8].label = "waits · 6m".into();
        cards[8].kind = CardKind::Waits;
        let hand = Hand {
            cards: cards.to_vec(),
            next_change: None,
        };
        let project = |address: &Address| {
            let id = match address.rest() {
                "t7" | "t2" | "t85" => 1,
                "t44" | "t16" | "t9" => 2,
                _ => return None,
            };
            Some(IdeaProject {
                address: Address::new("project", id),
                name: "reef@lab".into(),
                details: None,
            })
        };
        let rows = desk_rows(&hand, vec![], project);
        assert_eq!(rows.wait_count, 12);
        assert_eq!(rows.waiting[0].subject, "reef@lab");
        assert_eq!(rows.waiting[0].words, "2 failed");
        assert_eq!(rows.waiting[0].target, DeskTarget::Card(address(7)));
        assert_eq!(rows.waiting[1].target, DeskTarget::Card(address(85)));
        assert_eq!(rows.waiting[1].project, "reef@lab");
        assert_eq!(rows.waiting[6].target, DeskTarget::Card(address(44)));
        assert_eq!(rows.waiting[6].words, "3 stops");
        let overflow = rows.overflow.unwrap();
        assert_eq!(overflow.first, DeskTarget::Card(address(12)));
        assert_eq!(overflow.words, "and 2 more · unformatted channel");
        let afterwards = Hand {
            cards: hand
                .cards
                .into_iter()
                .filter(|card| card.address != address(7))
                .collect(),
            next_change: None,
        };
        let rows = desk_rows(&afterwards, vec![], project);
        assert_eq!(rows.waiting[1].target, DeskTarget::Card(address(2)));
        assert_eq!(rows.waiting[1].subject, "idea 2");
    }

    #[test]
    fn two_results_from_one_project_keep_their_subjects_and_name_hidden_cards() {
        let mut cards = [7, 85, 2, 71, 28, 25, 48, 44, 12, 16].map(card);
        for card in &mut cards[8..] {
            card.kind = CardKind::Waits;
            card.label = "waits · now".into();
        }
        let hand = Hand {
            cards: cards.into(),
            next_change: None,
        };
        let project = |_: &Address| {
            Some(IdeaProject {
                address: Address::new("project", 1),
                name: "clock@lab".into(),
                details: None,
            })
        };
        let rows = desk_rows(&hand, vec![], project);
        assert_eq!(rows.waiting[0].subject, "idea 7");
        assert_eq!(rows.waiting[1].subject, "idea 85");
        let overflow = rows.overflow.unwrap();
        assert_eq!(overflow.first, DeskTarget::Card(address(44)));
        assert_eq!(overflow.words, "and 3 more · clock@lab");

        let hand = Hand {
            cards: hand.cards[8..].into(),
            next_change: None,
        };
        let rows = desk_rows(&hand, vec![], project);
        assert_eq!(
            rows.waiting
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            ["idea 12", "idea 16"]
        );
        assert!(rows.overflow.is_none());
    }

    #[test]
    fn desktop_subject_uses_the_width_left_by_its_own_details() {
        let row = DeskRow {
            key: DeskKey::Waiting(address(7)),
            subject: "abcdefghijklmno".into(),
            words: "asks · 2m".into(),
            project: "a@b".into(),
            target: DeskTarget::Card(address(7)),
        };
        let layout = RowLayout::new(|text: &str| px(text.chars().count() as f32 * 5.));
        let (fits, gaps) = layout.item(&row, px(155.));
        assert_eq!(fits.text, "abcdefghijklmno a@b asks · 2m\n");
        assert_eq!(gaps, [(15, px(10.)), (19, px(10.))]);
        assert_eq!(
            layout.item(&row, px(150.)).0.text,
            "abcdefghijklm… a@b asks · 2m\n"
        );
    }

    #[test]
    fn a_narrow_moving_row_keeps_the_project_and_opens_from_either_line() {
        let thread = address(7);
        let row = DeskRow {
            key: DeskKey::Moving(thread.clone()),
            subject: "Clock correction edge cases".into(),
            words: "Replaying the slower timezone tests".into(),
            project: "tide@lab".into(),
            target: DeskTarget::Thread(thread),
        };
        let wide = RowLayout::new(|text: &str| px(text.chars().count() as f32 * 5.));
        let (item, gaps) = wide.compact(&row, px(150.), px(40.));
        assert_eq!(
            item.text,
            "Clock correction ed… tide@lab\n  Replaying the slower timezo…\n"
        );
        assert_eq!(gaps, [(22, px(10.))]);
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
                words: format!("running {n} tests"),
                changed: now - SignedDuration::from_mins(i64::from(n)),
            })
            .collect();
        let hand = Hand {
            cards: [3, 42, 45, 61, 80, 19, 99, 6].map(card).to_vec(),
            next_change: None,
        };
        let rows = desk_rows(&hand, moving, |_| None);
        assert_eq!(rows.wait_count, 8);
        assert_eq!(rows.waiting[0].target, DeskTarget::Card(address(3)));
        assert_eq!(rows.moving[0].words, "running 4 tests");
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
    fn laptop_rows_clip_in_shaped_pixels_without_padding_to_other_rows() {
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
        let row_layout = RowLayout::new(measure);
        for (room, moving_title) in [(72., "W…"), (84., "WW…"), (90., "WWWW")] {
            for (row, title) in [(&waiting, "WiWi"), (&moving, moving_title)] {
                let (item, gaps) = row_layout.item(row, px(room));
                let (class, range) = &item.styles[2];
                assert_eq!(*class, row.words_class());
                assert_eq!(&item.text[range.clone()], row.words);
                assert_eq!(&item.text[item.styles[0].1.clone()], title);
                assert_eq!(
                    gaps.iter().map(|(_, width)| *width).collect::<Vec<_>>(),
                    [px(8.), px(8.)],
                );
                assert_eq!(item.lines, std::slice::from_ref(&row.target));
                assert_eq!(item.text.lines().count(), 1);
            }
        }
    }

    #[test]
    fn an_arrival_does_not_reorder_visible_rows() {
        let row = |n| DeskRow {
            key: DeskKey::Waiting(address(n)),
            subject: format!("row {n}"),
            words: String::new(),
            project: "lab".into(),
            target: DeskTarget::Card(address(n)),
        };
        let before = [row(5), row(9)];
        let mut fresh = [row(3), row(9), row(5), row(2)];
        DeskRows::keep_order(&mut fresh, &before);
        assert_eq!(
            fresh.map(|row| row.subject),
            ["row 5", "row 9", "row 3", "row 2"]
        );
    }
}
