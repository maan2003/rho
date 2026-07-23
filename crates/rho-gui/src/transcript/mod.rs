//! Incremental transcript projection into a read-only buffer.
//!
//! The transcript buffer always equals `render(blocks)`, one record per
//! protocol block. A [`FrameSummary`] bounds every update: blocks before
//! `first_changed_block` are never touched (their anchors, highlights,
//! gutters and folds survive untouched); everything after is re-rendered.
//!
//! The model is editor-agnostic (emacs: decoration is buffer state, not
//! window state): records, styles, inlay content, and elision plans are
//! all anchor-based data. Any number of editors attach; after each sync
//! the model reconciles every attachment — highlights and gutters are
//! reapplied for changed classes, inlays and display elisions diffed
//! against the desired state — so every pane over the transcript stays
//! correct without owning any of it.
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
use elisions::{ElisionState, ElisionSync};
use gpui::{Context, Entity, WeakEntity};
use inlays::{InlayRecord, PlacedInlay};
use language::{Buffer, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_ui_proto::remote::UiAgentState;
use text::{Anchor, ToOffset as _};

use crate::highlights::{apply_class_highlights, excerpt_range};
use crate::render::elision::ElisionPlan;
use crate::render::{BlockKind, RenderedBlock, render_block_with_agent_labels};
use crate::store::{FrameSummary, IncrementalUpdate};
use crate::style::{Region, StyleClass};

pub struct TranscriptModel {
    buffer: Entity<Buffer>,
    /// The document multibuffer: the transcript excerpt, cropped when the
    /// turn is closed to end where the words end (no trailing turn
    /// separator). Preview editors read this; the full prompt-bearing
    /// multibuffer is composed by the agent model and reaches this model
    /// only through attachments.
    document_multi_buffer: Entity<MultiBuffer>,
    document_tail: Option<DocumentTail>,
    /// Whether the last-synced state had an open turn; decides the
    /// document tail policy between syncs (e.g. at attach time).
    turn_open: bool,
    records: Vec<BlockRecord>,
    /// First record of the live turn as of the last sync. Records before it
    /// carry their highlights in the history region, records from it onward
    /// in the live-turn region.
    turn_boundary: usize,
    elisions: ElisionSync,
    // Custom inlay ids share each editor's id space with the prompt
    // placeholder (id 0), so they start at 1. One counter serves every
    // attachment: ids only need uniqueness within an editor.
    next_inlay_id: usize,
    attachments: Vec<Attachment>,
}

/// One editor displaying this transcript, plus the per-editor state that
/// lives in that editor's id spaces (inlay ids, display elision ids).
/// Attachments carry their own multibuffer: full-prompt editors and
/// document previews compose the shared buffer differently, so anchor
/// resolution is per-attachment.
struct Attachment {
    editor: WeakEntity<Editor>,
    multi_buffer: Entity<MultiBuffer>,
    elisions: ElisionState,
    inlays: Vec<PlacedInlay>,
}

struct BlockRecord {
    range: Range<Anchor>,
    kind: BlockKind,
    visible: bool,
    text: String,
    gutter: Option<(StyleClass, Range<Anchor>)>,
    inlay: Option<InlayRecord>,
    styles: Vec<(StyleClass, Range<Anchor>)>,
}

type PlacedSpans = (
    Vec<Range<Anchor>>,
    Option<InlayRecord>,
    Option<(StyleClass, Range<Anchor>)>,
);

struct UserMessageGutter;
struct AgentMessageGutter;

/// The document excerpt's tail policy. Replacing an excerpt gives it a
/// new id (invalidating every anchor into it), so the tail changes shape
/// only at turn boundaries: streaming rides a growing excerpt; a closed
/// turn crops flush at the last content line.
#[derive(Clone, Copy, PartialEq)]
enum DocumentTail {
    /// The excerpt runs to the buffer's end and grows with appends.
    Growing,
    /// The excerpt is cropped flush at this point.
    Cropped(Point),
}

impl TranscriptModel {
    pub fn new(buffer: Entity<Buffer>, document_multi_buffer: Entity<MultiBuffer>) -> Self {
        Self {
            buffer,
            document_multi_buffer,
            document_tail: None,
            turn_open: false,
            records: Vec::new(),
            turn_boundary: 0,
            elisions: ElisionSync::default(),
            next_inlay_id: 1,
            attachments: Vec::new(),
        }
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    /// Attaches an editor showing this transcript (over whatever
    /// multibuffer the editor was built on), bringing it fully up to date
    /// with the model. Dropped editors detach themselves: the model only
    /// holds weak handles and prunes on the next apply.
    pub fn attach<V: 'static>(
        &mut self,
        editor: &Entity<Editor>,
        now_ms: u64,
        cx: &mut Context<V>,
    ) {
        self.attachments.push(Attachment {
            editor: editor.downgrade(),
            multi_buffer: editor.read(cx).buffer().clone(),
            elisions: ElisionState::default(),
            inlays: Vec::new(),
        });
        let history = classes_in(&self.records[..self.turn_boundary]);
        let live = classes_in(&self.records[self.turn_boundary..]);
        self.apply_to_attachments(now_ms, &history, &live, true, cx);
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
        self.turn_open = crate::store::turn_open(state.status);
        let Some(first_changed) = summary.first_changed_block else {
            // Status alone can close the turn; the document tail follows,
            // and a replaced excerpt triggers the full re-apply inside.
            let empty = HashSet::new();
            self.apply_to_attachments(now_ms, &empty, &empty, false, cx);
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

        self.refresh_elision_plans(state, start);
        self.apply_to_attachments(now_ms, &changed_history, &changed_live, gutters_changed, cx);
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

        let live_region = index >= self.turn_boundary;
        let mut changed = HashSet::new();
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

        let empty = HashSet::new();
        let (changed_history, changed_live) = if live_region {
            (&empty, &changed)
        } else {
            (&changed, &empty)
        };
        self.refresh_elision_plans(state, index);
        self.apply_to_attachments(now_ms, changed_history, changed_live, gutters_changed, cx);
        cx.notify();
        true
    }

    /// Refreshes running tools' duration inlays; buffer text is untouched.
    pub fn tick_timers<V: 'static>(&mut self, now_ms: u64, cx: &mut Context<V>) {
        if !self.has_timers() {
            return;
        }
        let empty = HashSet::new();
        self.apply_to_attachments(now_ms, &empty, &empty, false, cx);
        cx.notify();
    }

    pub fn has_timers(&self) -> bool {
        self.records
            .iter()
            .any(|record| record.inlay.as_ref().is_some_and(InlayRecord::ticks))
    }

    fn refresh_elision_plans(&mut self, state: &UiAgentState, first_changed_block: usize) {
        let visible = self
            .records
            .iter()
            .map(|record| record.visible)
            .collect::<Vec<_>>();
        let Self {
            records, elisions, ..
        } = self;
        elisions.refresh(
            &state.blocks,
            first_changed_block,
            &visible,
            crate::store::turn_open(state.status),
            |plan| plan_anchor_range(records, plan),
        );
    }

    /// Aligns the document excerpt with the tail policy. Returns whether
    /// the excerpt was replaced (its id changed): every anchor a document
    /// attachment holds is stale then, and it needs a full re-apply.
    fn update_document_excerpt<V: 'static>(&mut self, cx: &mut Context<V>) -> bool {
        let buffer = self.buffer.read(cx);
        let desired = if self.turn_open {
            DocumentTail::Growing
        } else {
            let len = buffer.len();
            let trailing = buffer
                .as_rope()
                .reversed_chars_at(len)
                .take_while(|c| *c == '\n')
                .count();
            DocumentTail::Cropped(buffer.offset_to_point(len - trailing))
        };
        if self.document_tail == Some(desired) {
            return false;
        }
        self.document_tail = Some(desired);
        let end = match desired {
            DocumentTail::Growing => buffer.max_point(),
            DocumentTail::Cropped(end) => end,
        };
        let buffer = self.buffer.clone();
        self.document_multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                buffer,
                [Point::zero()..end],
                0,
                cx,
            );
        });
        true
    }

    /// Brings every attached editor up to date with the model: changed
    /// highlight classes reapplied per region, gutters when they moved,
    /// inlays and display elisions reconciled. Dead attachments prune
    /// here. When the document excerpt was replaced, attachments over the
    /// document multibuffer re-apply everything from scratch — their old
    /// anchors all died with the excerpt.
    fn apply_to_attachments<V: 'static>(
        &mut self,
        now_ms: u64,
        changed_history: &HashSet<StyleClass>,
        changed_live: &HashSet<StyleClass>,
        gutters_changed: bool,
        cx: &mut Context<V>,
    ) {
        let document_replaced = self.update_document_excerpt(cx);
        if self.attachments.is_empty() {
            return;
        }
        let history_styles = region_styles(&self.records[..self.turn_boundary], changed_history);
        let live_styles = region_styles(&self.records[self.turn_boundary..], changed_live);
        let (full_history_styles, full_live_styles) = if document_replaced {
            let history = classes_in(&self.records[..self.turn_boundary]);
            let live = classes_in(&self.records[self.turn_boundary..]);
            (
                region_styles(&self.records[..self.turn_boundary], &history),
                region_styles(&self.records[self.turn_boundary..], &live),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let gutter_anchor_ranges = (gutters_changed || document_replaced).then(|| {
            self.records
                .iter()
                .filter_map(|record| record.gutter.clone())
                .collect::<Vec<_>>()
        });
        let desired_inlays = self
            .records
            .iter()
            .filter_map(|record| record.inlay.as_ref())
            .filter_map(|inlay| inlay.desired(now_ms))
            .collect::<Vec<_>>();

        let Self {
            document_multi_buffer,
            next_inlay_id,
            attachments,
            elisions,
            ..
        } = self;
        attachments.retain_mut(|attachment| {
            let Some(editor) = attachment.editor.upgrade() else {
                return false;
            };
            let refresh = document_replaced && attachment.multi_buffer == *document_multi_buffer;
            if refresh {
                // The replaced excerpt took the placed inlays and display
                // elisions with it; forget them so reconciliation places
                // them anew with live anchors.
                attachment.inlays.clear();
                attachment.elisions = ElisionState::default();
            }
            let (history_styles, live_styles) = if refresh {
                (&full_history_styles, &full_live_styles)
            } else {
                (&history_styles, &live_styles)
            };
            let multi_buffer = &attachment.multi_buffer;
            apply_class_highlights(
                &editor,
                multi_buffer,
                Region::History,
                history_styles
                    .iter()
                    .map(|(class, ranges)| (*class, ranges.as_slice())),
                cx,
            );
            apply_class_highlights(
                &editor,
                multi_buffer,
                Region::LiveTurn,
                live_styles
                    .iter()
                    .map(|(class, ranges)| (*class, ranges.as_slice())),
                cx,
            );
            if let Some(ranges) = &gutter_anchor_ranges
                && (gutters_changed || refresh)
            {
                let snapshot = multi_buffer.read(cx).snapshot(cx);
                let ranges = ranges
                    .iter()
                    .filter_map(|(class, range)| {
                        excerpt_range(&snapshot, range).map(|range| (*class, range))
                    })
                    .collect::<Vec<_>>();
                let user_ranges: Vec<_> = ranges
                    .iter()
                    .filter(|(class, _)| *class == StyleClass::UserMessage)
                    .map(|(_, range)| range.clone())
                    .collect();
                let agent_ranges: Vec<_> = ranges
                    .iter()
                    .filter(|(class, _)| *class == StyleClass::AgentMessage)
                    .map(|(_, range)| range.clone())
                    .collect();
                editor.update(cx, |editor, cx| {
                    editor.highlight_gutter::<UserMessageGutter>(
                        user_ranges,
                        crate::style::user_prompt_gutter_color,
                        cx,
                    );
                    editor.highlight_gutter::<AgentMessageGutter>(
                        agent_ranges,
                        crate::style::agent_message_gutter_color,
                        cx,
                    );
                });
            }
            inlays::reconcile_inlays(
                &desired_inlays,
                &mut attachment.inlays,
                next_inlay_id,
                multi_buffer,
                &editor,
                cx,
            );
            elisions.apply(&mut attachment.elisions, multi_buffer, &editor, cx);
            true
        });
    }
}

/// Every style class appearing in `records` — the "all changed" set for a
/// full application to a freshly attached editor.
fn classes_in(records: &[BlockRecord]) -> HashSet<StyleClass> {
    records
        .iter()
        .flat_map(|record| record.styles.iter().map(|(class, _)| *class))
        .collect()
}

/// Collects each changed class's full range list for a region; an empty
/// list clears the class.
fn region_styles(
    records: &[BlockRecord],
    changed: &HashSet<StyleClass>,
) -> Vec<(StyleClass, Vec<Range<Anchor>>)> {
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
    by_class.into_iter().collect()
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
) -> PlacedSpans {
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
) -> PlacedSpans {
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
            gutter = Some((
                span.class,
                buffer.anchor_before(offset)..buffer.anchor_before(offset + trimmed),
            ));
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
