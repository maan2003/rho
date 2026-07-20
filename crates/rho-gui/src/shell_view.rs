//! Comint-style editor surface for a daemon-owned shell.
//!
//! The multibuffer keeps a read-only projection of daemon-owned structured
//! shell state beside the writable pending command. State deltas update the
//! projection without disturbing a draft while commands run.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;

use editor::hover_links::InlayHighlight;
use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SelectionEffects, SizingBehavior};
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    Context, Entity, FontStyle, FontWeight, HighlightStyle, Subscription, WeakEntity, Window, px,
};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::InlayId;
use rho_ui_proto::shell::{
    MAX_STYLE_SPANS, ShellClientFrame, ShellColor, ShellServerFrame, ShellStyleSpan,
    ShellTextStyle, command_fits,
};
use theme::ActiveTheme as _;

use crate::connection::{ShellChannel, ShellSubmission};
use crate::highlights::{apply_class_highlights, excerpt_range};
use crate::style::{Region, StyleClass};

const PROMPT_INLAY_ID: usize = 0;
const ANSI_HIGHLIGHT_KEY_BASE: usize = usize::MAX / 2;
const MAX_RENDERED_STYLE_SPANS: usize = MAX_STYLE_SPANS;
const MAX_RENDERED_UNIQUE_STYLES: usize = 256;
// Zed does not lay out custom inlays for a wholly empty multibuffer excerpt.
// Keep an invisible cell in the writable excerpt so an idle prompt remains
// visible; it is never sent to the shell.
const INPUT_SENTINEL: &str = "\u{200b}";

pub struct ShellModel {
    transcript_buffer: Entity<Buffer>,
    input_buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    input_end: text::Anchor,
    transcript_attached: bool,
    transcript_styles: Vec<(StyleClass, Range<text::Anchor>)>,
    output_styles: Vec<(ShellTextStyle, Vec<Range<text::Anchor>>)>,
    output_highlight_slots: usize,
    editors: Vec<WeakEntity<Editor>>,
    submit: Option<tokio::sync::mpsc::Sender<ShellSubmission>>,
    control: Option<tokio::sync::mpsc::Sender<ShellClientFrame>>,
    exited: bool,
    disconnected: bool,
    submitting: bool,
    daemon_prompt: String,
    display_prompt: String,
    shell_state: rho_ui_proto::shell::ShellState,
    _read_task: gpui::Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ShellModel {
    pub fn new(channel: ShellChannel, cx: &mut Context<Self>) -> Self {
        let transcript_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let input_buffer = cx.new(|cx| Buffer::local(INPUT_SENTINEL, cx));
        let input_end = {
            let buffer = input_buffer.read(cx);
            buffer.anchor_after(buffer.len())
        };
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(1),
                input_buffer.clone(),
                [Point::zero()..input_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });

        let ShellChannel {
            mut frames,
            submit,
            control,
        } = channel;
        let subscriptions = vec![cx.subscribe(&input_buffer, |_, buffer, event, cx| {
            let has_sentinel = {
                let buffer = buffer.read(cx);
                buffer
                    .text_for_range(0..buffer.len())
                    .any(|chunk| chunk.contains(INPUT_SENTINEL))
            };
            if matches!(event, BufferEvent::Edited { .. }) && !has_sentinel {
                buffer.update(cx, |buffer, cx| {
                    buffer.edit([(0..0, INPUT_SENTINEL)], None, cx);
                });
            }
        })];
        let read_task = cx.spawn(async move |this, cx| {
            while let Some(frame) = frames.next().await {
                let exited = matches!(frame, ShellServerFrame::Exited { .. });
                let disconnected = this.update(cx, |model, cx| {
                    model.apply(frame, cx);
                    model.disconnected
                });
                if disconnected.is_err()
                    || disconnected.is_ok_and(|disconnected| disconnected)
                    || exited
                {
                    return;
                }
            }
            let _ = this.update(cx, |model, cx| model.mark_disconnected(cx));
        });
        Self {
            transcript_buffer,
            input_buffer,
            multi_buffer,
            input_end,
            transcript_attached: false,
            transcript_styles: Vec::new(),
            output_styles: Vec::new(),
            output_highlight_slots: 0,
            editors: Vec::new(),
            submit: Some(submit),
            control: Some(control),
            exited: false,
            disconnected: false,
            submitting: false,
            daemon_prompt: "> ".to_owned(),
            display_prompt: "> ".to_owned(),
            shell_state: rho_ui_proto::shell::ShellState::default(),
            _read_task: read_task,
            _subscriptions: subscriptions,
        }
    }

    /// Builds one pane-local editor over the shared transcript and draft.
    pub fn build_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let multi_buffer = self.multi_buffer.clone();
        let buffer_ids = [
            self.transcript_buffer.read(cx).remote_id(),
            self.input_buffer.read(cx).remote_id(),
        ];
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            crate::editor_config::configure(&mut editor, window, cx);
            for buffer_id in buffer_ids {
                editor.disable_header_for_buffer(buffer_id, cx);
            }
            editor
        });

        if let Some(draft_anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.input_end)
        {
            editor.update(cx, |editor, cx| {
                editor.set_autoscroll_pin(draft_anchor, AutoscrollStrategy::Bottom, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_anchor_ranges([draft_anchor..draft_anchor]);
                });
            });
        }
        self.editors.push(editor.downgrade());
        self.apply_prompt_to(&editor, cx);
        self.apply_transcript_styles_to(&editor, cx);
        self.apply_output_styles_to(&editor, cx);
        editor
    }

    /// Seals the current draft locally at once, while the daemon
    /// acknowledgement determines whether it became authoritative. A failed
    /// submission is restored rather than silently discarded.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.exited || self.disconnected || self.submitting {
            return;
        }
        let command = self.input_text(cx);
        if !command_fits(&command) {
            return;
        }
        let Some(submit) = &self.submit else {
            return;
        };
        let (accepted, accepted_rx) = tokio::sync::oneshot::channel();
        match submit.try_send(ShellSubmission {
            command: command.clone(),
            accepted,
        }) {
            Ok(()) => {
                self.submitting = true;
                self.input_buffer.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer.edit([(0..len, INPUT_SENTINEL)], None, cx);
                });
                self.set_prompt("[sending] ", cx);
                cx.spawn(async move |this, cx| {
                    let accepted = accepted_rx.await.is_ok();
                    let _ = this.update(cx, |model, cx| {
                        model.submitting = false;
                        if accepted {
                            if !model.exited && !model.disconnected {
                                let prompt = model.daemon_prompt.clone();
                                model.set_prompt(&prompt, cx);
                            }
                        } else {
                            model.input_buffer.update(cx, |buffer, cx| {
                                let draft =
                                    buffer.text_for_range(0..buffer.len()).collect::<String>();
                                let draft = draft.replace(INPUT_SENTINEL, "");
                                let restored = if draft.is_empty() {
                                    command
                                } else {
                                    format!("{command}\n{draft}")
                                };
                                let len = buffer.len();
                                buffer.edit(
                                    [(0..len, format!("{INPUT_SENTINEL}{restored}"))],
                                    None,
                                    cx,
                                );
                            });
                            model.mark_disconnected(cx);
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.mark_disconnected(cx);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
        }
    }

    pub fn interrupt(&mut self) {
        if let Some(control) = &self.control {
            let _ = control.try_send(ShellClientFrame::Interrupt);
        }
    }

    pub fn eof(&mut self, cx: &mut Context<Self>) {
        if !self.input_text(cx).is_empty() {
            return;
        }
        if let Some(control) = &self.control {
            let _ = control.try_send(ShellClientFrame::Eof);
        }
    }

    fn apply(&mut self, frame: ShellServerFrame, cx: &mut Context<Self>) {
        match frame {
            // Connection IO consumes acknowledgements to resolve submissions.
            ShellServerFrame::Accepted { .. } => {}
            ShellServerFrame::Snapshot { state } => {
                if !shell_state_styles_valid(&state) {
                    self.mark_disconnected(cx);
                    return;
                }
                self.daemon_prompt.clone_from(&state.prompt);
                self.shell_state = state;
                if !self.submitting {
                    let prompt = self.daemon_prompt.clone();
                    self.set_prompt(&prompt, cx);
                }
            }
            ShellServerFrame::ExecutionQueued { execution } => {
                if !output_styles_valid(&execution.styles, &execution.output, 0) {
                    self.mark_disconnected(cx);
                    return;
                }
                self.shell_state.executions.push(execution);
            }
            ShellServerFrame::ExecutionStarted {
                execution,
                prompt,
                cwd,
            } => {
                if let Some(block) = self
                    .shell_state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
                {
                    block.state = rho_ui_proto::shell::ShellExecutionState::Running;
                    block.prompt = prompt;
                    block.cwd = cwd;
                }
            }
            ShellServerFrame::ExecutionOutput {
                execution,
                start,
                end,
                text,
                styles,
            } => {
                let Some(block) = self
                    .shell_state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
                else {
                    self.mark_disconnected(cx);
                    return;
                };
                let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
                    self.mark_disconnected(cx);
                    return;
                };
                if start > end
                    || end != block.output.len()
                    || !block.output.is_char_boundary(start)
                    || !block.output.is_char_boundary(end)
                    || !replacement_styles_valid(&styles, start, &text)
                    || !replacement_style_count_valid(&block.styles, start, styles.len())
                {
                    self.mark_disconnected(cx);
                    return;
                }
                block.output.replace_range(start..end, &text);
                if !replace_output_styles(&mut block.styles, start, &block.output, styles) {
                    self.mark_disconnected(cx);
                    return;
                }
            }
            ShellServerFrame::ExecutionFinished { execution, status } => {
                if let Some(block) = self
                    .shell_state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
                {
                    block.state = rho_ui_proto::shell::ShellExecutionState::Finished { status };
                }
            }
            ShellServerFrame::ExecutionFailed { execution } => {
                if let Some(block) = execution.and_then(|execution| {
                    self.shell_state
                        .executions
                        .iter_mut()
                        .find(|block| block.execution == execution)
                }) {
                    block.state = rho_ui_proto::shell::ShellExecutionState::Failed;
                }
            }
            ShellServerFrame::TerminalOutput {
                start,
                end,
                text,
                styles,
            } => {
                let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
                    self.mark_disconnected(cx);
                    return;
                };
                let output = &mut self.shell_state.terminal_output;
                if start > end
                    || end != output.len()
                    || !output.is_char_boundary(start)
                    || !output.is_char_boundary(end)
                    || !replacement_styles_valid(&styles, start, &text)
                    || !replacement_style_count_valid(
                        &self.shell_state.terminal_styles,
                        start,
                        styles.len(),
                    )
                {
                    self.mark_disconnected(cx);
                    return;
                }
                output.replace_range(start..end, &text);
                if !replace_output_styles(
                    &mut self.shell_state.terminal_styles,
                    start,
                    output,
                    styles,
                ) {
                    self.mark_disconnected(cx);
                    return;
                }
            }
            ShellServerFrame::Prompt { prompt, cwd } => {
                self.shell_state.prompt.clone_from(&prompt);
                self.shell_state.cwd = cwd;
                self.daemon_prompt = prompt;
                if !self.submitting {
                    let prompt = self.daemon_prompt.clone();
                    self.set_prompt(&prompt, cx);
                }
            }
            ShellServerFrame::Exited { status } => {
                self.exited = true;
                self.submit = None;
                self.control = None;
                let label = status.map_or_else(
                    || "[shell exited] ".to_owned(),
                    |status| format!("[shell exited {status}] "),
                );
                self.set_prompt(&label, cx);
                self.input_buffer
                    .update(cx, |buffer, cx| buffer.set_capability(Capability::Read, cx));
            }
        }
        self.render_transcript(cx);
        cx.notify();
    }

    fn render_transcript(&mut self, cx: &mut Context<Self>) {
        let mut transcript = self.shell_state.terminal_output.clone();
        let mut styles = Vec::new();
        let mut output_styles = VecDeque::new();
        append_output_styles(
            &mut output_styles,
            &self.shell_state.terminal_styles,
            0,
            &self.shell_state.terminal_output,
        );
        for execution in &self.shell_state.executions {
            if matches!(
                execution.state,
                rho_ui_proto::shell::ShellExecutionState::Queued
            ) {
                continue;
            }
            let prompt_start = transcript.len();
            transcript.push_str(&execution.prompt);
            styles.push((StyleClass::ShellPrompt, prompt_start..transcript.len()));
            let command_start = transcript.len();
            transcript.push_str(&execution.command);
            styles.push((StyleClass::ShellCommand, command_start..transcript.len()));
            transcript.push('\n');
            let output_start = transcript.len();
            transcript.push_str(&execution.output);
            append_output_styles(
                &mut output_styles,
                &execution.styles,
                output_start,
                &execution.output,
            );
        }
        // MultiBuffer places the editable input excerpt on the next row. Do
        // not retain a second trailing newline and create a blank row there.
        if transcript.ends_with('\n') {
            transcript.pop();
        }
        self.replace_transcript(0, self.transcript_buffer.read(cx).len(), &transcript, cx);
        if !transcript.is_empty() && !self.transcript_attached {
            self.multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(0),
                    self.transcript_buffer.clone(),
                    [Point::zero()..self.transcript_buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            });
            self.transcript_attached = true;
        }
        let buffer = self.transcript_buffer.read(cx);
        self.transcript_styles = styles
            .into_iter()
            .filter(|(_, range)| !range.is_empty())
            .map(|(class, range)| {
                (
                    class,
                    buffer.anchor_before(range.start)..buffer.anchor_after(range.end),
                )
            })
            .collect();
        let mut grouped = Vec::<(ShellTextStyle, Vec<Range<text::Anchor>>)>::new();
        let mut group_indices = HashMap::new();
        for (style, range) in output_styles {
            if range.end > transcript.len() || range.is_empty() {
                continue;
            }
            let index = if let Some(index) = group_indices.get(&style) {
                *index
            } else if grouped.len() < MAX_RENDERED_UNIQUE_STYLES {
                let index = grouped.len();
                group_indices.insert(style, index);
                grouped.push((style, Vec::new()));
                index
            } else {
                continue;
            };
            grouped[index]
                .1
                .push(buffer.anchor_before(range.start)..buffer.anchor_after(range.end));
        }
        self.output_styles = grouped;
        self.output_highlight_slots = self.output_highlight_slots.max(self.output_styles.len());
        for editor in self.live_editors() {
            self.apply_transcript_styles_to(&editor, cx);
            self.apply_output_styles_to(&editor, cx);
        }
    }

    fn replace_transcript(&mut self, start: usize, end: usize, text: &str, cx: &mut Context<Self>) {
        self.transcript_buffer.update(cx, |buffer, cx| {
            buffer.edit([(start..end, text)], None, cx);
        });
    }

    fn mark_disconnected(&mut self, cx: &mut Context<Self>) {
        if self.exited || self.disconnected {
            return;
        }
        self.disconnected = true;
        self.submit = None;
        self.control = None;
        self.set_prompt("[shell disconnected] ", cx);
        self.input_buffer
            .update(cx, |buffer, cx| buffer.set_capability(Capability::Read, cx));
        cx.notify();
    }

    fn set_prompt(&mut self, prompt: &str, cx: &mut Context<Self>) {
        self.display_prompt.clear();
        self.display_prompt.push_str(prompt);
        self.refresh_prompt(cx);
    }

    fn input_text(&self, cx: &gpui::App) -> String {
        let buffer = self.input_buffer.read(cx);
        let text = buffer.text_for_range(0..buffer.len()).collect::<String>();
        text.replace(INPUT_SENTINEL, "")
    }

    fn refresh_prompt(&mut self, cx: &mut Context<Self>) {
        for editor in self.live_editors() {
            self.apply_prompt_to(&editor, cx);
        }
    }

    fn live_editors(&mut self) -> Vec<Entity<Editor>> {
        self.editors.retain(|editor| editor.upgrade().is_some());
        self.editors
            .iter()
            .filter_map(WeakEntity::upgrade)
            .collect()
    }

    fn apply_prompt_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let input_id = self.input_buffer.read(cx).remote_id();
        let Some(position) = snapshot.anchor_in_excerpt(text::Anchor::min_for_buffer(input_id))
        else {
            return;
        };
        let highlights = (!self.display_prompt.is_empty())
            .then(|| InlayHighlight {
                inlay: InlayId::Custom(PROMPT_INLAY_ID),
                inlay_position: position,
                range: 0..self.display_prompt.len(),
            })
            .into_iter()
            .collect();
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[InlayId::Custom(PROMPT_INLAY_ID)],
                vec![Inlay::custom(
                    PROMPT_INLAY_ID,
                    position,
                    self.display_prompt.clone(),
                )],
                cx,
            );
            editor.highlight_inlays(
                StyleClass::ShellPrompt.highlight_key(Region::System),
                highlights,
                StyleClass::ShellPrompt.resolve(cx),
                cx,
            );
        });
    }

    fn apply_transcript_styles_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let mut prompt = Vec::new();
        let mut command = Vec::new();
        for (class, range) in &self.transcript_styles {
            match class {
                StyleClass::ShellPrompt => prompt.push(range.clone()),
                StyleClass::ShellCommand => command.push(range.clone()),
                _ => {}
            }
        }
        apply_class_highlights(
            editor,
            &self.multi_buffer,
            Region::System,
            [
                (StyleClass::ShellPrompt, prompt.as_slice()),
                (StyleClass::ShellCommand, command.as_slice()),
            ],
            cx,
        );
    }

    fn apply_output_styles_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let updates = self
            .output_styles
            .iter()
            .map(|(style, ranges)| {
                let ranges = ranges
                    .iter()
                    .filter_map(|range| excerpt_range(&snapshot, range))
                    .collect::<Vec<_>>();
                (*style, ranges)
            })
            .collect::<Vec<_>>();
        editor.update(cx, |editor, cx| {
            for index in 0..self.output_highlight_slots {
                let (style, ranges) = updates.get(index).map_or(
                    (HighlightStyle::default(), Vec::new()),
                    |(style, ranges)| (resolve_output_style(*style, cx), ranges.clone()),
                );
                editor.highlight_text(
                    HighlightKey::SyntaxTreeView(ANSI_HIGHLIGHT_KEY_BASE + index),
                    ranges,
                    style,
                    cx,
                );
            }
        });
    }
}

fn replace_output_styles(
    current: &mut Vec<ShellStyleSpan>,
    replacement_start: usize,
    output: &str,
    incoming: Vec<ShellStyleSpan>,
) -> bool {
    if !output_styles_valid(&incoming, output, replacement_start)
        || !replacement_style_count_valid(current, replacement_start, incoming.len())
    {
        return false;
    }
    current.retain(|span| usize::try_from(span.end).is_ok_and(|end| end <= replacement_start));
    current.extend(incoming);
    true
}

fn replacement_style_count_valid(
    current: &[ShellStyleSpan],
    replacement_start: usize,
    incoming_len: usize,
) -> bool {
    let retained = current.partition_point(|span| span.end <= replacement_start as u64);
    retained.saturating_add(incoming_len) <= MAX_STYLE_SPANS
}

fn replacement_styles_valid(
    styles: &[ShellStyleSpan],
    replacement_start: usize,
    replacement: &str,
) -> bool {
    if styles.len() > MAX_STYLE_SPANS {
        return false;
    }
    let Some(replacement_end) = replacement_start.checked_add(replacement.len()) else {
        return false;
    };
    let mut previous_end = replacement_start;
    for span in styles {
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            return false;
        };
        if start < previous_end
            || start >= end
            || end > replacement_end
            || !replacement.is_char_boundary(start - replacement_start)
            || !replacement.is_char_boundary(end - replacement_start)
        {
            return false;
        }
        previous_end = end;
    }
    true
}

fn shell_state_styles_valid(state: &rho_ui_proto::shell::ShellState) -> bool {
    output_styles_valid(&state.terminal_styles, &state.terminal_output, 0)
        && state
            .executions
            .iter()
            .all(|execution| output_styles_valid(&execution.styles, &execution.output, 0))
}

fn output_styles_valid(styles: &[ShellStyleSpan], output: &str, minimum_start: usize) -> bool {
    if styles.len() > MAX_STYLE_SPANS {
        return false;
    }
    let mut previous_end = minimum_start;
    for span in styles {
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            return false;
        };
        if start < minimum_start
            || start < previous_end
            || start >= end
            || end > output.len()
            || !output.is_char_boundary(start)
            || !output.is_char_boundary(end)
        {
            return false;
        }
        previous_end = end;
    }
    true
}

fn append_output_styles(
    output: &mut VecDeque<(ShellTextStyle, Range<usize>)>,
    spans: &[ShellStyleSpan],
    offset: usize,
    text: &str,
) {
    for span in spans {
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            continue;
        };
        if start >= end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        let (Some(start), Some(end)) = (offset.checked_add(start), offset.checked_add(end)) else {
            continue;
        };
        output.push_back((span.style, start..end));
        if output.len() > MAX_RENDERED_STYLE_SPANS {
            output.pop_front();
        }
    }
}

fn resolve_output_style(style: ShellTextStyle, cx: &gpui::App) -> HighlightStyle {
    let colors = cx.theme().colors();
    let color = |color| match color {
        ShellColor::Indexed(index) => crate::terminal_view::terminal_indexed_color(index, colors),
        ShellColor::Rgb { red, green, blue } => {
            crate::terminal_view::terminal_rgb_color(red, green, blue)
        }
    };
    let mut foreground = style
        .foreground
        .map(color)
        .unwrap_or_else(|| colors.terminal_foreground.into());
    let mut background = style.background.map(color);
    if style.inverse {
        let old_foreground = foreground;
        foreground = background.unwrap_or_else(|| colors.terminal_background.into());
        background = Some(old_foreground);
    }
    HighlightStyle {
        color: Some(foreground),
        background_color: background,
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        fade_out: style.dim.then_some(0.3),
        underline: style.underline.then_some(gpui::UnderlineStyle {
            thickness: px(1.),
            color: Some(foreground),
            wavy: false,
        }),
        strikethrough: style.strikethrough.then_some(gpui::StrikethroughStyle {
            thickness: px(1.),
            color: Some(foreground),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed(index: u8) -> ShellTextStyle {
        ShellTextStyle {
            foreground: Some(ShellColor::Indexed(index)),
            ..Default::default()
        }
    }

    #[test]
    fn output_style_tail_replaces_old_ranges() {
        let mut current = vec![
            ShellStyleSpan {
                start: 0,
                end: 2,
                style: indexed(1),
            },
            ShellStyleSpan {
                start: 2,
                end: 4,
                style: indexed(2),
            },
        ];
        let incoming = vec![ShellStyleSpan {
            start: 2,
            end: 4,
            style: indexed(3),
        }];
        assert!(replace_output_styles(&mut current, 2, "abλ", incoming));
        assert_eq!(current.len(), 2);
        assert_eq!(current[0].style, indexed(1));
        assert_eq!(current[1].style, indexed(3));
    }

    #[test]
    fn output_style_tail_rejects_invalid_utf8_ranges() {
        let invalid = vec![ShellStyleSpan {
            start: 3,
            end: 4,
            style: indexed(1),
        }];
        assert!(!replace_output_styles(
            &mut Vec::new(),
            2,
            "abλ",
            invalid.clone(),
        ));
        assert!(!replacement_styles_valid(&invalid, 2, "λ"));
    }

    #[test]
    fn output_style_tail_rejects_combined_span_overflow() {
        let mut current = (0..MAX_STYLE_SPANS)
            .map(|index| ShellStyleSpan {
                start: index as u64,
                end: index as u64 + 1,
                style: indexed((index % 2) as u8),
            })
            .collect::<Vec<_>>();
        let incoming = vec![ShellStyleSpan {
            start: MAX_STYLE_SPANS as u64,
            end: MAX_STYLE_SPANS as u64 + 1,
            style: indexed(2),
        }];
        let output = "x".repeat(MAX_STYLE_SPANS + 1);
        assert!(!replace_output_styles(
            &mut current,
            MAX_STYLE_SPANS,
            &output,
            incoming,
        ));
    }
}
