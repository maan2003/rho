//! Emacs-style minibuffer: a completing-read strip at the bottom of the
//! window.
//!
//! Generic machinery — a caller opens it with a prompt, a candidate
//! source, and a submit handler. The input is a single-line editor (no
//! vim, like emacs); candidates recompute on every edit. Enter submits
//! the typed input, tab completes the selected candidate into the last
//! token, ctrl-n/p or arrows move the selection, escape cancels.

use std::rc::Rc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, SharedString, Subscription, Window, div, px,
};
use theme::ActiveTheme as _;

use crate::commands::Candidate;
use crate::style::StyleClass;
use crate::workspace::Workspace;

/// How long an echoed message stays visible.
pub const ECHO_DURATION: std::time::Duration = std::time::Duration::from_secs(6);
/// Long messages (`:help`) are capped in the echo area; the transcript
/// keeps the full copy.
const ECHO_MAX_LINES: usize = 12;

/// The emacs echo area: the most recent system notice, flashed in the
/// bottom strip. The durable copy lives in the transcript log; this is
/// the at-a-glance one. Dropping it (replacement or timer) dismisses it.
pub struct Echo {
    text: String,
    class: StyleClass,
    /// Timer that clears the message; cancelled by drop on replacement.
    pub _dismiss: gpui::Task<()>,
}

impl Echo {
    pub fn new(text: &str, class: StyleClass, dismiss: gpui::Task<()>) -> Self {
        Self {
            text: text.to_owned(),
            class,
            _dismiss: dismiss,
        }
    }

    pub fn render(&self, text_style: &gpui::TextStyle, cx: &Context<Workspace>) -> AnyElement {
        let colors = cx.theme().colors();
        let style = self.class.resolve(cx);
        let lines = self.text.lines().collect::<Vec<_>>();
        let truncated = lines.len() > ECHO_MAX_LINES;
        let mut rows = lines
            .into_iter()
            .take(ECHO_MAX_LINES)
            .map(|line| div().child(line.to_owned()))
            .collect::<Vec<_>>();
        if truncated {
            rows.push(
                div()
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

/// The shared chrome of the bottom line: same background, font, and size
/// as the editors above it — it reads as the vim command line, not a
/// panel.
fn bottom_strip(text_style: &gpui::TextStyle, cx: &Context<Workspace>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .px_2()
        .py(px(2.))
        .bg(cx.theme().colors().editor_background)
        .font_family(text_style.font_family.clone())
        .text_size(text_style.font_size)
}

/// Recomputes candidates for the current input.
pub type CandidateSource = Rc<dyn Fn(&Workspace, &str, &App) -> Vec<Candidate>>;
/// Receives the typed input (tab-completions applied) after the
/// minibuffer has closed.
pub type SubmitHandler = Rc<dyn Fn(&mut Workspace, String, &mut Window, &mut Context<Workspace>)>;

pub struct Minibuffer {
    prompt: SharedString,
    editor: Entity<Editor>,
    complete: CandidateSource,
    on_submit: SubmitHandler,
    candidates: Vec<Candidate>,
    selected: usize,
    _edits: Subscription,
}

/// Candidate rows shown at once; the list scrolls the selection into this
/// window rather than growing unbounded.
const VISIBLE_CANDIDATES: usize = 8;

impl Minibuffer {
    pub fn open(
        prompt: impl Into<SharedString>,
        text_style: &gpui::TextStyle,
        complete: CandidateSource,
        on_submit: SubmitHandler,
        window: &mut Window,
        cx: &mut Context<Workspace>,
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
        let edits = cx.subscribe(&editor, |this: &mut Workspace, _, event, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                this.refresh_minibuffer(cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self {
            prompt: prompt.into(),
            editor,
            complete,
            on_submit,
            candidates: Vec::new(),
            selected: 0,
            _edits: edits,
        }
    }

    pub fn input(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    /// Recomputes candidates against `workspace`; called by the workspace
    /// on open and after every edit.
    pub fn refresh(&mut self, workspace: &Workspace, cx: &App) {
        let input = self.input(cx);
        self.candidates = (self.complete)(workspace, &input, cx);
        self.selected = 0;
    }

    pub fn select_by_delta(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    /// Tab: replaces the last whitespace-delimited token of the input with
    /// the selected candidate.
    pub fn complete_selected(&mut self, window: &mut Window, cx: &mut App) {
        let Some(candidate) = self.candidates.get(self.selected).cloned() else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let start = crate::commands::token_start(&text);
            let new_text = format!("{}{}", &text[..start], candidate.value);
            let end = multi_buffer::MultiBufferOffset(new_text.len());
            editor.set_text(new_text, window, cx);
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([end..end]);
            });
        });
    }

    /// Consumes the minibuffer into its input and handler; the caller
    /// invokes the handler after restoring focus.
    pub fn into_submission(self, cx: &App) -> (String, SubmitHandler) {
        let input = self.input(cx);
        (input, self.on_submit)
    }

    pub fn render(&self, text_style: &gpui::TextStyle, cx: &Context<Workspace>) -> AnyElement {
        let colors = cx.theme().colors();
        // Keep the selection visible: scroll the window, not the list.
        let window_start = self
            .selected
            .saturating_sub(VISIBLE_CANDIDATES.saturating_sub(1));
        let rows = self
            .candidates
            .iter()
            .enumerate()
            .skip(window_start)
            .take(VISIBLE_CANDIDATES)
            .map(|(index, candidate)| {
                let selected = index == self.selected;
                let mut row = div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().child(candidate.value.clone()));
                if !candidate.description.is_empty() {
                    row = row.child(
                        div()
                            .text_color(colors.text_muted)
                            .child(candidate.description.clone()),
                    );
                }
                if selected {
                    row = row.bg(colors.element_selected);
                }
                row
            })
            .collect::<Vec<_>>();
        // Input first, candidates beneath it, like emacs completing-read.
        bottom_strip(text_style, cx)
            .key_context("RhoMinibuffer")
            .child(
                div()
                    .flex()
                    .flex_row()
                    .child(div().child(self.prompt.clone()))
                    .child(div().flex_grow(1.0).child(self.editor.clone())),
            )
            .children(rows)
            .into_any_element()
    }
}
