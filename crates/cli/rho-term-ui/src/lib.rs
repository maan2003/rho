// Derived from dpc/tau (crates/tau-cli-term-raw/src/lib.rs), MPL-2.0.
// This file remains under MPL-2.0.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inline terminal UI for the `rho` CLI: styled output blocks above a
//! multiline prompt, drawn in the normal terminal buffer.
//!
//! There is no alternate screen. Output that scrolls off the top lands in the
//! terminal's own scrollback, so the user scrolls and selects with their
//! terminal as usual. The redraw thread picks one of three paths:
//!
//! - **Differential update** (common case): the viewport is diffed against what
//!   [`Screen`] last drew and only changed cells are rewritten. Cursor movement
//!   is always relative; downward movement uses `\n` so it scrolls at the
//!   bottom edge.
//! - **Scrolling render**: when new rows push older rows off the top, changed
//!   rows are written top to bottom from the previous viewport and the
//!   terminal's own scrolling moves them into scrollback. Scrollback is filled
//!   as a side effect of drawing; a row that was never on screen never reaches
//!   scrollback.
//! - **Full render** (resize, [`TermHandle::invalidate_screen`], or a change to
//!   rows that already scrolled away): clear the screen *and* scrollback, then
//!   replay the last [`TermHandle::set_redraw_history_size`] history rows plus
//!   the tail, reflowed to the current width. Scrollback written before the
//!   terminal UI started is lost on the first full render.
//!
//! # Zones
//!
//! Blocks live in one store keyed by [`BlockId`] and are placed, top to
//! bottom, in: **history** (persistent, append-mostly output), **above
//! active** (live blocks such as a streaming reply), the **prompt** (left
//! prompt, editable input, right prompt), and **below** (status lines).
//! Everything from above-active down is bottom-anchored next to the prompt.
//!
//! # Which mutations need a full redraw
//!
//! The differential renderer can only reach the visible viewport. Changing a
//! row that has already scrolled into terminal scrollback would leave a
//! stale ("fossilized") copy there, so the renderer compares the hidden
//! prefix of history with what it drew and falls back to a full render when
//! it changed. In practice:
//!
//! - Cheap: editing the prompt, updating `below`, appending to a live
//!   `above_active` block, and appending new history blocks.
//! - Full render: `set_block` or removal of a history block that scrolled away,
//!   and reordering zones in a way that touches scrolled rows. These are
//!   detected automatically, but they clear and replay scrollback, so avoid
//!   them on hot paths. Call [`TermHandle::invalidate_screen`] when something
//!   outside the block model clobbered the terminal.
//! - A live block taller than the viewport has its top rows in scrollback
//!   already. Append-only growth is fine; retracting or rewriting its top
//!   leaves a stale copy until the next full render.
//!
//! Moving a finished live block to history (`remove_above_active` then
//! `push_history`, with unchanged content) stays on the cheap paths, even
//! when the block is taller than the viewport.
//!
//! # Threading
//!
//! [`Term::get_next_event`] blocks on one thread and handles prompt editing
//! itself. Any thread may hold a cloned [`TermHandle`] and mutate blocks;
//! mutators only change shared state, and [`TermHandle::redraw`] asks the
//! redraw thread, the only writer to the terminal, for a repaint. Redraw
//! requests coalesce. The first write or flush error stops that thread for
//! good, and later reads return an error for which [`is_output_failure`]
//! holds.
//!
//! Untrusted text is safe to display: layout replaces control characters,
//! and OSC 8 hyperlink targets are validated before they are written.

mod cell_row;
mod render;
mod screen;
mod style;

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use cell_row::CellRow;
use crossterm::event::{
    self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal;
pub use screen::{Screen, emit_styled_cells, layout_block, layout_lines, write_osc8_hyperlink};
pub use style::{
    Align, BlockId, Cell, Color, MAX_HYPERLINK_TARGET_BYTES, Span, Style, StyledBlock, StyledText,
    display_width, next_grapheme_boundary, previous_grapheme_boundary, sanitize_hyperlink_target,
    truncate_to_width,
};
use unicode_segmentation::UnicodeSegmentation;

/// Share of the terminal height the prompt input may occupy before it
/// scrolls internally.
const PROMPT_INPUT_MAX_HEIGHT_PERCENT: usize = 33;
/// Maximum number of nonempty drafts retained in input history.
const INPUT_HISTORY_MAX_ENTRIES: usize = 1000;
/// Maximum UTF-8 bytes retained across input-history drafts.
const INPUT_HISTORY_MAX_BYTES: usize = 16 * 1024 * 1024;
/// How long one blocking terminal poll waits before rechecking shutdown.
const INPUT_POLL_SLICE: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct PromptSnapshot {
    buffer: String,
    cursor: usize,
}

#[derive(Clone)]
struct PromptDraft {
    buffer: String,
    cursor: usize,
    undo: Vec<PromptSnapshot>,
    redo: Vec<PromptSnapshot>,
}

impl PromptDraft {
    fn submitted(buffer: String) -> Self {
        let cursor = buffer.len();
        Self {
            buffer,
            cursor,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

/// One draft navigable through prompt history with its retained-source link.
struct HistoryNavEntry {
    /// Draft content and local undo/redo state shown at this navigation slot.
    draft: PromptDraft,
    /// Original retained history position, absent for the work-in-progress
    /// draft.
    source_index: Option<usize>,
}

/// State for input-history navigation. Present only while Up/Down
/// has recalled a previous line and the user hasn't submitted or
/// dismissed yet.
struct HistoryNav {
    /// Snapshot of retained entries plus the WIP draft. Editing in history
    /// mode mutates the selected entry's draft and retained source.
    entries: Vec<HistoryNavEntry>,
    /// Current position within `entries`.
    index: usize,
}

/// Rendered output blocks and their placement around the prompt.
#[derive(Default)]
struct BlockLayoutState {
    /// Central block storage.
    blocks: HashMap<BlockId, StyledBlock>,
    /// Next auto-increment id.
    next_id: u64,
    /// Persistent output — append-mostly ordered list of block ids.
    history: Vec<BlockId>,
    /// Reference count of block ids present in `history`.
    history_refs: HashMap<BlockId, usize>,
    /// Bumped whenever persistent history content, order, or layout changes.
    history_generation: u64,
    /// Earliest history entry changed since the redraw cache last refreshed.
    ///
    /// Ordinary appends and removals mark their changed suffix. Content
    /// changes conservatively invalidate from entry zero.
    history_dirty_from: Option<usize>,
    /// Live blocks above the prompt (can be reordered).
    above_active: Vec<BlockId>,
    /// Blocks rendered below the prompt.
    below: Vec<BlockId>,
}

/// Prompt contents, editing history, and prompt-local viewport state.
#[derive(Default)]
struct PromptEditorState {
    /// Prompt rendered to the left of the editable input.
    left_prompt: StyledText,
    /// Prompt rendered at the right edge of the terminal.
    right_prompt: StyledText,
    /// Placeholder rendered while the editable input is empty.
    input_placeholder: StyledText,
    /// Current editable input.
    buffer: String,
    /// Byte offset of the input cursor.
    cursor: usize,
    /// Visual column the cursor "wants" to be on for vertical motion
    /// (Up/Down within the buffer and across history). Lazily set on
    /// the first vertical motion after a horizontal motion or edit,
    /// then preserved across consecutive vertical motions so jumping
    /// over short or empty lines doesn't permanently truncate the
    /// column. Cleared by any cursor change that isn't a vertical
    /// motion.
    sticky_col: Option<usize>,
    /// Bounded newest suffix of submitted lines. Each entry carries its own
    /// undo/redo stacks so history navigation can preserve draft-local
    /// editing state.
    input_history: Vec<PromptDraft>,
    /// Undo snapshots for the current editable input.
    current_undo: Vec<PromptSnapshot>,
    /// Redo snapshots for the current editable input.
    current_redo: Vec<PromptSnapshot>,
    /// Active history navigation, if any.
    history_nav: Option<HistoryNav>,
    /// First visual input row rendered in the prompt-local capped viewport.
    /// This is independent of terminal scrollback/history viewporting; plain
    /// Up/Down can adjust it before falling through to prompt history.
    input_viewport_start: usize,
}

/// Terminal dimensions, redraw coordination, and lifecycle flags.
struct TerminalRuntimeState {
    /// Current terminal width in columns.
    width: usize,
    /// Current terminal height in rows.
    height: usize,
    /// Set by `Term::drop` to signal the redraw thread to exit.
    shutdown: bool,
    /// Set by another owner, an input error, or virtual input disconnect to
    /// make the blocking input loop return sticky EOF.
    input_shutdown: bool,
    /// First reported renderer write or flush failure for this terminal.
    ///
    /// Once set, the redraw thread exits without performing another write.
    output_failure: Option<OutputFailure>,
    /// Forces the next redraw to wipe its `Screen` cache and repaint from
    /// scratch. The redraw loop reads-and-clears this flag.
    invalidate_screen: bool,
    /// Latest requested redraw synchronization generation.
    ///
    /// Callers bump this generation; the redraw thread copies it into
    /// `sync_completed` after rendering the state it observed.
    sync_requested: u64,
    /// Latest completed redraw synchronization generation.
    sync_completed: u64,
    /// Maximum persistent-history/log rows replayed during a full redraw.
    /// Older rows are omitted after clearing scrollback so slow terminals do
    /// not receive an unbounded transcript.
    redraw_history_size: usize,
    /// Number of full renders performed since creation.
    full_render_count: u64,
}

/// Mutable state shared between the input loop, redraw thread, and
/// any [`TermHandle`] holders.
struct SharedState {
    /// Rendered output blocks and their placement around the prompt.
    layout: BlockLayoutState,
    /// Prompt contents, editing history, and prompt-local viewport state.
    editor: PromptEditorState,
    /// Terminal dimensions, redraw coordination, and lifecycle flags.
    terminal: TerminalRuntimeState,
}

impl SharedState {
    fn new(width: usize, height: usize, left_prompt: StyledText) -> Self {
        Self {
            layout: BlockLayoutState::default(),
            editor: PromptEditorState {
                left_prompt,
                ..PromptEditorState::default()
            },
            terminal: TerminalRuntimeState {
                width,
                height,
                shutdown: false,
                input_shutdown: false,
                output_failure: None,
                invalidate_screen: false,
                sync_requested: 0,
                sync_completed: 0,
                redraw_history_size: usize::MAX,
                full_render_count: 0,
            },
        }
    }

    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.layout.next_id);
        self.layout.next_id += 1;
        id
    }

    fn mark_history_dirty_from(&mut self, entry: usize) {
        self.layout.history_generation = self.layout.history_generation.wrapping_add(1);
        self.layout.history_dirty_from = Some(
            self.layout
                .history_dirty_from
                .map_or(entry, |dirty| dirty.min(entry)),
        );
    }

    fn append_history(&mut self, id: BlockId) {
        let appended_at = self.layout.history.len();
        self.layout.history.push(id);
        *self.layout.history_refs.entry(id).or_insert(0) += 1;
        self.mark_history_dirty_from(appended_at);
    }

    fn block_in_history(&self, id: BlockId) -> bool {
        self.layout.history_refs.contains_key(&id)
    }

    /// Removes one block and every one of its rendered-zone references.
    fn remove_block(&mut self, id: BlockId) {
        self.layout.blocks.remove(&id);

        // `history_refs` is the authoritative membership index. Live blocks
        // occupy an active zone but not persistent history, so skipping this
        // scan keeps their removal independent of transcript length.
        if self.layout.history_refs.remove(&id).is_some() {
            let first_index = self
                .layout
                .history
                .iter()
                .position(|&candidate| candidate == id)
                .expect("history membership index must imply one matching entry");
            self.layout.history.retain(|&candidate| candidate != id);
            self.mark_history_dirty_from(first_index);
        }

        self.layout
            .above_active
            .retain(|&candidate| candidate != id);
        self.layout.below.retain(|&candidate| candidate != id);
    }

    fn current_snapshot(&self) -> PromptSnapshot {
        PromptSnapshot {
            buffer: self.editor.buffer.clone(),
            cursor: self.editor.cursor,
        }
    }

    fn current_draft(&self) -> PromptDraft {
        PromptDraft {
            buffer: self.editor.buffer.clone(),
            cursor: self.editor.cursor,
            undo: self.editor.current_undo.clone(),
            redo: self.editor.current_redo.clone(),
        }
    }

    /// Evicts the oldest drafts until the retained history is a bounded
    /// newest suffix of nonempty prompt text.
    fn limit_input_history(&mut self) {
        let history = &mut self.editor.input_history;
        let original_entries = history.len();
        history.retain(|draft| {
            !draft.buffer.is_empty() && draft.buffer.len() <= INPUT_HISTORY_MAX_BYTES
        });

        let mut retained_bytes = 0;
        let mut retained_start = history.len();
        for (retained_entries, (index, draft)) in history.iter().enumerate().rev().enumerate() {
            if retained_entries == INPUT_HISTORY_MAX_ENTRIES
                || draft.buffer.len() > INPUT_HISTORY_MAX_BYTES - retained_bytes
            {
                break;
            }
            retained_bytes += draft.buffer.len();
            retained_start = index;
        }
        history.drain(..retained_start);
        if history.len() != original_entries {
            // Navigation entries index into the history that just shifted.
            self.editor.history_nav = None;
        }
    }

    /// Takes the current undo state into a submitted history entry.
    fn take_submitted_draft(&mut self) -> PromptDraft {
        PromptDraft {
            buffer: self.editor.buffer.clone(),
            cursor: self.editor.cursor,
            undo: std::mem::take(&mut self.editor.current_undo),
            redo: std::mem::take(&mut self.editor.current_redo),
        }
    }

    fn load_draft(&mut self, draft: PromptDraft) {
        self.editor.buffer = draft.buffer;
        self.editor.current_undo = draft.undo;
        self.editor.current_redo = draft.redo;
        self.editor.cursor = draft.cursor.min(self.editor.buffer.len());
        self.ensure_input_cursor_visible();
    }

    fn record_undo(&mut self) {
        self.editor.current_undo.push(self.current_snapshot());
        self.editor.current_redo.clear();
    }

    /// Mirrors edits made to `buffer` and undo state into the live
    /// history-nav slot so navigating Down then Up returns to the
    /// user's edited copy. No-op when not navigating history.
    fn sync_buffer_to_history_nav(&mut self) {
        let draft = self.current_draft();
        if let Some(nav) = self.editor.history_nav.as_mut() {
            nav.entries[nav.index].draft = draft.clone();
            if let Some(source_index) = nav.entries[nav.index].source_index
                && let Some(source) = self.editor.input_history.get_mut(source_index)
            {
                *source = draft;
            }
        }
    }

    /// Visual `(row, col)` of the cursor against the current buffer.
    /// Row 0 starts after the left prompt, so `col` on row 0 is offset
    /// by the prompt width.
    fn visual_cursor_position(&self) -> (usize, usize) {
        let width = self.terminal.width.max(1);
        let left_cols = self.editor.left_prompt.char_count();
        buffer_position_for_byte(&self.editor.buffer, self.editor.cursor, width, left_cols)
    }

    /// Last visual row index of the current buffer.
    fn last_visual_row(&self) -> usize {
        let width = self.terminal.width.max(1);
        let left_cols = self.editor.left_prompt.char_count();
        let (max_row, _) = buffer_end_position(&self.editor.buffer, width, left_cols);
        max_row
    }

    /// Byte offset within the current buffer that lands the cursor at
    /// the given visual `(row, col)`. Clamps to the nearest reachable
    /// position.
    fn cursor_byte_at(&self, target_row: usize, target_col: usize) -> usize {
        let width = self.terminal.width.max(1);
        let left_cols = self.editor.left_prompt.char_count();
        byte_offset_for_buffer_position(
            &self.editor.buffer,
            target_row,
            target_col,
            width,
            left_cols,
        )
    }

    /// Visual column to use for the next vertical motion: returns the
    /// sticky column if one is set, otherwise captures the current
    /// cursor's visual column and stores it as sticky.
    fn vertical_target_col(&mut self) -> usize {
        if let Some(col) = self.editor.sticky_col {
            return col;
        }
        let (_, col) = self.visual_cursor_position();
        self.editor.sticky_col = Some(col);
        col
    }

    /// Sets the cursor as part of a horizontal motion or edit and
    /// invalidates the sticky vertical column. All cursor mutations
    /// outside of vertical motion must go through this — the sticky
    /// column only stays valid as long as the cursor is moving
    /// purely up/down.
    fn write_cursor(&mut self, new_cursor: usize) {
        self.editor.cursor = new_cursor;
        self.editor.sticky_col = None;
        self.ensure_input_cursor_visible();
    }

    /// Sets the cursor as part of a vertical motion. Preserves the
    /// sticky column so consecutive vertical moves can replay the
    /// original column over short or empty rows.
    fn write_cursor_keep_sticky(&mut self, new_cursor: usize) {
        self.editor.cursor = new_cursor;
        self.ensure_input_cursor_visible();
    }

    fn input_visible_rows(&self) -> usize {
        let total_rows = self.last_visual_row() + 1;
        let cap_rows = prompt_input_max_rows(self.terminal.height);
        let indicator_rows =
            prompt_scroll_indicator_rows(!self.editor.buffer.is_empty(), total_rows, cap_rows);
        prompt_editable_rows(total_rows, cap_rows, indicator_rows)
    }

    fn ensure_input_cursor_visible(&mut self) {
        let (cursor_row, _) = self.visual_cursor_position();
        let total_rows = self.last_visual_row() + 1;
        let visible_rows = self.input_visible_rows();
        self.editor.input_viewport_start = viewport_start_with_cursor(
            self.editor.input_viewport_start,
            cursor_row,
            total_rows,
            visible_rows,
        );
    }

    /// Pushes the current prompt onto input history and resets to a
    /// fresh empty prompt. An empty prompt does not add an entry, but
    /// still enforces retention while leaving history navigation. Clears
    /// the sticky column via `write_cursor`.
    fn push_current_as_history_entry(&mut self) -> bool {
        if self.editor.buffer.is_empty() {
            self.limit_input_history();
            return false;
        }
        let draft = self.take_submitted_draft();
        self.editor.input_history.push(draft);
        self.limit_input_history();
        self.editor.buffer.clear();
        self.write_cursor(0);
        true
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = self.editor.current_undo.pop() else {
            return false;
        };
        self.editor.current_redo.push(self.current_snapshot());
        self.editor.buffer = snapshot.buffer;
        self.write_cursor(snapshot.cursor.min(self.editor.buffer.len()));
        self.sync_buffer_to_history_nav();
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snapshot) = self.editor.current_redo.pop() else {
            return false;
        };
        self.editor.current_undo.push(self.current_snapshot());
        self.editor.buffer = snapshot.buffer;
        self.write_cursor(snapshot.cursor.min(self.editor.buffer.len()));
        self.sync_buffer_to_history_nav();
        true
    }

    /// Steps history navigation by `delta`. Enters history-nav mode
    /// from `Editing` when moving backward and history exists. Moving
    /// forward from a non-empty editing buffer stores it as history
    /// and opens a fresh empty prompt. Returns `true` if the buffer
    /// changed.
    ///
    /// Cursor placement preserves the visual column so that
    /// `Up`/`Down` across prompts feels like one continuous text:
    /// stepping back lands on the previous entry's last visual row,
    /// stepping forward lands on the next entry's first visual row,
    /// both at (or clamped to) the column the cursor was on.
    fn step_history(&mut self, delta: isize) -> bool {
        let target_col = self.vertical_target_col();
        if self.editor.history_nav.is_none() {
            if 0 < delta {
                return self.push_current_as_history_entry();
            }
            return self.enter_history_nav(target_col);
        }
        self.advance_history_nav(delta, target_col)
    }

    /// Switches from `Editing` into history-navigation mode at the
    /// most recent entry, with the cursor placed at the previous
    /// entry's last visual row at `target_col`.
    fn enter_history_nav(&mut self, target_col: usize) -> bool {
        if self.editor.input_history.is_empty() {
            return false;
        }
        let mut entries: Vec<_> = self
            .editor
            .input_history
            .iter()
            .cloned()
            .enumerate()
            .map(|(source_index, draft)| HistoryNavEntry {
                draft,
                source_index: Some(source_index),
            })
            .collect();
        entries.push(HistoryNavEntry {
            draft: self.current_draft(),
            source_index: None,
        });
        // The WIP buffer sits at `entries.last()`; the previous
        // history entry is one slot before it.
        let index = entries.len() - 2;
        self.load_draft(entries[index].draft.clone());
        let new_cursor = self.cursor_byte_at(self.last_visual_row(), target_col);
        self.write_cursor_keep_sticky(new_cursor);
        self.editor.history_nav = Some(HistoryNav { entries, index });
        true
    }

    /// Steps within an already-active history navigation. Going past
    /// the WIP slot (Down at the latest entry) pushes the WIP buffer
    /// onto history and returns to a fresh prompt, mirroring Down
    /// from `Editing`.
    fn advance_history_nav(&mut self, delta: isize, target_col: usize) -> bool {
        let current = self.current_draft();
        let nav = self
            .editor
            .history_nav
            .as_mut()
            .expect("caller checked Some");
        let new_index = nav.index as isize + delta;
        if new_index < 0 {
            return false;
        }
        if new_index >= nav.entries.len() as isize {
            let wip = nav.entries.last().map(|entry| entry.draft.clone());
            self.editor.history_nav = None;
            if let Some(wip) = wip {
                self.load_draft(wip);
            }
            return self.push_current_as_history_entry();
        }
        nav.entries[nav.index].draft = current.clone();
        if let Some(source_index) = nav.entries[nav.index].source_index
            && let Some(source) = self.editor.input_history.get_mut(source_index)
        {
            *source = current;
        }
        nav.index = new_index as usize;
        let new_draft = nav.entries[nav.index].draft.clone();
        self.load_draft(new_draft);
        let target_row = if delta < 0 { self.last_visual_row() } else { 0 };
        let new_cursor = self.cursor_byte_at(target_row, target_col);
        self.write_cursor_keep_sticky(new_cursor);
        true
    }
}

/// High-level events surfaced by [`Term::get_next_event`].
///
/// Prompt editing (typing, cursor motion, history, undo) is handled inside
/// the terminal; only events the caller acts on come out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The user submitted the prompt with Enter (or Ctrl-Enter). The prompt
    /// is cleared and the line is added to input history. The line may be
    /// empty and may contain newlines.
    Line(String),
    /// Ctrl-C on an empty prompt. Ctrl-C on a nonempty prompt clears it
    /// instead (undoable) and reports [`Event::BufferChanged`].
    Interrupt,
    /// Ctrl-D on an empty prompt. Also returned, on every later read, once
    /// input is shut down ([`TermHandle::request_input_shutdown`]) or virtual
    /// input is closed.
    Eof,
    /// A key the prompt editor does not consume.
    Key(Key),
    /// The terminal was resized. The redraw has already been requested.
    Resize { width: u16, height: u16 },
    /// The prompt buffer changed (typing, paste, history recall, undo).
    BufferChanged,
}

/// Keys the prompt editor leaves to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Escape.
    Esc,
}

/// Raw terminal input, as read from the terminal or injected into a virtual
/// terminal by tests.
#[derive(Clone, Debug)]
pub enum RawEvent {
    /// A decoded key press.
    Key(KeyEvent),
    /// Terminal resize event with width and height in cells.
    Resize(u16, u16),
    /// One bracketed paste, delivered whole so embedded newlines are
    /// inserted rather than submitted.
    Paste(String),
}

enum InputMessage {
    /// A raw terminal event to process unless sticky shutdown/EOF already won.
    Raw(RawEvent),
    /// Wake the receiver and transition it to sticky EOF.
    Shutdown,
    /// The terminal reader failed and stopped.
    ReadError(io::Error),
}

/// The first reported output failure that permanently stops one terminal.
#[derive(Clone, Debug)]
struct OutputFailure {
    /// Original standard I/O error classification.
    kind: io::ErrorKind,
    /// Stable error text retained after the originating error is consumed.
    message: String,
}

impl OutputFailure {
    /// Captures an output error for later delivery to the input owner.
    fn new(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    /// Reconstructs an I/O error carrying the output-failure marker.
    fn io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.clone())
    }
}

impl std::fmt::Display for OutputFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "terminal output failed: {}", self.message)
    }
}

impl std::error::Error for OutputFailure {}

/// Returns whether an I/O error from [`Term::get_next_event`] reports that
/// terminal output failed and the renderer stopped.
#[must_use]
pub fn is_output_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<OutputFailure>())
}

/// A cloneable handle for mutating blocks and the prompt from any thread.
///
/// Setters update shared state but do **not** trigger a redraw, except
/// [`print_output`](Self::print_output). Call [`redraw`](Self::redraw) after
/// making all changes.
#[derive(Clone)]
pub struct TermHandle {
    state: Arc<Mutex<SharedState>>,
    sync_condvar: Arc<Condvar>,
    /// Coalescing redraw notification: a full channel already has a pending
    /// request.
    redraw: SyncSender<()>,
    input_tx: Sender<InputMessage>,
}

impl TermHandle {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("term state mutex poisoned")
    }

    fn notify_redraw(&self) {
        let _ = self.redraw.try_send(());
    }

    /// Makes [`Term::get_next_event`] return [`Event::Eof`] from now on,
    /// waking it if it is blocked.
    pub fn request_input_shutdown(&self) {
        self.lock().terminal.input_shutdown = true;
        let _ = self.input_tx.send(InputMessage::Shutdown);
    }

    /// Requests a repaint. Multiple calls coalesce into a single repaint.
    pub fn redraw(&self) {
        self.notify_redraw();
    }

    /// Requests a repaint and blocks until the redraw thread has rendered
    /// the state as of this call.
    ///
    /// After an output failure this returns immediately.
    pub fn redraw_sync(&self) {
        let mut st = self.lock();
        if st.terminal.output_failure.is_some() {
            return;
        }
        st.terminal.sync_requested = st.terminal.sync_requested.wrapping_add(1);
        let target = st.terminal.sync_requested;
        drop(st);

        self.notify_redraw();

        let st = self.lock();
        let _st = self
            .sync_condvar
            .wait_while(st, |s| s.terminal.sync_completed < target)
            .expect("term state mutex poisoned");
    }

    /// Drops every block from every zone and forces a full repaint, which
    /// also clears terminal scrollback. The prompt, its buffer, and input
    /// history are kept.
    pub fn clear_output(&self) {
        let mut st = self.lock();
        let next_id = st.layout.next_id;
        st.layout = BlockLayoutState {
            next_id,
            history_generation: st.layout.history_generation.wrapping_add(1),
            ..BlockLayoutState::default()
        };
        st.terminal.invalidate_screen = true;
        drop(st);
        self.notify_redraw();
    }

    /// Forces the next redraw to take the full-render path: clear the
    /// screen and scrollback, then replay recent history and the tail.
    ///
    /// Needed only when something outside this crate wrote to the terminal.
    /// Block changes that reach rows already in scrollback are detected and
    /// fully redrawn without it.
    pub fn invalidate_screen(&self) {
        self.lock().terminal.invalidate_screen = true;
        self.notify_redraw();
    }

    /// Current terminal size `(width, height)` tracked by the renderer.
    pub fn size(&self) -> (usize, usize) {
        let st = self.lock();
        (st.terminal.width, st.terminal.height)
    }

    /// Number of full renders performed since creation. A diagnostic for
    /// callers checking that their updates stay on the cheap paths.
    pub fn full_render_count(&self) -> u64 {
        self.lock().terminal.full_render_count
    }

    /// Sets the maximum number of history rows replayed into scrollback by
    /// a full render. Defaults to unbounded. Takes effect at the next full
    /// render.
    pub fn set_redraw_history_size(&self, redraw_history_size: usize) {
        self.lock().terminal.redraw_history_size = redraw_history_size;
    }

    // --- Block management ---

    /// Allocates a new [`BlockId`] and stores the block without placing it
    /// in any zone.
    pub fn new_block(&self, block: impl Into<StyledBlock>) -> BlockId {
        let mut st = self.lock();
        let id = st.alloc_id();
        st.layout.blocks.insert(id, block.into());
        id
    }

    /// Replaces the content of a block, wherever it is placed.
    pub fn set_block(&self, id: BlockId, block: impl Into<StyledBlock>) {
        let mut st = self.lock();
        st.layout.blocks.insert(id, block.into());
        if st.block_in_history(id) {
            st.mark_history_dirty_from(0);
        }
    }

    /// Removes a block from the store and every zone that references it.
    pub fn remove_block(&self, id: BlockId) {
        self.lock().remove_block(id);
    }

    // --- Zone lists ---

    /// Appends a block id to history.
    pub fn push_history(&self, id: BlockId) {
        self.lock().append_history(id);
    }

    /// Appends a block id to the above-active zone, if not already present.
    pub fn push_above_active(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.layout.above_active.contains(&id) {
            st.layout.above_active.push(id);
        }
    }

    /// Removes a block id from the above-active zone. The block stays in
    /// the store, so it can be pushed to history next.
    pub fn remove_above_active(&self, id: BlockId) {
        self.lock().layout.above_active.retain(|&x| x != id);
    }

    /// Appends a block id to the below-prompt zone, if not already present.
    pub fn push_below(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.layout.below.contains(&id) {
            st.layout.below.push(id);
        }
    }

    /// Removes a block id from the below-prompt zone.
    pub fn remove_below(&self, id: BlockId) {
        self.lock().layout.below.retain(|&x| x != id);
    }

    /// Creates a new block, appends it to history, and requests a redraw.
    pub fn print_output(&self, block: impl Into<StyledBlock>) -> BlockId {
        let mut st = self.lock();
        let id = st.alloc_id();
        st.layout.blocks.insert(id, block.into());
        st.append_history(id);
        drop(st);
        self.notify_redraw();
        id
    }

    // --- Prompt ---

    /// Replaces the text shown before the editable input.
    pub fn set_left_prompt(&self, text: impl Into<StyledText>) {
        let mut st = self.lock();
        st.editor.left_prompt = text.into();
        st.ensure_input_cursor_visible();
    }

    /// Replaces the text right-aligned on the prompt's first row. It is
    /// hidden when the input wraps or does not leave room for it.
    pub fn set_right_prompt(&self, text: impl Into<StyledText>) {
        self.lock().editor.right_prompt = text.into();
    }

    /// Replaces the placeholder shown while the input is empty.
    pub fn set_input_placeholder(&self, text: impl Into<StyledText>) {
        self.lock().editor.input_placeholder = text.into();
    }

    /// Returns a copy of the current input buffer.
    pub fn get_buffer(&self) -> String {
        self.lock().editor.buffer.clone()
    }

    /// Returns the input cursor as a byte offset into the buffer.
    pub fn get_cursor(&self) -> usize {
        self.lock().editor.cursor
    }

    /// Replaces the input buffer, placing the cursor at byte offset `cursor`
    /// (clamped to a grapheme boundary). Leaves history navigation and drops
    /// the prompt's undo state: the new text is a fresh starting point.
    pub fn set_buffer(&self, text: String, cursor: usize) {
        let mut st = self.lock();
        let new_cursor = clamp_cursor_to_grapheme_boundary(&text, cursor);
        st.editor.buffer = text;
        let abandoned_history_nav = st.editor.history_nav.take().is_some();
        st.editor.current_undo.clear();
        st.editor.current_redo.clear();
        st.write_cursor(new_cursor);
        if abandoned_history_nav {
            st.limit_input_history();
        }
    }
}

/// The terminal prompt engine.
///
/// Owns raw mode and the input loop: call [`Term::get_next_event`] in a loop
/// on one thread. `Term` derefs to [`TermHandle`]. Dropping it renders a
/// final frame, moves the cursor below all content, and restores the
/// terminal.
pub struct Term {
    /// Cloneable handle exposing block and prompt mutators.
    handle: TermHandle,
    /// Receives raw input, read errors, and shutdown wakeups.
    input_rx: Receiver<InputMessage>,
    /// Tells the terminal reader thread to stop after its current poll.
    reader_stop: Arc<AtomicBool>,
    /// Terminal reader thread, absent for virtual terminals.
    reader_thread: Option<JoinHandle<()>>,
    /// Redraw thread handle — taken and joined on drop.
    redraw_thread: Option<JoinHandle<()>>,
    /// Whether to restore terminal modes on drop (false for virtual terms).
    owns_raw_mode: bool,
}

impl std::ops::Deref for Term {
    type Target = TermHandle;
    fn deref(&self) -> &TermHandle {
        &self.handle
    }
}

impl Term {
    /// Takes over the terminal on stdin/stdout: enables raw mode, bracketed
    /// paste, and the kitty keyboard protocol's escape disambiguation (so
    /// Shift-Enter is distinguishable where supported), then starts the
    /// reader and redraw threads.
    ///
    /// # Errors
    ///
    /// Returns terminal I/O errors from enabling raw mode or terminal input
    /// features. If feature setup fails after raw mode was enabled, raw mode is
    /// disabled on a best-effort basis before returning the error.
    pub fn new(left_prompt: impl Into<StyledText>) -> io::Result<(Self, TermHandle)> {
        let (width, height) = term_size();
        terminal::enable_raw_mode()?;
        if let Err(error) = write_enable_terminal_features(&mut io::stdout()) {
            let _ = write_disable_terminal_features(&mut io::stdout());
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        let (term, handle, input_tx) = Self::spawn(
            width,
            height,
            left_prompt.into(),
            Box::new(io::stdout()),
            true,
        );
        let mut term = term;
        let stop = Arc::clone(&term.reader_stop);
        term.reader_thread = Some(thread::spawn(move || read_terminal_input(&input_tx, &stop)));
        Ok((term, handle))
    }

    /// Creates a virtual terminal for tests.
    ///
    /// No raw mode and no terminal reader: output goes to `output`, and input
    /// is injected through the returned sender. Dropping every returned
    /// sender closes input and makes later reads return [`Event::Eof`].
    pub fn new_virtual(
        width: usize,
        height: usize,
        left_prompt: impl Into<StyledText>,
        output: Box<dyn Write + Send>,
    ) -> (Self, TermHandle, Sender<RawEvent>) {
        let (term, handle, input_tx) =
            Self::spawn(width, height, left_prompt.into(), output, false);
        let (raw_tx, raw_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(raw) = raw_rx.recv() {
                if input_tx.send(InputMessage::Raw(raw)).is_err() {
                    return;
                }
            }
            let _ = input_tx.send(InputMessage::Shutdown);
        });
        (term, handle, raw_tx)
    }

    fn spawn(
        width: usize,
        height: usize,
        left_prompt: StyledText,
        output: Box<dyn Write + Send>,
        owns_raw_mode: bool,
    ) -> (Self, TermHandle, Sender<InputMessage>) {
        let state = Arc::new(Mutex::new(SharedState::new(width, height, left_prompt)));
        let (redraw_tx, redraw_rx) = mpsc::sync_channel(1);
        let sync_condvar = Arc::new(Condvar::new());
        let (input_tx, input_rx) = mpsc::channel();

        let redraw_state = Arc::clone(&state);
        let redraw_sync_cv = Arc::clone(&sync_condvar);
        let redraw_input_tx = input_tx.clone();
        let redraw_thread = thread::spawn(move || {
            render::redraw_loop(
                redraw_state,
                redraw_rx,
                output,
                redraw_input_tx,
                &redraw_sync_cv,
            );
        });

        let handle = TermHandle {
            state,
            sync_condvar,
            redraw: redraw_tx,
            input_tx: input_tx.clone(),
        };
        handle.notify_redraw();

        let term = Self {
            handle: handle.clone(),
            input_rx,
            reader_stop: Arc::new(AtomicBool::new(false)),
            reader_thread: None,
            redraw_thread: Some(redraw_thread),
            owns_raw_mode,
        };
        (term, handle, input_tx)
    }

    /// Returns the embedded [`TermHandle`]. Most callers can use `Term`'s
    /// `Deref<Target = TermHandle>` instead.
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Appends previously submitted prompts to input history, oldest first.
    ///
    /// Intended for startup seeding from persistent history. Empty prompts
    /// are ignored and the current buffer is left intact.
    pub fn seed_input_history(&self, history: impl IntoIterator<Item = String>) {
        let mut st = self.handle.lock();
        st.editor.input_history.extend(
            history
                .into_iter()
                .filter(|buffer| !buffer.is_empty())
                .map(PromptDraft::submitted),
        );
        st.editor.history_nav = None;
        st.limit_input_history();
    }

    /// Blocks until the next event the caller should handle.
    ///
    /// Handles prompt editing internally and requests a redraw before
    /// returning so the edit is visible.
    ///
    /// # Errors
    ///
    /// Returns terminal read errors (after which input is shut down), and
    /// the retained output failure once the redraw thread has stopped; see
    /// [`is_output_failure`].
    pub fn get_next_event(&self) -> io::Result<Event> {
        loop {
            let Some(raw) = self.next_raw()? else {
                return Ok(Event::Eof);
            };

            match raw {
                RawEvent::Key(key) => {
                    let event = self.handle_key(key);
                    self.handle.redraw();
                    if let Some(event) = event {
                        return Ok(event);
                    }
                }
                RawEvent::Resize(w, h) => {
                    let (width, height) = {
                        let mut st = self.handle.lock();
                        let width = effective_resize_dimension(w, st.terminal.width);
                        let height = effective_resize_dimension(h, st.terminal.height);
                        st.terminal.width = width;
                        st.terminal.height = height;
                        st.ensure_input_cursor_visible();
                        (width, height)
                    };
                    self.handle.redraw();
                    return Ok(Event::Resize {
                        width: size_event_dimension(width),
                        height: size_event_dimension(height),
                    });
                }
                RawEvent::Paste(text) => {
                    let event = self.insert_text(&normalize_paste_text(text));
                    self.handle.redraw();
                    if let Some(event) = event {
                        return Ok(event);
                    }
                }
            }
        }
    }

    /// Reads the next raw event, blocking until one arrives. Returns `None`
    /// once input is shut down.
    fn next_raw(&self) -> io::Result<Option<RawEvent>> {
        let check = || {
            let st = self.handle.lock();
            if let Some(error) = &st.terminal.output_failure {
                return Err(error.io_error());
            }
            Ok(!st.terminal.input_shutdown)
        };
        if !check()? {
            return Ok(None);
        }
        let message = self.input_rx.recv();
        if !check()? {
            return Ok(None);
        }
        match message {
            Ok(InputMessage::Raw(raw)) => Ok(Some(raw)),
            Ok(InputMessage::Shutdown) | Err(_) => {
                self.handle.lock().terminal.input_shutdown = true;
                Ok(None)
            }
            Ok(InputMessage::ReadError(error)) => {
                self.handle.lock().terminal.input_shutdown = true;
                Err(error)
            }
        }
    }

    // --- Prompt editing ---

    fn move_cursor_left(&self) {
        let mut st = self.handle.lock();
        let prev = previous_grapheme_boundary(&st.editor.buffer, st.editor.cursor);
        st.write_cursor(prev);
    }

    fn move_cursor_right(&self) {
        let mut st = self.handle.lock();
        let next = next_grapheme_boundary(&st.editor.buffer, st.editor.cursor);
        st.write_cursor(next);
    }

    fn move_cursor_start(&self) {
        self.handle.lock().write_cursor(0);
    }

    fn move_cursor_end(&self) {
        let mut st = self.handle.lock();
        let len = st.editor.buffer.len();
        st.write_cursor(len);
    }

    /// Replaces `start..end` of the buffer with `text` as one undoable edit
    /// and leaves the cursor after the inserted text.
    fn edit(&self, start: usize, end: usize, text: &str) -> Event {
        let mut st = self.handle.lock();
        st.record_undo();
        st.editor.buffer.replace_range(start..end, text);
        st.write_cursor(start + text.len());
        st.sync_buffer_to_history_nav();
        Event::BufferChanged
    }

    fn insert_text(&self, text: &str) -> Option<Event> {
        if text.is_empty() {
            return None;
        }
        let cursor = self.handle.lock().editor.cursor;
        Some(self.edit(cursor, cursor, text))
    }

    fn delete_backward(&self) -> Option<Event> {
        let (prev, cursor) = {
            let st = self.handle.lock();
            let cursor = st.editor.cursor;
            (
                previous_grapheme_boundary(&st.editor.buffer, cursor),
                cursor,
            )
        };
        (prev < cursor).then(|| self.edit(prev, cursor, ""))
    }

    fn delete_forward(&self) -> Option<Event> {
        let (cursor, next) = {
            let st = self.handle.lock();
            let cursor = st.editor.cursor;
            (cursor, next_grapheme_boundary(&st.editor.buffer, cursor))
        };
        (cursor < next).then(|| self.edit(cursor, next, ""))
    }

    fn kill_to_start(&self) -> Event {
        let cursor = self.handle.lock().editor.cursor;
        self.edit(0, cursor, "")
    }

    fn kill_word_left(&self) -> Option<Event> {
        let (start, cursor) = {
            let st = self.handle.lock();
            let cursor = st.editor.cursor;
            (word_left_boundary(&st.editor.buffer, cursor), cursor)
        };
        (0 < cursor).then(|| self.edit(start, cursor, ""))
    }

    /// Ctrl-C: clears a nonempty prompt (undoably), otherwise interrupts.
    fn handle_ctrl_c_key(&self) -> Event {
        let mut st = self.handle.lock();
        if st.editor.buffer.is_empty() {
            return Event::Interrupt;
        }
        st.record_undo();
        st.editor.buffer.clear();
        let abandoned_history_nav = st.editor.history_nav.take().is_some();
        st.write_cursor(0);
        if abandoned_history_nav {
            st.limit_input_history();
        }
        Event::BufferChanged
    }

    fn undo(&self) -> Option<Event> {
        self.handle.lock().undo().then_some(Event::BufferChanged)
    }

    fn redo(&self) -> Option<Event> {
        self.handle.lock().redo().then_some(Event::BufferChanged)
    }

    fn step_history(&self, delta: isize) -> Option<Event> {
        self.handle
            .lock()
            .step_history(delta)
            .then_some(Event::BufferChanged)
    }

    /// Plain Up/Down: move within a multiline buffer first, then through
    /// input history.
    fn move_or_step_history(&self, delta: isize) -> Option<Event> {
        let mut st = self.handle.lock();
        let target_col = st.vertical_target_col();
        if let Some(new_cursor) = move_cursor_vertical(&st, delta, target_col) {
            st.write_cursor_keep_sticky(new_cursor);
            return Some(Event::BufferChanged);
        }
        st.step_history(delta).then_some(Event::BufferChanged)
    }

    fn submit(&self) -> Event {
        let mut st = self.handle.lock();
        st.editor.history_nav = None;
        let line = st.editor.buffer.clone();
        st.push_current_as_history_entry();
        Event::Line(line)
    }

    fn handle_enter_key(&self, shift: bool, alt: bool) -> Event {
        if shift || alt {
            // Shift+Enter only reaches us when the terminal stack emits CSI-u
            // format (e.g. `\e[13;2u`): native kitty protocol, fixterms, or
            // tmux 3.5+ with `extended-keys-format csi-u`. Crossterm does NOT
            // parse the xterm modifyOtherKeys CSI-27 form (`\e[27;2;13~`), so
            // tmux configured with `extended-keys-format xterm` will swallow
            // it. Alt+Enter is the universal fallback because every
            // terminal sends `\e\r` for it regardless of protocol
            // negotiation.
            let cursor = self.handle.lock().editor.cursor;
            return self.edit(cursor, cursor, "\n");
        }
        self.submit()
    }

    fn handle_control_char_key(&self, ch: char) -> Option<Event> {
        match ch {
            'c' => Some(self.handle_ctrl_c_key()),
            'd' => self
                .handle
                .lock()
                .editor
                .buffer
                .is_empty()
                .then_some(Event::Eof),
            'a' => {
                self.move_cursor_start();
                None
            }
            'e' => {
                self.move_cursor_end();
                None
            }
            'u' => Some(self.kill_to_start()),
            'w' => self.kill_word_left(),
            'z' => self.undo(),
            'y' => self.redo(),
            'j' => self.step_history(1),
            'k' => self.step_history(-1),
            _ => None,
        }
    }

    fn handle_key(&self, key: KeyEvent) -> Option<Event> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => Some(self.handle_enter_key(shift, alt)),
            KeyCode::Char(ch) if ctrl => self.handle_control_char_key(ch.to_ascii_lowercase()),
            KeyCode::Char(ch) => self.insert_text(ch.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace => self.delete_backward(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => {
                self.move_cursor_left();
                None
            }
            KeyCode::Right => {
                self.move_cursor_right();
                None
            }
            KeyCode::Home => {
                self.move_cursor_start();
                None
            }
            KeyCode::End => {
                self.move_cursor_end();
                None
            }
            KeyCode::Up if ctrl => self.step_history(-1),
            KeyCode::Down if ctrl => self.step_history(1),
            KeyCode::Up => self.move_or_step_history(-1),
            KeyCode::Down => self.move_or_step_history(1),
            KeyCode::Tab => Some(Event::Key(Key::Tab)),
            KeyCode::BackTab => Some(Event::Key(Key::BackTab)),
            KeyCode::Esc => Some(Event::Key(Key::Esc)),
            _ => None,
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // Final render, cursor below all content, then the redraw thread
        // exits. The flag is checked before the redraw thread blocks again.
        self.handle.lock().terminal.shutdown = true;
        self.handle.notify_redraw();
        if let Some(thread) = self.redraw_thread.take() {
            let _ = thread.join();
        }
        self.reader_stop.store(true, Ordering::Release);
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        if self.owns_raw_mode {
            let _ = write_disable_terminal_features(&mut io::stdout());
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn word_left_boundary(buffer: &str, cursor: usize) -> usize {
    let before_cursor = &buffer[..cursor];
    let trimmed_end = before_cursor.trim_end_matches(char::is_whitespace).len();
    before_cursor[..trimmed_end]
        .char_indices()
        .rev()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index + ch.len_utf8()))
        .unwrap_or(0)
}

/// Reads terminal events on the reader thread until stopped, the input owner
/// goes away, or a read fails. Each poll is bounded so a stop request is
/// noticed within [`INPUT_POLL_SLICE`].
fn read_terminal_input(tx: &Sender<InputMessage>, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        let message = match read_raw_event_slice(event::poll, event::read, terminal::size) {
            Ok(None) => continue,
            Ok(Some(raw)) => InputMessage::Raw(raw),
            Err(error) => InputMessage::ReadError(error),
        };
        let failed = matches!(message, InputMessage::ReadError(_));
        if tx.send(message).is_err() || failed {
            return;
        }
    }
}

/// Waits up to one poll slice for a terminal event the prompt handles.
fn read_raw_event_slice(
    mut poll: impl FnMut(Duration) -> io::Result<bool>,
    mut read: impl FnMut() -> io::Result<CtEvent>,
    mut term_size: impl FnMut() -> io::Result<(u16, u16)>,
) -> io::Result<Option<RawEvent>> {
    let deadline = Instant::now() + INPUT_POLL_SLICE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !poll(remaining)? {
            return Ok(None);
        }
        match read()? {
            CtEvent::Key(key) => {
                // The kitty protocol may report Release events; drop them so
                // each keystroke fires exactly once downstream.
                if key.kind != KeyEventKind::Release {
                    return Ok(Some(RawEvent::Key(key)));
                }
            }
            CtEvent::Resize(w, h) => {
                let (actual_w, actual_h) = term_size().unwrap_or((0, 0));
                return Ok(Some(RawEvent::Resize(
                    resample_resize_dimension(w, actual_w),
                    resample_resize_dimension(h, actual_h),
                )));
            }
            CtEvent::Paste(text) => return Ok(Some(RawEvent::Paste(text))),
            // Mouse and focus events are not enabled; skip anything else.
            _ => {}
        }
    }
}

fn write_enable_terminal_features(writer: &mut impl Write) -> io::Result<()> {
    // Bracketed paste delivers a paste as one event instead of keystrokes
    // whose embedded newlines would submit. Escape disambiguation makes
    // combos like Shift+Enter distinct where the terminal supports it;
    // other terminals ignore the request.
    crossterm::execute!(
        writer,
        event::EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )
}

fn write_disable_terminal_features(writer: &mut impl Write) -> io::Result<()> {
    crossterm::execute!(
        writer,
        PopKeyboardEnhancementFlags,
        event::DisableBracketedPaste
    )
}

fn prompt_input_max_rows(terminal_height: usize) -> usize {
    (terminal_height.max(1) * PROMPT_INPUT_MAX_HEIGHT_PERCENT / 100).max(1)
}

fn prompt_scroll_indicator_rows(
    buffer_non_empty: bool,
    total_rows: usize,
    cap_rows: usize,
) -> usize {
    usize::from(buffer_non_empty && 2 <= cap_rows && cap_rows < total_rows)
}

fn prompt_editable_rows(total_rows: usize, cap_rows: usize, indicator_rows: usize) -> usize {
    cap_rows
        .saturating_sub(indicator_rows)
        .max(1)
        .min(total_rows.max(1))
}

fn prompt_scroll_indicator_text(
    start: usize,
    visible_rows: usize,
    total_rows: usize,
    width: usize,
) -> String {
    let end = (start + visible_rows).min(total_rows);
    let hidden_above = start;
    let hidden_below = total_rows.saturating_sub(end);
    let full = format!(
        "↕ prompt rows {}-{}/{}  ↑{} ↓{}",
        start + 1,
        end,
        total_rows,
        hidden_above,
        hidden_below
    );
    if display_width(&full) <= width {
        return full;
    }
    let compact = format!("↕ ↑{} ↓{}", hidden_above, hidden_below);
    if display_width(&compact) <= width {
        return compact;
    }
    truncate_to_width("↕", width)
}

fn viewport_start_with_cursor(
    viewport_start: usize,
    cursor_row: usize,
    total_rows: usize,
    height: usize,
) -> usize {
    let height = height.max(1);
    let max_start = total_rows.saturating_sub(height);
    let mut start = viewport_start.min(max_start);

    if cursor_row < start {
        start = cursor_row;
    } else if start + height <= cursor_row {
        start = (cursor_row + 1).saturating_sub(height);
    }

    start.min(max_start)
}

// --- Helpers ---

fn move_cursor_vertical(st: &SharedState, delta: isize, target_col: usize) -> Option<usize> {
    let width = st.terminal.width.max(1);
    let left_cols = st.editor.left_prompt.char_count();
    let (current_row, _) =
        buffer_position_for_byte(&st.editor.buffer, st.editor.cursor, width, left_cols);

    let target_row = current_row as isize + delta;
    if target_row < 0 {
        return None;
    }
    let target_row = target_row as usize;

    let (max_row, _) = buffer_end_position(&st.editor.buffer, width, left_cols);
    if max_row < target_row {
        return None;
    }

    Some(byte_offset_for_buffer_position(
        &st.editor.buffer,
        target_row,
        target_col,
        width,
        left_cols,
    ))
}

fn term_size() -> (usize, usize) {
    raw_term_size()
        .map(|(w, h)| (usize::from(w).max(1), usize::from(h).max(1)))
        .unwrap_or((80, 24))
}

fn raw_term_size() -> io::Result<(u16, u16)> {
    terminal::size()
}

fn resample_resize_dimension(reported: u16, actual: u16) -> u16 {
    if 0 < reported { reported } else { actual }
}

fn effective_resize_dimension(reported: u16, fallback: usize) -> usize {
    let reported = usize::from(reported);
    if 0 < reported {
        reported
    } else {
        fallback.max(1)
    }
}

fn size_event_dimension(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn normalize_paste_text(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }

    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(ch);
        }
    }
    normalized
}

fn is_prompt_line_break(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r")
}

fn initial_buffer_position(initial_cols: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    (initial_cols / width, initial_cols % width)
}

fn buffer_position_for_byte(
    s: &str,
    byte_pos: usize,
    width: usize,
    initial_cols: usize,
) -> (usize, usize) {
    let width = width.max(1);
    let mut pos = initial_buffer_position(initial_cols, width);
    let mut pending_exact_wrap = false;

    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(s, true) {
        if byte_pos <= byte || byte_pos < byte + grapheme.len() {
            break;
        }
        advance_prompt_cursor_position(
            &mut pos.0,
            &mut pos.1,
            &mut pending_exact_wrap,
            grapheme,
            width,
        );
    }

    pos
}

fn advance_prompt_cursor_position(
    row: &mut usize,
    col: &mut usize,
    pending_exact_wrap: &mut bool,
    grapheme: &str,
    width: usize,
) {
    let width = width.max(1);
    if is_prompt_line_break(grapheme) {
        if *pending_exact_wrap {
            // A printable character exactly filled the previous visual row, so
            // the cursor is already at column 0 of this row. An explicit
            // newline at that byte position should consume that
            // pending wrap, not add a second blank row.
            *pending_exact_wrap = false;
        } else {
            *row += 1;
            *col = 0;
        }
        return;
    }

    *pending_exact_wrap = false;
    let grapheme_width = display_width(grapheme);
    if 0 < *col && width < *col + grapheme_width {
        *row += 1;
        *col = 0;
    }
    *col += grapheme_width;
    if width <= *col {
        *row += *col / width;
        *col %= width;
        *pending_exact_wrap = grapheme_width != 0 && *col == 0;
    }
}

fn buffer_end_position(s: &str, width: usize, initial_cols: usize) -> (usize, usize) {
    buffer_position_for_byte(s, s.len(), width, initial_cols)
}

fn byte_offset_for_buffer_position(
    s: &str,
    target_row: usize,
    target_col: usize,
    width: usize,
    initial_cols: usize,
) -> usize {
    let mut row_col = initial_buffer_position(initial_cols, width);
    let mut pending_exact_wrap = false;

    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(s, true) {
        let (row, col) = row_col;
        if target_row < row || (target_row == row && target_col <= col) {
            return byte;
        }
        if is_prompt_line_break(grapheme) && !pending_exact_wrap && target_row == row {
            return byte;
        }

        let mut next = row_col;
        let mut next_pending_exact_wrap = pending_exact_wrap;
        advance_prompt_cursor_position(
            &mut next.0,
            &mut next.1,
            &mut next_pending_exact_wrap,
            grapheme,
            width,
        );
        if !is_prompt_line_break(grapheme)
            && (target_row < next.0 || (target_row == next.0 && target_col <= next.1))
        {
            return byte + grapheme.len();
        }
        row_col = next;
        pending_exact_wrap = next_pending_exact_wrap;
    }

    s.len()
}

fn clamp_cursor_to_grapheme_boundary(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    if cursor == s.len() {
        return cursor;
    }

    let mut boundary = 0;
    for (idx, _) in UnicodeSegmentation::grapheme_indices(s, true) {
        if cursor < idx {
            break;
        }
        boundary = idx;
    }
    boundary
}
