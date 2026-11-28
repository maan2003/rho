//! The reading sheet a screen lays its words on (`design.sheet measure`),
//! and the page heading it draws in its own top-left corner.
//!
//! The window draws only the notch mask at the top centre and the global
//! status bar at the top right. Everything else, from the top edge down,
//! is the screen's: its heading sits left of the notch on the first
//! `TOP_LINE`, and its text sits below in a sheet `SHEET_COLUMNS` wide,
//! centred, with `READING_INSET` of air from the window edge when the
//! window is narrower. Text wraps at the prose measure, on the sheet's left
//! edge; the sheet's width beyond it holds what sits beside the text, such
//! as a thread's ages.
//!
//! The tokens here are the bundled dark appearance's (`product/design.md`).
//! Every screen of the notes, the chat and the agents lays out with this
//! one module; it lives in the lowest of those crates until the window
//! crate (`rho-window`) owns the sheet and its tokens.

use gpui::prelude::*;
use gpui::{
    App, Bounds, Entity, Font, Hsla, Pixels, TextRun, TextStyle, Window, WindowTextSystem, div,
    point, px,
};
use rho_frame::Layout;
use theme::ActiveTheme as _;

/// `design.sheet measure`.
pub const SHEET_COLUMNS: usize = rho_window::layout::SHEET_MAX_CH as usize;
/// `design.reading inset`.
pub const READING_INSET: Pixels = px(rho_window::layout::READING_INSET_PX);
/// `design.top line`.
pub const TOP_LINE: Pixels = px(rho_window::layout::TOP_ROW_PX);
/// The notch mask the window draws at the top centre.
pub const NOTCH: Pixels = px(rho_window::layout::NOTCH_MASK_PX);
/// `design.same band gap`.
pub const BAND_GAP: Pixels = px(rho_window::layout::BAND_GAP_PX);
/// `design.draft gutter stroke`: the editor gutter on a sheet, where the
/// user's `2 px` bar is drawn, with `10 px` of air before the first letter.
pub const GUTTER: Pixels = px(12.);
/// `design.top gap`: air between the top line and the sheet's first row.
pub const TOP_GAP: Pixels = px(rho_window::layout::TOP_GAP_PX);

/// The colour tokens of `product/design.md`, bundled dark appearance.
pub mod token {
    use gpui::{App, Color, Hsla};

    fn oklch(l: f32, c: f32, h: f32) -> Hsla {
        Color::oklch(l, c, h, 1.).into()
    }

    /// Every turn's words, the project's and the notes' alike, read or not.
    pub fn reading_text() -> Hsla {
        oklch(0.709, 0.012, 95.)
    }

    /// A page heading's title: a step brighter than the text, at its weight.
    pub fn title() -> Hsla {
        oklch(0.780, 0.012, 95.)
    }

    /// Context that can wait: read ages, settled things, hints.
    pub fn quiet() -> Hsla {
        oklch(0.560, 0.012, 110.)
    }

    /// Something to act on: an unread age, a push that waits.
    pub fn attention() -> Hsla {
        oklch(0.795, 0.145, 78.)
    }

    /// A step that did not finish.
    pub fn failed() -> Hsla {
        oklch(0.750, 0.145, 25.)
    }

    /// Links and short explanatory notices.
    pub fn answer() -> Hsla {
        oklch(0.780, 0.105, 230.)
    }

    /// The user's own words: their draft and their messages.
    pub fn marked() -> Hsla {
        oklch(0.720, 0.072, 185.)
    }

    /// The bar beside every line of the user's own words.
    pub fn marked_bar() -> Hsla {
        oklch(0.600, 0.085, 185.)
    }

    /// The same bar, as a transcript's gutter colour.
    pub fn marked_bar_in(_cx: &App) -> Hsla {
        marked_bar()
    }

    /// A crisp square boundary.
    pub fn seam() -> Hsla {
        oklch(0.320, 0.018, 150.)
    }

    /// A soft internal divider.
    pub fn soft_seam() -> Hsla {
        oklch(0.278, 0.016, 150.)
    }
}

/// What a page heading says: its title, the quiet context after it, and
/// at most one page marker, each in its own colour.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heading {
    pub title: String,
    pub context: Vec<String>,
    pub marker: Option<(String, Hsla)>,
}

impl Heading {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// Quiet words after the title, such as the project; an empty one is
    /// left out.
    pub fn context(mut self, words: impl Into<String>) -> Self {
        let words = words.into();
        if !words.trim().is_empty() {
            self.context.push(words);
        }
        self
    }

    pub fn marker(mut self, words: impl Into<String>, color: Hsla) -> Self {
        self.marker = Some((words.into(), color));
        self
    }

    /// The heading as one line of text, for a place's row and the window's
    /// own title: the parts with `row air` between them.
    pub fn text(&self) -> String {
        std::iter::once(self.title.as_str())
            .chain(self.context.iter().map(String::as_str))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// The width of one column of `text`.
pub fn column_width(text: &TextStyle, window: &Window) -> Pixels {
    let size = text.font_size.to_pixels(window.rem_size());
    let font = window.text_system().resolve_font(&text.font());
    window
        .text_system()
        .ch_advance(font, size)
        .unwrap_or(size * 0.6)
}

/// The sheet's width for `text`: the measure, the gutter where the bars
/// are drawn, and a column for the cursor at a line's end.
pub fn sheet_width(text: &TextStyle, window: &Window) -> Pixels {
    column_width(text, window) * (SHEET_COLUMNS as f32 + 1.) + GUTTER
}

/// `design.prose measure`: where text wraps, from the sheet's left edge
/// of the words.
pub fn prose_width(text: &TextStyle, window: &Window) -> Pixels {
    column_width(text, window) * rho_window::layout::PROSE_MAX_CH
}

/// The width of an editor whose words wrap at the prose measure: the
/// same gutter and cursor column as the whole sheet.
pub fn prose_editor_width(text: &TextStyle, window: &Window) -> Pixels {
    prose_width(text, window) + column_width(text, window) + GUTTER
}

/// Gives a sheet's editor the sheet's gutter: the bar at the sheet's left
/// edge, and the words after its air.
pub fn take_gutter(editor: &mut editor::Editor, cx: &mut gpui::Context<editor::Editor>) {
    editor.set_compact_gutter_width(Some(GUTTER), cx);
}

/// How wide text is, in columns of the buffer font (`ch`, the advance of
/// `0`). The font is part of the way to monospace, so a column count of
/// characters is not a width: a word of narrow letters is shorter than one
/// of wide ones. Right-aligned words are padded by measured width.
#[derive(Clone, Debug, PartialEq)]
pub enum Measure {
    /// Every character one column wide, as in a test without fonts.
    Columns,
    Font {
        font: Font,
        size: Pixels,
        column: Pixels,
    },
}

impl Measure {
    pub fn of(text: &TextStyle, window: &Window) -> Self {
        let font = text.font();
        let size = text.font_size.to_pixels(window.rem_size());
        let column = window
            .text_system()
            .layout_line("0", size, &[text.to_run(1)], None)
            .width;
        Self::Font { font, size, column }
    }

    /// How many columns of text a sheet `width` wide holds: the gutter,
    /// the cursor's column at a line's end and half a column of slack
    /// taken off, so that a line padded to it never wraps.
    pub fn columns_in(&self, width: Pixels) -> usize {
        match self {
            Self::Columns => SHEET_COLUMNS,
            Self::Font { column, .. } => {
                ((width - GUTTER) / *column - 1.5).floor().max(1.) as usize
            }
        }
    }

    /// The width of `text`, in columns, as the line is shaped: a glyph's
    /// own advance is not enough, since the font varies its weight and
    /// kerns its pairs, and only shaping applies them.
    pub fn width(&self, text: &str, cx: &App) -> f32 {
        match self {
            Self::Columns => text.chars().count() as f32,
            Self::Font { font, size, column } => {
                let run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    ..TextRun::default()
                };
                let width = WindowTextSystem::new(cx.text_system().clone())
                    .layout_line(text, *size, &[run], None)
                    .width;
                width / *column
            }
        }
    }

    /// How many spaces after `left` put the end of `right` at `columns`.
    pub fn pad(&self, left: &str, right: &str, columns: usize, cx: &App) -> usize {
        pad(&|text| self.width(text, cx), left, right, columns)
    }
}

/// How many spaces after `left` put the end of `right` at `columns`, when
/// `width` measures text in columns: two at least, `row air`, when the line
/// is too long for that.
pub fn pad(width: &dyn Fn(&str) -> f32, left: &str, right: &str, columns: usize) -> usize {
    let room = columns as f32 - width(left) - width(right);
    let space = width(" ").max(0.1);
    ((room / space).round().max(0.) as usize).max(2)
}

/// A screen laid out as the design says: on a laptop the heading left of
/// the notch on the top line, and `body` in the centred sheet below it.
/// On a phone the window's own bar names the page, and the body has the
/// whole width.
pub fn page(
    heading: &Heading,
    text: &TextStyle,
    body: impl IntoElement,
    window: &Window,
    cx: &App,
) -> gpui::Div {
    lay_out(heading, text, body, None, window, cx)
}

/// A [`page`] whose `editor` sticks its draft to the top edge
/// (`window.Edge-stuck excerpt`). While the draft is stuck, the laptop's
/// whole top zone is one canopy in the draft's own tone: from the window's
/// top edge, across the heading, the top gap and the width of the window,
/// down to the draft's lower edge. The window's status right of the notch
/// lies on it too. Its tone alone sets it apart from the history below.
pub fn page_with_canopy(
    heading: &Heading,
    text: &TextStyle,
    body: impl IntoElement,
    editor: &Entity<editor::Editor>,
    window: &Window,
    cx: &App,
) -> gpui::Div {
    lay_out(heading, text, body, Some(editor.clone()), window, cx)
}

fn lay_out(
    heading: &Heading,
    text: &TextStyle,
    body: impl IntoElement,
    canopy: Option<Entity<editor::Editor>>,
    window: &Window,
    cx: &App,
) -> gpui::Div {
    let background = cx.theme().colors().editor_background;
    let root = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(background)
        .font_family(text.font_family.clone())
        .text_size(text.font_size)
        .line_height(text.line_height);
    if Layout::of(window.viewport_size().width) == Layout::Phone {
        return root.child(div().flex_1().min_h_0().w_full().child(body));
    }
    let viewport = window.viewport_size().width;
    let sheet = sheet_width(text, window);
    // The heading starts where the sheet's words do, after the gutter, and
    // is cut a band gap before the notch.
    let left = rho_window::layout::sheet_left(viewport, sheet) + GUTTER;
    let ear = rho_window::layout::heading_room(left, rho_window::layout::Notch::of(viewport));
    // Painted under the heading and the sheet. The editor lays the draft
    // out before anything paints, so the canopy ends where the draft does
    // in this same frame.
    let canopy = canopy.map(|editor| {
        gpui::canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                if let Some(stuck) = editor.read(cx).stuck_excerpt_bounds() {
                    window.paint_quad(gpui::fill(
                        Bounds::from_corners(bounds.origin, point(bounds.right(), stuck.bottom())),
                        rho_window::style::command_strip_background(cx),
                    ));
                }
            },
        )
        .absolute()
        .size_full()
    });
    root.children(canopy)
        .child(
            div()
                .h(TOP_LINE)
                .flex_none()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .pl(left)
                .child(top_line(heading).max_w(ear)),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_row()
                .justify_center()
                .pt(TOP_GAP)
                .px(READING_INSET)
                .child(div().h_full().w_full().max_w(sheet).child(body)),
        )
}

/// The heading's words: the title in `title` at the text's own weight,
/// the context quiet, the marker in its colour. The title is cut first.
pub fn top_line(heading: &Heading) -> gpui::Div {
    let mut line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(BAND_GAP)
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(token::title())
                .child(heading.title.clone()),
        );
    for words in &heading.context {
        line = line.child(
            div()
                .flex_none()
                .text_color(token::quiet())
                .child(words.clone()),
        );
    }
    if let Some((words, color)) = &heading.marker {
        line = line.child(div().flex_none().text_color(*color).child(words.clone()));
    }
    line
}

/// How far the lines a list item or a quote wraps to are indented, in
/// columns: under the words after the marker (`- `, `* `, `+ `, `12. `)
/// or after the quote's `> `, past any indent before it. `None` for a line
/// with no marker, whose wrapped lines keep its own indent.
pub fn hanging_indent(line: &str) -> Option<u32> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let marker = ["- ", "* ", "+ ", "> "]
        .into_iter()
        .find(|marker| rest.starts_with(marker))
        .map(str::len)
        .or_else(|| {
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            (digits > 0 && rest[digits..].starts_with(". ")).then_some(digits + 2)
        })?;
    u32::try_from(indent + marker).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_hangs_its_wrapped_lines_under_its_words() {
        assert_eq!(hanging_indent("- one two"), Some(2));
        assert_eq!(hanging_indent("12. one two"), Some(4));
        assert_eq!(hanging_indent("> quoted"), Some(2));
        // A nested item hangs past its own indent too.
        assert_eq!(hanging_indent("    * nested"), Some(6));
        // Prose, a bare dash, and a number without its dot keep their indent.
        assert_eq!(hanging_indent("plain words"), None);
        assert_eq!(hanging_indent("-not a list"), None);
        assert_eq!(hanging_indent("2026 was"), None);
        assert_eq!(hanging_indent("  indented"), None);
    }

    #[test]
    fn a_heading_reads_as_its_parts_with_row_air() {
        let heading = Heading::new("Flaky clock test")
            .context("tide@lab")
            .context("");
        assert_eq!(heading.text(), "Flaky clock test  tide@lab");
    }
}
