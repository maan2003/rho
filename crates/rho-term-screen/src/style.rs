//! Styled text types for terminal rendering.
//!
//! Content is represented as sequences of [`Span`]s, each pairing a
//! plain-text string with a [`Style`]. Display width is always
//! computable from the text alone — no ANSI escape codes are stored
//! in the data model.

pub use crossterm::style::Color;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a string in terminal columns, measured by grapheme cluster.
///
/// The measurement uses the same control-character policy as cell conversion:
/// line breaks have no inline width, tabs render as one space, and other
/// control graphemes render as a visible replacement cell.
pub fn display_width(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true)
        .map(screen_grapheme_width)
        .sum()
}

/// Returns a string that fits within `max_width` terminal columns, appending an
/// ellipsis when truncation is needed.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 1 {
        return "…".to_owned();
    }

    let mut out = String::new();
    let mut width = 0;
    let prefix_width = max_width - 1;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = screen_grapheme_width(grapheme);
        if prefix_width < width + grapheme_width {
            break;
        }
        width += grapheme_width;
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

/// Returns the previous grapheme-cluster boundary before `pos`.
pub fn previous_grapheme_boundary(text: &str, pos: usize) -> usize {
    let pos = pos.min(text.len());
    UnicodeSegmentation::grapheme_indices(text, true)
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx < pos)
        .last()
        .unwrap_or(0)
}

/// Returns the next grapheme-cluster boundary after `pos`.
pub fn next_grapheme_boundary(text: &str, pos: usize) -> usize {
    if text.len() <= pos {
        return text.len();
    }
    for (idx, grapheme) in UnicodeSegmentation::grapheme_indices(text, true) {
        let end = idx + grapheme.len();
        if pos < end {
            return end;
        }
    }
    text.len()
}

pub(crate) fn is_line_break_grapheme(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r")
}

pub(crate) fn screen_grapheme_width(grapheme: &str) -> usize {
    if is_line_break_grapheme(grapheme) {
        0
    } else if grapheme == "\t" || grapheme.chars().any(char::is_control) {
        1
    } else {
        UnicodeWidthStr::width(grapheme)
    }
}

pub(crate) fn push_grapheme_cells(cells: &mut Vec<Cell>, grapheme: &str, style: Style) {
    if grapheme == "\t" {
        cells.push(Cell::new(' ', style));
        return;
    }
    if grapheme.chars().any(char::is_control) {
        cells.push(Cell::new('�', style));
        return;
    }
    let grapheme_width = screen_grapheme_width(grapheme);
    for (idx, ch) in grapheme.chars().enumerate() {
        let width = if idx == 0 { grapheme_width } else { 0 };
        cells.push(Cell::new(ch, style).with_width(width));
    }
}

pub(crate) fn visit_styled_graphemes(spans: &[Span], mut f: impl FnMut(&str, Style)) {
    let mut text = String::new();
    let mut char_styles = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            char_styles.push((text.len(), span.style));
            text.push(ch);
        }
    }

    let mut style_idx = 0;
    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(text.as_str(), true) {
        while style_idx + 1 < char_styles.len() && char_styles[style_idx + 1].0 <= byte {
            style_idx += 1;
        }
        let style = char_styles
            .get(style_idx)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        f(grapheme, style);
    }
}

/// Visual attributes for a single character cell.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
    pub strikethrough: bool,
}

impl Style {
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }
}

/// A terminal cell: one character, its visual style, and display width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    /// Character emitted for this cell.
    pub ch: char,
    /// Visual style applied while emitting this cell.
    pub style: Style,
    /// Display width in terminal columns.
    pub width: usize,
}

impl Cell {
    pub(crate) fn sanitized_char(ch: char) -> char {
        if ch == '\t' {
            ' '
        } else if ch.is_control() {
            '�'
        } else {
            ch
        }
    }

    pub fn new(ch: char, style: Style) -> Self {
        let ch = Self::sanitized_char(ch);
        Self {
            ch,
            style,
            width: ch.width().unwrap_or(0),
        }
    }

    pub fn plain(ch: char) -> Self {
        let ch = Self::sanitized_char(ch);
        Self {
            ch,
            style: Style::default(),
            width: ch.width().unwrap_or(0),
        }
    }

    pub(crate) fn normalized(self) -> Self {
        let ch = Self::sanitized_char(self.ch);
        if ch == self.ch {
            self
        } else {
            Self {
                ch,
                style: self.style,
                width: ch.width().unwrap_or(0),
            }
        }
    }

    /// Returns a copy of this cell with an explicit display width.
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// Display width in terminal columns (1 for ASCII, 2 for wide
    /// chars like emoji/CJK, 0 for zero-width combiners).
    pub fn col_width(&self) -> usize {
        self.width
    }
}

/// A run of text with a uniform style.
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }
}

/// A sequence of styled spans representing rich text.
///
/// Can be constructed from plain `&str` / `String` (unstyled),
/// a single [`Span`], or a `Vec<Span>`.
#[derive(Clone, Debug, Default)]
pub struct StyledText {
    spans: Vec<Span>,
}

impl StyledText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Total display width in terminal columns.
    ///
    /// Wide characters and emoji grapheme clusters count as terminal columns,
    /// not Unicode scalar values.
    pub fn char_count(&self) -> usize {
        let mut text = String::new();
        for span in &self.spans {
            text.push_str(&span.text);
        }
        display_width(&text)
    }

    /// Returns `true` if there is no text content.
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// Converts to a flat sequence of [`Cell`]s (newlines excluded).
    pub fn to_cells(&self) -> Vec<Cell> {
        let mut cells = Vec::new();
        visit_styled_graphemes(&self.spans, |grapheme, style| {
            if !is_line_break_grapheme(grapheme) {
                push_grapheme_cells(&mut cells, grapheme, style);
            }
        });
        cells
    }
}

impl From<&str> for StyledText {
    fn from(s: &str) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<String> for StyledText {
    fn from(s: String) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<Span> for StyledText {
    fn from(span: Span) -> Self {
        Self { spans: vec![span] }
    }
}

impl From<Vec<Span>> for StyledText {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

/// Opaque numeric identifier for a [`StyledBlock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(pub u64);

/// Horizontal alignment within a block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
}

/// A unit of layout: styled content with background, alignment, and margins.
///
/// When rendered, the block's content is word-wrapped to the available
/// width (after subtracting margins), aligned within that space, and
/// the block's background color fills any remaining cells.
#[derive(Clone, Debug)]
pub struct StyledBlock {
    pub content: StyledText,
    pub right_content: StyledText,
    pub bg: Option<Color>,
    pub align: Align,
    pub margin_left: u16,
    pub margin_right: u16,
}

impl StyledBlock {
    pub fn new(content: impl Into<StyledText>) -> Self {
        Self {
            content: content.into(),
            right_content: StyledText::new(),
            bg: None,
            align: Align::Left,
            margin_left: 0,
            margin_right: 0,
        }
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn right_content(mut self, content: impl Into<StyledText>) -> Self {
        self.right_content = content.into();
        self
    }

    pub fn margin_left(mut self, n: u16) -> Self {
        self.margin_left = n;
        self
    }

    pub fn margin_right(mut self, n: u16) -> Self {
        self.margin_right = n;
        self
    }

    pub fn margins(mut self, left: u16, right: u16) -> Self {
        self.margin_left = left;
        self.margin_right = right;
        self
    }
}

impl From<&str> for StyledBlock {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for StyledBlock {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<StyledText> for StyledBlock {
    fn from(text: StyledText) -> Self {
        Self::new(text)
    }
}
