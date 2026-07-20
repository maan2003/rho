//! A daemon-owned terminal shown as a surface.
//!
//! [`TerminalModel`] is the buffer role: it owns the wire display state
//! ([`WireScreen`]), the input stream, and the read task — shared by every
//! pane showing the terminal. [`TerminalView`] is the window role: one per
//! pane, with its own focus, scrollback offset, and mode.
//!
//! Input is deliberately mode-free on the wire — keystrokes go to the
//! daemon as structured [`TermKeystroke`]s and are encoded against the
//! terminal's live modes there, so this side never tracks application
//! cursor keys, bracketed paste, or anything else stateful.
//!
//! Views have two modes, vim-style: **raw** (the default) forwards every
//! keystroke to the pty; **normal** (`ctrl-\ ctrl-n`, or `ctrl-shift-n`)
//! releases the keyboard back to rho — `:` opens the command line, the
//! space leader works, and j/k/ctrl-d/ctrl-u/gg/G browse scrollback.
//! `i`/`a`/`enter` return to raw.
//!
//! Only the focused view's size is sent to the pty (tmux `window-size
//! latest`): a terminal shown in differently-sized splits follows whichever
//! pane you're typing in instead of thrashing the pty with competing sizes.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollDelta,
    ScrollWheelEvent, Styled as _, StyledText, Subscription, TextStyle, Window, canvas, div, px,
};
use rho_ui_proto::term::{
    FrameApplied, ScrollbackItem, TermCell, TermCellFlags, TermClientFrame, TermColor,
    TermKeystroke, TermRow, TermServerFrame, WireScreen,
};
use settings::Settings as _;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;

use crate::connection::TerminalChannel;

/// Client-side scrollback retention; the daemon replays up to its own cap.
const SCROLLBACK_LIMIT: usize = 8192;

/// The shared terminal state: wire screen, input stream, and read task.
/// Panes hold views over it; the model outlives any of them.
pub struct TerminalModel {
    screen: WireScreen,
    input: futures_mpsc::UnboundedSender<TermClientFrame>,
    /// Monotonic count of lines appended to scrollback, so views can keep
    /// their place while history arrives (the ring length saturates).
    history_appended: u64,
    /// (cols, rows) last sent to the daemon; shared with the focused
    /// view's paint-time measurement so resizes go straight to the stream.
    sent_size: Rc<Cell<(u16, u16)>>,
    /// The stream ended without an `Exited` status (daemon or dial gone).
    disconnected: bool,
    _read_task: gpui::Task<()>,
}

impl TerminalModel {
    pub fn new(channel: TerminalChannel, cx: &mut Context<Self>) -> Self {
        let TerminalChannel {
            terminal_id: _,
            mut frames,
            input,
        } = channel;
        let read_task = cx.spawn(async move |this, cx| {
            while let Some(frame) = frames.next().await {
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
        let _ = self.input.unbounded_send(frame);
    }
}

/// One pane's view of a terminal: own focus, scroll offset, and mode.
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
            _model_changed: model_changed,
        }
    }

    pub fn model(&self) -> &Entity<TerminalModel> {
        &self.model
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.raw {
            return;
        }
        let ks = &event.keystroke;
        if ks.modifiers.platform {
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
        self.model
            .read(cx)
            .send(TermClientFrame::Keystroke(keystroke));
        cx.notify();
        if handled {
            cx.stop_propagation();
        }
    }

    fn paste(&mut self, _: &crate::TerminalPaste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
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

        // Size measurement happens at paint time: compare the pane bounds
        // to the cell metrics and tell the daemon when the grid changed.
        // Only the focused view drives the pty size (tmux `window-size
        // latest`) — competing sizes from other splits would thrash it.
        let model = self.model.read(cx);
        let sent_size = model.sent_size.clone();
        let input = model.input.clone();
        let grid_origin_px = self.grid_origin_px.clone();
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
                    let _ = input.unbounded_send(TermClientFrame::Resize {
                        cols: size.0,
                        rows: size.1,
                    });
                }
            },
            |_, _, _, _| {},
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
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.terminal_background)
            .font_family(font.family.clone())
            .text_size(font_size)
            .line_height(line_height)
            .child(div().absolute().size_full().child(measure))
            // The grid paints in an absolute layer: pty content is sized by
            // the pane, never the other way around, so a wide row can only
            // be clipped — it can't push the split.
            .child(
                div()
                    .absolute()
                    .size_full()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
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
            }))
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

    /// `None` for the default background so cells inherit the pane color.
    fn bg(&self, color: TermColor) -> Option<Hsla> {
        match color {
            TermColor::Background => None,
            other => Some(self.fg(other)),
        }
    }
}

pub(crate) fn terminal_indexed_color(index: u8, colors: &theme::ThemeColors) -> Hsla {
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
        // xterm 6×6×6 color cube.
        16..=231 => {
            let index = index - 16;
            let component = |value: u8| if value == 0 { 0 } else { value * 40 + 55 };
            return terminal_rgb_color(
                component(index / 36),
                component(index / 6 % 6),
                component(index % 6),
            );
        }
        // Grayscale ramp.
        232..=255 => {
            let level = (index - 232) * 10 + 8;
            return terminal_rgb_color(level, level, level);
        }
    };
    named.into()
}

pub(crate) fn terminal_rgb_color(r: u8, g: u8, b: u8) -> Hsla {
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

/// Whether the daemon will write PTY bytes for this keystroke — the
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
