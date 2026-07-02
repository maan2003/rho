//! Incremental transcript projection into a read-only buffer.
//!
//! The transcript buffer always equals `render(blocks) + render(frontier)`.
//! A [`FrameSummary`] bounds every update: blocks before `first_changed_block`
//! are never touched (their anchors, highlights, gutters and folds survive
//! untouched), and streaming-only frames splice just the frontier region.
//!
//! Highlights are bucketed per [`StyleClass`] into two editor highlight keys
//! each — sealed ranges change only when blocks change; frontier ranges are
//! small and replaced wholesale per streaming event.

mod elisions;
mod timers;

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use editor::Editor;
use gpui::{Context, Entity};
use language::Buffer;
use multi_buffer::MultiBuffer;
use project::InlayId;
use rho_ui_proto::remote::UiAgentState;
use text::{Anchor, ToOffset as _};

use crate::highlights::{apply_class_highlights, excerpt_range};
use crate::render::elision::{ElisionEnd, ElisionPlan};
use crate::render::{BlockKind, RenderedBlock, render_block, render_streaming_item};
use crate::store::FrameSummary;
use crate::style::{Region, StyleClass};
use elisions::ElisionSync;
use timers::TimerRecord;

pub struct TranscriptModel {
    buffer: Entity<Buffer>,
    buffer_id: text::BufferId,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    records: Vec<BlockRecord>,
    frontier: FrontierRecord,
    sealed_styles: HashMap<StyleClass, Vec<Range<Anchor>>>,
    frontier_classes_applied: HashSet<StyleClass>,
    elisions: ElisionSync,
    // Timer inlay ids share the editor's custom-inlay id space with the
    // prompt placeholder (id 0), so they start at 1.
    next_timer_inlay_id: usize,
}

struct BlockRecord {
    range: Range<Anchor>,
    kind: BlockKind,
    visible: bool,
    gutter: Option<Range<Anchor>>,
    timer: Option<TimerRecord>,
    style_counts: Vec<(StyleClass, usize)>,
}

struct FrontierRecord {
    start: Anchor,
    visible: bool,
    timers: Vec<TimerRecord>,
    styles: HashMap<StyleClass, Vec<Range<Anchor>>>,
}

struct UserMessageGutter;

impl TranscriptModel {
    pub fn new<V>(
        buffer: Entity<Buffer>,
        multi_buffer: Entity<MultiBuffer>,
        editor: Entity<Editor>,
        cx: &Context<V>,
    ) -> Self {
        let frontier_start = buffer.read(cx).anchor_before(0);
        let buffer_id = buffer.read(cx).remote_id();
        Self {
            buffer,
            buffer_id,
            multi_buffer,
            editor,
            records: Vec::new(),
            frontier: FrontierRecord {
                start: frontier_start,
                visible: false,
                timers: Vec::new(),
                styles: HashMap::new(),
            },
            sealed_styles: HashMap::new(),
            frontier_classes_applied: HashSet::new(),
            elisions: ElisionSync::default(),
            next_timer_inlay_id: 1,
        }
    }

    /// Applies a state change bounded by `summary`.
    pub fn sync<V: 'static>(
        &mut self,
        state: &UiAgentState,
        summary: FrameSummary,
        now_ms: u64,
        cx: &mut Context<V>,
    ) {
        if summary.is_noop() {
            return;
        }
        let block_start = summary
            .first_changed_block
            .map(|index| index.min(self.records.len()));

        // Render changed blocks and the frontier before any buffer mutation;
        // rendering only needs read access to the app (theme, languages).
        let changed_blocks = block_start.map(|start| {
            let mut prev_kind = last_visible_kind(&self.records[..start]);
            let mut rendered = Vec::new();
            for block in state.blocks.get(start..).unwrap_or(&[]) {
                let block = render_block(block, prev_kind, now_ms, cx);
                if block.visible() {
                    prev_kind = Some(block.kind);
                }
                rendered.push(block);
            }
            (start, rendered)
        });
        let rendered_frontier = {
            let mut prev_kind = match &changed_blocks {
                Some((start, rendered)) => rendered
                    .iter()
                    .rev()
                    .find(|block| block.visible())
                    .map(|block| block.kind)
                    .or_else(|| last_visible_kind(&self.records[..*start])),
                None => last_visible_kind(&self.records),
            };
            let mut rendered = Vec::new();
            for item in &state.pending_response {
                let item = render_streaming_item(item, prev_kind, now_ms, cx);
                if item.visible() {
                    prev_kind = Some(item.kind);
                }
                rendered.push(item);
            }
            rendered
        };

        let mut sealed_changed = HashSet::new();
        let mut stale_inlays = Vec::new();
        self.buffer.clone().update(cx, |buffer, cx| {
            if let Some((start, rendered_blocks)) = changed_blocks {
                let removed = self.records.split_off(start);
                for record in &removed {
                    for (class, count) in &record.style_counts {
                        if let Some(ranges) = self.sealed_styles.get_mut(class) {
                            ranges.truncate(ranges.len().saturating_sub(*count));
                        }
                        sealed_changed.insert(*class);
                    }
                    stale_inlays.extend(record.timer.as_ref().and_then(TimerRecord::inlay_id));
                }
                let start_offset = removed
                    .first()
                    .map(|record| record.range.start.to_offset(buffer))
                    .unwrap_or_else(|| self.frontier.start.to_offset(buffer));
                if start_offset < buffer.len() {
                    buffer.edit([(start_offset..buffer.len(), "")], None, cx);
                }

                for rendered in rendered_blocks {
                    let record = append_block(
                        buffer,
                        cx,
                        rendered,
                        &mut self.sealed_styles,
                        &mut sealed_changed,
                    );
                    self.records.push(record);
                }
                self.frontier.start = buffer.anchor_before(buffer.len());
            } else {
                let start_offset = self.frontier.start.to_offset(buffer);
                if start_offset < buffer.len() {
                    buffer.edit([(start_offset..buffer.len(), "")], None, cx);
                }
            }
            self.frontier.styles.clear();
            stale_inlays.extend(
                self.frontier
                    .timers
                    .drain(..)
                    .filter_map(|timer| timer.inlay_id()),
            );
            self.frontier.visible = false;

            for rendered in rendered_frontier {
                append_frontier_item(buffer, cx, rendered, &mut self.frontier);
            }
        });

        self.apply_sealed_styles(&sealed_changed, cx);
        self.apply_frontier_styles(cx);
        self.refresh_timer_inlays(now_ms, stale_inlays, cx);
        if block_start.is_some() {
            self.refresh_gutters(cx);
        }
        self.refresh_elisions(state, block_start, cx);
        cx.notify();
    }

    /// Refreshes running tools' duration inlays; buffer text is untouched.
    pub fn tick_timers<V: 'static>(&mut self, now_ms: u64, cx: &mut Context<V>) {
        if !self.has_timers() {
            return;
        }
        self.refresh_timer_inlays(now_ms, Vec::new(), cx);
        cx.notify();
    }

    fn refresh_timer_inlays<V: 'static>(
        &mut self,
        now_ms: u64,
        stale: Vec<InlayId>,
        cx: &mut Context<V>,
    ) {
        let Self {
            records,
            frontier,
            multi_buffer,
            editor,
            next_timer_inlay_id,
            ..
        } = self;
        timers::refresh_timer_inlays(
            records
                .iter_mut()
                .filter_map(|record| record.timer.as_mut())
                .chain(frontier.timers.iter_mut()),
            now_ms,
            stale,
            next_timer_inlay_id,
            multi_buffer,
            editor,
            cx,
        );
    }

    pub fn has_timers(&self) -> bool {
        !self.frontier.timers.is_empty()
            || self.records.iter().any(|record| record.timer.is_some())
    }

    fn apply_sealed_styles<V: 'static>(
        &self,
        changed: &HashSet<StyleClass>,
        cx: &mut Context<V>,
    ) {
        if changed.is_empty() {
            return;
        }
        let empty: &[Range<Anchor>] = &[];
        let styles = changed.iter().map(|class| {
            (
                *class,
                self.sealed_styles
                    .get(class)
                    .map(Vec::as_slice)
                    .unwrap_or(empty),
            )
        });
        apply_class_highlights(&self.editor, &self.multi_buffer, Region::Sealed, styles, cx);
    }

    fn apply_frontier_styles<V: 'static>(&mut self, cx: &mut Context<V>) {
        let stale = self
            .frontier_classes_applied
            .iter()
            .filter(|class| !self.frontier.styles.contains_key(class))
            .copied()
            .collect::<Vec<_>>();
        self.frontier_classes_applied = self.frontier.styles.keys().copied().collect();
        let empty: &[Range<Anchor>] = &[];
        let styles = stale.iter().map(|class| (*class, empty)).chain(
            self.frontier
                .styles
                .iter()
                .map(|(class, ranges)| (*class, ranges.as_slice())),
        );
        apply_class_highlights(
            &self.editor,
            &self.multi_buffer,
            Region::Frontier,
            styles,
            cx,
        );
    }

    fn refresh_gutters<V: 'static>(&self, cx: &mut Context<V>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let ranges = self
            .records
            .iter()
            .filter_map(|record| record.gutter.as_ref())
            .filter_map(|range| excerpt_range(&snapshot, range))
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            editor.highlight_gutter::<UserMessageGutter>(
                ranges,
                crate::style::user_prompt_gutter_color,
                cx,
            );
        });
    }

    fn refresh_elisions<V: 'static>(
        &mut self,
        state: &UiAgentState,
        first_changed_block: Option<usize>,
        cx: &mut Context<V>,
    ) {
        let visible = self
            .records
            .iter()
            .map(|record| record.visible)
            .collect::<Vec<_>>();
        let Self {
            records,
            frontier,
            buffer_id,
            elisions,
            multi_buffer,
            editor,
            ..
        } = self;
        elisions.refresh(
            state,
            first_changed_block,
            &visible,
            frontier.visible,
            |plan| plan_anchor_range(records, frontier.start, *buffer_id, plan),
            multi_buffer,
            editor,
            cx,
        );
    }
}

fn last_visible_kind(records: &[BlockRecord]) -> Option<BlockKind> {
    records
        .iter()
        .rev()
        .find(|record| record.visible)
        .map(|record| record.kind)
}

fn plan_anchor_range(
    records: &[BlockRecord],
    frontier_start: Anchor,
    buffer_id: text::BufferId,
    plan: &ElisionPlan,
) -> Option<Range<Anchor>> {
    let start = match records.get(plan.start_block) {
        Some(record) => record.range.start,
        None => frontier_start,
    };
    let end = match plan.end {
        ElisionEnd::Block(index) => records.get(index)?.range.end,
        ElisionEnd::Frontier => Anchor::max_for_buffer(buffer_id),
    };
    Some(start..end)
}

fn append_block(
    buffer: &mut Buffer,
    cx: &mut Context<Buffer>,
    rendered: RenderedBlock,
    sealed_styles: &mut HashMap<StyleClass, Vec<Range<Anchor>>>,
    sealed_changed: &mut HashSet<StyleClass>,
) -> BlockRecord {
    let start = buffer.len();
    let (span_ranges, timer, gutter) = append_spans(buffer, cx, &rendered);
    let mut style_counts: Vec<(StyleClass, usize)> = Vec::new();
    for (span, range) in rendered.spans.iter().zip(&span_ranges) {
        if span.class == StyleClass::Default || span.text.is_empty() {
            continue;
        }
        sealed_styles
            .entry(span.class)
            .or_default()
            .push(range.clone());
        sealed_changed.insert(span.class);
        match style_counts.iter_mut().find(|(class, _)| *class == span.class) {
            Some((_, count)) => *count += 1,
            None => style_counts.push((span.class, 1)),
        }
    }
    BlockRecord {
        range: buffer.anchor_before(start)..buffer.anchor_before(buffer.len()),
        kind: rendered.kind,
        visible: rendered.visible(),
        gutter,
        timer,
        style_counts,
    }
}

fn append_frontier_item(
    buffer: &mut Buffer,
    cx: &mut Context<Buffer>,
    rendered: RenderedBlock,
    frontier: &mut FrontierRecord,
) {
    if rendered.visible() {
        frontier.visible = true;
    }
    let (span_ranges, timer, _) = append_spans(buffer, cx, &rendered);
    for (span, range) in rendered.spans.iter().zip(&span_ranges) {
        if span.class == StyleClass::Default || span.text.is_empty() {
            continue;
        }
        frontier
            .styles
            .entry(span.class)
            .or_default()
            .push(range.clone());
    }
    frontier.timers.extend(timer);
}

/// Appends a rendered block's text at the end of the buffer and returns the
/// anchor range of each span. The timer span is empty; its position anchors
/// the running-tool duration inlay.
fn append_spans(
    buffer: &mut Buffer,
    cx: &mut Context<Buffer>,
    rendered: &RenderedBlock,
) -> (Vec<Range<Anchor>>, Option<TimerRecord>, Option<Range<Anchor>>) {
    let start = buffer.len();
    let text = rendered
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    if !text.is_empty() {
        buffer.edit([(start..start, text)], None, cx);
    }

    let mut ranges = Vec::with_capacity(rendered.spans.len());
    let mut timer = None;
    let mut gutter = None;
    let mut offset = start;
    for (index, span) in rendered.spans.iter().enumerate() {
        let end = offset + span.text.len();
        let range = buffer.anchor_before(offset)..buffer.anchor_before(end);
        if let Some(spec) = rendered.timer.filter(|spec| spec.span_index == index) {
            timer = Some(TimerRecord::new(range.start, spec.started_at_ms));
        }
        if rendered.gutter_span == Some(index) {
            let trimmed = span.text.trim_end_matches('\n').len();
            gutter = Some(buffer.anchor_before(offset)..buffer.anchor_before(offset + trimmed));
        }
        ranges.push(range);
        offset = end;
    }
    (ranges, timer, gutter)
}
