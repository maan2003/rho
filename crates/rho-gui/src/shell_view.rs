//! Comint-style editor surface for a daemon-owned shell.
//!
//! The multibuffer keeps a read-only projection of daemon-owned structured
//! shell state beside the writable pending command. State deltas update the
//! projection without disturbing a draft while commands run.

use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, SelectionEffects, SizingBehavior};
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{Context, Entity, Window};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_ui_proto::shell::{ShellClientFrame, ShellServerFrame, command_fits};

use crate::connection::{ShellChannel, ShellSubmission};

pub struct ShellModel {
    transcript_buffer: Entity<Buffer>,
    prompt_buffer: Entity<Buffer>,
    input_buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    input_end: text::Anchor,
    submit: Option<tokio::sync::mpsc::Sender<ShellSubmission>>,
    control: Option<tokio::sync::mpsc::Sender<ShellClientFrame>>,
    exited: bool,
    disconnected: bool,
    submitting: bool,
    daemon_prompt: String,
    shell_state: rho_ui_proto::shell::ShellState,
    _read_task: gpui::Task<()>,
}

impl ShellModel {
    pub fn new(channel: ShellChannel, cx: &mut Context<Self>) -> Self {
        let transcript_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let prompt_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("> ", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let input_buffer = cx.new(|cx| Buffer::local("", cx));
        let input_end = input_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            for (key, buffer) in [
                (0, &transcript_buffer),
                (1, &prompt_buffer),
                (2, &input_buffer),
            ] {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(key),
                    buffer.clone(),
                    [Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            }
            multi_buffer
        });

        let ShellChannel {
            mut frames,
            submit,
            control,
        } = channel;
        let read_task = cx.spawn(async move |this, cx| {
            while let Some(frame) = frames.next().await {
                let exited = matches!(frame, ShellServerFrame::Exited { .. });
                if this.update(cx, |model, cx| model.apply(frame, cx)).is_err() || exited {
                    return;
                }
            }
            let _ = this.update(cx, |model, cx| model.mark_disconnected(cx));
        });
        Self {
            transcript_buffer,
            prompt_buffer,
            input_buffer,
            multi_buffer,
            input_end,
            submit: Some(submit),
            control: Some(control),
            exited: false,
            disconnected: false,
            submitting: false,
            daemon_prompt: "> ".to_owned(),
            shell_state: rho_ui_proto::shell::ShellState::default(),
            _read_task: read_task,
        }
    }

    /// Builds one pane-local editor over the shared transcript and draft.
    pub fn build_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let multi_buffer = self.multi_buffer.clone();
        let buffer_ids = [
            self.transcript_buffer.read(cx).remote_id(),
            self.prompt_buffer.read(cx).remote_id(),
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
        crate::banner::insert(&editor, &self.multi_buffer, cx);
        editor
    }

    /// Seals the current draft locally at once, while the daemon
    /// acknowledgement determines whether it became authoritative. A failed
    /// submission is restored rather than silently discarded.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.exited || self.disconnected || self.submitting {
            return;
        }
        let command = self
            .input_buffer
            .read(cx)
            .text_for_range(0..self.input_buffer.read(cx).len())
            .collect::<String>();
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
                    buffer.edit([(0..len, "")], None, cx);
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
                                let restored = if draft.is_empty() {
                                    command
                                } else {
                                    format!("{command}\n{draft}")
                                };
                                let len = buffer.len();
                                buffer.edit([(0..len, restored)], None, cx);
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
        if !self.input_buffer.read(cx).is_empty() {
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
                self.daemon_prompt.clone_from(&state.prompt);
                self.shell_state = state;
                if !self.submitting {
                    let prompt = self.daemon_prompt.clone();
                    self.set_prompt(&prompt, cx);
                }
            }
            ShellServerFrame::ExecutionQueued { execution } => {
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
                    || end > block.output.len()
                    || !block.output.is_char_boundary(start)
                    || !block.output.is_char_boundary(end)
                {
                    self.mark_disconnected(cx);
                    return;
                }
                block.output.replace_range(start..end, &text);
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
            ShellServerFrame::TerminalOutput { start, end, text } => {
                let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
                    self.mark_disconnected(cx);
                    return;
                };
                let output = &mut self.shell_state.terminal_output;
                if start > end
                    || end > output.len()
                    || !output.is_char_boundary(start)
                    || !output.is_char_boundary(end)
                {
                    self.mark_disconnected(cx);
                    return;
                }
                output.replace_range(start..end, &text);
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
        for execution in &self.shell_state.executions {
            if matches!(
                execution.state,
                rho_ui_proto::shell::ShellExecutionState::Queued
            ) {
                continue;
            }
            transcript.push_str(&execution.prompt);
            transcript.push_str(&execution.command);
            transcript.push('\n');
            transcript.push_str(&execution.output);
        }
        self.replace_transcript(0, self.transcript_buffer.read(cx).len(), &transcript, cx);
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
        self.prompt_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, prompt)], None, cx);
        });
    }
}
