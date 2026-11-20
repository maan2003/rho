//! The desk shows the dealer's hand, recent status changes, and an editable
//! idea.

use std::any::TypeId;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use editor::display_map::Crease;
use editor::{Editor, EditorMode, FoldPlaceholder, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Font, HighlightStyle, MouseButton,
    MouseDownEvent, Pixels, TextRun, Window, WindowTextSystem, div, px,
};
use jiff::{SignedDuration, Timestamp};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBufferOffset, ToPoint as _};
use rho_dealer::Address;
use rho_dealer::rank::Hand;
use rho_transcript::{Item, Transcript};
use text::{Bias, ToOffset as _, ToPoint as _};
use theme::ActiveTheme as _;

use crate::idea::IdeaProject;
use crate::phone::Layout;
use crate::surface::{Subject, Surface};
use crate::{Frame, FrameLink, draft};

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
    Waiting(Address),
    WaitingGroup(Address, String),
    WaitingMore,
    Gap,
    MovingHeader,
    Moving(Address),
    More,
    Idea,
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
    hidden_waiting: usize,
    hidden_results: usize,
    more: usize,
    pub(crate) next_age: Option<Timestamp>,
}

pub(crate) fn desk_rows(
    hand: &Hand,
    mut moving: Vec<MovingThread>,
    project_of: impl Fn(&Address) -> Option<IdeaProject>,
    now: Timestamp,
) -> DeskRows {
    // A group occupies its first card's place in the hand. The dealer
    // still owns every card and its order, including those behind a group.
    let mut groups = Vec::new();
    let mut indices = HashMap::new();
    for card in &hand.cards {
        let project = project_of(&card.address);
        let (reason, _) = card.label.split_once(" · ").unwrap_or((&card.label, ""));
        let key = match &project {
            Some(project) => DeskKey::WaitingGroup(project.address.clone(), reason.to_owned()),
            None => DeskKey::Waiting(card.address.clone()),
        };
        let index = *indices.entry(key.clone()).or_insert_with(|| {
            let index = groups.len();
            groups.push((
                DeskRow {
                    key,
                    subject: card.title.clone(),
                    words: card.label.clone(),
                    project: project.map_or_else(|| card.context.clone(), |project| project.name),
                    target: DeskTarget::Card(card.address.clone()),
                },
                0,
                reason == "result",
            ));
            index
        });
        groups[index].1 += 1;
    }
    let (hidden_waiting, hidden_results) =
        groups
            .iter()
            .skip(WAITING_LIMIT)
            .fold((0, 0), |(waiting, results), (_, count, result)| {
                (waiting + count, results + if *result { *count } else { 0 })
            });
    let waiting = groups
        .into_iter()
        .take(WAITING_LIMIT)
        .map(|(mut row, count, result)| {
            if count > 1 {
                row.subject = std::mem::take(&mut row.project);
                row.words = if result {
                    format!("{count} {}", row.words.replacen("result", "results", 1))
                } else {
                    format!("{count} {}", row.words)
                };
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
        hidden_waiting,
        hidden_results,
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
    draft: Entity<Buffer>,
    idea_label: String,
    rows: DeskRows,
    layout: DeskLayout,
    text_system: Arc<WindowTextSystem>,
}

#[derive(PartialEq, Eq)]
struct DeskLayout {
    viewport: Pixels,
    font: Font,
    font_size: Pixels,
    inset: Pixels,
}

impl DeskLayout {
    fn read(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> Self {
        editor.update(cx, |editor, cx| {
            let style = editor.style(cx).clone();
            let font = style.text.font();
            let font_size = style.text.font_size.to_pixels(window.rem_size());
            let font_id = window.text_system().resolve_font(&font);
            let inset = editor
                .snapshot(window, cx)
                .gutter_dimensions(font_id, font_size, &style, window, cx)
                .width;
            Self {
                viewport: window.viewport_size().width,
                font,
                font_size,
                inset,
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
    pub fn new(draft: Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = draft::document(&buffer, &draft, cx);
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
            editor.disable_headers_for_buffers(
                [buffer.read(cx).remote_id(), draft.read(cx).remote_id()],
                cx,
            );
            let history = buffer.clone();
            editor.set_prepare_for_insert(
                Some(
                    move |editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>| {
                        if !draft::on_tail(editor, &history, cx) {
                            let tail = draft::start(&history, cx);
                            editor.change_selections(
                                Default::default(),
                                window,
                                cx,
                                |selections| {
                                    selections.select_ranges([tail..tail]);
                                },
                            );
                        }
                    },
                ),
                cx,
            );
            editor
        });
        let layout = DeskLayout::read(&editor, window, cx);
        let mut view = Self {
            transcript: Transcript::new(buffer),
            editor,
            draft,
            idea_label: String::new(),
            rows: DeskRows::default(),
            layout,
            text_system: window.text_system().clone(),
        };
        view.transcript.attach(&view.editor.clone(), cx);
        view.reconcile(cx);
        view
    }

    pub(crate) fn set_idea_label(&mut self, label: String, cx: &mut Context<Self>) {
        if label != self.idea_label {
            self.idea_label = label;
            self.transcript.replace(
                &DeskKey::Idea,
                line(DeskKey::Idea, self.idea_label.clone()),
                cx,
            );
            cx.notify();
        }
    }

    pub(crate) fn set_draft(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        draft::replace(&self.editor, &buffer, cx);
        self.draft = buffer;
        cx.notify();
    }

    pub(crate) fn on_draft(&self, cx: &App) -> bool {
        draft::on_tail(self.editor.read(cx), self.transcript.buffer(), cx)
    }

    pub(crate) fn focus_draft(&self, window: &mut Window, cx: &mut App) {
        draft::focus(&self.editor, self.transcript.buffer(), None, window, cx);
    }

    fn tap_idea(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left || event.modifiers.modified() {
            return;
        }
        let Some(hit) = draft::hit(
            self.editor.read(cx),
            self.transcript.buffer(),
            &self.draft,
            event.position,
            cx,
        ) else {
            return;
        };
        let ask = self.draft.read(cx).capability() == Capability::Read
            || (matches!(hit, draft::Hit::Separator) && self.on_draft(cx));
        if ask {
            let frame = FrameLink::new(
                window
                    .window_handle()
                    .downcast::<Frame>()
                    .expect("the frame owns the desk"),
            );
            frame.with(cx, |frame, window, cx| frame.prompt_idea(window, cx));
        } else {
            let position = match hit {
                draft::Hit::Text(anchor) => Some(anchor),
                draft::Hit::Separator => None,
            };
            draft::focus(&self.editor, self.transcript.buffer(), position, window, cx);
        }
        window.prevent_default();
        cx.stop_propagation();
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
        self.select_row(row, cx);
        window.focus(&self.editor.focus_handle(cx), cx);
        true
    }

    pub(crate) fn set_rows(&mut self, rows: DeskRows, cx: &mut Context<Self>) {
        if rows == self.rows {
            return;
        }
        let was_empty = self.rows.waiting.is_empty() && self.rows.moving.is_empty();
        self.rows = rows;
        self.reconcile(cx);
        if was_empty && !self.on_draft(cx) {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    pub(crate) fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let first = self
            .rows
            .waiting
            .first()
            .or_else(|| self.rows.moving.first());
        let row = first
            .and_then(|row| self.transcript.range_of(&row.key))
            .map_or(0, |range| {
                range
                    .start
                    .to_point(&self.transcript.buffer().read(cx).snapshot())
                    .row
            });
        self.select_row(row, cx);
    }

    fn select_row(&mut self, row: u32, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let rows = || self.rows.waiting.iter().chain(&self.rows.moving);
        let measure = |text: &str| self.layout.measure(&self.text_system, text);
        let ch = measure("0");
        // Leave room for the editor's inset.
        let room = (self.layout.viewport - ch * 4.).max(px(0.));
        let wide = WideRow::new(room, ch, rows().map(|row| row.words.as_str()), measure);
        let phone = Layout::of(self.layout.viewport) == Layout::Phone;
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
                wide.item(row)
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
        if self.rows.hidden_waiting > 0 {
            let mut words = format!("and {} more", self.rows.hidden_waiting);
            match self.rows.hidden_results {
                0 => {}
                1 => words.push_str(" · 1 result"),
                n => words.push_str(&format!(" · {n} results")),
            }
            waiting.push(line(DeskKey::WaitingMore, words));
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
        waiting.push(line(DeskKey::Idea, self.idea_label.clone()));
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
        "desk".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn undo_verdict_action(&self, cx: &App) -> Option<Box<dyn gpui::Action>> {
        (!self.on_draft(cx))
            .then(|| Box::new(crate::keys::RedoIdeaOrUndoVerdict) as Box<dyn gpui::Action>)
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

    fn item(&self, row: &DeskRow) -> LaidOutRow {
        let room = |cell: Pixels| (cell - self.space * 2.).max(px(0.));
        let subject = self.cut(&row.subject, room(self.subject), "");
        let words = self.words(row, room(self.words));
        let project = self.cut(&row.project, self.project, "");
        let mut text = String::new();
        let mut styles = Vec::new();
        let mut gaps = Vec::new();
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &subject,
            DeskClass::Title,
            Some(self.subject),
        );
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &words,
            row.words_class(),
            (!project.is_empty()).then_some(self.words),
        );
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &project,
            DeskClass::Muted,
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
        text.push_str("\n  ");
        let words = self.words(row, room - 2. * self.space);
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
                .with_lines(vec![row.target.clone(), row.target.clone()]),
            gaps,
        )
    }

    fn words(&self, row: &DeskRow, room: Pixels) -> String {
        let text = first_line(&row.words);
        if (self.measure)(text) <= room {
            text.to_owned()
        } else if let Some((words, age)) = text.rsplit_once(" · ") {
            self.cut(words, room, &format!(" · {age}"))
        } else {
            self.cut(text, room, "")
        }
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
            .key_context(if self.draft.read(cx).capability() == Capability::Read {
                "RhoDesk RhoIdea RhoIdeaUnaddressed"
            } else {
                "RhoDesk RhoIdea"
            })
            .capture_any_mouse_down(cx.listener(Self::tap_idea))
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
            |address| {
                Some(IdeaProject {
                    address: address.clone(),
                    name: "shoal@lab".into(),
                })
            },
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
    fn folding_preserves_the_first_card_and_reports_hidden_cards_not_groups() {
        let mut cards = [7, 85, 2, 71, 28, 25, 48, 44, 12, 16, 9, 1].map(card);
        for index in [0, 2, 7, 9, 10] {
            cards[index].label = format!("failed · {index}m");
        }
        cards[8].label = "result · 6m".into();
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
            })
        };
        let rows = desk_rows(&hand, vec![], project, Timestamp::UNIX_EPOCH);
        assert_eq!(rows.wait_count, 12);
        assert_eq!(rows.waiting[0].subject, "reef@lab");
        assert_eq!(rows.waiting[0].words, "2 failed · 0m");
        assert_eq!(rows.waiting[0].target, DeskTarget::Card(address(7)));
        assert_eq!(rows.waiting[1].target, DeskTarget::Card(address(85)));
        assert_eq!(rows.waiting[1].project, "reef@lab");
        assert_eq!(rows.waiting[6].target, DeskTarget::Card(address(44)));
        assert_eq!(rows.waiting[6].words, "3 failed · 7m");
        assert_eq!((rows.hidden_waiting, rows.hidden_results), (2, 1));
        let afterwards = Hand {
            cards: hand
                .cards
                .into_iter()
                .filter(|card| card.address != address(7))
                .collect(),
            next_change: None,
        };
        let rows = desk_rows(&afterwards, vec![], project, Timestamp::UNIX_EPOCH);
        assert_eq!(rows.waiting[1].target, DeskTarget::Card(address(2)));
        assert_eq!(rows.waiting[1].subject, "idea 2");
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
        let wide = WideRow::new(px(150.), px(5.), [&*row.words].into_iter(), |text: &str| {
            px(text.chars().count() as f32 * 5.)
        });
        let (item, gaps) = wide.compact(&row, px(150.), px(40.));
        assert_eq!(
            item.text,
            "Clock correction ed… tide@lab\n  Replaying the slower… · 16m\n"
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
        for (room, moving_title) in [(100., "WW…"), (117., "WW…"), (135., "WWWW")] {
            let wide = WideRow::new(
                px(room),
                px(5.),
                [&waiting, &moving]
                    .map(|row| row.words.as_str())
                    .into_iter(),
                measure,
            );
            for (row, title) in [(&waiting, "WiWi"), (&moving, moving_title)] {
                let (item, gaps) = wide.item(row);
                let (class, range) = &item.styles[1];
                assert_eq!(*class, row.words_class());
                assert_eq!(&item.text[range.clone()], row.words);
                assert_eq!(&item.text[item.styles[0].1.clone()], title);
                assert_eq!(measure(title) + gaps[0].1, px(room * (1. / 3.)));
                assert_eq!(
                    measure(&row.words) + gaps[1].1,
                    px(32.).min((px(room) - px(room * (1. / 3.))) * (2. / 3.))
                );
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
