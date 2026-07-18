//! A daemon-owned terminal shown as a surface: renders the wire display
//! state ([`WireScreen`]) and forwards input.
//!
//! The view is deliberately mode-free — keystrokes go to the daemon as
//! structured [`TermKeystroke`]s and are encoded against the terminal's live
//! modes there, so this side never tracks application cursor keys, bracketed
//! paste, or anything else stateful. Scrollback lives client-side in the
//! [`WireScreen`] ring; the wheel scrolls over it, any keystroke snaps back
//! to the live screen.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::{
    AnyElement, Context, FocusHandle, Focusable, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollDelta, ScrollWheelEvent,
    Styled as _, StyledText, TextStyle, Window, canvas, div, px,
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

pub struct TerminalView {
    screen: WireScreen,
    input: futures_mpsc::UnboundedSender<TermClientFrame>,
    focus_handle: FocusHandle,
    /// Whole lines scrolled up into history; 0 pins to the live screen.
    scroll_offset: usize,
    /// (cols, rows) last sent to the daemon; shared with the paint-time
    /// measurement so resizes go straight to the stream without an update.
    sent_size: Rc<Cell<(u16, u16)>>,
    /// One terminal line in pixels, for wheel-delta conversion.
    line_height_px: Rc<Cell<f32>>,
    /// The stream ended without an `Exited` status (daemon or dial gone).
    disconnected: bool,
    _read_task: gpui::Task<()>,
}

impl TerminalView {
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
                    .update(cx, |view: &mut TerminalView, cx| view.apply(frame, cx))
                    .is_err()
                    || exited
                {
                    return;
                }
            }
            let _ = this.update(cx, |view, cx| {
                view.disconnected = true;
                cx.notify();
            });
        });
        Self {
            screen: WireScreen::new(SCROLLBACK_LIMIT),
            input,
            focus_handle: cx.focus_handle(),
            scroll_offset: 0,
            sent_size: Rc::new(Cell::new((0, 0))),
            line_height_px: Rc::new(Cell::new(16.0)),
            disconnected: false,
            _read_task: read_task,
        }
    }

    fn apply(&mut self, frame: TermServerFrame, cx: &mut Context<Self>) {
        let before = self.screen.scrollback.len();
        let applied = self.screen.apply(frame);
        if matches!(applied, FrameApplied::History) && self.scroll_offset > 0 {
            // Keep the viewed lines fixed while new history arrives below.
            self.scroll_offset += self.screen.scrollback.len() - before;
        }
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
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
            cx.notify();
        }
        let _ = self
            .input
            .unbounded_send(TermClientFrame::Keystroke(keystroke));
        if handled {
            cx.stop_propagation();
        }
    }

    fn paste(&mut self, _: &crate::TerminalPaste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.scroll_offset = 0;
            let _ = self.input.unbounded_send(TermClientFrame::Paste(text));
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / self.line_height_px.get().max(1.0),
        };
        let offset = self.scroll_offset as f32 + lines * 3.0;
        let clamped = offset
            .round()
            .clamp(0.0, self.screen.scrollback.len() as f32) as usize;
        if clamped != self.scroll_offset {
            self.scroll_offset = clamped;
            cx.notify();
        }
    }

    /// The window of lines the viewport shows: the live screen, shifted up
    /// into scrollback by `scroll_offset`.
    fn visible_lines(&self) -> Vec<VisibleLine<'_>> {
        let height = self.screen.rows.len().max(1);
        let scrollback = &self.screen.scrollback;
        let total = scrollback.len() + self.screen.rows.len();
        let offset = self.scroll_offset.min(scrollback.len());
        let end = total - offset;
        let start = end.saturating_sub(height);
        let cursor = &self.screen.cursor;
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
                        row: &self.screen.rows[row_index],
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

        // Size measurement happens at paint time: compare the pane bounds to
        // the cell metrics and tell the daemon when the grid size changed.
        let sent_size = self.sent_size.clone();
        let input = self.input.clone();
        let measure = canvas(
            move |bounds, _window, _cx| {
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

        let focused = self.focus_handle.is_focused(window);
        let palette = Palette {
            foreground,
            background,
            colors: &colors,
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for line in self.visible_lines() {
            rows.push(match line {
                VisibleLine::Row { row, cursor } => {
                    let cursor = if focused { cursor } else { None };
                    row_element(row, cursor, &text_style, &palette)
                }
                VisibleLine::Gap(lost) => div()
                    .h(line_height)
                    .child(format!("· · · {lost} lines lost · · ·"))
                    .text_color(foreground.opacity(0.5))
                    .into_any_element(),
            });
        }

        let status = if let Some(status) = self.screen.exited {
            Some(match status {
                Some(code) => format!("terminal exited ({code})"),
                None => "terminal exited".to_owned(),
            })
        } else if self.disconnected {
            Some("terminal disconnected".to_owned())
        } else {
            None
        };

        div()
            .id("rho-terminal")
            .track_focus(&self.focus_handle)
            .key_context("RhoTerminal")
            .on_action(cx.listener(Self::paste))
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
            .child(div().flex().flex_col().children(rows))
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
            TermColor::Indexed(index) => self.indexed(index),
            TermColor::Rgb(r, g, b) => rgb8(r, g, b),
        }
    }

    /// `None` for the default background so cells inherit the pane color.
    fn bg(&self, color: TermColor) -> Option<Hsla> {
        match color {
            TermColor::Background => None,
            other => Some(self.fg(other)),
        }
    }

    fn indexed(&self, index: u8) -> Hsla {
        let named: gpui::Color = match index {
            0 => self.colors.terminal_ansi_black,
            1 => self.colors.terminal_ansi_red,
            2 => self.colors.terminal_ansi_green,
            3 => self.colors.terminal_ansi_yellow,
            4 => self.colors.terminal_ansi_blue,
            5 => self.colors.terminal_ansi_magenta,
            6 => self.colors.terminal_ansi_cyan,
            7 => self.colors.terminal_ansi_white,
            8 => self.colors.terminal_ansi_bright_black,
            9 => self.colors.terminal_ansi_bright_red,
            10 => self.colors.terminal_ansi_bright_green,
            11 => self.colors.terminal_ansi_bright_yellow,
            12 => self.colors.terminal_ansi_bright_blue,
            13 => self.colors.terminal_ansi_bright_magenta,
            14 => self.colors.terminal_ansi_bright_cyan,
            15 => self.colors.terminal_ansi_bright_white,
            // xterm 6×6×6 color cube.
            16..=231 => {
                let index = index - 16;
                let component = |value: u8| if value == 0 { 0 } else { value * 40 + 55 };
                return rgb8(
                    component(index / 36),
                    component(index / 6 % 6),
                    component(index % 6),
                );
            }
            // Grayscale ramp.
            232..=255 => {
                let level = (index - 232) * 10 + 8;
                return rgb8(level, level, level);
            }
        };
        named.into()
    }
}

fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
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
    text_style: &TextStyle,
    palette: &Palette<'_>,
) -> AnyElement {
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
            style.background_color = Some(palette.foreground);
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
                    background_color: Some(palette.foreground),
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
