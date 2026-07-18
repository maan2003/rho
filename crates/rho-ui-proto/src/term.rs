//! Wire vocabulary for daemon-owned terminals.
//!
//! A terminal stream is dedicated by a [`crate::ClientMessage::TerminalOpen`]
//! first frame (like zed channels); after the
//! [`crate::ServerMessage::TerminalOpened`] handshake the stream carries senax
//! frames of [`TermClientFrame`] and [`TermServerFrame`].
//!
//! The protocol is deliberately dumb on the client side: the daemon owns the
//! only terminal emulator, and the wire carries *display state* — cell rows,
//! cursor, title — never escape sequences. Clients render rows and send input;
//! they answer no terminal queries and track no modes.

use std::collections::VecDeque;

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Client → daemon frames after the terminal handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TermClientFrame {
    /// Raw bytes for the PTY (already-encoded keys from a passthrough client).
    Input(Vec<u8>),
    /// Ask the daemon to resize the terminal (last writer wins). The daemon
    /// answers every attached client with [`TermServerFrame::Screen`] carrying
    /// the new authoritative size.
    Resize { cols: u16, rows: u16 },
    /// A key event, encoded to bytes daemon-side against the terminal's live
    /// modes (application cursor keys etc.), so clients never track modes.
    Keystroke(TermKeystroke),
    /// Pasted text; the daemon applies bracketed-paste mode.
    Paste(String),
}

/// A key event in gpui keystroke vocabulary: `key` is a lowercase key name
/// ("a", "enter", "up", "pageup", "f5"), modifiers carried separately.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TermKeystroke {
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The text this key would insert (IME-resolved), if any; written to the
    /// PTY when no escape encoding applies.
    pub key_char: Option<String>,
}

/// Daemon → client frames after the terminal handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TermServerFrame {
    /// Full visible screen replacing whatever the client had, including the
    /// authoritative size. Sent at attach, after resizes, and whenever the
    /// daemon prefers a full sync over a diff.
    Snapshot(TermScreen),
    /// Changed rows of the visible screen since the last frame this client
    /// received. Row indexes are 0-based from the screen top.
    Screen {
        rows: Vec<(u16, TermRow)>,
        cursor: TermCursor,
    },
    /// Lines that scrolled off the top of the screen into history since the
    /// last `History` frame, oldest first. `lost` counts lines that scrolled
    /// by but are not included (output outran the sync budget); clients may
    /// mark the discontinuity.
    History {
        lines: Vec<TermRow>,
        lost: u64,
    },
    Title(String),
    /// The terminal's child process exited. The daemon drops the terminal;
    /// the stream closes after this frame.
    Exited {
        status: Option<i32>,
    },
}

/// A full visible screen.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TermScreen {
    pub cols: u16,
    pub rows: Vec<TermRow>,
    pub cursor: TermCursor,
}

/// One row of cells, trailing default-blank cells trimmed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TermRow {
    pub cells: Vec<TermCell>,
}

impl TermRow {
    /// The row's text, wide-character spacers skipped.
    pub fn text(&self) -> String {
        self.cells
            .iter()
            .filter(|cell| cell.flags & TermCellFlags::WIDE_SPACER == 0)
            .map(|cell| cell.c)
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TermCell {
    pub c: char,
    /// Zero-width characters combined onto this cell, if any.
    pub extra: Option<String>,
    pub fg: TermColor,
    pub bg: TermColor,
    /// [`TermCellFlags`] bits.
    pub flags: u16,
}

impl Default for TermCell {
    fn default() -> Self {
        Self {
            c: ' ',
            extra: None,
            fg: TermColor::DEFAULT_FG,
            bg: TermColor::DEFAULT_BG,
            flags: 0,
        }
    }
}

/// Cell attribute bits carried in [`TermCell::flags`].
pub struct TermCellFlags;

impl TermCellFlags {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const STRIKEOUT: u16 = 1 << 5;
    pub const HIDDEN: u16 = 1 << 6;
    /// First cell of a double-width character.
    pub const WIDE: u16 = 1 << 7;
    /// Spacer cell following a double-width character.
    pub const WIDE_SPACER: u16 = 1 << 8;
    /// The row soft-wrapped into the next one (selection/copy join hint).
    pub const WRAPLINE: u16 = 1 << 9;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TermColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// 256-color palette index (0-15 are the named ANSI colors).
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl TermColor {
    pub const DEFAULT_FG: Self = Self::Foreground;
    pub const DEFAULT_BG: Self = Self::Background;
}

/// One running terminal in a [`crate::ServerMessage::TerminalList`] reply.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TerminalInfo {
    /// Encoded agent id ("eng-ht08").
    pub agent: String,
    pub terminal_id: u64,
    /// Current OSC title; empty if the shell never set one.
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    /// Clients attached right now.
    pub clients: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TermCursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: TermCursorShape,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TermCursorShape {
    #[default]
    Block,
    Underline,
    Beam,
}

/// One retained scrollback entry above the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScrollbackItem {
    Line(TermRow),
    /// This many lines scrolled by without being delivered (output outran
    /// the daemon's retention); render as a discontinuity marker.
    Gap(u64),
}

/// What one applied frame changed, for clients that repaint incrementally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameApplied {
    /// The whole screen was replaced.
    Snapshot,
    /// Exactly these screen rows (0-based from the top) changed.
    Rows(Vec<u16>),
    /// Scrollback grew (lines, possibly preceded by a gap).
    History,
    Title,
    Exited {
        status: Option<i32>,
    },
}

/// Client-side reconstruction of a terminal display from
/// [`TermServerFrame`]s.
///
/// This is the one implementation of the wire protocol's client semantics:
/// every viewer (CLI painter, GUI surface, tests) folds frames through it and
/// renders from the result.
#[derive(Debug)]
pub struct WireScreen {
    pub cols: u16,
    pub rows: Vec<TermRow>,
    pub cursor: TermCursor,
    /// Last OSC title; empty until the shell sets one.
    pub title: String,
    /// Scrollback above the screen, oldest first, at most `history_limit`
    /// entries (older ones are dropped).
    pub scrollback: VecDeque<ScrollbackItem>,
    /// `Some` once the terminal's child exited.
    pub exited: Option<Option<i32>>,
    history_limit: usize,
}

impl WireScreen {
    pub fn new(history_limit: usize) -> Self {
        Self {
            cols: 0,
            rows: Vec::new(),
            cursor: TermCursor {
                row: 0,
                col: 0,
                visible: true,
                shape: TermCursorShape::Block,
            },
            title: String::new(),
            scrollback: VecDeque::new(),
            exited: None,
            history_limit,
        }
    }

    pub fn apply(&mut self, frame: TermServerFrame) -> FrameApplied {
        match frame {
            TermServerFrame::Snapshot(screen) => {
                self.cols = screen.cols;
                self.rows = screen.rows;
                self.cursor = screen.cursor;
                FrameApplied::Snapshot
            }
            TermServerFrame::Screen { rows, cursor } => {
                let mut changed = Vec::with_capacity(rows.len());
                for (row, cells) in rows {
                    let index = row as usize;
                    if index >= self.rows.len() {
                        self.rows.resize_with(index + 1, TermRow::default);
                    }
                    self.rows[index] = cells;
                    changed.push(row);
                }
                self.cursor = cursor;
                FrameApplied::Rows(changed)
            }
            TermServerFrame::History { lines, lost } => {
                if lost > 0 {
                    self.scrollback.push_back(ScrollbackItem::Gap(lost));
                }
                self.scrollback
                    .extend(lines.into_iter().map(ScrollbackItem::Line));
                while self.scrollback.len() > self.history_limit {
                    self.scrollback.pop_front();
                }
                FrameApplied::History
            }
            TermServerFrame::Title(title) => {
                self.title = title;
                FrameApplied::Title
            }
            TermServerFrame::Exited { status } => {
                self.exited = Some(status);
                FrameApplied::Exited { status }
            }
        }
    }

    /// Total lines reported lost across all scrollback gaps.
    pub fn lost_lines(&self) -> u64 {
        self.scrollback
            .iter()
            .map(|item| match item {
                ScrollbackItem::Gap(lost) => *lost,
                ScrollbackItem::Line(_) => 0,
            })
            .sum()
    }
}
