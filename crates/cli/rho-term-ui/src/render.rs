// Derived from dpc/tau (crates/tau-cli-term-raw/src/lib.rs), MPL-2.0.
// This file remains under MPL-2.0.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layout of the block zones and prompt, and the redraw thread that chooses
//! between the differential, scrolling, and full render paths.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::Print;
use crossterm::{QueueableCommand, terminal};

use crate::{
    BlockId, Cell, CellRow, InputMessage, OutputFailure, Screen, SharedState, Span, StyledBlock,
    StyledText, buffer_position_for_byte, emit_styled_cells, layout_block, layout_lines,
    prompt_editable_rows, prompt_input_max_rows, prompt_scroll_indicator_rows,
    prompt_scroll_indicator_text, viewport_start_with_cursor,
};

/// Lays out blocks referenced by an id list, skipping missing ids
/// and blocks with empty content (so callers can "hide" a block by
/// swapping its content to empty without leaving a blank row).
fn layout_id_list(
    ids: &[BlockId],
    blocks: &HashMap<BlockId, StyledBlock>,
    width: usize,
    out: &mut Vec<CellRow>,
) {
    for block in ids.iter().filter_map(|id| blocks.get(id)) {
        if !block.is_empty() {
            out.extend(layout_block(block, width).into_iter().map(CellRow::from));
        }
    }
}

/// Cached layout for persistent history blocks.
struct HistoryLayoutCache {
    /// Terminal width used to lay out cached entries.
    width: usize,
    /// Shared-state history generation represented by this cache.
    generation: u64,
    /// Generation represented before the most recent refresh.
    previous_generation: u64,
    /// Rendered line where an append-only refresh began.
    appended_from_line: Option<usize>,
    /// Start line for each cached history entry plus one final end offset.
    entry_line_offsets: Vec<usize>,
    /// Rendered persistent-history lines.
    lines: Vec<CellRow>,
}

impl Default for HistoryLayoutCache {
    fn default() -> Self {
        Self {
            width: 0,
            generation: 0,
            previous_generation: 0,
            appended_from_line: None,
            entry_line_offsets: vec![0],
            lines: Vec::new(),
        }
    }
}

impl HistoryLayoutCache {
    /// Refreshes the changed history suffix and returns entries laid out.
    fn refresh(&mut self, st: &mut SharedState) -> usize {
        if self.width == st.terminal.width && self.generation == st.layout.history_generation {
            return 0;
        }

        let previous_generation = self.generation;
        let previous_entry_count = self.entry_line_offsets.len().saturating_sub(1);
        let width_changed = self.width != st.terminal.width;
        let requested_dirty_from = st.layout.history_dirty_from.take().unwrap_or(0);
        let can_reuse_prefix = !width_changed
            && requested_dirty_from <= previous_entry_count
            && requested_dirty_from <= st.layout.history.len();
        let dirty_from = if can_reuse_prefix {
            requested_dirty_from
        } else {
            0
        };
        let line_start = self
            .entry_line_offsets
            .get(dirty_from)
            .copied()
            .unwrap_or(0);
        let append_only = can_reuse_prefix
            && dirty_from == previous_entry_count
            && previous_entry_count <= st.layout.history.len();

        self.lines.truncate(line_start);
        self.entry_line_offsets.truncate(dirty_from + 1);
        for id in &st.layout.history[dirty_from..] {
            layout_id_list(
                std::slice::from_ref(id),
                &st.layout.blocks,
                st.terminal.width,
                &mut self.lines,
            );
            self.entry_line_offsets.push(self.lines.len());
        }

        self.width = st.terminal.width;
        self.previous_generation = previous_generation;
        self.generation = st.layout.history_generation;
        self.appended_from_line = append_only.then_some(line_start);
        st.layout.history.len().saturating_sub(dirty_from)
    }

    /// Rebuilds an independent cache without consuming redraw dirty state.
    fn rebuild(st: &SharedState) -> Self {
        let mut cache = Self {
            width: st.terminal.width,
            generation: st.layout.history_generation,
            ..Self::default()
        };
        for id in &st.layout.history {
            layout_id_list(
                std::slice::from_ref(id),
                &st.layout.blocks,
                st.terminal.width,
                &mut cache.lines,
            );
            cache.entry_line_offsets.push(cache.lines.len());
        }
        cache
    }
}

/// Layout for everything after persistent history.
struct TailLayout {
    /// Lines for above-active plus fixed prompt and below rows.
    lines: Vec<CellRow>,
    /// Number of leading `lines` entries that belong to above-active.
    active_height: usize,
    /// Absolute cursor row after persistent history is prepended.
    cursor_row: usize,
    /// Cursor column.
    cursor_col: usize,
}

impl TailLayout {
    fn fixed_height(&self) -> usize {
        self.lines.len().saturating_sub(self.active_height)
    }
}

/// Result of laying out all content.
struct LayoutAll {
    /// All rendered lines without rubber (log + fixed area).
    all_lines: Vec<CellRow>,
    /// Index in `all_lines` where the fixed area starts.
    ///
    /// Lines before this are scrollable log content. Lines from this point on
    /// are the prompt and below area. Rubber rows may be inserted at
    /// this boundary to absorb visible log shrinkage without moving the fixed
    /// area upward.
    log_end: usize,
    /// Persistent-history generation used to build this layout.
    history_generation: u64,
    /// Terminal width used to build persistent-history lines.
    history_width: usize,
    /// Number of leading log rows owned by persistent history.
    history_height: usize,
    /// Absolute cursor row in `all_lines`.
    cursor_row: usize,
    /// Cursor column.
    cursor_col: usize,
}

struct ViewPlan {
    /// Top row of the physical terminal viewport within `render_lines`.
    viewport_start: usize,
    rubber_height: usize,
    render_lines: Vec<CellRow>,
    cursor_row: usize,
}

impl ViewPlan {
    fn visible_start(&self, _height: usize) -> usize {
        self.viewport_start.min(self.render_lines.len())
    }

    fn visible_lines(&self, height: usize) -> &[CellRow] {
        let start = self.visible_start(height);
        let end = (start + height).min(self.render_lines.len());
        &self.render_lines[start..end]
    }

    fn cursor_in_visible(&self, height: usize) -> usize {
        self.cursor_row.saturating_sub(self.visible_start(height))
    }
}

struct PlanMetrics {
    viewport_start: usize,
    rubber_height: usize,
    render_len: usize,
    cursor_row: usize,
}

/// Renderer-side model of the terminal content the renderer believes it owns.
///
/// `viewport_start` is the top row of the physical terminal viewport within
/// the most recent planned `render_lines`. Rows before
/// `viewport_start.min(known_lines.len())` are scrollable log rows already in
/// terminal scrollback. `rubber_height` is temporary blank space inserted
/// between log and fixed rows to absorb visible shrinkage before pulling rows
/// back from scrollback.
#[derive(Default)]
struct TerminalModel {
    /// First absolute row currently represented by the physical viewport.
    viewport_start: usize,
    /// Temporary blank rows retaining the viewport after visible shrinkage.
    rubber_height: usize,
    /// Persistent-history generation represented by `known_lines`.
    history_generation: u64,
    /// Width used for the represented persistent-history layout.
    history_width: usize,
    /// Leading persistent-history rows in `known_lines`.
    history_height: usize,
    /// Mutable active rows following persistent history in `known_lines`.
    active_height: usize,
    /// Complete represented log rows, including hidden scrollback.
    known_lines: Vec<CellRow>,
}

impl TerminalModel {
    fn desired_viewport_start(layout: &LayoutAll, height: usize) -> usize {
        layout.all_lines.len().saturating_sub(height)
    }

    fn history_cache_matches(&self, history: &HistoryLayoutCache) -> bool {
        self.history_generation == history.generation
            && self.history_width == history.width
            && history.lines.len() <= self.known_lines.len()
    }

    fn history_append_matches(&self, history: &HistoryLayoutCache) -> bool {
        self.history_generation == history.previous_generation
            && self.history_width == history.width
            && history.appended_from_line == Some(self.history_height)
            // History is inserted before the mutable active zone. If active rows
            // existed in the prior frame, an append can replace rather than
            // merely follow those rows; use the full hidden-prefix check.
            && self.active_height == 0
    }

    fn hidden_prefix_changed(&self, layout: &LayoutAll) -> bool {
        hidden_lines_changed(
            &self.known_lines,
            &layout.all_lines[..layout.log_end],
            self.viewport_start.min(layout.log_end),
        )
    }

    fn build_plan(layout: &LayoutAll, viewport_start: usize, rubber_height: usize) -> ViewPlan {
        let mut render_lines = Vec::with_capacity(layout.all_lines.len() + rubber_height);
        render_lines.extend_from_slice(&layout.all_lines[..layout.log_end]);
        render_lines
            .extend(std::iter::repeat_with(|| CellRow::new(Vec::new())).take(rubber_height));
        render_lines.extend_from_slice(&layout.all_lines[layout.log_end..]);

        let cursor_row = if layout.log_end <= layout.cursor_row {
            layout.cursor_row + rubber_height
        } else {
            layout.cursor_row
        };

        ViewPlan {
            viewport_start,
            rubber_height,
            render_lines,
            cursor_row,
        }
    }

    fn full_redraw_plan(layout: &LayoutAll, height: usize) -> ViewPlan {
        let plan = Self::build_plan(layout, Self::desired_viewport_start(layout, height), 0);
        Self::keep_cursor_visible(plan, height)
    }

    #[cfg(test)]
    fn bottom_aligned_plan(layout: &LayoutAll, height: usize) -> ViewPlan {
        let mut plan = Self::build_plan(layout, Self::desired_viewport_start(layout, height), 0);
        plan.viewport_start = plan.visible_start(height);
        plan
    }

    fn keep_cursor_visible(mut plan: ViewPlan, height: usize) -> ViewPlan {
        let height = height.max(1);
        let bottom_start = plan.visible_start(height);
        let viewport_start = viewport_start_with_cursor(
            bottom_start,
            plan.cursor_row,
            plan.render_lines.len(),
            height,
        );

        if viewport_start < bottom_start {
            let viewport_end = (viewport_start + height).min(plan.render_lines.len());
            plan.render_lines.truncate(viewport_end);
        }

        plan.viewport_start = plan.visible_start(height);
        plan
    }

    fn plan_metrics(
        &self,
        log_height: usize,
        fixed_height: usize,
        cursor_row: usize,
        height: usize,
    ) -> PlanMetrics {
        let height = height.max(1);
        let viewport_start = self.viewport_start.min(log_height);
        let mut rubber_height = self.rubber_height;

        if fixed_height < height {
            let occupied = log_height.saturating_sub(viewport_start) + rubber_height + fixed_height;
            if occupied < height {
                // Only create rubber after the viewport has overflowed once.
                // Before that, keep the normal terminal behavior where the
                // prompt follows the transcript instead of being bottom-pinned.
                if 0 < self.viewport_start || 0 < rubber_height {
                    rubber_height += height - occupied;
                }
            } else if height < occupied {
                let overflow = occupied - height;
                let consume_rubber = rubber_height.min(overflow);
                rubber_height -= consume_rubber;
            }
        } else {
            rubber_height = 0;
        }

        let render_len = log_height + rubber_height + fixed_height;
        let cursor_row = if log_height <= cursor_row {
            cursor_row + rubber_height
        } else {
            cursor_row
        };
        let bottom_start = render_len.saturating_sub(height);
        let visible_start =
            viewport_start_with_cursor(bottom_start, cursor_row, render_len, height);
        let render_len = if visible_start < bottom_start {
            (visible_start + height).min(render_len)
        } else {
            render_len
        };

        PlanMetrics {
            viewport_start: render_len.saturating_sub(height),
            rubber_height,
            render_len,
            cursor_row,
        }
    }

    fn plan_view(&self, layout: &LayoutAll, height: usize) -> ViewPlan {
        let fixed_height = layout.all_lines.len().saturating_sub(layout.log_end);
        let metrics = self.plan_metrics(layout.log_end, fixed_height, layout.cursor_row, height);
        let mut plan = Self::build_plan(layout, metrics.viewport_start, metrics.rubber_height);
        plan.cursor_row = metrics.cursor_row;
        plan.render_lines.truncate(metrics.render_len);
        plan.viewport_start = metrics.viewport_start;
        plan
    }

    fn apply_fast_plan(
        &mut self,
        history: &HistoryLayoutCache,
        tail: &TailLayout,
        metrics: &PlanMetrics,
    ) {
        self.viewport_start = metrics.viewport_start;
        self.rubber_height = metrics.rubber_height;
        self.history_generation = history.generation;
        self.history_width = history.width;
        self.known_lines.truncate(self.history_height);
        self.known_lines
            .extend_from_slice(&history.lines[self.history_height..]);
        self.history_height = history.lines.len();
        self.active_height = tail.active_height;
        self.known_lines
            .extend_from_slice(&tail.lines[..tail.active_height]);
    }

    fn reset_to_layout(&mut self, layout: &LayoutAll, viewport_start: usize, rubber_height: usize) {
        self.viewport_start = viewport_start;
        self.rubber_height = rubber_height;
        self.history_generation = layout.history_generation;
        self.history_width = layout.history_width;
        self.history_height = layout.history_height;
        self.active_height = layout.log_end.saturating_sub(layout.history_height);
        self.known_lines = layout.all_lines[..layout.log_end].to_vec();
    }
}

fn layout_tail(st: &SharedState, history_height: usize) -> TailLayout {
    let width = st.terminal.width;
    let mut lines: Vec<CellRow> = Vec::new();

    layout_id_list(
        &st.layout.above_active,
        &st.layout.blocks,
        width,
        &mut lines,
    );
    // The gap is drawn with the active blocks, so it never fossilizes into
    // the history above it.
    if st.editor.prompt_gap && history_height + lines.len() > 0 {
        lines.push(CellRow::new(Vec::new()));
    }
    let active_height = lines.len();
    let above_end = history_height + lines.len();

    let mut input_content = st.editor.left_prompt.clone();
    if st.editor.buffer.is_empty() {
        for span in st.editor.input_placeholder.spans() {
            input_content.push(span.clone());
        }
    } else {
        input_content.push(Span::plain(&st.editor.buffer));
    }
    // Preserve a trailing-newline blank row so a buffer ending in
    // `\n` (the user just hit Shift+Enter / Alt+Enter) gives the
    // cursor somewhere to sit and the prompt grows immediately
    // rather than only after the next typed character.
    let mut input_lines = layout_lines(&input_content, width, true);

    let left_cols = st.editor.left_prompt.char_count();
    let (buffer_cursor_row, cursor_col) =
        buffer_position_for_byte(&st.editor.buffer, st.editor.cursor, width, left_cols);
    // Prompt input is special because it owns a visible cursor. When the
    // cursor sits at the end and the final column has just been filled, it
    // must appear immediately at column 0 of the next visual row, growing the
    // prompt height before any further character is typed. This is easy to
    // overlook and has regressed repeatedly. Do not move this behavior into
    // general block layout: static blocks have no cursor and must not gain a
    // phantom trailing row just because their content exactly fills a line.
    while input_lines.len() <= buffer_cursor_row {
        input_lines.push(Vec::new());
    }

    if !st.editor.right_prompt.is_empty() && !input_lines.is_empty() {
        let first_line = &input_lines[0];
        let right_cells = st.editor.right_prompt.to_cells();
        let first_cols: usize = first_line.iter().map(|c| c.col_width()).sum();
        let right_cols: usize = right_cells.iter().map(|c| c.col_width()).sum();
        let needed = first_cols + 1 + right_cols;
        if needed <= width && input_lines.len() == 1 {
            let padding = width - first_cols - right_cols;
            let mut padded = first_line.clone();
            padded.extend(std::iter::repeat_n(Cell::plain(' '), padding));
            padded.extend(right_cells);
            input_lines[0] = padded;
        }
    }

    let input_total_rows = input_lines.len().max(1);
    let cap_rows = prompt_input_max_rows(st.terminal.height);
    let indicator_rows =
        prompt_scroll_indicator_rows(!st.editor.buffer.is_empty(), input_total_rows, cap_rows);
    let visible_input_rows = prompt_editable_rows(input_total_rows, cap_rows, indicator_rows);
    let viewport_start = viewport_start_with_cursor(
        st.editor.input_viewport_start,
        buffer_cursor_row,
        input_total_rows,
        visible_input_rows,
    );
    let cursor_row = above_end + indicator_rows + buffer_cursor_row.saturating_sub(viewport_start);

    if indicator_rows == 1 {
        let indicator = prompt_scroll_indicator_text(
            viewport_start,
            visible_input_rows,
            input_total_rows,
            width,
        );
        lines.push(StyledText::from(indicator).to_cells().into());
    }

    let viewport_end = (viewport_start + visible_input_rows).min(input_lines.len());
    lines.extend(
        input_lines
            .into_iter()
            .take(viewport_end)
            .skip(viewport_start)
            .map(CellRow::from),
    );
    layout_id_list(&st.layout.below, &st.layout.blocks, width, &mut lines);

    TailLayout {
        lines,
        active_height,
        cursor_row,
        cursor_col,
    }
}

fn layout_all_from_cached_history(history: &HistoryLayoutCache, tail: TailLayout) -> LayoutAll {
    let log_end = history.lines.len() + tail.active_height;
    let cursor_row = tail.cursor_row;
    let cursor_col = tail.cursor_col;
    let mut all_lines = Vec::with_capacity(history.lines.len() + tail.lines.len());
    all_lines.extend_from_slice(&history.lines);
    all_lines.extend(tail.lines);

    LayoutAll {
        all_lines,
        log_end,
        history_generation: history.generation,
        history_width: history.width,
        history_height: history.lines.len(),
        cursor_row,
        cursor_col,
    }
}

/// Lays out the full content (history + above + input + below).
fn layout_all(st: &SharedState) -> LayoutAll {
    let history = HistoryLayoutCache::rebuild(st);
    let tail = layout_tail(st, history.lines.len());
    layout_all_from_cached_history(&history, tail)
}

fn visible_lines_from_parts(
    history_lines: &[CellRow],
    tail: &TailLayout,
    metrics: &PlanMetrics,
) -> Vec<CellRow> {
    render_rows_from(history_lines, tail, metrics, metrics.viewport_start)
}

/// Materializes rendered rows from `start` through the current plan end.
fn render_rows_from(
    history_lines: &[CellRow],
    tail: &TailLayout,
    metrics: &PlanMetrics,
    start: usize,
) -> Vec<CellRow> {
    let history_height = history_lines.len();
    let log_height = history_height + tail.active_height;
    let fixed_start = log_height + metrics.rubber_height;
    let mut rows = Vec::with_capacity(metrics.render_len.saturating_sub(start));

    for idx in start..metrics.render_len {
        if idx < history_height {
            rows.push(
                history_lines
                    .get(idx)
                    .expect("requested history row should exist")
                    .clone(),
            );
        } else if idx < log_height {
            rows.push(
                tail.lines
                    .get(idx - history_height)
                    .expect("requested active row should exist")
                    .clone(),
            );
        } else if idx < fixed_start {
            rows.push(CellRow::new(Vec::new()));
        } else {
            rows.push(
                tail.lines
                    .get(tail.active_height + idx - fixed_start)
                    .expect("requested fixed row should exist")
                    .clone(),
            );
        }
    }

    rows
}

/// Builds the bounded scrolling input rebased at the prior viewport.
fn scrolling_suffix(
    history_lines: &[CellRow],
    tail: &TailLayout,
    metrics: &PlanMetrics,
    terminal_model: &TerminalModel,
) -> Vec<CellRow> {
    render_rows_from(history_lines, tail, metrics, terminal_model.viewport_start)
}

// --- Redraw thread ---

enum RenderFrame {
    Fast {
        tail: TailLayout,
        metrics: PlanMetrics,
    },
    Full {
        layout: LayoutAll,
    },
}

struct RedrawPass {
    width: usize,
    height: usize,
    size_changed: bool,
    force_full: bool,
    sync_gen: u64,
    redraw_history_size: usize,
    frame: RenderFrame,
}

pub(crate) fn redraw_loop(
    state: Arc<Mutex<SharedState>>,
    notify_rx: Receiver<()>,
    writer: Box<dyn Write + Send>,
    input_tx: Sender<InputMessage>,
    sync_condvar: &Condvar,
) {
    let mut writer = BufWriter::new(writer);
    let (w, h) = {
        let st = state.lock().expect("term state mutex poisoned");
        (st.terminal.width, st.terminal.height)
    };
    let mut screen = Screen::new(w);
    let mut prev_width = w;
    let mut prev_height = h;
    let mut history_cache = HistoryLayoutCache::default();
    let mut terminal_model = TerminalModel::default();

    loop {
        if render_shutdown_if_requested(
            &state,
            &mut writer,
            &mut screen,
            &terminal_model,
            prev_width,
            sync_condvar,
        ) {
            break;
        }

        if !wait_for_redraw_or_sync(&state, &notify_rx) {
            break;
        }

        let pass = prepare_redraw_pass(
            &state,
            &mut history_cache,
            &terminal_model,
            prev_width,
            prev_height,
        );
        let result = render_redraw_pass(
            &state,
            &mut writer,
            &mut screen,
            &history_cache,
            &mut terminal_model,
            &pass,
        )
        .and_then(|()| writer.flush());
        if let Err(error) = result {
            fail_terminal_output(&state, &input_tx, sync_condvar, error);
            discard_failed_output(writer);
            return;
        }

        prev_width = pass.width;
        prev_height = pass.height;
        complete_redraw_sync(&state, pass.sync_gen, sync_condvar);
    }
}

/// Drops a failed buffered writer without retrying bytes retained in its
/// userspace buffer.
fn discard_failed_output(writer: BufWriter<Box<dyn Write + Send>>) {
    let _ = writer.into_parts();
}

fn render_shutdown_if_requested(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &TerminalModel,
    prev_width: usize,
    sync_condvar: &Condvar,
) -> bool {
    let st = state.lock().expect("term state mutex poisoned");
    if !st.terminal.shutdown {
        return false;
    }

    // Final render + move cursor below all content.
    let layout = layout_all(&st);
    let height = st.terminal.height.max(1);
    let plan = terminal_model.plan_view(&layout, height);
    let visible = plan.visible_lines(height);
    let cursor_in_visible = plan.cursor_in_visible(height);
    drop(st);

    screen.set_width(prev_width);
    let _ = screen.update(writer, visible, (cursor_in_visible, layout.cursor_col));
    let below = plan.render_lines.len().saturating_sub(plan.cursor_row + 1);
    for _ in 0..=below {
        let _ = writer.queue(Print("\r\n"));
    }
    let _ = writer.flush();
    {
        let mut st = state.lock().expect("term state mutex poisoned");
        st.terminal.sync_completed = st.terminal.sync_requested;
    }
    sync_condvar.notify_all();
    true
}

fn wait_for_redraw_or_sync(state: &Arc<Mutex<SharedState>>, notify_rx: &Receiver<()>) -> bool {
    // If a sync was requested but not yet completed, skip blocking on recv and
    // render immediately. Otherwise block until the next notification arrives.
    {
        let st = state.lock().expect("term state mutex poisoned");
        if st.terminal.sync_completed < st.terminal.sync_requested {
            return true;
        }
    }
    notify_rx.recv().is_ok()
}

fn prepare_redraw_pass(
    state: &Arc<Mutex<SharedState>>,
    history_cache: &mut HistoryLayoutCache,
    terminal_model: &TerminalModel,
    prev_width: usize,
    prev_height: usize,
) -> RedrawPass {
    let mut st = state.lock().expect("term state mutex poisoned");
    let width = st.terminal.width;
    let height = st.terminal.height.max(1);
    let size_changed = prev_width != width || prev_height != height;
    // Take-and-clear so the flag is one-shot.
    let force_full = std::mem::take(&mut st.terminal.invalidate_screen);
    // Capture the sync generation we're rendering against. We must not advance
    // sync_completed beyond this value, because a later bump to sync_requested
    // may have arrived with state changes we haven't read yet.
    let sync_gen = st.terminal.sync_requested;
    let redraw_history_size = st.terminal.redraw_history_size;

    history_cache.refresh(&mut st);
    let tail = layout_tail(&st, history_cache.lines.len());
    let log_height = history_cache.lines.len() + tail.active_height;
    let fixed_height = tail.fixed_height();
    let metrics = terminal_model.plan_metrics(log_height, fixed_height, tail.cursor_row, height);
    let can_fast = !size_changed
        && !force_full
        && ((terminal_model.history_cache_matches(history_cache)
            && metrics.viewport_start == terminal_model.viewport_start)
            || (terminal_model.history_append_matches(history_cache)
                && terminal_model.viewport_start <= metrics.viewport_start))
        && metrics.viewport_start <= history_cache.lines.len();
    let frame = if can_fast {
        RenderFrame::Fast { tail, metrics }
    } else {
        RenderFrame::Full {
            layout: layout_all_from_cached_history(history_cache, tail),
        }
    };
    RedrawPass {
        width,
        height,
        size_changed,
        force_full,
        sync_gen,
        redraw_history_size,
        frame,
    }
}

fn render_redraw_pass(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    history_cache: &HistoryLayoutCache,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
) -> io::Result<()> {
    if pass.force_full {
        // Something else wrote to the terminal. Wipe Screen's idea of what is
        // on it so `full_render` redraws from scratch.
        screen.invalidate();
    }

    match &pass.frame {
        RenderFrame::Fast { tail, metrics } => render_fast_frame(
            writer,
            screen,
            history_cache,
            terminal_model,
            pass,
            tail,
            metrics,
        ),
        RenderFrame::Full { layout } => {
            render_full_frame(state, writer, screen, terminal_model, pass, layout)
        }
    }
}

fn render_fast_frame(
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    history_cache: &HistoryLayoutCache,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    tail: &TailLayout,
    metrics: &PlanMetrics,
) -> io::Result<()> {
    screen.set_width(pass.width);
    if terminal_model.viewport_start < metrics.viewport_start {
        let previous_viewport_start = terminal_model.viewport_start;
        // `Screen` retains only the old visible rows. Rebase the bounded suffix
        // beginning at that viewport to row zero: `prev_viewport_top = 0` then
        // describes the same physical screen without copying or comparing older
        // terminal scrollback.
        let suffix = scrolling_suffix(&history_cache.lines, tail, metrics, terminal_model);
        let cursor_row = metrics.cursor_row.saturating_sub(previous_viewport_start);
        screen.render_scrolling(
            writer,
            &suffix,
            0,
            pass.height,
            (cursor_row, tail.cursor_col),
        )?;
    } else {
        let visible = visible_lines_from_parts(&history_cache.lines, tail, metrics);
        let cursor_in_visible = metrics.cursor_row.saturating_sub(metrics.viewport_start);
        screen.update(writer, &visible, (cursor_in_visible, tail.cursor_col))?;
    }
    terminal_model.apply_fast_plan(history_cache, tail, metrics);
    Ok(())
}

fn render_full_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
) -> io::Result<()> {
    screen.set_width(pass.width);
    if pass.size_changed || pass.force_full {
        return render_marked_full_frame(state, writer, screen, terminal_model, pass, layout);
    }

    let incremental_plan = terminal_model.plan_view(layout, pass.height);
    if incremental_plan.viewport_start < terminal_model.viewport_start
        || terminal_model.hidden_prefix_changed(layout)
    {
        // Either the desired viewport moved upward to keep the input cursor
        // visible, so rows that should re-enter the screen exist only in
        // terminal scrollback, or scrollback rows changed content. Neither can
        // be patched incrementally: repaint from scratch, discarding rubber.
        render_marked_full_frame(state, writer, screen, terminal_model, pass, layout)
    } else if terminal_model.viewport_start < incremental_plan.viewport_start {
        render_scrolling_frame(
            writer,
            screen,
            terminal_model,
            pass,
            layout,
            incremental_plan,
        )
    } else {
        render_diff_frame(
            writer,
            screen,
            terminal_model,
            pass,
            layout,
            incremental_plan,
        )
    }
}

fn render_marked_full_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
) -> io::Result<()> {
    let plan = TerminalModel::full_redraw_plan(layout, pass.height);
    state
        .lock()
        .expect("term state mutex poisoned")
        .terminal
        .full_render_count += 1;
    full_render(
        writer,
        screen,
        layout,
        &plan,
        pass.width,
        pass.height,
        pass.redraw_history_size,
    )?;
    let viewport_start =
        full_render_effective_viewport_start(layout, &plan, pass.height, pass.redraw_history_size);
    terminal_model.reset_to_layout(layout, viewport_start, plan.rubber_height);
    Ok(())
}

fn render_scrolling_frame(
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
    plan: ViewPlan,
) -> io::Result<()> {
    // Content pushed log rows off the top. Use the scrolling renderer
    // (Pi-style). Rubber is part of the virtual tail, so it shrinks before any
    // extra log row enters scrollback.
    screen.render_scrolling(
        writer,
        &plan.render_lines,
        terminal_model.viewport_start,
        pass.height,
        (plan.cursor_row, layout.cursor_col),
    )?;
    terminal_model.reset_to_layout(layout, plan.viewport_start, plan.rubber_height);
    Ok(())
}

fn render_diff_frame(
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
    plan: ViewPlan,
) -> io::Result<()> {
    // No new scrollback rows — normal differential update. This includes
    // visible shrinkage: rubber grows instead of moving the viewport
    // upward.
    let visible = plan.visible_lines(pass.height);
    let cursor_in_visible = plan.cursor_in_visible(pass.height);
    screen.update(writer, visible, (cursor_in_visible, layout.cursor_col))?;
    terminal_model.reset_to_layout(layout, plan.viewport_start, plan.rubber_height);
    Ok(())
}

fn complete_redraw_sync(state: &Arc<Mutex<SharedState>>, sync_gen: u64, sync_condvar: &Condvar) {
    // Advance sync_completed to the generation we captured before rendering.
    // Using max() is defensive — renders are sequential so sync_gen is
    // monotonically increasing, but max() makes the invariant explicit.
    {
        let mut st = state.lock().expect("term state mutex poisoned");
        st.terminal.sync_completed = st.terminal.sync_completed.max(sync_gen);
    }
    sync_condvar.notify_all();
}

/// Records the first output error, releases redraw waiters, and wakes the
/// input owner.
fn fail_terminal_output(
    state: &Arc<Mutex<SharedState>>,
    input_tx: &Sender<InputMessage>,
    sync_condvar: &Condvar,
    error: io::Error,
) {
    let mut st = state.lock().expect("term state mutex poisoned");
    if st.terminal.output_failure.is_none() {
        st.terminal.output_failure = Some(OutputFailure::new(error));
    }
    st.terminal.input_shutdown = true;
    st.terminal.sync_completed = st.terminal.sync_requested;
    drop(st);
    sync_condvar.notify_all();
    let _ = input_tx.send(InputMessage::Shutdown);
}

fn hidden_lines_changed(
    prev_all_lines: &[CellRow],
    all_lines: &[CellRow],
    prev_visible_start: usize,
) -> bool {
    (0..prev_visible_start).any(|idx| prev_all_lines.get(idx) != all_lines.get(idx))
}

fn full_render_replay_start(
    layout: &LayoutAll,
    plan: &ViewPlan,
    redraw_history_size: usize,
) -> usize {
    let total = plan.render_lines.len();
    let log_end = layout.log_end.min(total);
    log_end.saturating_sub(redraw_history_size)
}

fn full_render_effective_viewport_start(
    layout: &LayoutAll,
    plan: &ViewPlan,
    height: usize,
    redraw_history_size: usize,
) -> usize {
    let replay_start = full_render_replay_start(layout, plan, redraw_history_size);
    let replay_len = plan.render_lines.len().saturating_sub(replay_start);
    if height < replay_len {
        plan.render_lines.len().saturating_sub(height)
    } else {
        replay_start
    }
}

/// Full re-render: clear screen + scrollback, output the configured suffix of
/// rendered history/log rows plus the fixed tail, and position the cursor. Used
/// on resize and after invalidation. Callers should pass a no-rubber plan so a
/// full repaint drops rubber instead of preserving temporary blank space.
/// Overflow rebuilds recent terminal scrollback naturally. After rendering,
/// Screen tracks the visible viewport for subsequent differential updates.
fn full_render(
    stdout: &mut impl Write,
    screen: &mut Screen,
    layout: &LayoutAll,
    plan: &ViewPlan,
    width: usize,
    height: usize,
    redraw_history_size: usize,
) -> io::Result<()> {
    screen.set_width(width);

    let all_lines = &plan.render_lines;
    let replay_start = full_render_replay_start(layout, plan, redraw_history_size);
    let replay_lines = &all_lines[replay_start..];
    let replay_total = replay_lines.len();
    let effective_viewport_start =
        full_render_effective_viewport_start(layout, plan, height, redraw_history_size);

    with_synchronized_update(stdout, |stdout| {
        // Clear screen, home cursor, and clear scrollback. The scrollback is
        // rebuilt by replaying the capped no-rubber suffix below.
        // Disable autowrap while replaying so exact-width rows don't
        // create phantom blank rows before the explicit CRLF between
        // logical rows.
        stdout.queue(Print("\x1b[2J\x1b[H\x1b[3J\x1b[?7l"))?;

        // Output the capped logical suffix starting at the top. Overflow
        // scrolls into scrollback naturally. Short content stays at the
        // top, so the prompt sits directly under content instead of
        // being bottom-pinned by rubber.
        for (i, line) in replay_lines.iter().enumerate() {
            if 0 < i {
                stdout.queue(Print("\r\n"))?;
            }
            emit_styled_cells(stdout, line)?;
        }

        stdout.queue(Print("\x1b[?7h"))?;

        // After outputting, the cursor is at the last content line. When
        // content overflowed, that line is at the terminal bottom;
        // otherwise it is at its natural row below the transcript.
        let current_screen_row = if height <= replay_total {
            height - 1
        } else {
            replay_total.saturating_sub(1)
        };

        let cursor_screen_row = plan.cursor_row.saturating_sub(effective_viewport_start);

        let up = current_screen_row.saturating_sub(cursor_screen_row);
        if 0 < up {
            stdout.queue(MoveUp(up as u16))?;
        }
        stdout.queue(MoveToColumn(layout.cursor_col as u16))?;
        Ok(())
    })?;

    // Track what's visible on the terminal so the next
    // screen.update() can diff correctly.
    let visible_end = (effective_viewport_start + height).min(plan.render_lines.len());
    let visible_lines = plan.render_lines[effective_viewport_start..visible_end].to_vec();
    let cursor_in_visible = plan.cursor_row.saturating_sub(effective_viewport_start);
    screen.reset_to(visible_lines, cursor_in_visible, layout.cursor_col);

    Ok(())
}

/// Queues one balanced synchronized-update transaction without flushing.
///
/// The closing marker is attempted even when `body` fails. If both operations
/// fail, the body error takes precedence because it identifies the first loss
/// of frame output.
fn with_synchronized_update<W, F>(writer: &mut W, body: F) -> io::Result<()>
where
    W: Write,
    F: FnOnce(&mut W) -> io::Result<()>,
{
    writer.queue(terminal::BeginSynchronizedUpdate)?;
    let body_result = body(writer);
    let end_result = writer.queue(terminal::EndSynchronizedUpdate).map(|_| ());
    body_result.and(end_result)
}

// The crate's tests live in `src/tests.rs` but are declared here so they can
// reach the renderer's private plan and model types as well as the crate
// root's private state.
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
