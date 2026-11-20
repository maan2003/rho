//! Terminals as the frame shows them.
//!
//! A terminal hangs under its agent: the agents system opens a
//! [`TerminalSurface`] for `agent:<id>/term/<n>`, and offers the rows that
//! start one. Terminals keep no stream to a host: each surface dials its
//! terminal when it opens and drops the attachment when it closes, and the
//! terminal keeps running on the host.

use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, NoAction, Window, div};
use rho_agent_hosts::Link;
use rho_frame::keys::{Close, Home, Palette, VerdictMenu};
use rho_frame::{Mode, Surface};
use theme::ActiveTheme as _;

use crate::protocol::TerminalInfo;
use crate::{
    TerminalModel, TerminalNormalMode, TerminalPaste, TerminalRawMode, TerminalScrollBottom,
    TerminalScrollHalfPageDown, TerminalScrollHalfPageUp, TerminalScrollLineDown,
    TerminalScrollLineUp, TerminalScrollTop, TerminalView, channel,
};

/// Which of an agent's terminals to open, given the ones that run: with
/// `new` a fresh id past them all, else the first that runs, or `0` when
/// none does.
pub fn pick_terminal(running: &[TerminalInfo], new: bool) -> u64 {
    match (new, running.first()) {
        (true, _) => channel::next_terminal_id(running),
        (false, Some(info)) => info.terminal_id,
        (false, None) => 0,
    }
}

/// What a terminal surface stands on.
enum State {
    /// Dropping the dial cancels it.
    Dialing {
        _dial: gpui::Task<()>,
    },
    Open(Entity<TerminalView>),
    Failed(String),
}

/// A terminal as a surface: its view once the host has attached it, and
/// until then what is happening.
pub struct TerminalSurface {
    /// The agent, as the host names it and as the user reads it.
    agent: String,
    label: String,
    terminal: u64,
    state: State,
    focus: FocusHandle,
}

impl TerminalSurface {
    /// Dials terminal `terminal` of `agent` (its encoded id) over `link`,
    /// the agent's host. `label` is what the title calls the agent.
    pub fn new(
        link: &Link,
        agent: String,
        label: String,
        terminal: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The pty takes its size from the view on its first paint.
        let dial = channel::open(link, agent.clone(), terminal, 80, 24);
        let state = State::Dialing {
            _dial: cx.spawn_in(window, async move |this, cx| {
                let dialed = dial.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.state = match dialed {
                        Ok(channel) => {
                            let model = cx.new(|cx| TerminalModel::new(channel, cx));
                            let view = cx.new(|cx| TerminalView::new(model.clone(), cx));
                            cx.observe(&model, |_, _, cx| cx.notify()).detach();
                            // The keyboard was on the placeholder; it
                            // moves to the terminal it stood for.
                            if this.focus.is_focused(window) {
                                window.focus(&view.focus_handle(cx), cx);
                            }
                            State::Open(view)
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
            terminal,
            state,
            focus: cx.focus_handle(),
        }
    }
}

impl Surface for TerminalSurface {
    fn title(&self, cx: &App) -> String {
        let mut title = format!("{} · term {}", self.label, self.terminal);
        if let State::Open(view) = &self.state {
            let osc = view.read(cx).model().read(cx).title();
            if !osc.is_empty() {
                title = format!("{title} · {osc}");
            }
        }
        title
    }

    /// Typing while the terminal has the keys, reading once
    /// `ctrl-shift-n` took them back; a terminal not yet open only reads.
    fn mode(&self, cx: &App) -> Option<Mode> {
        Some(match &self.state {
            State::Open(view) if view.read(cx).is_raw() => Mode::Insert,
            State::Open(_) | State::Dialing { .. } | State::Failed(_) => Mode::Normal,
        })
    }
}

impl Focusable for TerminalSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Open(view) => view.focus_handle(cx),
            State::Dialing { .. } | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Render for TerminalSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let note = match &self.state {
            State::Open(view) => return div().size_full().child(view.clone()),
            State::Dialing { .. } => {
                format!("connecting to term {} of {}…", self.terminal, self.agent)
            }
            State::Failed(error) => format!("term {} of {}: {error}", self.terminal, self.agent),
        };
        // The placeholder takes the keys of a terminal in normal mode, so
        // `q` closes it and `:` is the palette.
        div()
            .size_full()
            .key_context("RhoTerminalNormal")
            .track_focus(&self.focus)
            .bg(colors.terminal_background)
            .text_color(colors.text_muted)
            .p_2()
            .child(note)
    }
}

/// The terminal's keys, bound once by whoever shows terminals.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Raw mode: every unbound key goes to the pty, so the terminal's
        // own chords are ones shells do not see. `ctrl-\ ctrl-n` is vim's
        // terminal escape; `ctrl-shift-n` the discoverable one. The palette
        // chord, `ctrl-shift-;`, is the frame's own.
        KeyBinding::new("ctrl-shift-v", TerminalPaste, Some("RhoTerminal")),
        KeyBinding::new("ctrl-\\ ctrl-n", TerminalNormalMode, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-n", TerminalNormalMode, Some("RhoTerminal")),
        // The frame walks history on ctrl-j and ctrl-k; in a shell they are
        // the shell's.
        KeyBinding::new("ctrl-j", NoAction, Some("RhoTerminal")),
        KeyBinding::new("ctrl-k", NoAction, Some("RhoTerminal")),
        // Normal mode: the keyboard is rho's again; vim keys browse
        // scrollback and insert returns to raw.
        KeyBinding::new("i", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("a", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("enter", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("j", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("k", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new("down", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("up", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new(
            "ctrl-d",
            TerminalScrollHalfPageDown,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new(
            "ctrl-u",
            TerminalScrollHalfPageUp,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new("g g", TerminalScrollTop, Some("RhoTerminalNormal")),
        KeyBinding::new("shift-g", TerminalScrollBottom, Some("RhoTerminalNormal")),
        KeyBinding::new(":", Palette, Some("RhoTerminalNormal")),
        KeyBinding::new("space", VerdictMenu, Some("RhoTerminalNormal")),
        KeyBinding::new("tab", Home, Some("RhoTerminalNormal")),
        KeyBinding::new("q", Close, Some("RhoTerminalNormal")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(agent: &str, terminal_id: u64) -> TerminalInfo {
        TerminalInfo {
            agent: agent.to_owned(),
            terminal_id,
            title: String::new(),
            cols: 80,
            rows: 24,
            clients: 0,
        }
    }

    #[test]
    fn an_agent_opens_its_first_terminal_or_a_fresh_one() {
        let running = [info("eng-ht08", 2), info("eng-ht08", 5)];
        assert_eq!(pick_terminal(&running, false), 2, "the first running");
        assert_eq!(
            pick_terminal(&running, true),
            6,
            "one past the highest, not past the first"
        );
        assert_eq!(pick_terminal(&[], false), 0);
        assert_eq!(pick_terminal(&[], true), 0);
    }
}
