//! Incremental transcript projection into a read-only buffer.
//!
//! The transcript buffer always equals `render(blocks)`, one record per
//! protocol block. A [`FrameSummary`] bounds every update: blocks before
//! `first_changed_block` are never touched (their anchors, highlights,
//! gutters and folds survive untouched); everything after is re-rendered.
//!
//! Highlights are bucketed per [`StyleClass`] into two editor highlight keys
//! each, split at the start of the live turn (after the last user message) —
//! history ranges change at most once per turn; live-turn ranges are small,
//! so per-streaming-event churn stays bounded. The boundary is derived from
//! the block list itself; moving it re-buckets highlights without touching
//! the buffer.

mod elisions;
mod inlays;

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use editor::Editor;
use elisions::ElisionSync;
use gpui::{Context, Entity};
use inlays::InlayRecord;
use language::Buffer;
use multi_buffer::MultiBuffer;
use project::InlayId;
use rho_ui_proto::remote::UiAgentState;
use text::{Anchor, ToOffset as _};

use crate::highlights::{apply_class_highlights, excerpt_range};
use crate::render::elision::ElisionPlan;
use crate::render::{BlockKind, RenderedBlock, render_block_with_agent_labels};
use crate::store::{FrameSummary, IncrementalUpdate};
use crate::style::{Region, StyleClass};

pub struct TranscriptModel {
    buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    records: Vec<BlockRecord>,
    /// First record of the live turn as of the last sync. Records before it
    /// carry their highlights in the history region, records from it onward
    /// in the live-turn region.
    turn_boundary: usize,
    elisions: ElisionSync,
    // Custom inlay ids share the editor's id space with the prompt
    // placeholder (id 0), so they start at 1.
    next_inlay_id: usize,
}

struct BlockRecord {
    range: Range<Anchor>,
    kind: BlockKind,
    visible: bool,
    text: String,
    gutter: Option<Range<Anchor>>,
    inlay: Option<InlayRecord>,
    styles: Vec<(StyleClass, Range<Anchor>)>,
}

struct UserMessageGutter;

impl TranscriptModel {
    pub fn new<V>(
        buffer: Entity<Buffer>,
        multi_buffer: Entity<MultiBuffer>,
        editor: Entity<Editor>,
        _cx: &Context<V>,
    ) -> Self {
        Self {
            buffer,
            multi_buffer,
            editor,
            records: Vec::new(),
            turn_boundary: 0,
            elisions: ElisionSync::default(),
            next_inlay_id: 1,
        }
    }

    /// Applies a state change bounded by `summary`.
    pub fn sync<V: 'static>(
        &mut self,
        state: &UiAgentState,
        summary: FrameSummary,
        now_ms: u64,
        agent_label: &impl Fn(rho_ui_proto::AgentId) -> String,
        cx: &mut Context<V>,
    ) {
        let Some(first_changed) = summary.first_changed_block else {
            return;
        };

        if let Some(incremental) = summary.incremental
            && self.try_incremental_sync(state, first_changed, incremental, now_ms, agent_label, cx)
        {
            return;
        }

        let start = first_changed.min(self.records.len());

        // Render changed blocks before any buffer mutation; rendering only
        // needs read access to the app (theme, languages).
        let mut prev_kind = last_visible_kind(&self.records[..start]);
        let rendered_blocks = state
            .blocks
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .map(|block| {
                let block =
                    render_block_with_agent_labels(block, prev_kind, now_ms, agent_label, cx);
                if block.visible() {
                    prev_kind = Some(block.kind);
                }
                block
            })
            .collect::<Vec<_>>();

        let old_boundary = self.turn_boundary;
        let mut changed_history = HashSet::new();
        let mut changed_live = HashSet::new();
        let mut stale_inlays = Vec::new();
        let mut gutters_changed = false;
        self.buffer.clone().update(cx, |buffer, cx| {
            let removed = self.records.split_off(start);
            for (offset, record) in removed.iter().enumerate() {
                let changed = if start + offset < old_boundary {
                    &mut changed_history
                } else {
                    &mut changed_live
                };
                for (class, _) in &record.styles {
                    changed.insert(*class);
                }
                stale_inlays.extend(record.inlay.as_ref().and_then(InlayRecord::inlay_id));
                gutters_changed |= record.gutter.is_some();
            }
            let start_offset = removed
                .first()
                .map(|record| record.range.start.to_offset(buffer))
                .unwrap_or_else(|| buffer.len());
            if start_offset < buffer.len() {
                buffer.edit([(start_offset..buffer.len(), "")], None, cx);
            }

            for rendered in rendered_blocks {
                let record = append_block(buffer, cx, rendered);
                gutters_changed |= record.gutter.is_some();
                self.records.push(record);
            }
        });

        let new_boundary = turn_boundary(&self.records);
        for (index, record) in self.records.iter().enumerate().skip(start) {
            let changed = if index < new_boundary {
                &mut changed_history
            } else {
                &mut changed_live
            };
            for (class, _) in &record.styles {
                changed.insert(*class);
            }
        }
        // Records the boundary moved across keep their text and anchors but
        // switch highlight regions; re-bucket both sides. Records at or past
        // `start` were re-rendered and are already counted above.
        let migrated_end = old_boundary.max(new_boundary).min(start);
        let migrated_start = old_boundary.min(new_boundary).min(migrated_end);
        for record in &self.records[migrated_start..migrated_end] {
            for (class, _) in &record.styles {
                changed_history.insert(*class);
                changed_live.insert(*class);
            }
        }
        self.turn_boundary = new_boundary;

        self.apply_region_styles(Region::History, &changed_history, cx);
        self.apply_region_styles(Region::LiveTurn, &changed_live, cx);
        self.refresh_inlays(now_ms, stale_inlays, cx);
        if gutters_changed {
            self.refresh_gutters(cx);
        }
        self.refresh_elisions(state, start, cx);
        cx.notify();
    }

    fn try_incremental_sync<V: 'static>(
        &mut self,
        state: &UiAgentState,
        first_changed: usize,
        incremental: IncrementalUpdate,
        now_ms: u64,
        agent_label: &impl Fn(rho_ui_proto::AgentId) -> String,
        cx: &mut Context<V>,
    ) -> bool {
        let index = match incremental {
            IncrementalUpdate::AssistantText { index }
            | IncrementalUpdate::ReasoningText { index }
            | IncrementalUpdate::Tool { index } => index,
        };
        if index != first_changed || index >= self.records.len() {
            return false;
        }

        let prev_kind = last_visible_kind(&self.records[..index]);
        let Some(block) = state.blocks.get(index) else {
            return false;
        };
        let rendered = render_block_with_agent_labels(block, prev_kind, now_ms, agent_label, cx);
        let old_record = &self.records[index];
        if index + 1 < self.records.len()
            && (old_record.kind != rendered.kind || old_record.visible != rendered.visible())
        {
            return false;
        }

        let new_text = rendered_text(&rendered);
        let Some(edit) = rendered_text_edit(&old_record.text, &new_text) else {
            return false;
        };

        let old_boundary = self.turn_boundary;
        let region = if index < old_boundary {
            Region::History
        } else {
            Region::LiveTurn
        };
        let mut changed = HashSet::new();
        let stale_inlays = old_record
            .inlay
            .as_ref()
            .and_then(InlayRecord::inlay_id)
            .into_iter()
            .collect::<Vec<_>>();

        let mut gutters_changed = false;
        self.buffer.clone().update(cx, |buffer, cx| {
            let block_start = self.records[index].range.start.to_offset(buffer);
            let old_relative_styles =
                relative_style_ranges(buffer, block_start, &self.records[index].styles);
            let edit_start = block_start + edit.old_range.start;
            let edit_end = block_start + edit.old_range.end;
            buffer.edit([(edit_start..edit_end, edit.inserted.clone())], None, cx);

            let (span_ranges, inlay, gutter) = spans_for_rendered(buffer, block_start, &rendered);
            let styles = rendered
                .spans
                .iter()
                .zip(&span_ranges)
                .filter(|(span, _)| span.class != StyleClass::Default && !span.text.is_empty())
                .map(|(span, range)| (span.class, range.clone()))
                .collect::<Vec<_>>();
            let new_relative_styles = relative_style_ranges(buffer, block_start, &styles);
            changed.extend(changed_style_classes(
                &old_relative_styles,
                &new_relative_styles,
            ));

            let new_end = block_start + new_text.len();
            gutters_changed = self.records[index].gutter.is_some() || gutter.is_some();
            self.records[index] = BlockRecord {
                range: buffer.anchor_before(block_start)..buffer.anchor_before(new_end),
                kind: rendered.kind,
                visible: rendered.visible(),
                text: new_text,
                gutter,
                inlay,
                styles,
            };
        });

        self.apply_region_styles(region, &changed, cx);
        self.refresh_inlays(now_ms, stale_inlays, cx);
        if gutters_changed {
            self.refresh_gutters(cx);
        }
        self.refresh_elisions(state, index, cx);
        cx.notify();
        true
    }

    /// Refreshes running tools' duration inlays; buffer text is untouched.
    pub fn tick_timers<V: 'static>(&mut self, now_ms: u64, cx: &mut Context<V>) {
        if !self.has_timers() {
            return;
        }
        self.refresh_inlays(now_ms, Vec::new(), cx);
        cx.notify();
    }

    fn refresh_inlays<V: 'static>(
        &mut self,
        now_ms: u64,
        stale: Vec<InlayId>,
        cx: &mut Context<V>,
    ) {
        let Self {
            records,
            multi_buffer,
            editor,
            next_inlay_id,
            ..
        } = self;
        inlays::refresh_inlays(
            records
                .iter_mut()
                .filter_map(|record| record.inlay.as_mut()),
            now_ms,
            stale,
            next_inlay_id,
            multi_buffer,
            editor,
            cx,
        );
    }

    pub fn has_timers(&self) -> bool {
        self.records
            .iter()
            .any(|record| record.inlay.as_ref().is_some_and(InlayRecord::ticks))
    }

    fn apply_region_styles<V: 'static>(
        &self,
        region: Region,
        changed: &HashSet<StyleClass>,
        cx: &mut Context<V>,
    ) {
        if changed.is_empty() {
            return;
        }
        let records = match region {
            Region::LiveTurn => &self.records[self.turn_boundary..],
            _ => &self.records[..self.turn_boundary],
        };
        let mut by_class = changed
            .iter()
            .map(|class| (*class, Vec::new()))
            .collect::<HashMap<_, _>>();
        for record in records {
            for (class, range) in &record.styles {
                if let Some(ranges) = by_class.get_mut(class) {
                    ranges.push(range.clone());
                }
            }
        }
        apply_class_highlights(
            &self.editor,
            &self.multi_buffer,
            region,
            by_class
                .iter()
                .map(|(class, ranges)| (*class, ranges.as_slice())),
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
        first_changed_block: usize,
        cx: &mut Context<V>,
    ) {
        let visible = self
            .records
            .iter()
            .map(|record| record.visible)
            .collect::<Vec<_>>();
        let Self {
            records,
            elisions,
            multi_buffer,
            editor,
            ..
        } = self;
        elisions.refresh(
            &state.blocks,
            first_changed_block,
            &visible,
            crate::store::turn_open(state.status),
            |plan| plan_anchor_range(records, plan),
            multi_buffer,
            editor,
            cx,
        );
    }
}

/// First record of the live turn: everything after the last user message.
fn turn_boundary(records: &[BlockRecord]) -> usize {
    records
        .iter()
        .rposition(|record| matches!(record.kind, BlockKind::User))
        .map_or(0, |index| index + 1)
}

fn last_visible_kind(records: &[BlockRecord]) -> Option<BlockKind> {
    records
        .iter()
        .rev()
        .find(|record| record.visible)
        .map(|record| record.kind)
}

fn plan_anchor_range(records: &[BlockRecord], plan: &ElisionPlan) -> Option<Range<Anchor>> {
    let start = records.get(plan.start_block)?.range.start;
    let end = records.get(plan.end_block)?.range.end;
    Some(start..end)
}

fn append_block(
    buffer: &mut Buffer,
    cx: &mut Context<Buffer>,
    rendered: RenderedBlock,
) -> BlockRecord {
    let start = buffer.len();
    let (span_ranges, inlay, gutter) = append_spans(buffer, cx, &rendered);
    let styles = rendered
        .spans
        .iter()
        .zip(&span_ranges)
        .filter(|(span, _)| span.class != StyleClass::Default && !span.text.is_empty())
        .map(|(span, range)| (span.class, range.clone()))
        .collect();
    BlockRecord {
        range: buffer.anchor_before(start)..buffer.anchor_before(buffer.len()),
        kind: rendered.kind,
        visible: rendered.visible(),
        text: rendered_text(&rendered),
        gutter,
        inlay,
        styles,
    }
}

fn rendered_text(rendered: &RenderedBlock) -> String {
    rendered
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect()
}

struct RenderedTextEdit {
    old_range: Range<usize>,
    inserted: String,
}

fn rendered_text_edit(old: &str, new: &str) -> Option<RenderedTextEdit> {
    if old == new {
        return None;
    }

    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(old, new)| old == new)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }

    let old_tail = &old[prefix..];
    let new_tail = &new[prefix..];
    let mut suffix = old_tail
        .bytes()
        .rev()
        .zip(new_tail.bytes().rev())
        .take_while(|(old, new)| old == new)
        .count();
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }

    Some(RenderedTextEdit {
        old_range: prefix..old.len() - suffix,
        inserted: new[prefix..new.len() - suffix].to_owned(),
    })
}

fn relative_style_ranges(
    buffer: &Buffer,
    block_start: usize,
    styles: &[(StyleClass, Range<Anchor>)],
) -> HashMap<StyleClass, Vec<Range<usize>>> {
    let mut by_class: HashMap<_, Vec<_>> = HashMap::new();
    for (class, range) in styles {
        let start = range.start.to_offset(buffer).saturating_sub(block_start);
        let end = range.end.to_offset(buffer).saturating_sub(block_start);
        by_class.entry(*class).or_default().push(start..end);
    }
    by_class
}

fn changed_style_classes(
    old: &HashMap<StyleClass, Vec<Range<usize>>>,
    new: &HashMap<StyleClass, Vec<Range<usize>>>,
) -> HashSet<StyleClass> {
    old.keys()
        .chain(new.keys())
        .filter(|class| old.get(class) != new.get(class))
        .copied()
        .collect()
}

/// Appends a rendered block's text at the end of the buffer and returns the
/// anchor range of each span. The inlay span is empty; its position anchors
/// the block's custom inlay (running duration or queue label).
fn append_spans(
    buffer: &mut Buffer,
    cx: &mut Context<Buffer>,
    rendered: &RenderedBlock,
) -> (
    Vec<Range<Anchor>>,
    Option<InlayRecord>,
    Option<Range<Anchor>>,
) {
    let start = buffer.len();
    let text = rendered
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    if !text.is_empty() {
        buffer.edit([(start..start, text)], None, cx);
    }

    spans_for_rendered(buffer, start, rendered)
}

fn spans_for_rendered(
    buffer: &Buffer,
    start: usize,
    rendered: &RenderedBlock,
) -> (
    Vec<Range<Anchor>>,
    Option<InlayRecord>,
    Option<Range<Anchor>>,
) {
    let mut ranges = Vec::with_capacity(rendered.spans.len());
    let mut inlay = None;
    let mut gutter = None;
    let mut offset = start;
    for (index, span) in rendered.spans.iter().enumerate() {
        let end = offset + span.text.len();
        let range = buffer.anchor_before(offset)..buffer.anchor_before(end);
        if let Some(spec) = rendered.inlay.filter(|spec| spec.span_index == index) {
            inlay = Some(InlayRecord::new(range.start, spec.content));
        }
        if rendered.gutter_span == Some(index) {
            let trimmed = span.text.trim_end_matches('\n').len();
            gutter = Some(buffer.anchor_before(offset)..buffer.anchor_before(offset + trimmed));
        }
        ranges.push(range);
        offset = end;
    }
    (ranges, inlay, gutter)
}

#[cfg(test)]
mod tests {
    use super::rendered_text_edit;

    #[test]
    fn rendered_text_edit_appends_ascii_suffix() {
        let edit = rendered_text_edit("hel", "hello").expect("edit");
        assert_eq!(edit.old_range, 3..3);
        assert_eq!(edit.inserted, "lo");
    }

    #[test]
    fn rendered_text_edit_inserts_before_common_suffix() {
        let edit = rendered_text_edit("$ …\n", "$ echo …\n").expect("edit");
        assert_eq!(edit.old_range, 2..2);
        assert_eq!(edit.inserted, "echo ");
    }

    #[test]
    fn rendered_text_edit_is_utf8_boundary_safe() {
        let edit = rendered_text_edit("a🙂c", "a🙂bc").expect("edit");
        assert_eq!(edit.old_range, "a🙂".len().."a🙂".len());
        assert_eq!(edit.inserted, "b");
    }
}
