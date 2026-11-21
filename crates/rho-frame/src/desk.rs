//! The Desk shows only what waits in the dealer's hand.

use std::any::TypeId;
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeskKey {
    Waiting(Address),
    Stops(Address),
    WaitingMore,
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
    detail: DeskDetail,
    flash: Option<String>,
    card: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeskDetail {
    Project(String),
    Stops(String),
}

impl DeskDetail {
    fn project(&self) -> Option<&str> {
        match self {
            Self::Project(project) => Some(project),
            Self::Stops(_) => None,
        }
    }
}

#[derive(Default, PartialEq, Eq)]
pub(crate) struct DeskRows {
    wait_count: usize,
    waiting: Vec<DeskRow>,
    overflow: Option<WaitingOverflow>,
}

#[derive(PartialEq, Eq)]
struct WaitingOverflow {
    first: Address,
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
    project_of: impl Fn(&Address) -> Option<IdeaProject>,
    flash_at: impl Fn(&Address) -> Option<String>,
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
                detail: DeskDetail::Project(
                    project.map_or_else(|| card.context.clone(), |project| project.name),
                ),
                flash: flash_at(&card.address),
                card: card.address.clone(),
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
            if let Some(project) = row.detail.project()
                && !project.is_empty()
                && projects.insert(project)
            {
                words.push_str(" · ");
                words.push_str(project);
            }
        }
        Some(WaitingOverflow {
            first: first.card.clone(),
            words,
        })
    });
    let waiting = waiting
        .into_iter()
        .take(WAITING_LIMIT)
        .map(|(mut row, kind, count, mixed)| {
            if let CardKind::Stop(reason) = kind
                && count > 1
                && let DeskDetail::Project(project) = row.detail
            {
                row.subject = project;
                let reason = if mixed { "stops" } else { reason };
                row.detail = DeskDetail::Stops(format!("{count} {reason}"));
                row.flash = None;
            }
            row
        })
        .collect();
    DeskRows {
        wait_count: hand.cards.len(),
        waiting,
        overflow,
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

/// The Desk uses a keyed transcript so that changing the hand does not
/// reset the reader's selection or scroll.
pub(crate) struct DeskView {
    transcript: Transcript<DeskKey, DeskClass, Address>,
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

    pub(crate) fn cursor_card(&self, cx: &App) -> Option<Address> {
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
        }
        if rows == self.rows && !changed_scope {
            if changed_title {
                cx.notify();
            }
            return;
        }
        self.rows = rows;
        self.reconcile(cx);
        if self.cursor_card(cx).is_none() {
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
        let project_width = self
            .rows
            .waiting
            .iter()
            .filter_map(|row| row.detail.project())
            .map(|project| measure(first_line(project)))
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
        if let Some(overflow) = &self.rows.overflow {
            waiting.push(
                Item::new(DeskKey::WaitingMore, overflow.words.clone())
                    .with_styles(vec![(DeskClass::Muted, 0..overflow.words.len())])
                    .with_lines(vec![overflow.first.clone()]),
            );
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

    fn subject(&self, _: Option<rho_dealer::facts::Seen>, cx: &App) -> Subject {
        self.cursor_card(cx).map_or(Subject::None, Subject::Address)
    }
}

impl Focusable for DeskView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
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
        let (detail, class) = match &row.detail {
            DeskDetail::Project(project) => (project, DeskClass::Muted),
            DeskDetail::Stops(stops) => (stops, DeskClass::Label),
        };
        let detail = self.cut(detail, room - gap);
        let has_detail = !detail.is_empty();
        let subject_room = if has_detail {
            room - (self.measure)(&detail) - gap
        } else {
            room
        };
        let subject = self.cut(&row.subject, subject_room);
        let mut text = String::new();
        let mut styles = Vec::new();
        let mut gaps = Vec::new();
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &subject,
            DeskClass::Title,
            has_detail.then(|| (self.measure)(&subject) + gap),
        );
        let used = (self.measure)(&subject)
            + if has_detail {
                gap + (self.measure)(&detail)
            } else {
                px(0.)
            };
        let flash = row
            .flash
            .as_ref()
            .map(|words| self.cut(words, room - used - gap))
            .filter(|words| !words.is_empty());
        self.cell(
            &mut text,
            &mut styles,
            &mut gaps,
            &detail,
            class,
            flash.as_ref().map(|_| (self.measure)(&detail) + gap),
        );
        if let Some(flash) = flash {
            self.cell(
                &mut text,
                &mut styles,
                &mut gaps,
                &flash,
                DeskClass::Muted,
                None,
            );
        }
        (
            Item::new(row.key.clone(), text)
                .with_styles(styles)
                .with_lines(vec![row.card.clone()]),
            gaps,
        )
    }

    fn compact(&self, row: &DeskRow, room: Pixels, project_width: Pixels) -> LaidOutRow {
        let project = row
            .detail
            .project()
            .map_or_else(String::new, |project| self.cut(project, project_width));
        let has_project = !project.is_empty();
        let column = room - project_width;
        let subject_room = if has_project {
            column - self.space * 2.
        } else {
            room
        };
        let subject = self.cut(&row.subject, subject_room);
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
        let mut lines = vec![row.card.clone()];
        let extra = match &row.detail {
            DeskDetail::Stops(stops) => Some((stops, DeskClass::Label)),
            DeskDetail::Project(_) => row.flash.as_ref().map(|words| (words, DeskClass::Muted)),
        };
        if let Some((extra, class)) = extra {
            let extra = self.cut(extra, room - 2. * self.space);
            if !extra.is_empty() {
                text.push_str("\n  ");
                self.cell(&mut text, &mut styles, &mut gaps, &extra, class, None);
                lines.push(row.card.clone());
            }
        }
        (
            Item::new(row.key.clone(), text)
                .with_styles(styles)
                .with_lines(lines),
            gaps,
        )
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

    fn cut(&self, text: &str, room: Pixels) -> String {
        let text = first_line(text);
        let mut shown = text.to_owned();
        if (self.measure)(&shown) <= room {
            return shown;
        }
        shown = "…".into();
        if (self.measure)(&shown) > room {
            return String::new();
        }
        for end in text.char_indices().skip(1).map(|(end, _)| end) {
            let prefix = text[..end].trim_end();
            let candidate = format!("{prefix}…");
            if (self.measure)(&candidate) > room {
                break;
            }
            shown = candidate;
        }
        shown
    }
}

type LaidOutRow = (Item<DeskKey, DeskClass, Address>, Vec<(usize, Pixels)>);

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
            seen: Default::default(),
            skipped: false,
        }
    }

    #[test]
    fn current_words_are_quiet_context_on_the_existing_card_only() {
        let hand = Hand {
            cards: [7, 2].map(card).to_vec(),
            next_change: None,
        };
        let with_flash = desk_rows(
            &hand,
            |_| None,
            |id| (*id == address(7)).then(|| "Running clock checks".into()),
        );
        let without_flash = desk_rows(&hand, |_| None, |_| None);
        assert_eq!(with_flash.wait_count, without_flash.wait_count);
        assert_eq!(with_flash.waiting[1], without_flash.waiting[1]);
        let row = &with_flash.waiting[0];
        assert_eq!(row.subject, without_flash.waiting[0].subject);
        assert_eq!(row.card, without_flash.waiting[0].card);
        let layout = RowLayout::new(|s: &str| px(s.chars().count() as f32));
        let (wide, _) = layout.item(row, px(55.));
        assert_eq!(
            wide.text,
            "idea 7 unformatted channel Running clock checks\n"
        );
        let (phone, _) = layout.compact(row, px(40.), px(19.));
        assert_eq!(phone.lines, vec![address(7), address(7)]);
        assert_eq!(
            phone.text,
            "idea 7 unformatted channel\n  Running clock checks\n"
        );
        assert!(
            phone
                .styles
                .iter()
                .any(|(class, bytes)| *class == DeskClass::Muted
                    && &phone.text[bytes.clone()] == "Running clock checks")
        );
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
            |address| {
                Some(IdeaProject {
                    address: address.clone(),
                    name: "shoal@lab".into(),
                    details: None,
                    runtime: None,
                })
            },
            |_| None,
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
        assert_eq!(rows.waiting[1].detail.project(), Some("shoal@lab"));
        assert_eq!(rows.waiting[1].card, address(94));
    }

    #[test]
    fn folding_preserves_the_first_card_and_reports_hidden_cards_not_groups() {
        let mut cards = [7, 85, 2, 71, 28, 25, 48, 44, 12, 16, 9, 1, 3, 11].map(card);
        for index in [0, 2, 7, 9, 10, 12, 13] {
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
                "t3" | "t11" => 3,
                _ => return None,
            };
            Some(IdeaProject {
                address: Address::new("project", id),
                name: if id == 3 { "clock@lab" } else { "reef@lab" }.into(),
                details: None,
                runtime: None,
            })
        };
        let rows = desk_rows(&hand, project, |_| None);
        assert_eq!(rows.wait_count, 14);
        assert_eq!(rows.waiting[0].subject, "reef@lab");
        assert_eq!(rows.waiting[0].detail, DeskDetail::Stops("2 failed".into()));
        assert_eq!(rows.waiting[0].card, address(7));
        assert_eq!(rows.waiting[1].card, address(85));
        assert_eq!(rows.waiting[1].detail.project(), Some("reef@lab"));
        assert_eq!(rows.waiting[6].card, address(44));
        assert_eq!(rows.waiting[6].detail, DeskDetail::Stops("3 stops".into()));
        let overflow = rows.overflow.unwrap();
        assert_eq!(overflow.first, address(12));
        assert_eq!(
            overflow.words,
            "and 4 more · unformatted channel · clock@lab"
        );
        let afterwards = Hand {
            cards: hand
                .cards
                .into_iter()
                .filter(|card| card.address != address(7))
                .collect(),
            next_change: None,
        };
        let rows = desk_rows(&afterwards, project, |_| None);
        assert_eq!(rows.waiting[1].card, address(2));
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
                runtime: None,
            })
        };
        let rows = desk_rows(&hand, project, |_| None);
        assert_eq!(rows.waiting[0].subject, "idea 7");
        assert_eq!(rows.waiting[1].subject, "idea 85");
        let overflow = rows.overflow.unwrap();
        assert_eq!(overflow.first, address(44));
        assert_eq!(overflow.words, "and 3 more · clock@lab");

        let hand = Hand {
            cards: hand.cards[8..].into(),
            next_change: None,
        };
        let rows = desk_rows(&hand, project, |_| None);
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
    fn a_narrow_stop_group_opens_from_either_line() {
        let row = DeskRow {
            key: DeskKey::Stops(Address::new("project", 1)),
            subject: "tide@lab".into(),
            detail: DeskDetail::Stops("12 failed".into()),
            flash: None,
            card: address(7),
        };
        let layout = RowLayout::new(|text: &str| px(text.chars().count() as f32 * 5.));
        let (item, gaps) = layout.compact(&row, px(64.), px(29.));
        assert_eq!(item.text, "tide@lab\n  12 failed\n");
        assert_eq!(
            item.styles,
            [(DeskClass::Title, 0..8), (DeskClass::Label, 11..20)]
        );
        assert!(gaps.is_empty());
        assert_eq!(item.lines, [address(7), address(7)]);
    }

    #[test]
    fn laptop_rows_clip_in_shaped_pixels_without_padding_to_other_rows() {
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
            detail: DeskDetail::Project("mini".into()),
            flash: None,
            card: address(3),
        };
        let stops = DeskRow {
            key: DeskKey::Stops(Address::new("project", 1)),
            subject: "WWWW".into(),
            detail: DeskDetail::Stops("2 failed".into()),
            flash: None,
            card: address(4),
        };
        let layout = RowLayout::new(measure);
        // Waiting needs 22 + 8 + 14 = 44 px; the stop row needs the full
        // 36 + 8 + 36 = 80 px. One pixel less must clip only the latter.
        for (room, stops_title) in [(68., "WW…"), (79., "WWW…"), (80., "WWWW")] {
            for (row, title, detail, class) in [
                (&waiting, "WiWi", "mini", DeskClass::Muted),
                (&stops, stops_title, "2 failed", DeskClass::Label),
            ] {
                let (item, gaps) = layout.item(row, px(room));
                assert_eq!(item.text, format!("{title} {detail}\n"));
                assert_eq!(
                    item.styles,
                    [
                        (DeskClass::Title, 0..title.len()),
                        (class, title.len() + 1..item.text.len() - 1)
                    ]
                );
                assert_eq!(gaps, [(title.len(), px(8.))]);
                assert_eq!(item.lines, std::slice::from_ref(&row.card));
            }
        }
    }

    #[test]
    fn an_arrival_does_not_reorder_visible_rows() {
        let row = |n| DeskRow {
            key: DeskKey::Waiting(address(n)),
            subject: format!("row {n}"),
            detail: DeskDetail::Project("lab".into()),
            flash: None,
            card: address(n),
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
