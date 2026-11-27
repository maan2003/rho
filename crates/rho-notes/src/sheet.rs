//! The reading sheet a screen lays its words on (`design.sheet measure`),
//! and the page heading it draws in its own top-left corner.
//!
//! The window draws only the notch mask at the top centre and the global
//! status bar at the top right. Everything else, from the top edge down,
//! is the screen's: its heading sits left of the notch on the first
//! `TOP_LINE`, and its text sits below in a sheet `SHEET_COLUMNS` wide,
//! centred, with `READING_INSET` of air from the window edge when the
//! window is narrower. Prose wraps at `PROSE_COLUMNS` on the same left edge
//! as the wide blocks (code, diffs, tables, push requests) that use the
//! whole sheet.
//!
//! The tokens here are the bundled dark appearance's (`product/design.md`).
//! Every screen of the notes, the chat and the agents lays out with this
//! one module; it lives in the lowest of those crates until the window
//! crate (`rho-window`) owns the sheet and its tokens.

use gpui::prelude::*;
use gpui::{
    App, Font, FontWeight, Hsla, Pixels, TextRun, TextStyle, Window, WindowTextSystem, div, px,
};
use rho_frame::Layout;
use theme::ActiveTheme as _;

/// `design.sheet measure`.
pub const SHEET_COLUMNS: usize = rho_window::layout::SHEET_MAX_CH as usize;
/// `design.prose measure`.
pub const PROSE_COLUMNS: usize = rho_window::layout::PROSE_MAX_CH as usize;
/// `design.reading inset`.
pub const READING_INSET: Pixels = px(rho_window::layout::READING_INSET_PX);
/// `design.top line`.
pub const TOP_LINE: Pixels = px(rho_window::layout::TOP_ROW_PX);
/// The notch mask the window draws at the top centre.
pub const NOTCH: Pixels = px(rho_window::layout::NOTCH_MASK_PX);
/// `design.same band gap`.
pub const BAND_GAP: Pixels = px(12.);

/// The colour tokens of `product/design.md`, bundled dark appearance.
pub mod token {
    use gpui::{App, Color, Hsla};

    fn oklch(l: f32, c: f32, h: f32) -> Hsla {
        Color::oklch(l, c, h, 1.).into()
    }

    /// The newest unread turn and the page title.
    pub fn unread_text() -> Hsla {
        oklch(0.895, 0.008, 95.)
    }

    /// Primary readable body text.
    pub fn reading_text() -> Hsla {
        oklch(0.835, 0.010, 95.)
    }

    /// Body text the reader has already read.
    pub fn read_text() -> Hsla {
        oklch(0.735, 0.010, 95.)
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
        oklch(0.810, 0.065, 185.)
    }

    /// The bar beside every line of the user's own words.
    pub fn marked_bar() -> Hsla {
        oklch(0.680, 0.095, 185.)
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

/// The sheet's width for `text`: the measure, the editor's half-column
/// gutter where the bars are drawn, and a column for the cursor at a line's
/// end.
pub fn sheet_width(text: &TextStyle, window: &Window) -> Pixels {
    column_width(text, window) * (SHEET_COLUMNS as f32 + 1.5)
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

    /// How many columns of text a sheet `width` wide holds: the half-column
    /// gutter, the cursor's column at a line's end and half a column of
    /// slack taken off, so that a line padded to it never wraps.
    pub fn columns_in(&self, width: Pixels) -> usize {
        match self {
            Self::Columns => SHEET_COLUMNS,
            Self::Font { column, .. } => (width / *column - 2.).floor().max(1.) as usize,
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
    // The heading has all the top line left of the notch: from the reading
    // inset to a band gap before the notch. Under the sheet's left edge it
    // would cut a long subject to a few letters on a laptop.
    let ear = (viewport / 2. - NOTCH / 2. - BAND_GAP - READING_INSET).max(px(0.));
    root.child(
        div()
            .h(TOP_LINE)
            .flex_none()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .pl(READING_INSET)
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
            .px(READING_INSET)
            .child(div().h_full().w_full().max_w(sheet).child(body)),
    )
}

/// The heading's words: the title in `unread text` with emphasis, the
/// context quiet, the marker in its colour. The title is cut first.
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
                .font_weight(FontWeight(680.))
                .text_color(token::unread_text())
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

/// One line of prose wrapped at `width` columns, at spaces: a word longer
/// than the line, such as a link, keeps its own line. A list item or a
/// quote keeps its marker's indent, or the quote's `> `, on every line it
/// wraps to, so the wrapped text reads as one item.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_owned()];
    }
    let (lead, hang) = hanging(line);
    let mut out = Vec::new();
    let mut current = lead.to_owned();
    let mut current_len = lead.chars().count();
    let mut empty = true;
    for word in line[lead.len()..].split(' ') {
        let len = word.chars().count();
        if !empty && current_len + 1 + len > width {
            out.push(std::mem::replace(&mut current, hang.clone()));
            current_len = hang.chars().count();
            empty = true;
        }
        if !empty {
            current.push(' ');
            current_len += 1;
        }
        current.push_str(word);
        current_len += len;
        empty = false;
    }
    out.push(current);
    out
}

/// The text a wrapped line starts with, and what each line it wraps to
/// starts with instead.
fn hanging(line: &str) -> (&str, String) {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    if rest.starts_with("> ") {
        let lead = &line[..indent + 2];
        return (lead, lead.to_owned());
    }
    let marker = ["- ", "* ", "+ "]
        .into_iter()
        .find(|marker| rest.starts_with(marker))
        .map(str::len)
        .or_else(|| {
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            (digits > 0 && rest[digits..].starts_with(". ")).then_some(digits + 2)
        });
    match marker {
        Some(marker) => (&line[..indent + marker], " ".repeat(indent + marker)),
        None => (&line[..indent], " ".repeat(indent)),
    }
}

/// A body's lines, each marked whether it is prose that wraps at the
/// prose measure or part of a wide block that takes the whole sheet: a
/// fenced block (code, a diff), an indented code line, or a table row.
pub fn blocks(body: &str) -> Vec<(&str, bool)> {
    let mut fenced = false;
    body.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
            let wide = fenced
                || fence
                || line.starts_with("    ")
                || line.starts_with('\t')
                || trimmed.starts_with('|');
            if fence {
                fenced = !fenced;
            }
            (line, !wide)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_wraps_at_spaces_and_keeps_its_markers() {
        let words = "alpha beta gamma delta";
        assert_eq!(wrap_line(words, 11), ["alpha beta", "gamma delta"]);
        assert_eq!(wrap_line(words, 40), [words]);
        // A link longer than the line keeps its own line, whole.
        let link = "see https://example.com/a/very/long/path now";
        assert_eq!(
            wrap_line(link, 12),
            ["see", "https://example.com/a/very/long/path", "now"]
        );
        assert_eq!(wrap_line("- one two three", 9), ["- one two", "  three"]);
        assert_eq!(
            wrap_line("12. one two three", 10),
            ["12. one", "    two", "    three"]
        );
        assert_eq!(wrap_line("> one two three", 9), ["> one two", "> three"]);
    }

    #[test]
    fn fenced_indented_and_table_lines_are_wide() {
        let body = "Prose here.\n```rust\nlet x = 1;\n```\n| a | b |\n    indented\nafter";
        let wide: Vec<_> = blocks(body).into_iter().map(|(_, prose)| !prose).collect();
        assert_eq!(wide, [false, true, true, true, true, true, false]);
    }

    #[test]
    fn a_heading_reads_as_its_parts_with_row_air() {
        let heading = Heading::new("Flaky clock test")
            .context("tide@lab")
            .context("");
        assert_eq!(heading.text(), "Flaky clock test  tide@lab");
    }
}
