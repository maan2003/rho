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
use crate::workspace::Workspace;

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
        complete: CandidateSource,
        on_submit: SubmitHandler,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let editor = cx.new(|cx| Editor::single_line(window, cx));
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

    pub fn render(&self, cx: &Context<Workspace>) -> AnyElement {
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
                    .px_2()
                    .child(div().child(candidate.value.clone()).when(selected, |el| {
                        el.text_color(colors.text_accent)
                    }));
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
        div()
            .key_context("RhoMinibuffer")
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .border_t_1()
            .border_color(colors.border_variant)
            .bg(colors.elevated_surface_background)
            .children(rows)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .px_2()
                    .py(px(2.))
                    .child(div().text_color(colors.text_accent).child(self.prompt.clone()))
                    .child(div().flex_grow(1.0).child(self.editor.clone())),
            )
            .into_any_element()
    }
}
