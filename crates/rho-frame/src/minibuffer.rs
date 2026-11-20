//! Emacs-style minibuffer: a completing-read strip at the bottom of the
//! window.
//!
//! Generic machinery — a caller opens it with a prompt, a candidate
//! source, and a submit handler. The input is a single-line editor (no
//! vim, like emacs); candidates recompute on every edit. Enter takes the
//! highlighted answer or the typed input if none fits, tab completes
//! without submitting, ctrl-n/p or arrows move the selection, escape cancels.

use std::rc::Rc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, SharedString, Subscription, Window, div, px,
};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::frame::Frame;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub description: String,
}

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
    let mut background: gpui::Hsla = cx.theme().colors().editor_background.into();
    if background.l < 0.5 {
        background.l += 0.04;
    } else {
        background.l -= 0.04;
    }
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .py(px(2.))
        .bg(background)
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
/// Receives what the reader chose in a prompt whose rows are its answers,
/// after the minibuffer has closed.
pub type ChooseHandler = Rc<dyn Fn(&mut Frame, Chosen, &mut Window, &mut Context<Frame>)>;

/// What enter chose in a prompt whose rows are its answers: a row, which
/// two rows of the same words cannot confuse, or, when no row fits, the
/// words typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chosen {
    /// The index of the highlighted row in what the candidate source
    /// returned last.
    Row(usize),
    Typed(String),
}

/// What a prompt does with enter.
pub(crate) enum Answer {
    /// Hands over the line, a highlighted row's words put into it.
    Text(SubmitHandler),
    /// Hands over the highlighted row.
    Row(ChooseHandler),
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
}

pub struct Minibuffer {
    prompt: SharedString,
    editor: Entity<Editor>,
    complete: CandidateSource,
    answer: Answer,
    on_cancel: Option<CancelHandler>,
    candidates: Vec<Candidate>,
    selected: usize,
    completion_input: CompletionInput,
    completed_selection: Option<(Candidate, usize)>,
    /// What is typed here is a credential: the journal keeps none of it.
    secret: bool,
    _edits: Subscription,
}

/// Candidate rows shown at once; the list scrolls the selection into this
/// window rather than growing unbounded.
const VISIBLE_CANDIDATES: usize = 8;

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
        complete: CandidateSource,
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
            complete,
            completion_input: CompletionInput::Whole,
            answer,
            on_cancel,
            candidates: Vec::new(),
            selected: 0,
            completed_selection: None,
            secret: false,
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
    pub fn refresh(&mut self, workspace: &Frame, cx: &App) {
        let input = self.input(cx);
        self.candidates = (self.complete)(workspace, &input, cx);
        if let Some((candidate, occurrence)) = self.completed_selection.clone()
            && input == candidate.value
            && let Some(index) = self
                .candidates
                .iter()
                .enumerate()
                .filter(|(_, other)| **other == candidate)
                .nth(occurrence)
                .map(|(index, _)| index)
        {
            self.selected = index;
        } else {
            self.completed_selection = None;
            self.selected = 0;
        }
    }

    pub fn select_by_delta(&mut self, delta: isize) {
        if !self.candidates.is_empty() {
            self.selected = (self.selected as isize + delta)
                .rem_euclid(self.candidates.len() as isize) as usize;
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.candidates.len() {
            self.selected = index;
        }
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
        let Some((candidate, occurrence)) = self.selected_candidate() else {
            return;
        };
        let text = self.completed_text(&self.input(cx), &candidate);
        self.completed_selection = Some((candidate, occurrence));
        self.set_input(text, window, cx);
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

    pub(crate) fn complete_project_address(&mut self) {
        self.completion_input = CompletionInput::Address;
    }

    /// Which row is highlighted, for a caller that needs the row itself
    /// rather than the text on it.
    pub fn selected_row(&self) -> usize {
        self.selected
    }

    pub fn selected_candidate(&self) -> Option<(Candidate, usize)> {
        let candidate = self.candidates.get(self.selected)?.clone();
        let occurrence = self.candidates[..self.selected]
            .iter()
            .filter(|other| *other == &candidate)
            .count();
        Some((candidate, occurrence))
    }

    /// Consumes the minibuffer into its input and handler; the caller
    /// invokes the handler after restoring focus.
    pub(crate) fn into_submission(self, cx: &App) -> (String, Submission) {
        let typed = self.input(cx);
        let candidate = self.candidates.get(self.selected);
        let input = candidate.map_or_else(|| typed.clone(), |row| self.completed_text(&typed, row));
        let chosen = match candidate {
            Some(_) => Chosen::Row(self.selected),
            None => Chosen::Typed(typed),
        };
        let submission: Submission = match self.answer {
            Answer::Text(on_submit) => {
                let input = input.clone();
                Box::new(move |frame, window, cx| on_submit(frame, input, window, cx))
            }
            Answer::Row(on_choose) => {
                Box::new(move |frame, window, cx| on_choose(frame, chosen, window, cx))
            }
        };
        (input, submission)
    }

    pub fn render(
        &self,
        text_style: &gpui::TextStyle,
        layout: crate::Layout,
        cx: &Context<Frame>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        // Keep the selection visible: scroll the window, not the list.
        let window_start = self
            .selected
            .saturating_sub(VISIBLE_CANDIDATES.saturating_sub(1));
        let address = matches!(self.completion_input, CompletionInput::Address);
        let rows = self
            .candidates
            .iter()
            .enumerate()
            .skip(window_start)
            .take(VISIBLE_CANDIDATES)
            .map(|(index, candidate)| {
                let selected = index == self.selected;
                // Padding lives on the row, not the strip, so the
                // selection background spans the full width.
                let mut row = div()
                    .flex()
                    .flex_row()
                    .gap_x_2()
                    .when(layout == crate::Layout::Phone, |row| row.flex_wrap())
                    .w_full()
                    .px_2()
                    .child(div().max_w_full().child(candidate.value.clone()));
                if !candidate.description.is_empty() {
                    row = row.child(
                        div()
                            .max_w_full()
                            .text_color(colors.text_muted)
                            .child(candidate.description.clone()),
                    );
                }
                if selected {
                    row = row.bg(colors.element_selected);
                }
                let row = row.id(("rho-prompt-candidate", index));
                if address {
                    row.cursor_pointer()
                        .on_click(cx.listener(move |frame, _, window, cx| {
                            if let Some(prompt) = frame.minibuffer_mut() {
                                prompt.select(index);
                                frame.minibuffer_confirm(window, cx);
                            }
                            cx.stop_propagation();
                        }))
                } else {
                    row
                }
            })
            .collect::<Vec<_>>();
        // A prompt whose rows are its answers says when none answers, so
        // an empty list does not read as one still loading.
        let no_match = rows.is_empty()
            && matches!(self.answer, Answer::Row(_))
            && !self.input(cx).trim().is_empty();
        // Input first, candidates beneath it, like emacs completing-read.
        bottom_strip(text_style, cx)
            .key_context("RhoMinibuffer")
            .child(
                // A long question wraps, and the answer goes under it
                // rather than off the edge of a phone.
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .child(div().max_w_full().child(self.prompt.clone()))
                    .child(
                        div()
                            .flex_grow(1.0)
                            .min_w(px(160.))
                            .child(self.editor.clone()),
                    ),
            )
            .children(rows)
            .children(
                no_match.then(|| div().px_2().text_color(colors.text_muted).child("no match")),
            )
            .into_any_element()
    }
}
