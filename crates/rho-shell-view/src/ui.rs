//! Shells as the frame shows them.
//!
//! A shell hangs under its agent: an agent has one host-owned shell, and
//! the agents system opens a [`ShellSurface`] over it for
//! `agent:<id>/shell`, starting the shell when none runs.

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, Window, div};
use rho_agent_hosts::Link;
use rho_frame::Surface;
use rho_frame::keys::Close;
use theme::ActiveTheme as _;

use crate::protocol::PagerAction;
use crate::{
    ShellEof, ShellInterrupt, ShellModel, ShellPagerAll, ShellPagerMore, ShellPagerQuit,
    ShellSubmit, channel,
};

/// What a shell surface stands on.
enum State {
    /// Dropping the dial cancels it.
    Dialing {
        _dial: gpui::Task<()>,
    },
    Open {
        model: Entity<ShellModel>,
        editor: Entity<Editor>,
    },
    Failed(String),
}

/// A shell as a surface: its transcript and pending command once the host
/// has attached it, and until then what is happening.
pub struct ShellSurface {
    /// The agent, as the host names it and as the user reads it.
    agent: String,
    label: String,
    state: State,
    focus: FocusHandle,
}

impl ShellSurface {
    /// Dials the shell of `agent` (its encoded id) over `link`, the
    /// agent's host. `label` is what the title calls the agent.
    pub fn new(
        link: &Link,
        agent: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dial = channel::open(link, agent.clone());
        let state = State::Dialing {
            _dial: cx.spawn_in(window, async move |this, cx| {
                let dialed = dial.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.state = match dialed {
                        Ok(channel) => {
                            let model = cx.new(|cx| ShellModel::new(channel, cx));
                            let editor =
                                model.update(cx, |model, cx| model.build_editor(window, cx));
                            // The keyboard was on the placeholder; it
                            // moves to the shell it stood for.
                            if this.focus.is_focused(window) {
                                window.focus(&editor.focus_handle(cx), cx);
                            }
                            State::Open { model, editor }
                        }
                        Err(error) => State::Failed(format!("{error:#}")),
                    };
                    cx.notify();
                });
            }),
        };
        Self {
            agent,
            label,
            state,
            focus: cx.focus_handle(),
        }
    }

    fn model(&self) -> Option<&Entity<ShellModel>> {
        match &self.state {
            State::Open { model, .. } => Some(model),
            State::Dialing { .. } | State::Failed(_) => None,
        }
    }

    fn submit(&mut self, _: &ShellSubmit, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.update(cx, |model, cx| model.submit(cx));
        }
    }

    fn interrupt(&mut self, _: &ShellInterrupt, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.update(cx, |model, _| model.interrupt());
        }
    }

    fn eof(&mut self, _: &ShellEof, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.update(cx, |model, cx| model.eof(cx));
        }
    }

    fn pager(&mut self, action: PagerAction, cx: &mut Context<Self>) {
        if let Some(model) = self.model() {
            model.read(cx).pager_action(action);
        }
    }
}

impl Surface for ShellSurface {
    fn title(&self, _cx: &App) -> String {
        format!("{} · shell", self.label)
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        match &self.state {
            State::Open { editor, .. } => Some(editor.clone()),
            State::Dialing { .. } | State::Failed(_) => None,
        }
    }
}

impl Focusable for ShellSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Open { editor, .. } => editor.focus_handle(cx),
            State::Dialing { .. } | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Render for ShellSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let root = div()
            .key_context("RhoShell")
            .size_full()
            .bg(colors.editor_background)
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::interrupt))
            .on_action(cx.listener(Self::eof))
            .on_action(
                cx.listener(|this, _: &ShellPagerMore, _, cx| {
                    this.pager(PagerAction::Continue, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ShellPagerAll, _, cx| this.pager(PagerAction::Drain, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShellPagerQuit, _, cx| this.pager(PagerAction::Quit, cx)),
            );
        let note = match &self.state {
            State::Open { editor, .. } => return root.child(editor.clone()),
            State::Dialing { .. } => format!("connecting to the shell of {}…", self.agent),
            State::Failed(error) => format!("shell of {}: {error}", self.agent),
        };
        root.child(
            div()
                .key_context("RhoShellNote")
                .track_focus(&self.focus)
                .size_full()
                .p_2()
                .text_color(colors.text_muted)
                .child(note),
        )
    }
}

/// The shell's keys, bound once by whoever shows shells.
pub fn bind_keys(cx: &mut App) {
    // The shell is an editor with vim keys: insert mode types the command,
    // and enter in it sends it.
    let insert = "RhoShell > Editor && vim_mode == insert";
    cx.bind_keys([
        KeyBinding::new("enter", ShellSubmit, Some(insert)),
        KeyBinding::new("ctrl-c", ShellInterrupt, Some(insert)),
        KeyBinding::new("ctrl-d", ShellEof, Some(insert)),
        KeyBinding::new("alt-enter", ShellPagerMore, Some("RhoShell > Editor")),
        KeyBinding::new("alt-a", ShellPagerAll, Some("RhoShell > Editor")),
        KeyBinding::new("alt-q", ShellPagerQuit, Some("RhoShell > Editor")),
        // Before the shell is attached there is no editor: `q` still closes.
        KeyBinding::new("q", Close, Some("RhoShellNote")),
    ]);
}
