//! Emacs-style minibuffer: a completing-read strip at the bottom of the
//! window.
//!
//! Generic machinery — a caller opens it with a prompt, a candidate
//! source, and a submit handler. The input is a single-line editor (no
//! vim, like emacs); candidates recompute on every edit. Enter takes the
//! highlighted answer or the typed input if none fits, tab completes
//! without submitting, ctrl-n/p or arrows move the selection, escape cancels.

use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use editor::Editor;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, HighlightStyle, Pixels, SharedString,
    StyledText, Subscription, Window, div, px, rems,
};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::frame::Frame;
use crate::menu::Pick;
use crate::palette::{Choice, Listing, Matched};
use crate::phone::{cut_at_word, measure};
use crate::text_search::{TextHit, TextQuery, TextScope, TextSearch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub description: String,
}

/// What a project address prompt calls a row that makes the project.
pub const NEW_PROJECT: &str = "new project";

pub fn token_start(text_before_cursor: &str) -> usize {
    text_before_cursor
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            character
                .is_whitespace()
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0)
}

/// `ctrl-w` on a prompt line: deletes the word before the cursor, with
/// any spaces between it and the cursor, or the selection when there is one.
pub fn delete_word(editor: &mut Editor, window: &mut Window, cx: &mut gpui::Context<Editor>) {
    let selection = editor
        .selections
        .newest::<multi_buffer::MultiBufferOffset>(&editor.display_snapshot(cx));
    if selection.is_empty() {
        let text = editor.text(cx);
        let start = token_start(text[..selection.start.0].trim_end());
        let start = multi_buffer::MultiBufferOffset(start);
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([start..selection.end]);
        });
    }
    editor.insert("", window, cx);
}

pub fn completion_start(text_before_cursor: &str, whole_input: bool) -> usize {
    if whole_input {
        0
    } else {
        token_start(text_before_cursor)
    }
}

/// How long an echoed message stays visible.
pub const ECHO_DURATION: std::time::Duration = std::time::Duration::from_secs(2);
/// Long messages (`:help`) are capped in the echo area; the transcript
/// keeps the full copy.
const ECHO_MAX_LINES: usize = 12;

/// The most recent system notice, temporarily substituted into the shared
/// status line. Dropping it (replacement or timer) dismisses it.
pub struct Echo {
    text: String,
    class: StyleClass,
    /// Timer that clears the message; cancelled by drop on replacement.
    pub _dismiss: gpui::Task<()>,
}

impl Echo {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The echo is one line in the status line's place: of a text of
    /// several, such as an error a host sent, the first.
    pub fn new(text: &str, class: StyleClass, dismiss: gpui::Task<()>) -> Self {
        Self {
            text: text
                .lines()
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_owned(),
            class,
            _dismiss: dismiss,
        }
    }

    pub fn render(&self, text_style: &gpui::TextStyle, cx: &Context<Frame>) -> AnyElement {
        let colors = cx.theme().colors();
        let style = self.class.resolve(cx);
        let lines = self.text.lines().collect::<Vec<_>>();
        let truncated = lines.len() > ECHO_MAX_LINES;
        let mut rows = lines
            .into_iter()
            .take(ECHO_MAX_LINES)
            .map(|line| div().px_2().child(line.to_owned()))
            .collect::<Vec<_>>();
        if truncated {
            rows.push(
                div()
                    .px_2()
                    .text_color(colors.text_muted)
                    .child("… (full text in the transcript)"),
            );
        }
        let mut area = bottom_strip(text_style, cx);
        if let Some(color) = style.color {
            area = area.text_color(color);
        }
        area.children(rows).into_any_element()
    }
}

/// The shared chrome of the bottom line: same font and size as the
/// editors above it — it reads as the vim command line, not a panel. The
/// background is the editor's, nudged a few percent in lightness so the
/// strip reads as its own region without a border.
pub(crate) fn bottom_strip(text_style: &gpui::TextStyle, cx: &gpui::App) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .py(px(2.))
        .bg(rho_window::style::command_strip_background(cx))
        .text_color(text_style.color)
        .font_family(text_style.font_family.clone())
        .font_weight(text_style.font_weight)
        .text_size(text_style.font_size)
        .line_height(text_style.line_height)
}

/// Recomputes candidates for the current input.
pub type CandidateSource = Rc<dyn Fn(&Frame, &str, &App) -> Vec<Candidate>>;
/// Receives the typed input (tab-completions applied) after the
/// minibuffer has closed.
pub type SubmitHandler = Rc<dyn Fn(&mut Frame, String, &mut Window, &mut Context<Frame>)>;
/// What a prompt does with enter.
pub(crate) enum Answer {
    Text {
        complete: CandidateSource,
        submit: SubmitHandler,
        rows: Vec<Candidate>,
    },
    Palette {
        listing: Listing,
        rows: Vec<Matched>,
        search: Option<PendingText>,
    },
}

impl Answer {
    pub(crate) fn text(complete: CandidateSource, submit: SubmitHandler) -> Self {
        Self::Text {
            complete,
            submit,
            rows: Vec::new(),
        }
    }

    pub(crate) fn palette(listing: Listing) -> Self {
        Self::Palette {
            listing,
            rows: Vec::new(),
            search: None,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Text { rows, .. } => rows.len(),
            Self::Palette { rows, .. } => rows.len(),
        }
    }
}

enum Completed {
    Text(Candidate, usize),
    Palette { index: usize, input: String },
}

pub(crate) struct PendingText {
    cancelled: Arc<AtomicBool>,
    _task: gpui::Task<()>,
}

impl PendingText {
    fn start(search: TextSearch, input: String, cx: &Context<Frame>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let task = cx.spawn(async move |frame, cx| {
            let hits = cx.background_spawn(async move { search.find(&stop) }).await;
            let _ = frame.update(cx, |frame, cx| {
                frame.finish_palette_search(&input, hits, cx)
            });
        });
        Self {
            cancelled,
            _task: task,
        }
    }
}

impl Drop for PendingText {
    fn drop(&mut self) {
        // Dropping the GPUI task prevents delivery; the flag also stops a
        // scan already running on a worker instead of delaying the next key.
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// A closed prompt's answer, run once focus is back where it was.
pub(crate) type Submission = Box<dyn FnOnce(&mut Frame, &mut Window, &mut Context<Frame>)>;
/// Runs when Escape dismisses a prompt that owns external state.
pub type CancelHandler = Rc<dyn Fn(&mut Frame, &mut Window, &mut Context<Frame>)>;

#[derive(Clone, Copy)]
enum CompletionInput {
    Word,
    Whole,
    /// A whole-input project address, with tappable rows.
    Address,
    /// Enter takes the literal input until the reader chooses a row.
    Explicit,
}

pub struct Minibuffer {
    prompt: SharedString,
    editor: Entity<Editor>,
    answer: Answer,
    on_cancel: Option<CancelHandler>,
    validate: Option<fn(&str) -> Option<&'static str>>,
    selected: usize,
    moved_selection: bool,
    completion_input: CompletionInput,
    completed_selection: Option<Completed>,
    /// What is typed here is a credential: the journal keeps none of it.
    secret: bool,
    /// The host's question this prompt asks, while it asks one.
    pub(crate) question: Option<crate::hosts::QuestionKey>,
    _edits: Subscription,
}

/// Candidate rows shown at once; the list scrolls the selection into this
/// window rather than growing unbounded.
const VISIBLE_CANDIDATES: usize = 8;

fn candidate_window(
    selected: usize,
    total: usize,
    layout: crate::Layout,
) -> std::ops::Range<usize> {
    let start = match layout {
        crate::Layout::Desktop => selected.saturating_sub(VISIBLE_CANDIDATES - 1),
        crate::Layout::Phone => selected / VISIBLE_CANDIDATES * VISIBLE_CANDIDATES,
    };
    start..(start + VISIBLE_CANDIDATES).min(total)
}

impl Minibuffer {
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn set_secret(&mut self) {
        self.secret = true;
    }

    pub fn is_secret(&self) -> bool {
        self.secret
    }

    pub(crate) fn open(
        prompt: impl Into<SharedString>,
        text_style: &gpui::TextStyle,
        answer: Answer,
        on_cancel: Option<CancelHandler>,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) -> Self {
        let font = text_style.font();
        let font_size = text_style.font_size;
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            // The input line reads as part of the editor above it, not as
            // a UI widget: same buffer font and size.
            editor.set_text_style_refinement(gpui::TextStyleRefinement {
                font_family: Some(font.family),
                font_size: Some(font_size),
                ..Default::default()
            });
            editor
        });
        let edits = cx.subscribe(&editor, |this: &mut Frame, _, event, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                this.refresh_minibuffer(cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self {
            prompt: prompt.into(),
            editor,
            completion_input: CompletionInput::Whole,
            answer,
            on_cancel,
            validate: None,
            selected: 0,
            moved_selection: false,
            completed_selection: None,
            secret: false,
            question: None,
            _edits: edits,
        }
    }

    pub fn input(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    /// The line the answer is typed in.
    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The prompt's change handler, if it set one. Cloned rather than
    /// borrowed because running it needs the workspace mutably, and the
    /// minibuffer lives in the workspace.
    pub fn on_cancel(&self) -> Option<CancelHandler> {
        self.on_cancel.clone()
    }

    pub fn set_input(&mut self, input: String, window: &mut Window, cx: &mut App) {
        self.editor.update(cx, |editor, cx| {
            editor.set_text(input.clone(), window, cx);
            let end = multi_buffer::MultiBufferOffset(input.len());
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([end..end]);
            });
        });
    }

    /// Recomputes candidates against `workspace`; called by the workspace
    /// on open and after every edit.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn refresh(&mut self, workspace: &Frame, cx: &Context<Frame>) {
        let input = self.input(cx);
        let completed_row = match &mut self.answer {
            Answer::Text { complete, rows, .. } => {
                *rows = complete(workspace, &input, cx);
                match &self.completed_selection {
                    Some(Completed::Text(candidate, occurrence)) if input == candidate.value => {
                        rows.iter()
                            .enumerate()
                            .filter(|(_, other)| *other == candidate)
                            .nth(*occurrence)
                            .map(|(index, _)| index)
                    }
                    _ => None,
                }
            }
            Answer::Palette {
                listing,
                rows,
                search,
            } => {
                // Cancel before preparing the new rows. Named results don't
                // depend on the mailbox, so show them immediately.
                *search = None;
                listing.clear_text();
                let folded = input.trim().to_lowercase();
                let query = TextQuery::parse(&folded);
                *rows = listing.rank_query(&query);
                if query.wants_text() {
                    let slots = crate::palette::LISTED - listing.text_position(rows);
                    let batch = workspace.text_search(folded, slots, cx);
                    if !batch.is_empty() {
                        *search = Some(PendingText::start(batch, input.clone(), cx));
                    }
                }
                match &self.completed_selection {
                    Some(Completed::Palette {
                        index,
                        input: value,
                    }) if input == *value => rows.iter().position(|row| row.index == *index),
                    _ => None,
                }
            }
        };
        self.moved_selection = completed_row.is_some();
        if let Some(index) = completed_row {
            self.selected = index;
        } else {
            self.completed_selection = None;
            self.selected = 0;
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn finish_text(
        &mut self,
        workspace: &Frame,
        input: &str,
        hits: Vec<TextHit>,
        cx: &App,
    ) {
        if self.input(cx) != input {
            return;
        }
        let Answer::Palette {
            listing,
            rows,
            search,
        } = &mut self.answer
        else {
            return;
        };
        if search.take().is_none() {
            return;
        }
        let selected = self
            .moved_selection
            .then(|| rows.get(self.selected).map(|row| row.index))
            .flatten();
        let resolved = workspace.describe_palette_text(hits, cx);
        listing.append_text(rows, resolved);
        self.selected = selected
            .and_then(|index| rows.iter().position(|row| row.index == index))
            .unwrap_or(0);
    }

    pub fn select_by_delta(&mut self, delta: isize) {
        let count = self.answer.len() as isize;
        if count != 0 {
            let selected = if self.takes_candidate() {
                self.selected as isize
            } else if delta > 0 {
                -1
            } else {
                0
            };
            self.selected = (selected + delta).rem_euclid(count) as usize;
            self.moved_selection = true;
        }
    }

    fn select(&mut self, index: usize) -> bool {
        if index >= self.answer.len() {
            return false;
        }
        self.selected = index;
        if let Some((candidate, occurrence)) = self.selected_candidate() {
            self.completed_selection = Some(Completed::Text(candidate, occurrence));
        }
        true
    }

    fn completed_text(&self, input: &str, candidate: &Candidate) -> String {
        let start = completion_start(
            input,
            !matches!(self.completion_input, CompletionInput::Word),
        );
        format!("{}{}", &input[..start], candidate.value)
    }

    /// Tab: completes the highlighted answer into the prompt without
    /// submitting.
    pub fn complete_selected(&mut self, window: &mut Window, cx: &mut App) {
        let value = match &self.answer {
            Answer::Text { .. } => {
                let Some((candidate, occurrence)) = self.selected_candidate() else {
                    return;
                };
                let value = self.completed_text(&self.input(cx), &candidate);
                self.completed_selection = Some(Completed::Text(candidate, occurrence));
                value
            }
            Answer::Palette { listing, rows, .. } => {
                let Some(row) = rows.get(self.selected) else {
                    return;
                };
                let choice = &listing.choices[row.index];
                if matches!(choice, Choice::Words(_)) {
                    // A body hit is not a completion of the typed words.
                    return;
                }
                let input = self.input(cx);
                let folded = input.trim().to_lowercase();
                let prefix = if TextQuery::parse(&folded).scope == TextScope::Commands {
                    "> "
                } else {
                    ""
                };
                let value = format!("{prefix}{}", choice.name());
                self.completed_selection = Some(Completed::Palette {
                    index: row.index,
                    input: value.clone(),
                });
                value
            }
        };
        self.set_input(value, window, cx);
    }

    pub(crate) fn clear_input(&mut self, window: &mut Window, cx: &mut App) {
        self.completed_selection = None;
        self.selected = 0;
        self.editor
            .update(cx, |editor, cx| editor.clear(window, cx));
    }

    pub(crate) fn complete_last_word(&mut self) {
        self.completion_input = CompletionInput::Word;
    }

    /// The answer is a whole project address, chosen from tappable rows;
    /// a row described as [`NEW_PROJECT`] leads with those words.
    pub fn complete_project_address(&mut self) {
        self.completion_input = CompletionInput::Address;
    }

    pub fn complete_only_when_chosen(&mut self) {
        self.completion_input = CompletionInput::Explicit;
    }

    fn takes_candidate(&self) -> bool {
        !matches!(self.completion_input, CompletionInput::Explicit)
            || self.moved_selection
            || self.completed_selection.is_some()
    }

    pub fn selected_candidate(&self) -> Option<(Candidate, usize)> {
        let Answer::Text { rows, .. } = &self.answer else {
            return None;
        };
        let candidate = rows.get(self.selected)?.clone();
        let occurrence = rows[..self.selected]
            .iter()
            .filter(|other| *other == &candidate)
            .count();
        Some((candidate, occurrence))
    }

    pub fn validate(&mut self, validate: fn(&str) -> Option<&'static str>) {
        self.validate = Some(validate);
    }

    /// Why enter cannot take what it would answer, if a validator says so.
    pub(crate) fn rejection(&self, cx: &App) -> Option<&'static str> {
        self.validate
            .and_then(|validate| validate(&self.answer_text(cx)))
    }

    /// What enter would answer: the chosen row completed into the line, or
    /// the line as typed.
    fn answer_text(&self, cx: &App) -> String {
        let typed = self.input(cx);
        match &self.answer {
            Answer::Text { rows, .. } if self.takes_candidate() => rows
                .get(self.selected)
                .map_or_else(|| typed.clone(), |row| self.completed_text(&typed, row)),
            _ => typed,
        }
    }

    pub(crate) fn is_palette(&self) -> bool {
        matches!(self.answer, Answer::Palette { .. })
    }

    pub(crate) fn has_answer(&self) -> bool {
        !self.is_palette() || self.selected < self.answer.len()
    }

    pub(crate) fn delete_word(&mut self, window: &mut Window, cx: &mut App) {
        self.editor
            .update(cx, |editor, cx| delete_word(editor, window, cx));
    }

    /// Consumes the minibuffer into its input and handler; the caller
    /// invokes the handler after restoring focus.
    pub(crate) fn into_submission(self, cx: &App) -> (String, Submission) {
        let input = self.answer_text(cx);
        let submission: Submission = match self.answer {
            Answer::Text { submit, .. } => {
                let input = input.clone();
                Box::new(move |frame, window, cx| submit(frame, input, window, cx))
            }
            Answer::Palette { listing, rows, .. } => {
                let index = rows[self.selected].index;
                let blank = input.trim().is_empty();
                Box::new(move |frame, window, cx| match &listing.choices[index] {
                    Choice::Place(place) => frame.choose_place(place, blank, window, cx),
                    Choice::Words(hit) => frame.open_text_match(hit, window, cx),
                    Choice::Command(entry) => match &entry.pick {
                        Pick::Run { run, .. } | Pick::Infix { run, .. } => {
                            run(frame, None, window, cx)
                        }
                        Pick::Menu(rows) => frame.open_menu(rows.clone(), window, cx),
                    },
                })
            }
        };
        (input, submission)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn render(
        &self,
        text_style: &gpui::TextStyle,
        layout: crate::Layout,
        width: Pixels,
        window: &Window,
        cx: &Context<Frame>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let total = self.answer.len();
        let visible = candidate_window(self.selected, total, layout);
        let address = matches!(self.completion_input, CompletionInput::Address);
        let measure = measure(text_style, window);
        let inset = rems(0.5);
        let row_width = (width - inset.to_pixels(window.rem_size()) * 2.).max(px(0.));
        let columns = match &self.answer {
            Answer::Palette { listing, rows, .. } => Some(PaletteColumns::new(
                rows.iter()
                    .filter_map(|row| {
                        let choice = &listing.choices[row.index];
                        matches!(choice, Choice::Place(_)).then(|| choice.fields())
                    })
                    .take(VISIBLE_CANDIDATES),
                row_width,
                layout,
                &measure,
            )),
            Answer::Text { .. } => None,
        };
        let pages = (layout == crate::Layout::Phone && total > VISIBLE_CANDIDATES).then(|| {
            let page = |id, title, index: Option<usize>| {
                div().flex_1().children(index.map(|index| {
                    div()
                        .h_full()
                        .flex()
                        .items_center()
                        .id(id)
                        .justify_center()
                        .text_color(colors.text_accent)
                        .cursor_pointer()
                        .child(title)
                        .on_click(cx.listener(move |frame, _, _, cx| {
                            if let Some(prompt) = frame.minibuffer_mut()
                                && index < prompt.answer.len()
                            {
                                prompt.selected = index;
                                prompt.moved_selection = true;
                                cx.notify();
                            }
                            cx.stop_propagation();
                        }))
                }))
            };
            div()
                .flex()
                .h(px(56.))
                .border_t_1()
                .border_color(colors.border)
                .child(page(
                    "rho-prompt-previous-page",
                    "‹ previous",
                    visible.start.checked_sub(VISIBLE_CANDIDATES),
                ))
                .child(page(
                    "rho-prompt-next-page",
                    "next ›",
                    (visible.end < total).then_some(visible.end),
                ))
        });
        let rows = visible
            .map(|index| {
                let selected = index == self.selected && self.takes_candidate();
                // Padding lives on the row, not the strip, so the
                // selection background spans the full width.
                let mut row = div().flex().flex_row().gap(inset).w_full().px(inset);
                if let Answer::Palette { listing, rows, .. } = &self.answer {
                    let matched = &rows[index];
                    let choice = &listing.choices[matched.index];
                    let field_colors = [
                        colors.text,
                        if choice.attention() {
                            cx.theme().status().warning
                        } else {
                            colors.text_muted
                        },
                        colors.text_muted,
                    ];
                    let part = |field: usize, value: &str, retained: usize| {
                        let marks = matched.marks[field]
                            .iter()
                            .filter(|range| range.start < retained)
                            .map(|range| {
                                (
                                    range.start..range.end.min(retained),
                                    HighlightStyle {
                                        color: Some(colors.text_accent.into()),
                                        ..HighlightStyle::default()
                                    },
                                )
                            });
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(field_colors[field])
                            .child(StyledText::new(value.to_owned()).with_highlights(marks))
                    };
                    let [title, detail, beside] = choice.fields();
                    let columns = columns.as_ref().expect("palette has columns");
                    row = row.gap(columns.air);
                    match (choice, layout) {
                        (Choice::Place(_), crate::Layout::Phone) => {
                            let has_beside = !beside.is_empty();
                            let title_room = if has_beside { columns.title } else { row_width };
                            let (title, retained) = fit_title(title, title_room, &measure);
                            // As on Desk, the subject and address share the
                            // first line; the card
                            // or status words get their own line.
                            row = row
                                .flex_col()
                                .gap_0()
                                .child(
                                    div()
                                        .flex()
                                        .gap(columns.air)
                                        .child(part(0, &title, retained).w(title_room).flex_none())
                                        .children(has_beside.then(|| {
                                            part(2, beside, beside.len())
                                                .w(columns.beside)
                                                .flex_none()
                                        })),
                                )
                                .children(
                                    (!detail.is_empty())
                                        .then(|| part(1, detail, detail.len()).pl(columns.air)),
                                );
                        }
                        (Choice::Place(_), crate::Layout::Desktop) => {
                            let (title, retained) = fit_title(title, columns.title, &measure);
                            row = row
                                .child(part(0, &title, retained).w(columns.title).flex_none())
                                .children((columns.detail > px(0.)).then(|| {
                                    part(1, detail, detail.len()).w(columns.detail).flex_none()
                                }))
                                .children((!beside.is_empty()).then(|| {
                                    part(2, beside, beside.len()).w(columns.beside).flex_none()
                                }));
                        }
                        (Choice::Words(_), _) => {
                            row = row
                                .flex_col()
                                .gap_0()
                                .child(part(0, title, title.len()))
                                .child(part(1, detail, detail.len()).pl(inset));
                        }
                        (Choice::Command(_), _) => {
                            row = row.child(part(0, title, title.len())).children(
                                (!beside.is_empty() && layout == crate::Layout::Desktop)
                                    .then(|| part(2, beside, beside.len()).flex_none()),
                            );
                        }
                    }
                } else if let Answer::Text { rows, .. } = &self.answer {
                    let candidate = &rows[index];
                    row = row.when(layout == crate::Layout::Phone, |row| row.flex_wrap());
                    let new_project = address && candidate.description == NEW_PROJECT;
                    if new_project {
                        row = row.child(div().max_w_full().child(candidate.description.clone()));
                    }
                    row = row.child(div().max_w_full().child(candidate.value.clone()));
                    if !new_project && !candidate.description.is_empty() {
                        row = row.child(
                            div()
                                .max_w_full()
                                .text_color(colors.text_muted)
                                .child(candidate.description.clone()),
                        );
                    }
                }
                if selected {
                    row = row.bg(colors.element_selected);
                }
                // A click or a tap answers with the row, as on a menu.
                row.id(("rho-prompt-candidate", index))
                    .cursor_pointer()
                    .on_click(cx.listener(move |frame, _, window, cx| {
                        if frame
                            .minibuffer_mut()
                            .is_some_and(|prompt| prompt.select(index))
                        {
                            frame.minibuffer_confirm(window, cx);
                        }
                        cx.stop_propagation();
                    }))
            })
            .collect::<Vec<_>>();
        // A prompt whose rows are its answers says when none answers, so
        // an empty list does not read as one still loading.
        let palette = self.is_palette();
        let missing = if total == 0 && palette && !self.input(cx).trim().is_empty() {
            Some(
                if matches!(
                    self.answer,
                    Answer::Palette {
                        search: Some(_),
                        ..
                    }
                ) {
                    "searching…"
                } else {
                    "no match"
                },
            )
        } else {
            None
        };
        let position = if total == 0 { 0 } else { self.selected + 1 };
        // Input first, candidates beneath it, like emacs completing-read.
        bottom_strip(text_style, cx)
            .key_context("RhoMinibuffer")
            .child(
                // A long question wraps, and the answer goes under it
                // rather than off the edge of a phone.
                div()
                    .flex()
                    .flex_row()
                    .when(!palette, |line| line.flex_wrap())
                    .items_center()
                    .gap_1()
                    .px(inset)
                    .child(div().max_w_full().child(self.prompt.clone()))
                    .child(
                        div()
                            .flex_grow(1.0)
                            .min_w(if palette { px(0.) } else { px(160.) })
                            .child(self.editor.clone()),
                    )
                    .children(palette.then(|| {
                        div()
                            .flex_none()
                            .text_color(colors.text_muted)
                            .child(format!("{position}/{total}"))
                    })),
            )
            .children(rows)
            .children(pages)
            .children(missing.map(|words| div().px_2().text_color(colors.text_muted).child(words)))
            .into_any_element()
    }
}

/// A single column grid for all matching places, so it stays put when the
/// visible rows scroll. As on Desk, the narrow grid moves detail down.
struct PaletteColumns {
    title: Pixels,
    detail: Pixels,
    beside: Pixels,
    air: Pixels,
}

impl PaletteColumns {
    fn new<'a>(
        places: impl Iterator<Item = [&'a str; 3]>,
        width: Pixels,
        layout: crate::Layout,
        measure: &impl Fn(&str) -> Pixels,
    ) -> Self {
        let [title, detail, beside] = places.fold([px(0.); 3], |mut widest, fields| {
            for (column, text) in fields.into_iter().enumerate() {
                if !text.is_empty() && (column != 0 || layout == crate::Layout::Desktop) {
                    widest[column] = widest[column].max(measure(text));
                }
            }
            widest
        });
        let air = measure("  ");
        let (detail, beside) = match layout {
            crate::Layout::Phone => (px(0.), beside.min(width * 0.45)),
            crate::Layout::Desktop => (detail.min(width * 0.40), beside.min(width * 0.25)),
        };
        let gap_count = u8::from(detail > px(0.)) + u8::from(beside > px(0.));
        let heading_room = (width - detail - beside - air * f32::from(gap_count)).max(px(0.));
        let title = match layout {
            crate::Layout::Phone => heading_room,
            crate::Layout::Desktop => title.min(heading_room),
        };
        Self {
            title,
            detail,
            beside,
            air,
        }
    }
}

/// The displayed heading and its retained byte count, excluding the added
/// ellipsis, so match highlights stop at the cut.
fn fit_title<'a>(
    title: &'a str,
    room: Pixels,
    measure: &impl Fn(&str) -> Pixels,
) -> (Cow<'a, str>, usize) {
    if measure(title) <= room {
        return (Cow::Borrowed(title), title.len());
    }
    let clipped = cut_at_word(title, room, measure);
    let retained = clipped.strip_suffix('…').unwrap_or(&clipped).len();
    (Cow::Owned(clipped), retained)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_exposes_whole_pages_and_the_last_partial_page() {
        use crate::Layout::{Desktop, Phone};
        assert_eq!(candidate_window(7, 17, Phone), 0..8);
        assert_eq!(candidate_window(8, 17, Phone), 8..16);
        assert_eq!(candidate_window(16, 17, Phone), 16..17);
        assert_eq!(candidate_window(8, 17, Desktop), 1..9);
    }

    #[test]
    fn card_words_and_addresses_have_stable_columns_for_asymmetric_titles() {
        let measure = |text: &str| px(text.chars().count() as f32);
        let places = [
            ["A long project subject", "asks · 1h", "tide@lab"],
            ["Quiet work", "", "long-project@lab"],
        ];
        let desktop = PaletteColumns::new(
            places.into_iter(),
            px(60.),
            crate::Layout::Desktop,
            &measure,
        );
        assert_eq!(desktop.title + desktop.air, px(24.));
        assert_eq!(desktop.title + desktop.air * 2. + desktop.detail, px(35.));
        let phone =
            PaletteColumns::new(places.into_iter(), px(20.), crate::Layout::Phone, &measure);
        assert_eq!(
            (phone.title, phone.air, phone.beside),
            (px(9.), px(2.), px(9.))
        );
        assert_eq!(phone.detail, px(0.));
    }

    #[test]
    fn fitted_title_cuts_at_a_word_without_highlighting_its_ellipsis() {
        let measure = |text: &str| px(text.chars().count() as f32);
        let title = "Réparer le chronomètre";
        assert_eq!(
            fit_title(title, px(11.), &measure),
            ("Réparer le…".into(), 11)
        );
        assert_eq!(fit_title(title, px(10.), &measure), ("Réparer…".into(), 8));
        assert_eq!(fit_title(title, px(22.), &measure), (title.into(), 24));
        assert_eq!(fit_title(title, px(0.), &measure), ("".into(), 0));
    }
}
