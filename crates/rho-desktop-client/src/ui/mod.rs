//! Desktops as the frame shows them.
//!
//! A desktop hangs under its agent: the agents system hears each host's
//! desktops stream ([`crate::stream::DesktopStreams`]) and opens a
//! [`DesktopSurface`] for `agent:<id>/desktop/<name>`, which dials the
//! live view; only an iroh host carries one.

mod view;

use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Task, Window, div};
use rho_agent_hosts::Link;
use rho_frame::{CardHere, Command, Mode, Spelling, Surface};
use theme::ActiveTheme as _;
pub use view::DesktopView;

pub static DRAW_ON_DESKTOP: Spelling = Spelling {
    name: "draw on desktop",
    aliases: &[("annotate", 9), ("draw", 8), ("sketch", 6), ("mark up", 6)],
    argument: None,
};

pub static BACK_TO_LIVE: Spelling = Spelling {
    name: "back to live",
    aliases: &[("live", 8), ("stop drawing", 8), ("resume desktop", 6)],
    argument: None,
};

/// How the desktop's commands are spelled, for ranking every system's
/// together.
pub static SPELLINGS: &[&Spelling] = &[&DRAW_ON_DESKTOP, &BACK_TO_LIVE];

/// What a desktop surface stands on.
enum State {
    /// Dropping the dial cancels it.
    Dialing {
        _dial: Task<()>,
    },
    Open(Entity<DesktopView>),
    Failed(String),
}

/// A desktop as a surface: its live view once the host has sent it, and
/// until then what is happening.
pub struct DesktopSurface {
    /// The agent, as the host names it and as the user reads it.
    agent: String,
    label: String,
    name: String,
    state: State,
    focus: FocusHandle,
}

impl DesktopSurface {
    /// Dials desktop `name` of `agent` (its encoded id) over `link`, the
    /// agent's host. `label` is what the title calls the agent.
    pub fn new(
        link: &Link,
        agent: String,
        label: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dial = crate::viewer::open(link, agent.clone(), name.clone());
        let state = State::Dialing {
            _dial: cx.spawn_in(window, async move |this, cx| {
                let dialed = dial.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.state = match dialed {
                        Ok(viewer) => {
                            let view = cx.new(|cx| DesktopView::new(viewer, window, cx));
                            // The keyboard was on the placeholder; it
                            // moves to the desktop it stood for.
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
            name,
            state,
            focus: cx.focus_handle(),
        }
    }
}

impl Surface for DesktopSurface {
    fn title(&self, _cx: &App) -> String {
        format!("{} · desktop {}", self.label, self.name)
    }

    /// A workset's desktop is its work, looked into from its card.
    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Inspected
    }

    /// `INS` only while the app takes the keys: never while it connects,
    /// once escaped, or while drawing.
    fn mode(&self, _window: &Window, cx: &App) -> Option<Mode> {
        Some(match &self.state {
            State::Open(view) if view.read(cx).live() => Mode::Insert,
            State::Open(_) | State::Dialing { .. } | State::Failed(_) => Mode::Normal,
        })
    }

    /// An explicit open is to use the app.
    fn opened(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let State::Open(view) = &self.state {
            view.update(cx, |view, cx| view.go_live(cx));
        }
    }

    /// Drawing on the frame in view, and leaving it for the live app.
    fn commands(
        &mut self,
        _: Option<rho_frame::surface::Seen>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Command> {
        let State::Open(view) = &self.state else {
            return Vec::new();
        };
        let drawing = view.read(cx).annotating();
        let title = self.title(cx);
        let (draw, live) = (view.clone(), view.clone());
        vec![
            // Already drawing, it stays drawing.
            Command::simple(&DRAW_ON_DESKTOP, move |_, window, cx| {
                draw.update(cx, |view, cx| {
                    if !view.annotating() {
                        view.toggle_annotation(window, cx)
                    }
                })
            })
            .target(title.clone()),
            Command::simple(&BACK_TO_LIVE, move |_, window, cx| {
                live.update(cx, |view, cx| match view.annotating() {
                    true => view.toggle_annotation(window, cx),
                    false => view.go_live(cx),
                })
            })
            .target(match drawing {
                true => format!("{title} · drawing"),
                false => title,
            }),
        ]
    }
}

impl Focusable for DesktopSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            State::Open(view) => view.focus_handle(cx),
            State::Dialing { .. } | State::Failed(_) => self.focus.clone(),
        }
    }
}

impl Render for DesktopSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let note = match &self.state {
            State::Open(view) => return div().size_full().child(view.clone()),
            State::Dialing { .. } => {
                format!("connecting to desktop {} of {}…", self.name, self.agent)
            }
            State::Failed(error) => format!("desktop {} of {}: {error}", self.name, self.agent),
        };
        // Connecting is no buffer: the window's keys, so `q` closes it.
        div()
            .key_context(rho_frame::keys::PLACE)
            .track_focus(&self.focus)
            .size_full()
            .p_2()
            .bg(colors.editor_background)
            .text_color(colors.text_muted)
            .child(note)
    }
}
