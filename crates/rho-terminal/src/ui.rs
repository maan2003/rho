//! Terminals as the frame shows them.
//!
//! A terminal hangs under its agent: the agents system opens a
//! [`TerminalSurface`] for `agent:<id>/term/<n>`, and its commands start
//! or pick one. Terminals keep no stream to a host: each surface dials its
//! terminal when it opens and drops the attachment when it closes, and the
//! terminal keeps running on the host.

use editor::{Editor, EditorElement, HighlightKey, SelectionEffects};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, NoAction, Window, div};
use multi_buffer::MultiBufferOffset;
use rho_agent_hosts::Link;
use rho_frame::keys::{self, GuestEscape, Live, Normal, SearchForward};
use rho_frame::{CardHere, Mode, Surface};
use theme::ActiveTheme as _;

use crate::{TerminalModel, TerminalPaste, TerminalView, channel};

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
    reader: Option<Entity<Editor>>,
    phone_ctrl: std::rc::Rc<std::cell::Cell<bool>>,
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
                            let ctrl = this.phone_ctrl.clone();
                            let view = cx.new(|cx| TerminalView::new(model.clone(), ctrl, cx));
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
            reader: None,
            phone_ctrl: Default::default(),
        }
    }
    /// `escape`: the window's keys, over a read-only copy of the screen
    /// and its scrollback.
    fn read(&mut self, _: &Normal, window: &mut Window, cx: &mut Context<Self>) {
        let State::Open(view) = &self.state else {
            return;
        };
        self.phone_ctrl.set(false);
        let reading = view.read(cx).reading_text(cx);
        let reader = cx.new(|cx| {
            let buffer = cx.new(|cx| language::Buffer::local(reading.text, cx));
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (group, (style, ranges)) in reading.painted.into_iter().enumerate() {
                editor.highlight_text(
                    HighlightKey::SyntaxTreeView(group),
                    ranges
                        .into_iter()
                        .map(|range| {
                            snapshot.anchor_after(MultiBufferOffset(range.start))
                                ..snapshot.anchor_before(MultiBufferOffset(range.end))
                        })
                        .collect(),
                    style,
                    cx,
                );
            }
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                let point = language::Point::new(reading.row as u32, 0);
                selections.select_ranges([point..point]);
            });
            editor
        });
        window.focus(&reader.focus_handle(cx), cx);
        self.reader = Some(reader);
        cx.notify();
    }

    /// `f9`: the escape that `escape` itself never sends.
    fn guest_escape(&mut self, _: &GuestEscape, _: &mut Window, cx: &mut Context<Self>) {
        if let State::Open(view) = &self.state {
            view.update(cx, |view, cx| view.phone_key("escape", false, cx));
        }
    }

    fn type_again(&mut self, _: &Live, window: &mut Window, cx: &mut Context<Self>) {
        if self.reader.take().is_some() {
            if let State::Open(view) = &self.state {
                view.update(cx, |view, cx| view.show_live(cx));
            }
            window.focus(&self.focus_handle(cx), cx);
            cx.notify();
        }
    }
}

impl Surface for TerminalSurface {
    /// A workset's terminal is its work, looked into from its card.
    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Inspected
    }

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

    fn phone_tools(&self, _: bool, _cx: &App) -> Vec<rho_frame::PhoneTool> {
        let State::Open(view) = &self.state else {
            return Vec::new();
        };
        use rho_frame::PhoneTool;
        if self.reader.is_some() {
            return vec![
                PhoneTool::new("⌕", "search", |window, cx| {
                    window.dispatch_action(Box::new(SearchForward), cx)
                }),
                PhoneTool::new("✎", "type", |window, cx| {
                    window.dispatch_action(Box::new(Live), cx)
                }),
            ];
        }
        let ctrl = self.phone_ctrl.clone();
        let mut modifier = PhoneTool::new("^", "ctrl", move |_, _| ctrl.set(!ctrl.get()));
        modifier.active = self.phone_ctrl.get();
        let mut tools = vec![modifier];
        tools.extend(
            [
                ("⎋", "esc", "escape"),
                ("⇥", "tab", "tab"),
                ("←", "left", "left"),
                ("↓", "down", "down"),
                ("↑", "up", "up"),
                ("→", "right", "right"),
            ]
            .map(|(icon, label, key)| {
                let ctrl = self.phone_ctrl.clone();
                let view = view.clone();
                PhoneTool::new(icon, label, move |_, cx| {
                    view.update(cx, |view, cx| view.phone_key(key, ctrl.replace(false), cx));
                })
            }),
        );
        tools.push(PhoneTool::new("≡", "read", |window, cx| {
            window.dispatch_action(Box::new(Normal), cx)
        }));
        tools
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        self.reader.clone()
    }

    /// An explicit open is to type.
    fn opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.type_again(&Live, window, cx);
    }

    fn mode(&self, _window: &Window, _cx: &App) -> Option<Mode> {
        match (&self.state, &self.reader) {
            (State::Open(_), None) => Some(Mode::Insert),
            (State::Open(_), Some(_)) => None,
            _ => Some(Mode::Normal),
        }
    }
}

impl Focusable for TerminalSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(reader) = &self.reader {
            return reader.focus_handle(cx);
        }
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
            State::Open(view) => {
                // Live, the pty takes every key the window does not keep.
                let context = match self.reader {
                    Some(_) => "RhoTerminalSurface".to_owned(),
                    None => format!("RhoTerminalSurface {}", keys::LIVE),
                };
                return div()
                    .size_full()
                    .key_context(context.as_str())
                    .on_action(cx.listener(Self::read))
                    .on_action(cx.listener(Self::guest_escape))
                    .on_action(cx.listener(Self::type_again))
                    .child(if let Some(reader) = &self.reader {
                        let background = colors.terminal_background.into();
                        let foreground = colors.terminal_foreground.into();
                        let style = reader.update(cx, |editor, cx| {
                            let mut style = editor.style(cx).clone();
                            style.background = background;
                            style.text.color = foreground;
                            style.text.font_family = ".ZedMono".into();
                            style
                        });
                        EditorElement::new(reader, style).into_any_element()
                    } else {
                        view.clone().into_any_element()
                    });
            }
            State::Dialing { .. } => {
                format!("connecting to term {} of {}…", self.terminal, self.agent)
            }
            State::Failed(error) => format!("term {} of {}: {error}", self.terminal, self.agent),
        };
        // The placeholder is no buffer: the window's keys, so `q` closes it
        // and `:` opens `Commands`.
        div()
            .size_full()
            .key_context(keys::PLACE)
            .track_focus(&self.focus)
            .bg(colors.terminal_background)
            .text_color(colors.text_muted)
            .p_2()
            .child(note)
    }
}

/// The terminal's keys, bound once by whoever shows terminals.
pub fn bind_keys(cx: &mut App) {
    // Live, every key the window does not keep goes to the pty
    // (`rho_frame::keys`); paste is the one chord of the terminal's own,
    // and no shell sees it.
    cx.bind_keys([KeyBinding::new(
        "ctrl-shift-v",
        TerminalPaste,
        Some("RhoTerminal"),
    )]);
    for context in rho_frame::normal_contexts("RhoTerminalSurface", " && vim_operator == none") {
        cx.bind_keys([
            KeyBinding::new("i", Live, Some(&context)),
            KeyBinding::new("a", Live, Some(&context)),
            KeyBinding::new("enter", NoAction, Some(&context)),
        ]);
    }
}
