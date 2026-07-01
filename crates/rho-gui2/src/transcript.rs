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

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use editor::display_map::{BlockContext, BlockStyle};
use editor::{DisplayElisionId, DisplayElisionProperties, Editor};
use gpui::prelude::*;
use gpui::{Context, Entity, HighlightStyle};
use language::Buffer;
use multi_buffer::MultiBuffer;
use rho_ui_proto::remote::UiAgentState;
use text::{Anchor, ToOffset as _};
use ui::{Icon, IconName, IconSize, div};

use crate::render::elision::{ElisionEnd, elision_label, elision_plans};
use crate::render::{BlockKind, RenderedBlock, render_block, render_streaming_item};
use crate::store::FrameSummary;
use crate::style::{Region, StyleClass};

pub struct TranscriptModel {
    buffer: Entity<Buffer>,
    buffer_id: text::BufferId,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    records: Vec<BlockRecord>,
    frontier: FrontierRecord,
    sealed_styles: HashMap<StyleClass, Vec<Range<Anchor>>>,
    frontier_classes_applied: HashSet<StyleClass>,
    elisions: Vec<ActiveElision>,
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

#[derive(Clone)]
struct TimerRecord {
    range: Range<Anchor>,
    started_at_ms: u64,
}

struct ActiveElision {
    id: DisplayElisionId,
    range: Range<Anchor>,
    tool_count: usize,
    tail_rows: u32,
}

struct ElisionCandidate {
    range: Range<Anchor>,
    tool_count: usize,
    tail_rows: u32,
}

pub struct UserMessageGutter;

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
            elisions: Vec::new(),
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
        let rendered_blocks = match block_start {
            Some(start) => {
                let mut prev_kind = self.records[..start]
                    .iter()
                    .rev()
                    .find(|record| record.visible)
                    .map(|record| record.kind);
                let mut rendered = Vec::new();
                for block in state.blocks.get(start..).unwrap_or(&[]) {
                    let block = render_block(block, prev_kind, now_ms, cx);
                    if block.visible() {
                        prev_kind = Some(block.kind);
                    }
                    rendered.push(block);
                }
                Some(rendered)
            }
            None => None,
        };
        let rendered_frontier = {
            let mut prev_kind = match (&rendered_blocks, block_start) {
                (Some(rendered), Some(start)) => rendered
                    .iter()
                    .rev()
                    .find(|block| block.visible())
                    .map(|block| block.kind)
                    .or_else(|| {
                        self.records[..start]
                            .iter()
                            .rev()
                            .find(|record| record.visible)
                            .map(|record| record.kind)
                    }),
                _ => self
                    .records
                    .iter()
                    .rev()
                    .find(|record| record.visible)
                    .map(|record| record.kind),
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
        self.buffer.clone().update(cx, |buffer, cx| {
            if let Some(start) = block_start {
                let removed = self.records.split_off(start);
                for record in &removed {
                    for (class, count) in &record.style_counts {
                        if let Some(ranges) = self.sealed_styles.get_mut(class) {
                            ranges.truncate(ranges.len().saturating_sub(*count));
                        }
                        sealed_changed.insert(*class);
                    }
                }
                let start_offset = removed
                    .first()
                    .map(|record| record.range.start.to_offset(buffer))
                    .unwrap_or_else(|| self.frontier.start.to_offset(buffer));
                if start_offset < buffer.len() {
                    buffer.edit([(start_offset..buffer.len(), "")], None, cx);
                }

                for rendered in rendered_blocks.unwrap_or_default() {
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
                self.frontier.styles.clear();
                self.frontier.timers.clear();
                self.frontier.visible = false;
            } else {
                let start_offset = self.frontier.start.to_offset(buffer);
                if start_offset < buffer.len() {
                    buffer.edit([(start_offset..buffer.len(), "")], None, cx);
                }
                self.frontier.styles.clear();
                self.frontier.timers.clear();
                self.frontier.visible = false;
            }

            for rendered in rendered_frontier {
                append_frontier_item(buffer, cx, rendered, &mut self.frontier);
            }
        });

        self.apply_sealed_styles(&sealed_changed, cx);
        self.apply_frontier_styles(cx);
        if block_start.is_some() {
            self.refresh_gutters(cx);
        }
        self.refresh_elisions(state, cx);
        cx.notify();
    }

    /// Splices updated duration labels into running tools' timer spans.
    pub fn tick_timers<V: 'static>(&mut self, now_ms: u64, cx: &mut Context<V>) {
        if !self.has_timers() {
            return;
        }
        self.buffer.clone().update(cx, |buffer, cx| {
            let timers = self
                .records
                .iter()
                .filter_map(|record| record.timer.clone())
                .chain(self.frontier.timers.iter().cloned());
            let mut edits = Vec::new();
            for timer in timers {
                let start = timer.range.start.to_offset(buffer);
                let end = timer.range.end.to_offset(buffer);
                let new_text = crate::render::format_running_duration(timer.started_at_ms, now_ms);
                let old_text = buffer.text_for_range(start..end).collect::<String>();
                if old_text != new_text {
                    edits.push((start..end, new_text));
                }
            }
            if !edits.is_empty() {
                buffer.edit(edits, None, cx);
            }
        });
        cx.notify();
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
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let empty = Vec::new();
        let updates = changed
            .iter()
            .map(|class| {
                let ranges = self
                    .sealed_styles
                    .get(class)
                    .unwrap_or(&empty)
                    .iter()
                    .filter_map(|range| {
                        Some(snapshot.anchor_in_excerpt(range.start)?
                            ..snapshot.anchor_in_excerpt(range.end)?)
                    })
                    .collect::<Vec<_>>();
                (*class, ranges)
            })
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            for (class, ranges) in updates {
                editor.highlight_text(
                    class.highlight_key(Region::Sealed),
                    ranges,
                    class.resolve(cx),
                    cx,
                );
            }
        });
    }

    fn apply_frontier_styles<V: 'static>(&mut self, cx: &mut Context<V>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let updates = self
            .frontier
            .styles
            .iter()
            .map(|(class, ranges)| {
                let ranges = ranges
                    .iter()
                    .filter_map(|range| {
                        Some(snapshot.anchor_in_excerpt(range.start)?
                            ..snapshot.anchor_in_excerpt(range.end)?)
                    })
                    .collect::<Vec<_>>();
                (*class, ranges)
            })
            .collect::<Vec<_>>();
        let stale = self
            .frontier_classes_applied
            .iter()
            .filter(|class| !self.frontier.styles.contains_key(class))
            .copied()
            .collect::<Vec<_>>();
        self.frontier_classes_applied = self.frontier.styles.keys().copied().collect();
        self.editor.update(cx, |editor, cx| {
            for class in stale {
                editor.highlight_text(
                    class.highlight_key(Region::Frontier),
                    Vec::new(),
                    HighlightStyle::default(),
                    cx,
                );
            }
            for (class, ranges) in updates {
                editor.highlight_text(
                    class.highlight_key(Region::Frontier),
                    ranges,
                    class.resolve(cx),
                    cx,
                );
            }
        });
    }

    pub fn refresh_gutters<V: 'static>(&self, cx: &mut Context<V>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let ranges = self
            .records
            .iter()
            .filter_map(|record| record.gutter.as_ref())
            .filter_map(|range| {
                Some(snapshot.anchor_in_excerpt(range.start)?
                    ..snapshot.anchor_in_excerpt(range.end)?)
            })
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            editor.highlight_gutter::<UserMessageGutter>(
                ranges,
                crate::style::user_prompt_gutter_color,
                cx,
            );
        });
    }

    fn refresh_elisions<V: 'static>(&mut self, state: &UiAgentState, cx: &mut Context<V>) {
        let visible = self
            .records
            .iter()
            .map(|record| record.visible)
            .collect::<Vec<_>>();
        let plans = elision_plans(state, &visible, self.frontier.visible);
        let candidates = plans
            .iter()
            .filter_map(|plan| {
                let start = if plan.start_block < self.records.len() {
                    self.records[plan.start_block].range.start
                } else {
                    self.frontier.start
                };
                let end = match plan.end {
                    ElisionEnd::Block(index) => self.records.get(index)?.range.end,
                    ElisionEnd::Frontier => Anchor::max_for_buffer(self.buffer_id),
                };
                Some(ElisionCandidate {
                    range: start..end,
                    tool_count: plan.tool_count,
                    tail_rows: plan.tail_rows,
                })
            })
            .collect::<Vec<_>>();

        let mut removed_ids = self.elisions[candidates.len().min(self.elisions.len())..]
            .iter()
            .map(|elision| elision.id)
            .collect::<rustc_hash::FxHashSet<_>>();
        let mut updates = Vec::new();
        let mut inserted_candidates = Vec::new();
        let mut inserted_properties = Vec::new();
        let mut next_elisions = Vec::new();

        for (index, candidate) in candidates.into_iter().enumerate() {
            let Some(properties) = self.elision_properties(&candidate, cx) else {
                if let Some(existing) = self.elisions.get(index) {
                    removed_ids.insert(existing.id);
                }
                continue;
            };
            if let Some(existing) = self.elisions.get(index) {
                if existing.range != candidate.range
                    || existing.tool_count != candidate.tool_count
                    || existing.tail_rows != candidate.tail_rows
                {
                    updates.push((existing.id, properties));
                }
                next_elisions.push(ActiveElision {
                    id: existing.id,
                    range: candidate.range,
                    tool_count: candidate.tool_count,
                    tail_rows: candidate.tail_rows,
                });
            } else {
                inserted_properties.push(properties);
                inserted_candidates.push(candidate);
            }
        }

        let inserted_ids = self.editor.update(cx, |editor, cx| {
            if !removed_ids.is_empty() {
                editor.remove_display_elisions(removed_ids, None, cx);
            }
            if !updates.is_empty() {
                editor.update_display_elisions(updates, None, cx);
            }
            editor.insert_display_elisions(inserted_properties, None, cx)
        });
        next_elisions.extend(inserted_ids.into_iter().zip(inserted_candidates).map(
            |(id, candidate)| ActiveElision {
                id,
                range: candidate.range,
                tool_count: candidate.tool_count,
                tail_rows: candidate.tail_rows,
            },
        ));
        self.elisions = next_elisions;
    }

    fn elision_properties<V: 'static>(
        &self,
        candidate: &ElisionCandidate,
        cx: &Context<V>,
    ) -> Option<DisplayElisionProperties<multi_buffer::Anchor>> {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let start = snapshot.anchor_in_excerpt(candidate.range.start)?;
        let end = snapshot.anchor_in_excerpt(candidate.range.end)?;
        let label = elision_label(candidate.tool_count);
        Some(DisplayElisionProperties {
            range: start..end,
            tail_rows: candidate.tail_rows,
            height: Some(1),
            style: BlockStyle::Flex,
            render: Arc::new(move |cx| render_elision_block(&label, cx).into_any_element()),
            priority: 0,
            type_tag: None,
        })
    }

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
        if span.class == StyleClass::Default {
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
        if span.class == StyleClass::Default {
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
/// anchor range of each span. Timer spans get a right-biased end anchor so
/// in-place duration splices stay inside both the timer record and its
/// highlight range.
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
        let is_timer = rendered.timer.is_some_and(|spec| spec.span_index == index);
        let end_anchor = if is_timer {
            buffer.anchor_after(end)
        } else {
            buffer.anchor_before(end)
        };
        let range = buffer.anchor_before(offset)..end_anchor;
        if is_timer {
            timer = Some(TimerRecord {
                range: range.clone(),
                started_at_ms: rendered.timer.expect("checked above").started_at_ms,
            });
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

fn render_elision_block(label: &str, cx: &mut BlockContext<'_, '_>) -> impl IntoElement {
    let text_style = cx.editor_style.text.clone();
    let cursor_color = cx.editor_style.local_player.cursor;
    let text_color = if cx.selected {
        text_style.color
    } else {
        crate::style::hint_color(cx.app)
    };
    div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .h(cx.line_height)
        .flex()
        .items_center()
        .font_family(text_style.font_family.clone())
        .text_size(text_style.font_size)
        .line_height(text_style.line_height)
        .text_color(text_color)
        .child(
            div()
                .h(cx.line_height)
                .flex()
                .items_center()
                .gap_1()
                .pr_1()
                .when(cx.selected, |this| this.bg(cursor_color.opacity(0.22)))
                .child(
                    Icon::new(IconName::ChevronRight)
                        .size(IconSize::XSmall)
                        .color(text_color.into()),
                )
                .child(label.to_owned()),
        )
}
