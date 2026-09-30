//! The terminal surface: its model and its views.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    Focusable, HighlightStyle, Hsla, IntoElement, KeyDownEvent, MouseButton, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, StyledText, Subscription, TextStyle, UTF16Selection, Window,
    canvas, div, px,
};
use settings::Settings as _;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;

use crate::channel::TerminalChannel;
use crate::protocol::{
    FrameApplied, ScrollbackItem, TermCell, TermCellFlags, TermClientFrame, TermColor,
    TermKeystroke, TermRow, TermServerFrame, WireScreen,
};

gpui::actions!(
    rho_terminal,
    [
        TerminalPaste,
        TerminalNormalMode,
        TerminalRawMode,
        TerminalScrollLineUp,
        TerminalScrollLineDown,
        TerminalScrollHalfPageUp,
        TerminalScrollHalfPageDown,
        TerminalScrollTop,
        TerminalScrollBottom,
    ]
);

/// Client-side scrollback retention; the agent host replays up to its own cap.
const SCROLLBACK_LIMIT: usize = 8192;
const TOUCH_KEY_HEIGHT: f32 = 48.;

/// The shared terminal state: wire screen, input stream, and read task.
/// Panes hold views over it; the model outlives any of them.
pub struct TerminalModel {
    screen: WireScreen,
    input: futures_mpsc::Sender<TermClientFrame>,
    /// Monotonic count of lines appended to scrollback, so views can keep
    /// their place while history arrives (the ring length saturates).
    history_appended: u64,
    /// (cols, rows) last sent to the agent host; shared with the focused
    /// view's paint-time measurement so resizes go straight to the stream.
    sent_size: Rc<Cell<(u16, u16)>>,
    /// The stream ended without an `Exited` status (agent host or dial gone).
    disconnected: bool,
    _read_task: gpui::Task<()>,
    _transport: rho_rpc::ChannelTask,
}

impl TerminalModel {
    pub fn new(channel: TerminalChannel, cx: &mut Context<Self>) -> Self {
        let TerminalChannel {
            terminal_id: _,
            mut frames,
            input,
            transport,
        } = channel;
        let read_task = cx.spawn(async move |this, cx| {
            while let Some(Ok(frame)) = frames.next().await {
                let exited = matches!(frame, TermServerFrame::Exited { .. });
                if this
                    .update(cx, |model: &mut TerminalModel, cx| model.apply(frame, cx))
                    .is_err()
                    || exited
                {
                    return;
                }
            }
            let _ = this.update(cx, |model, cx| {
                model.disconnected = true;
                cx.notify();
            });
        });
        Self {
            screen: WireScreen::new(SCROLLBACK_LIMIT),
            input,
            history_appended: 0,
            sent_size: Rc::new(Cell::new((0, 0))),
            disconnected: false,
            _read_task: read_task,
            _transport: transport,
        }
    }

    fn apply(&mut self, frame: TermServerFrame, cx: &mut Context<Self>) {
        let before = self.screen.scrollback.len();
        let applied = self.screen.apply(frame);
        if matches!(applied, FrameApplied::History) {
            self.history_appended += (self.screen.scrollback.len() - before) as u64;
        }
        cx.notify();
    }

    fn send(&self, frame: TermClientFrame) {
        let _ = self.input.clone().try_send(frame);
    }
}

struct Preedit {
    text: String,
    selection: Range<usize>,
}

/// A terminal's viewport: focus, scroll offset, and mode.
pub struct TerminalView {
    model: Entity<TerminalModel>,
    focus_handle: FocusHandle,
    /// Raw mode forwards keystrokes to the pty; normal mode releases the
    /// keyboard to rho bindings.
    raw: bool,
    /// Whole lines scrolled up into history; 0 pins to the live screen.
    scroll_offset: usize,
    /// `history_appended` as of the last observe, for offset preservation.
    seen_history: u64,
    /// One terminal line in pixels, for wheel-delta conversion.
    line_height_px: Rc<Cell<f32>>,
    /// Paint-time cell geometry for mouse-report coordinates.
    cell_width_px: Rc<Cell<f32>>,
    grid_origin_px: Rc<Cell<(f32, f32)>>,
    /// IME preedit stays local: only committed text reaches the PTY.
    marked_text: Option<Preedit>,
    _model_changed: Subscription,
}

impl TerminalView {
    pub fn new(model: Entity<TerminalModel>, cx: &mut Context<Self>) -> Self {
        let seen_history = model.read(cx).history_appended;
        let model_changed = cx.observe(&model, |view, model, cx| {
            let (appended, limit) = {
                let model = model.read(cx);
                (model.history_appended, model.screen.scrollback.len())
            };
            let delta = (appended - view.seen_history) as usize;
            view.seen_history = appended;
            if view.scroll_offset > 0 {
                // Keep the viewed lines fixed while new history arrives below.
                view.scroll_offset = (view.scroll_offset + delta).min(limit);
            }
            cx.notify();
        });
        Self {
            model,
            focus_handle: cx.focus_handle(),
            raw: true,
            scroll_offset: 0,
            seen_history,
            line_height_px: Rc::new(Cell::new(16.0)),
            cell_width_px: Rc::new(Cell::new(8.0)),
            grid_origin_px: Rc::new(Cell::new((0.0, 0.0))),
            marked_text: None,
            _model_changed: model_changed,
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.raw {
            return;
        }
        let ks = &event.keystroke;
        if ks.modifiers.platform {
            return;
        }
        // GPUI delivers printable text through the registered input handler
        // after key dispatch. Sending it here too would type it twice.
        if !ks.modifiers.control
            && !ks.modifiers.alt
            && ((ks.key_char.is_some() && !matches!(ks.key.as_str(), "enter" | "tab"))
                || self.marked_text.is_some())
        {
            return;
        }
        let keystroke = TermKeystroke {
            key: ks.key.clone(),
            ctrl: ks.modifiers.control,
            alt: ks.modifiers.alt,
            shift: ks.modifiers.shift,
            key_char: ks.key_char.clone(),
        };
        let handled = probably_produces_bytes(&keystroke);
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
        }
        self.send_keystroke(keystroke, cx);
        if handled {
            cx.stop_propagation();
        }
    }

    /// Hardware and touch key controls use the same terminal input stream.
    pub fn send_keystroke(&mut self, keystroke: TermKeystroke, cx: &mut Context<Self>) {
        self.raw = true;
        self.scroll_offset = 0;
        self.model
            .read(cx)
            .send(TermClientFrame::Keystroke(keystroke));
        cx.notify();
    }

    fn paste(&mut self, _: &crate::TerminalPaste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.marked_text = None;
            self.scroll_offset = 0;
            self.model.read(cx).send(TermClientFrame::Paste(text));
            cx.notify();
        }
    }

    fn enter_normal_mode(
        &mut self,
        _: &crate::TerminalNormalMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.raw = false;
        self.marked_text = None;
        cx.notify();
    }

    fn enter_raw_mode(
        &mut self,
        _: &crate::TerminalRawMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.raw = true;
        self.scroll_offset = 0;
        cx.notify();
    }

    fn scroll_lines(&mut self, delta: isize, cx: &mut Context<Self>) {
        let limit = self.model.read(cx).screen.scrollback.len() as isize;
        let offset = (self.scroll_offset as isize + delta).clamp(0, limit) as usize;
        if offset != self.scroll_offset {
            self.scroll_offset = offset;
            cx.notify();
        }
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    fn half_page(&self, cx: &Context<Self>) -> isize {
        (self.model.read(cx).screen.rows.len() as isize / 2).max(1)
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / self.line_height_px.get().max(1.0),
        };
        let lines = (lines * 3.0).round() as isize;
        let (application_scroll, cols, rows) = {
            let model = self.model.read(cx);
            (
                model.screen.application_scroll,
                model.screen.cols,
                model.screen.rows.len(),
            )
        };
        if self.raw && application_scroll && lines != 0 {
            let (origin_x, origin_y) = self.grid_origin_px.get();
            let col = ((f32::from(event.position.x) - origin_x) / self.cell_width_px.get().max(1.0))
                .floor()
                .clamp(0.0, f32::from(cols.saturating_sub(1))) as u16;
            let row = ((f32::from(event.position.y) - origin_y)
                / self.line_height_px.get().max(1.0))
            .floor()
            .clamp(0.0, rows.saturating_sub(1) as f32) as u16;
            self.scroll_offset = 0;
            self.model.read(cx).send(TermClientFrame::Scroll {
                lines: lines.clamp(i16::MIN as isize, i16::MAX as isize) as i16,
                col,
                row,
                ctrl: event.modifiers.control,
                alt: event.modifiers.alt,
                shift: event.modifiers.shift,
            });
            cx.notify();
        } else {
            self.scroll_lines(lines, cx);
        }
    }

    /// The window of lines the viewport shows: the live screen, shifted up
    /// into scrollback by `scroll_offset`.
    fn visible_lines<'a>(&self, screen: &'a WireScreen) -> Vec<VisibleLine<'a>> {
        let height = screen.rows.len().max(1);
        let scrollback = &screen.scrollback;
        let total = scrollback.len() + screen.rows.len();
        let offset = self.scroll_offset.min(scrollback.len());
        let end = total - offset;
        let start = end.saturating_sub(height);
        let cursor = &screen.cursor;
        (start..end)
            .map(|index| {
                if index < scrollback.len() {
                    match &scrollback[index] {
                        ScrollbackItem::Line(row) => VisibleLine::Row { row, cursor: None },
                        ScrollbackItem::Gap(lost) => VisibleLine::Gap(*lost),
                    }
                } else {
                    let row_index = index - scrollback.len();
                    let at_cursor =
                        offset == 0 && cursor.visible && usize::from(cursor.row) == row_index;
                    VisibleLine::Row {
                        row: &screen.rows[row_index],
                        cursor: at_cursor.then_some(cursor.col),
                    }
                }
            })
            .collect()
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        // The terminal application's screen is not an editable text document.
        None
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        (self.raw || ignore_disabled_input).then(|| UTF16Selection {
            range: self
                .marked_text
                .as_ref()
                .map_or(0..0, |marked| marked.selection.clone()),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|marked| 0..marked.text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if self.raw && !text.is_empty() {
            self.scroll_offset = 0;
            self.model
                .read(cx)
                .send(TermClientFrame::Input(text.as_bytes().to_vec()));
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.raw {
            self.marked_text = (!text.is_empty()).then(|| {
                let end = text.encode_utf16().count();
                Preedit {
                    text: text.to_owned(),
                    selection: selected_range.unwrap_or(end..end),
                }
            });
            self.scroll_offset = 0;
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cursor = &self.model.read(cx).screen.cursor;
        let (x, y) = self.grid_origin_px.get();
        Some(Bounds {
            origin: Point::new(
                px(x + f32::from(cursor.col) * self.cell_width_px.get()),
                px(y + f32::from(cursor.row) * self.line_height_px.get()),
            ),
            size: gpui::size(px(self.cell_width_px.get()), px(self.line_height_px.get())),
        })
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        self.raw
    }
}

enum VisibleLine<'a> {
    Row {
        row: &'a TermRow,
        /// Draw the cursor over this column.
        cursor: Option<u16>,
    },
    Gap(u64),
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let settings = ThemeSettings::get_global(cx);
        let font = settings.buffer_font.clone();
        let font_size = settings.buffer_font_size(cx);
        let line_height = font_size * settings.buffer_line_height.value();
        self.line_height_px.set(f32::from(line_height));

        let mut text_style = window.text_style();
        text_style.font_family = font.family.clone();
        text_style.font_features = font.features.clone();
        text_style.font_fallbacks = font.fallbacks.clone();
        text_style.font_weight = font.weight;
        text_style.font_size = font_size.into();
        text_style.line_height = line_height.into();
        let foreground: Hsla = colors.terminal_foreground.into();
        let background: Hsla = colors.terminal_background.into();
        text_style.color = foreground;

        let cell_width = window
            .text_system()
            .em_advance(
                window.text_system().resolve_font(&text_style.font()),
                font_size,
            )
            .unwrap_or(px(8.0));
        self.cell_width_px.set(f32::from(cell_width));

        let focused = self.focus_handle.is_focused(window);

        // Size measurement happens at paint time: compare the viewport bounds
        // to the cell metrics and tell the agent host when the grid changed.
        // Only the focused view drives the pty size (tmux `window-size
        // latest`) so hidden terminal surfaces do not resize the pty.
        let model = self.model.read(cx);
        let sent_size = model.sent_size.clone();
        let input = model.input.clone();
        let grid_origin_px = self.grid_origin_px.clone();
        let raw = self.raw;
        let focus_handle = self.focus_handle.clone();
        let view = cx.entity();
        let measure = canvas(
            move |bounds, _window, _cx| {
                grid_origin_px.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
                if !focused {
                    return;
                }
                let cols = (f32::from(bounds.size.width) / f32::from(cell_width)).floor();
                let rows = (f32::from(bounds.size.height) / f32::from(line_height)).floor();
                let size = (cols.max(2.0) as u16, rows.max(2.0) as u16);
                if sent_size.get() != size {
                    sent_size.set(size);
                    let _ = input.clone().try_send(TermClientFrame::Resize {
                        cols: size.0,
                        rows: size.1,
                    });
                }
            },
            move |bounds, _, window, cx| {
                if raw {
                    window.handle_input(
                        &focus_handle,
                        ElementInputHandler::new(bounds, view.clone()),
                        cx,
                    );
                }
            },
        )
        .size_full();

        let palette = Palette {
            foreground,
            background,
            colors: &colors,
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for line in self.visible_lines(&model.screen) {
            rows.push(match line {
                VisibleLine::Row { row, cursor } => {
                    let cursor = if focused { cursor } else { None };
                    row_element(row, cursor, self.raw, &text_style, &palette)
                }
                VisibleLine::Gap(lost) => div()
                    .h(line_height)
                    .child(format!("· · · {lost} lines lost · · ·"))
                    .text_color(foreground.opacity(0.5))
                    .into_any_element(),
            });
        }

        let status = if let Some(status) = model.screen.exited {
            Some(match status {
                Some(code) => format!("terminal exited ({code})"),
                None => "terminal exited".to_owned(),
            })
        } else if model.disconnected {
            Some("terminal disconnected".to_owned())
        } else {
            None
        };
        let normal_badge = (!self.raw).then(|| {
            div()
                .absolute()
                .top_0()
                .right_2()
                .px_1()
                .bg(colors.element_background)
                .text_color(foreground.opacity(0.7))
                .child("NORMAL")
        });

        let touch = cx
            .try_global::<rho_window::TouchMode>()
            .is_some_and(|mode| mode.0);
        let mut strip = div()
            .id("terminal-touch-keys")
            .w_full()
            .h(px(TOUCH_KEY_HEIGHT))
            .flex_shrink_0()
            .flex()
            .overflow_x_scroll()
            .bg(colors.element_background)
            .text_color(colors.text);
        for (index, (label, key, ctrl)) in [
            ("Esc", "escape", false),
            ("Tab", "tab", false),
            ("Ctrl-C", "c", true),
            ("←", "left", false),
            ("↓", "down", false),
            ("↑", "up", false),
            ("→", "right", false),
            ("Enter", "enter", false),
        ]
        .into_iter()
        .enumerate()
        {
            strip = strip.child(
                div()
                    .id(("terminal-touch-key", index))
                    .min_w(px(TOUCH_KEY_HEIGHT))
                    .h_full()
                    .px_2()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.send_keystroke(
                            TermKeystroke {
                                key: key.into(),
                                ctrl,
                                ..Default::default()
                            },
                            cx,
                        );
                        window.focus(&this.focus_handle, cx);
                    })),
            );
        }
        strip = strip.child(
            div()
                .id("terminal-touch-paste")
                .min_w(px(TOUCH_KEY_HEIGHT))
                .h_full()
                .px_2()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child("Paste")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.raw = true;
                    this.paste(&crate::TerminalPaste, window, cx);
                    window.focus(&this.focus_handle, cx);
                })),
        );
        let preedit = self.marked_text.as_ref().map(|marked| {
            let text = &marked.text;
            div()
                .absolute()
                .left(cell_width * f32::from(model.screen.cursor.col))
                .top(line_height * f32::from(model.screen.cursor.row))
                .bg(background)
                .child(StyledText::new(text.clone()).with_default_highlights(
                    &text_style,
                    [(
                        0..text.len(),
                        HighlightStyle {
                            underline: Some(gpui::UnderlineStyle {
                                thickness: px(1.),
                                color: Some(foreground),
                                wavy: false,
                            }),
                            ..Default::default()
                        },
                    )],
                ))
        });
        let grid = div()
            .relative()
            .w_full()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(div().absolute().size_full().child(measure))
            // PTY content is sized by the viewport, never the other way round.
            .child(
                div()
                    .absolute()
                    .size_full()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .children(preedit)
            .children(normal_badge)
            .children(status.map(|status| {
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .px_2()
                    .bg(colors.element_background)
                    .text_color(foreground.opacity(0.8))
                    .child(status)
            }));

        div()
            .id("rho-terminal")
            .track_focus(&self.focus_handle)
            .key_context(if self.raw {
                "RhoTerminal"
            } else {
                "RhoTerminalNormal"
            })
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::enter_normal_mode))
            .on_action(cx.listener(Self::enter_raw_mode))
            .on_action(
                cx.listener(|this, _: &crate::TerminalScrollLineDown, _, cx| {
                    this.scroll_lines(-1, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::TerminalScrollLineUp, _, cx| {
                this.scroll_lines(1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::TerminalScrollHalfPageDown, _, cx| {
                    this.scroll_lines(-this.half_page(cx), cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::TerminalScrollHalfPageUp, _, cx| {
                    this.scroll_lines(this.half_page(cx), cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::TerminalScrollTop, _, cx| {
                this.scroll_lines(isize::MAX / 2, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::TerminalScrollBottom, _, cx| {
                this.scroll_lines(isize::MIN / 2, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.terminal_background)
            .font_family(font.family.clone())
            .text_size(font_size)
            .line_height(line_height)
            .flex()
            .flex_col()
            .child(grid)
            .when(touch, |el| el.child(strip))
    }
}

struct Palette<'a> {
    foreground: Hsla,
    background: Hsla,
    colors: &'a theme::ThemeColors,
}

impl Palette<'_> {
    fn fg(&self, color: TermColor) -> Hsla {
        match color {
            TermColor::Foreground => self.foreground,
            TermColor::Background => self.background,
            TermColor::Indexed(index) => terminal_indexed_color(index, self.colors),
            TermColor::Rgb(r, g, b) => terminal_rgb_color(r, g, b),
        }
    }

    /// `None` for the default background so cells inherit the surface color.
    fn bg(&self, color: TermColor) -> Option<Hsla> {
        match color {
            TermColor::Background => None,
            other => Some(self.fg(other)),
        }
    }
}

pub fn terminal_indexed_color(index: u8, colors: &theme::ThemeColors) -> Hsla {
    let named: gpui::Color = match index {
        0 => colors.terminal_ansi_black,
        1 => colors.terminal_ansi_red,
        2 => colors.terminal_ansi_green,
        3 => colors.terminal_ansi_yellow,
        4 => colors.terminal_ansi_blue,
        5 => colors.terminal_ansi_magenta,
        6 => colors.terminal_ansi_cyan,
        7 => colors.terminal_ansi_white,
        8 => colors.terminal_ansi_bright_black,
        9 => colors.terminal_ansi_bright_red,
        10 => colors.terminal_ansi_bright_green,
        11 => colors.terminal_ansi_bright_yellow,
        12 => colors.terminal_ansi_bright_blue,
        13 => colors.terminal_ansi_bright_magenta,
        14 => colors.terminal_ansi_bright_cyan,
        15 => colors.terminal_ansi_bright_white,
        // Preserve xterm's 6×6×6 coordinates, but derive the cube from the
        // theme's bright ANSI corners instead of bypassing the theme with
        // fixed sRGB values.
        16..=231 => {
            let index = index - 16;
            let corners = [
                colors.terminal_ansi_black,
                colors.terminal_ansi_bright_red,
                colors.terminal_ansi_bright_green,
                colors.terminal_ansi_bright_yellow,
                colors.terminal_ansi_bright_blue,
                colors.terminal_ansi_bright_magenta,
                colors.terminal_ansi_bright_cyan,
                colors.terminal_ansi_bright_white,
            ]
            .map(color_to_rgba);
            return cube_color(corners, index / 36, index / 6 % 6, index % 6).into();
        }
        // Keep grayscale theme-relative too, with bright black as its middle
        // anchor rather than a fixed xterm gray.
        232..=255 => {
            let t = f32::from(index - 232) / 23.0;
            let black = color_to_rgba(colors.terminal_ansi_black);
            let gray = color_to_rgba(colors.terminal_ansi_bright_black);
            let white = color_to_rgba(colors.terminal_ansi_bright_white);
            return grayscale_color(black, gray, white, t).into();
        }
    };
    named.into()
}

fn color_to_rgba(color: gpui::Color) -> gpui::Rgba {
    let color: Hsla = color.into();
    color.into()
}

fn cube_color(corners: [gpui::Rgba; 8], red: u8, green: u8, blue: u8) -> gpui::Rgba {
    let component = |value: u8| {
        if value == 0 {
            0.0
        } else {
            f32::from(value * 40 + 55) / 255.0
        }
    };
    let red = component(red);
    let green = component(green);
    let blue = component(blue);
    let black_red = mix_rgba(corners[0], corners[1], red);
    let green_yellow = mix_rgba(corners[2], corners[3], red);
    let blue_magenta = mix_rgba(corners[4], corners[5], red);
    let cyan_white = mix_rgba(corners[6], corners[7], red);
    let dark = mix_rgba(black_red, green_yellow, green);
    let light = mix_rgba(blue_magenta, cyan_white, green);
    mix_rgba(dark, light, blue)
}

fn mix_rgba(from: gpui::Rgba, to: gpui::Rgba, amount: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: from.r + (to.r - from.r) * amount,
        g: from.g + (to.g - from.g) * amount,
        b: from.b + (to.b - from.b) * amount,
        a: from.a + (to.a - from.a) * amount,
    }
}

fn grayscale_color(
    black: gpui::Rgba,
    gray: gpui::Rgba,
    white: gpui::Rgba,
    amount: f32,
) -> gpui::Rgba {
    if amount <= 0.5 {
        mix_rgba(black, gray, amount * 2.0)
    } else {
        mix_rgba(gray, white, (amount - 0.5) * 2.0)
    }
}

pub fn terminal_rgb_color(r: u8, g: u8, b: u8) -> Hsla {
    gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

fn cell_highlight(cell: &TermCell, palette: &Palette<'_>) -> HighlightStyle {
    let mut fg = palette.fg(cell.fg);
    let mut bg = palette.bg(cell.bg);
    if cell.flags & TermCellFlags::INVERSE != 0 {
        let old_fg = fg;
        fg = bg.unwrap_or(palette.background);
        bg = Some(old_fg);
    }
    if cell.flags & TermCellFlags::HIDDEN != 0 {
        fg = bg.unwrap_or(palette.background);
    }
    let mut style = HighlightStyle {
        color: Some(fg),
        background_color: bg,
        ..Default::default()
    };
    if cell.flags & TermCellFlags::BOLD != 0 {
        style.font_weight = Some(gpui::FontWeight::BOLD);
    }
    if cell.flags & TermCellFlags::ITALIC != 0 {
        style.font_style = Some(gpui::FontStyle::Italic);
    }
    if cell.flags & TermCellFlags::DIM != 0 {
        style.fade_out = Some(0.3);
    }
    if cell.flags & TermCellFlags::UNDERLINE != 0 {
        style.underline = Some(gpui::UnderlineStyle {
            thickness: px(1.0),
            color: Some(fg),
            wavy: false,
        });
    }
    if cell.flags & TermCellFlags::STRIKEOUT != 0 {
        style.strikethrough = Some(gpui::StrikethroughStyle {
            thickness: px(1.0),
            color: Some(fg),
        });
    }
    style
}

fn row_element(
    row: &TermRow,
    cursor_col: Option<u16>,
    raw: bool,
    text_style: &TextStyle,
    palette: &Palette<'_>,
) -> AnyElement {
    // Normal mode dims the cursor: the keyboard is rho's, not the pty's.
    let cursor_bg = if raw {
        palette.foreground
    } else {
        palette.foreground.opacity(0.5)
    };
    let mut text = String::new();
    let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut push_run = |range: Range<usize>, style: HighlightStyle| match runs.last_mut() {
        Some((last, prev)) if last.end == range.start && *prev == style => last.end = range.end,
        _ => runs.push((range, style)),
    };
    for (column, cell) in row.cells.iter().enumerate() {
        if cell.flags & TermCellFlags::WIDE_SPACER != 0 {
            continue;
        }
        let start = text.len();
        text.push(cell.c);
        if let Some(extra) = &cell.extra {
            text.push_str(extra);
        }
        let mut style = cell_highlight(cell, palette);
        let under_cursor = cursor_col.is_some_and(|col| {
            let col = usize::from(col);
            col == column || (col == column + 1 && cell.flags & TermCellFlags::WIDE != 0)
        });
        if under_cursor {
            style.color = Some(palette.background);
            style.background_color = Some(cursor_bg);
        }
        push_run(start..text.len(), style);
    }
    // The cursor can sit past the trimmed row end; pad up to it.
    if let Some(col) = cursor_col {
        let col = usize::from(col);
        if col >= row.cells.len() {
            for _ in row.cells.len()..col {
                text.push(' ');
            }
            let start = text.len();
            text.push(' ');
            push_run(
                start..text.len(),
                HighlightStyle {
                    color: Some(palette.background),
                    background_color: Some(cursor_bg),
                    ..Default::default()
                },
            );
        }
    }
    if text.is_empty() {
        // Keep empty rows one line tall.
        text.push(' ');
    }
    StyledText::new(text)
        .with_default_highlights(text_style, runs)
        .into_any_element()
}

/// Whether the agent host will write PTY bytes for this keystroke — the
/// client-side mirror of the encoder's coverage, deciding whether to stop
/// propagation (a swallowed key must really be consumed).
fn probably_produces_bytes(ks: &TermKeystroke) -> bool {
    if ks.key_char.is_some() {
        return true;
    }
    matches!(
        ks.key.as_str(),
        "tab"
            | "escape"
            | "enter"
            | "backspace"
            | "space"
            | "home"
            | "end"
            | "up"
            | "down"
            | "left"
            | "right"
            | "back"
            | "insert"
            | "delete"
            | "pageup"
            | "pagedown"
    ) || (ks.key.len() >= 2 && ks.key.starts_with('f') && ks.key[1..].parse::<u8>().is_ok())
        || ((ks.ctrl || ks.alt) && ks.key.is_ascii() && ks.key.len() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<TerminalModel>,
        futures_mpsc::Receiver<TermClientFrame>,
        futures_mpsc::Sender<anyhow::Result<TermServerFrame>>,
    ) {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (input, received) = futures_mpsc::channel(64);
        let (server, frames) = futures_mpsc::channel(8);
        // The tests observe the exact view-to-host frames, without a live PTY.
        // A real channel task supplies the ownership contract of TerminalChannel.
        let (_, _, transport) = rho_rpc::Stream::new(tokio::io::empty(), tokio::io::sink())
            .into_channel::<TermClientFrame, TermServerFrame>(rho_rpc::ChannelConfig {
                tx_limit: 1024,
                rx_limit: 1024,
                tx_capacity: 1,
                rx_capacity: 1,
            })
            .into_parts();
        let model = cx.new(|cx| {
            TerminalModel::new(
                TerminalChannel {
                    terminal_id: 1,
                    frames,
                    input,
                    transport,
                },
                cx,
            )
        });
        (model, received, server)
    }

    #[gpui::test]
    fn ime_preedit_never_executes_and_commit_uses_utf8_once(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _runtime = runtime.enter();
        let (model, mut received, _server) = test_model(cx);
        let window = cx.add_window(|_, cx| TerminalView::new(model, cx));
        window
            .update(cx, |view, window, cx| {
                view.replace_and_mark_text_in_range(None, "A😀é", Some(1..3), window, cx);
                assert_eq!(view.marked_text_range(window, cx), Some(0..4));
                assert_eq!(
                    view.selected_text_range(false, window, cx).unwrap().range,
                    1..3
                );
                assert!(received.try_recv().is_err());
                view.replace_and_mark_text_in_range(Some(0..4), "中文", None, window, cx);
                assert_eq!(view.marked_text_range(window, cx), Some(0..2));
                assert!(received.try_recv().is_err());
                // Enter while composition is active must not run the half-written command.
                view.key_down(
                    &KeyDownEvent {
                        keystroke: gpui::Keystroke::parse("enter").unwrap(),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                assert!(received.try_recv().is_err());
                view.replace_text_in_range(Some(0..2), "中文😀", window, cx);
                match received.try_recv().unwrap() {
                    TermClientFrame::Input(bytes) => assert_eq!(bytes, "中文😀".as_bytes()),
                    other => panic!("expected committed UTF-8 input, got {other:?}"),
                }
                assert!(received.try_recv().is_err());
                assert!(view.marked_text_range(window, cx).is_none());
                view.replace_and_mark_text_in_range(None, "not committed", None, window, cx);
                view.unmark_text(window, cx);
                assert!(received.try_recv().is_err());
                view.enter_normal_mode(&crate::TerminalNormalMode, window, cx);
                assert!(!view.accepts_text_input(window, cx));
                assert!(view.selected_text_range(false, window, cx).is_none());
                view.replace_text_in_range(None, "must not execute", window, cx);
                assert!(received.try_recv().is_err());
            })
            .unwrap();
    }

    #[gpui::test]
    fn hardware_text_commits_once_while_control_keys_keep_protocol(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _runtime = runtime.enter();
        let (model, mut received, _server) = test_model(cx);
        let window = cx.add_window(|window, cx| {
            let view = TerminalView::new(model, cx);
            window.focus(&view.focus_handle, cx);
            view
        });
        cx.refresh().unwrap();
        while received.try_recv().is_ok() {}
        // This uses GPUI's real key -> input-handler dispatch, not separate calls
        // that could miss accidental double forwarding in key_down.
        for key in ["a", "é"] {
            cx.dispatch_keystroke(window.into(), gpui::Keystroke::parse(key).unwrap());
        }
        let mut bytes = Vec::new();
        while let Ok(frame) = received.try_recv() {
            match frame {
                TermClientFrame::Input(input) => bytes.extend(input),
                TermClientFrame::Resize { .. } => {}
                other => panic!("printable text was forwarded as a duplicate key: {other:?}"),
            }
        }
        assert_eq!(bytes, "aé".as_bytes());
        for key in ["ctrl-c", "left", "enter", "tab"] {
            cx.simulate_keystrokes(window.into(), key);
            let mut sent = Vec::new();
            while let Ok(frame) = received.try_recv() {
                if let TermClientFrame::Keystroke(key) = frame {
                    sent.push(key);
                } else if !matches!(frame, TermClientFrame::Resize { .. }) {
                    panic!("control key used text path: {frame:?}");
                }
            }
            assert_eq!(sent.len(), 1, "{key}");
            assert_eq!(sent[0].key, key.strip_prefix("ctrl-").unwrap_or(key));
            assert_eq!(sent[0].ctrl, key.starts_with("ctrl-"));
        }
    }

    #[gpui::test]
    fn touch_keys_reserve_grid_height_and_send_direct_keys(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _runtime = runtime.enter();
        let (model, mut received, _server) = test_model(cx);
        let window = cx.open_window(gpui::size(px(390.), px(500.)), |window, cx| {
            let view = TerminalView::new(model, cx);
            window.focus(&view.focus_handle, cx);
            view
        });
        cx.draw_window(window.into());
        let desktop_rows = std::iter::from_fn(|| received.try_recv().ok())
            .find_map(|frame| match frame {
                TermClientFrame::Resize { rows, .. } => Some(rows),
                _ => None,
            })
            .unwrap();
        cx.set_global(rho_window::TouchMode(true));
        cx.draw_window(window.into());
        let phone_rows = std::iter::from_fn(|| received.try_recv().ok())
            .find_map(|frame| match frame {
                TermClientFrame::Resize { rows, .. } => Some(rows),
                _ => None,
            })
            .unwrap();
        let row_height = cx.update(|cx| {
            let settings = ThemeSettings::get_global(cx);
            f32::from(settings.buffer_font_size(cx)) * settings.buffer_line_height.value()
        });
        assert_eq!(desktop_rows, (500. / row_height).floor() as u16);
        assert_eq!(phone_rows, ((500. - 48.) / row_height).floor() as u16);
        assert!(phone_rows < desktop_rows);

        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(Point::new(px(24.), px(476.)), gpui::Modifiers::default());
        let frame = received.try_recv().unwrap();
        assert!(
            matches!(frame, TermClientFrame::Keystroke(TermKeystroke { ref key, ctrl: false, .. }) if key == "escape")
        );
        assert!(received.try_recv().is_err());

        // Touch controls return normal mode to raw input and refocus the grid.
        window
            .update(&mut visual, |view, window, cx| {
                view.enter_normal_mode(&crate::TerminalNormalMode, window, cx);
            })
            .unwrap();
        visual.draw_window(window.into());
        visual.simulate_click(Point::new(px(72.), px(476.)), gpui::Modifiers::default());
        let frame = received.try_recv().unwrap();
        assert!(
            matches!(frame, TermClientFrame::Keystroke(TermKeystroke { ref key, .. }) if key == "tab")
        );
        window
            .update(&mut visual, |view, window, cx| {
                assert!(view.raw);
                assert!(view.focus_handle.is_focused(window));
                assert!(view.accepts_text_input(window, cx));
            })
            .unwrap();
    }

    fn rgba(red: f32, green: f32, blue: f32) -> gpui::Rgba {
        gpui::Rgba {
            r: red,
            g: green,
            b: blue,
            a: 1.0,
        }
    }

    fn assert_rgba_eq(actual: gpui::Rgba, expected: gpui::Rgba) {
        assert!((actual.r - expected.r).abs() < f32::EPSILON);
        assert!((actual.g - expected.g).abs() < f32::EPSILON);
        assert!((actual.b - expected.b).abs() < f32::EPSILON);
        assert!((actual.a - expected.a).abs() < f32::EPSILON);
    }

    #[test]
    fn extended_cube_uses_themed_color_corners() {
        let corners = [
            rgba(0.0, 0.0, 0.0),
            rgba(1.0, 0.0, 0.0),
            rgba(0.0, 1.0, 0.0),
            rgba(1.0, 1.0, 0.0),
            rgba(0.0, 0.0, 1.0),
            rgba(1.0, 0.0, 1.0),
            rgba(0.0, 1.0, 1.0),
            rgba(1.0, 1.0, 1.0),
        ];
        assert_rgba_eq(cube_color(corners, 0, 0, 0), corners[0]);
        assert_rgba_eq(cube_color(corners, 5, 0, 0), corners[1]);
        assert_rgba_eq(cube_color(corners, 0, 5, 0), corners[2]);
        assert_rgba_eq(cube_color(corners, 5, 5, 5), corners[7]);

        let level = 95.0 / 255.0;
        assert_rgba_eq(cube_color(corners, 1, 0, 0), rgba(level, 0.0, 0.0));
    }

    #[test]
    fn extended_grayscale_passes_through_theme_anchors() {
        let black = rgba(0.1, 0.2, 0.3);
        let gray = rgba(0.4, 0.5, 0.6);
        let white = rgba(0.7, 0.8, 0.9);
        assert_rgba_eq(grayscale_color(black, gray, white, 0.0), black);
        assert_rgba_eq(grayscale_color(black, gray, white, 0.5), gray);
        assert_rgba_eq(grayscale_color(black, gray, white, 1.0), white);
    }
}
