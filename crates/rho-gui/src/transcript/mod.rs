//! Incremental transcript projection into per-turn read-only buffers.
//!
//! Response runs (assistant/tool/assistant until the next user message) each
//! own one Markdown buffer; user-originated records own plain buffers. A
//! [`FrameSummary`] bounds every update: blocks before
//! `first_changed_block` are never touched (their anchors, highlights,
//! gutters and folds survive untouched); everything after is re-rendered.
//!
//! The model is editor-agnostic (emacs: decoration is buffer state, not
//! window state): records, styles, inlay content, and elision plans are
//! all anchor-based data. Any number of editors attach; after each sync
//! the model reconciles every attachment — highlights and gutters are
//! reapplied for changed classes, inlays and display elisions diffed
//! against the desired state — so every view over the transcript stays
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
use std::sync::Arc;

use editor::Editor;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use elisions::{ElisionState, ElisionSync};
use futures::FutureExt as _;
use futures::future::LocalBoxFuture;
use gpui::{AppContext as _, Context, Entity, IntoElement as _, Reservation, WeakEntity};
use inlays::{InlayRecord, PlacedInlay};
use language::{Buffer, Point};
use multi_buffer::{MultiBuffer, PathKey, ToOffset as _};
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::UiAgentState;
use text::{Anchor, Buffer as TextBuffer, ToOffset as _};

use crate::connection::VisualizationClient;
use crate::highlights::{apply_class_highlights, excerpt_range};
use crate::render::elision::ElisionPlan;
use crate::render::{BlockKind, RenderedBlock, render_block_with_agent_labels};
use crate::store::{FrameSummary, IncrementalUpdate};
use crate::style::{Region, StyleClass};
use crate::visualization::Visualization;

pub struct TranscriptModel {
    multi_buffer: Entity<MultiBuffer>,
    /// The document multibuffer: the transcript excerpt omits its rendering
    /// sentinel, then crops every trailing separator when the turn closes.
    /// Preview editors read this; the full prompt-bearing
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
    buffers: Vec<TranscriptBuffer>,
    elisions: ElisionSync,
    // Custom inlay ids share each editor's id space with the prompt
    // placeholder (id 0), so they start at 1. One counter serves every
    // attachment: ids only need uniqueness within an editor.
    next_inlay_id: usize,
    visualization_client: VisualizationClient,
    visualization_cache: HashMap<String, Entity<Visualization>>,
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
    visualizations: Vec<PlacedVisualization>,
}

struct BlockRecord {
    buffer: Entity<Buffer>,
    range: Range<Anchor>,
    kind: BlockKind,
    visible: bool,
    text: String,
    gutter: Option<(StyleClass, Range<Anchor>)>,
    inlays: Vec<InlayRecord>,
    styles: Vec<(StyleClass, Range<Anchor>)>,
    terminal_newline_supplied_by_excerpt: bool,
    visualizations: Vec<VisualizationAnchor>,
}

struct TranscriptBuffer {
    start_block: usize,
    composed: bool,
    buffer: Entity<Buffer>,
}

pub(crate) struct PreparedInitialTranscript {
    state: UiAgentState,
    chunks: Vec<PreparedChunk>,
}

struct PreparedChunk {
    start_block: usize,
    markdown: bool,
    rendered: Vec<RenderedBlock>,
    terminal_record: Option<usize>,
    text: String,
}

impl PreparedInitialTranscript {
    pub(crate) fn buffer_count(&self) -> usize {
        self.chunks.len()
    }

    pub(crate) fn take_texts(&mut self) -> Vec<String> {
        self.chunks
            .iter_mut()
            .map(|chunk| std::mem::take(&mut chunk.text))
            .collect()
    }
}

#[derive(Clone)]
struct VisualizationAnchor {
    id: String,
    rows: u32,
    range: Range<Anchor>,
}

struct PlacedVisualization {
    id: String,
    rows: u32,
    source_range: Range<Anchor>,
    block_id: CustomBlockId,
}

type PlacedSpans = (
    Vec<Range<Anchor>>,
    Vec<InlayRecord>,
    Option<(StyleClass, Range<Anchor>)>,
    Vec<VisualizationAnchor>,
);

struct UserMessageGutter;
struct AgentMessageGutter;

/// The document excerpt's tail policy. Replacing an excerpt gives it a
/// new id (invalidating every anchor into it), so the tail changes shape
/// only at turn boundaries: streaming rides a growing excerpt; a closed
/// turn crops flush at the last content line.
#[derive(Clone, Copy, PartialEq)]
enum DocumentTail {
    /// The excerpt omits the terminal rendering sentinel and grows with
    /// inserts made immediately before it.
    Growing(text::BufferId),
    /// The excerpt is cropped flush at this point.
    Cropped(text::BufferId, Point),
}

impl TranscriptModel {
    pub fn new(
        multi_buffer: Entity<MultiBuffer>,
        document_multi_buffer: Entity<MultiBuffer>,
        visualization_client: VisualizationClient,
    ) -> Self {
        Self {
            multi_buffer,
            document_multi_buffer,
            document_tail: None,
            turn_open: false,
            records: Vec::new(),
            turn_boundary: 0,
            buffers: Vec::new(),
            elisions: ElisionSync::default(),
            next_inlay_id: 1,
            visualization_client,
            visualization_cache: HashMap::new(),
            attachments: Vec::new(),
        }
    }

    /// Pure initial projection. This runs on a worker before the subscribed
    /// agent is considered ready, so first focus never pays transcript
    /// rendering or text concatenation costs.
    pub(crate) fn prepare_initial(
        state: UiAgentState,
        now_ms: u64,
        agent_labels: HashMap<AgentId, String>,
    ) -> PreparedInitialTranscript {
        let mut previous = None;
        let rendered_blocks = state
            .blocks
            .iter()
            .map(|block| {
                let rendered = render_block_with_agent_labels(block, previous, now_ms, &|id| {
                    agent_labels.get(&id).cloned().unwrap_or_default()
                });
                if rendered.visible() {
                    previous = Some(rendered.kind);
                }
                rendered
            })
            .collect::<Vec<_>>();

        let mut chunks: Vec<(usize, bool, Vec<RenderedBlock>)> = Vec::new();
        for (block_index, rendered) in rendered_blocks.into_iter().enumerate() {
            let markdown = rendered.markdown;
            if chunks
                .last()
                .is_none_or(|(_, current_markdown, _)| markdown != *current_markdown)
            {
                chunks.push((block_index, markdown, Vec::new()));
            }
            chunks.last_mut().unwrap().2.push(rendered);
        }
        let chunks = chunks
            .into_iter()
            .map(|(start_block, markdown, rendered)| PreparedChunk {
                start_block,
                markdown,
                terminal_record: rendered.iter().rposition(RenderedBlock::visible),
                text: rendered.iter().map(rendered_text).collect(),
                rendered,
            })
            .collect();
        PreparedInitialTranscript { state, chunks }
    }

    /// Installs a worker-prepared initial transcript into its reserved GPUI
    /// entities. Only registration, language wiring, anchors, and multibuffer
    /// mutation remain on the foreground thread.
    pub(crate) fn install_initial<V: 'static>(
        &mut self,
        prepared: PreparedInitialTranscript,
        text_buffers: Vec<(Reservation<Buffer>, TextBuffer)>,
        now_ms: u64,
        cx: &mut Context<V>,
    ) -> Vec<LocalBoxFuture<'static, ()>> {
        debug_assert!(self.records.is_empty());
        debug_assert!(self.buffers.is_empty());
        debug_assert_eq!(prepared.chunks.len(), text_buffers.len());

        self.turn_open = crate::store::turn_open(prepared.state.status);
        let mut installed = Vec::with_capacity(prepared.chunks.len());
        // Register newest buffers first; syntax activation below follows the
        // same order so the visible tail leads the historical parser backlog.
        for (chunk, (reservation, text_buffer)) in
            prepared.chunks.into_iter().zip(text_buffers).rev()
        {
            let buffer = cx.insert_entity(reservation, |cx| {
                let mut buffer = Buffer::build(text_buffer, None, language::Capability::Read);
                if chunk.markdown {
                    crate::render::markdown::configure_buffer(&mut buffer, cx);
                }
                buffer
            });
            installed.push((chunk, buffer));
        }

        let mut gutters_changed = false;
        for (chunk, buffer) in installed.into_iter().rev() {
            let mut offset = 0;
            {
                let snapshot = buffer.read(cx);
                for (record_index, rendered) in chunk.rendered.into_iter().enumerate() {
                    let record = block_record(
                        &buffer,
                        snapshot,
                        offset,
                        rendered,
                        chunk.terminal_record == Some(record_index),
                    );
                    offset += record.text.len();
                    gutters_changed |= record.gutter.is_some();
                    self.records.push(record);
                }
            }
            let composed = !buffer.read(cx).is_empty();
            if composed {
                let buffer_id = buffer.read(cx).remote_id();
                for attachment in &self.attachments {
                    if let Some(editor) = attachment.editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.disable_header_for_buffer(buffer_id, cx)
                        });
                    }
                }
            }
            self.buffers.push(TranscriptBuffer {
                start_block: chunk.start_block,
                composed,
                buffer,
            });
        }

        self.turn_boundary = turn_boundary(&self.records);
        self.reset_full_excerpts(cx);
        self.document_tail = None;
        self.reset_document_excerpts(cx);
        self.refresh_elision_plans(&prepared.state, 0);
        let history = classes_in(&self.records[..self.turn_boundary]);
        let live = classes_in(&self.records[self.turn_boundary..]);
        self.apply_to_attachments(now_ms, &history, &live, gutters_changed, cx);
        let parsing = Self::warm_syntax(
            self.buffers.iter().rev().map(|turn| turn.buffer.clone()),
            cx,
        );
        cx.notify();
        parsing
    }

    fn warm_syntax<V: 'static>(
        buffers: impl IntoIterator<Item = Entity<Buffer>>,
        cx: &mut Context<V>,
    ) -> Vec<LocalBoxFuture<'static, ()>> {
        let mut parsing = Vec::new();
        for buffer in buffers {
            let has_language = buffer.read(cx).language().is_some();
            if !has_language {
                continue;
            }
            buffer.update(cx, |buffer, cx| {
                buffer.ensure_syntax_parsed(cx);
            });
            parsing.push(buffer.read(cx).parsing_idle().boxed_local());
        }
        parsing
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
        editor.update(cx, |editor, cx| {
            let buffer_ids = self
                .buffers
                .iter()
                .map(|turn| turn.buffer.read(cx).remote_id())
                .collect::<Vec<_>>();
            editor.disable_headers_for_buffers(buffer_ids, cx);
        });
        self.attachments.push(Attachment {
            editor: editor.downgrade(),
            multi_buffer: editor.read(cx).buffer().clone(),
            elisions: ElisionState::default(),
            inlays: Vec::new(),
            visualizations: Vec::new(),
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

        let requested_start = first_changed.min(self.records.len());
        let start = self.rebuild_start(requested_start, state);

        let changed_blocks = state.blocks.get(start..).unwrap_or(&[]);
        let mut prev_kind = last_visible_kind(&self.records[..start]);
        let rendered_blocks = changed_blocks
            .iter()
            .map(|block| {
                let block = render_block_with_agent_labels(block, prev_kind, now_ms, agent_label);
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
        self.replace_buffers_from(start, rendered_blocks, &mut gutters_changed, cx);

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

    fn rebuild_start(&self, requested: usize, state: &UiAgentState) -> usize {
        if let Some(record) = self.records.get(requested) {
            return self
                .buffers
                .iter()
                .find(|turn| turn.buffer == record.buffer)
                .map_or(requested, |turn| turn.start_block);
        }
        let next_is_response = state.blocks.get(requested).is_some_and(|block| {
            matches!(crate::render::block_kind(block), BlockKind::Response { .. })
        });
        if next_is_response
            && self
                .records
                .last()
                .is_some_and(|record| matches!(record.kind, BlockKind::Response { .. }))
            && let Some(turn) = self.buffers.last()
        {
            return turn.start_block;
        }
        requested
    }

    fn replace_buffers_from<V: 'static>(
        &mut self,
        start: usize,
        rendered_blocks: Vec<RenderedBlock>,
        gutters_changed: &mut bool,
        cx: &mut Context<V>,
    ) {
        let old_last_composed = self
            .buffers
            .iter()
            .rfind(|turn| turn.composed)
            .map(|turn| turn.start_block);
        let first_removed = self
            .buffers
            .iter()
            .position(|turn| turn.start_block >= start)
            .unwrap_or(self.buffers.len());
        let removed = self.buffers.split_off(first_removed);
        let removed_buffers = removed
            .into_iter()
            .map(|turn| (transcript_path(turn.start_block), turn.buffer))
            .collect::<Vec<_>>();

        let mut chunks: Vec<(usize, bool, Vec<RenderedBlock>)> = Vec::new();
        for (offset, rendered) in rendered_blocks.into_iter().enumerate() {
            let block_index = start + offset;
            let markdown = rendered.markdown;
            let starts_chunk = chunks
                .last()
                .is_none_or(|(_, current_markdown, _)| markdown != *current_markdown);
            if starts_chunk {
                chunks.push((block_index, markdown, Vec::new()));
            }
            chunks.last_mut().unwrap().2.push(rendered);
        }

        let mut prepared = Vec::with_capacity(chunks.len());
        // Materializing a long restored transcript can fill the parser pool.
        // Create newest turns first so the visible live turn receives its
        // bounded foreground parse before historical work is queued.
        for (start_block, markdown, rendered) in chunks.into_iter().rev() {
            let terminal_record = rendered.iter().rposition(RenderedBlock::visible);
            let text = rendered.iter().map(rendered_text).collect::<String>();
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(&text, cx);
                if markdown {
                    crate::render::markdown::configure_buffer(&mut buffer, cx);
                }
                buffer.set_capability(language::Capability::Read, cx);
                buffer
            });
            prepared.push((start_block, rendered, terminal_record, buffer));
        }

        for (start_block, rendered, terminal_record, buffer) in prepared.into_iter().rev() {
            let mut offset = 0;
            {
                let snapshot = buffer.read(cx);
                for (record_index, rendered) in rendered.into_iter().enumerate() {
                    let record = block_record(
                        &buffer,
                        snapshot,
                        offset,
                        rendered,
                        terminal_record == Some(record_index),
                    );
                    offset += record.text.len();
                    *gutters_changed |= record.gutter.is_some();
                    self.records.push(record);
                }
            }
            let composed = !buffer.read(cx).is_empty();
            if composed {
                let buffer_id = buffer.read(cx).remote_id();
                for attachment in &self.attachments {
                    if let Some(editor) = attachment.editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.disable_header_for_buffer(buffer_id, cx)
                        });
                    }
                }
            }
            self.buffers.push(TranscriptBuffer {
                start_block,
                composed,
                buffer,
            });
        }
        let new_buffers = self.buffers[first_removed..]
            .iter()
            .rev()
            .map(|turn| turn.buffer.clone())
            .collect::<Vec<_>>();
        self.reset_rebuilt_excerpts(first_removed, old_last_composed, removed_buffers, cx);
        Self::warm_syntax(new_buffers, cx);
    }

    /// Reinstalls only the excerpts replaced by a suffix rebuild.
    /// Re-registering every path turns an otherwise local block replacement
    /// into a whole-transcript edit and forces all settled rows through
    /// fold, wrap, and block layout again.
    fn reset_rebuilt_excerpts<V: 'static>(
        &mut self,
        first_rebuilt: usize,
        old_last_composed: Option<usize>,
        removed_buffers: Vec<(PathKey, Entity<Buffer>)>,
        cx: &mut Context<V>,
    ) {
        let new_last_composed = self
            .buffers
            .iter()
            .rfind(|turn| turn.composed)
            .map(|turn| turn.start_block);
        let tail_changed = old_last_composed != new_last_composed;
        let mut new_document_tail = None;
        let affected = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(index, turn)| {
                turn.composed
                    && (*index >= first_rebuilt
                        || tail_changed
                            && (Some(turn.start_block) == old_last_composed
                                || Some(turn.start_block) == new_last_composed))
            })
            .map(|(_, turn)| {
                let buffer = turn.buffer.read(cx);
                let full_end = if Some(turn.start_block) == new_last_composed {
                    prompt_gap_excerpt_end(buffer)
                } else {
                    composed_excerpt_end(buffer)
                };
                let document_end = if Some(turn.start_block) == new_last_composed {
                    let (tail, end) = desired_document_tail(buffer, self.turn_open);
                    new_document_tail = Some(tail);
                    end
                } else {
                    composed_excerpt_end(buffer)
                };
                (
                    transcript_path(turn.start_block),
                    turn.buffer.clone(),
                    full_end,
                    document_end,
                )
            })
            .collect::<Vec<_>>();
        let rebuilt_paths = affected
            .iter()
            .map(|(path, ..)| path.clone())
            .collect::<HashSet<_>>();
        let removed_buffers = removed_buffers
            .into_iter()
            .filter(|(path, _)| !rebuilt_paths.contains(path))
            .collect::<Vec<_>>();
        self.multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(
                affected
                    .iter()
                    .map(|(path, buffer, end, _)| {
                        (path.clone(), buffer.clone(), vec![Point::zero()..*end])
                    })
                    .chain(
                        removed_buffers
                            .iter()
                            .map(|(path, buffer)| (path.clone(), buffer.clone(), Vec::new())),
                    ),
                0,
                cx,
            );
        });
        self.document_multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(
                affected
                    .iter()
                    .map(|(path, buffer, _, end)| {
                        (path.clone(), buffer.clone(), vec![Point::zero()..*end])
                    })
                    .chain(
                        removed_buffers
                            .iter()
                            .map(|(path, buffer)| (path.clone(), buffer.clone(), Vec::new())),
                    ),
                0,
                cx,
            );
        });
        if new_document_tail.is_some() || new_last_composed.is_none() {
            self.document_tail = new_document_tail;
        }
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
        let rendered = render_block_with_agent_labels(block, prev_kind, now_ms, agent_label);
        let old_record = &self.records[index];
        if old_record.kind != rendered.kind || old_record.visible != rendered.visible() {
            return false;
        }
        let terminal_newline_supplied_by_excerpt = old_record.terminal_newline_supplied_by_excerpt;

        let new_text = rendered_text(&rendered);
        let Some(edit) = rendered_text_edit(&old_record.text, &new_text) else {
            return false;
        };

        let live_region = index >= self.turn_boundary;
        let mut changed = HashSet::new();
        let mut gutters_changed = false;
        let record_buffer = self.records[index].buffer.clone();
        record_buffer.update(cx, |buffer, cx| {
            let block_start = self.records[index].range.start.to_offset(buffer);
            let old_relative_styles =
                relative_style_ranges(buffer, block_start, &self.records[index].styles);
            let edit_start = block_start + edit.old_range.start;
            let edit_end = block_start + edit.old_range.end;
            buffer.edit([(edit_start..edit_end, edit.inserted.clone())], None, cx);

            let (span_ranges, inlays, gutter, visualizations) =
                spans_for_rendered(buffer, block_start, &rendered);
            let style_end = new_text
                .len()
                .checked_sub(usize::from(terminal_newline_supplied_by_excerpt))
                .map(|len| block_start + len);
            let styles = styles_for_rendered(buffer, &rendered, &span_ranges, style_end);
            let new_relative_styles = relative_style_ranges(buffer, block_start, &styles);
            changed.extend(changed_style_classes(
                &old_relative_styles,
                &new_relative_styles,
            ));

            let new_end =
                block_start + new_text.len() - usize::from(terminal_newline_supplied_by_excerpt);
            gutters_changed = self.records[index].gutter.is_some() || gutter.is_some();
            self.records[index] = BlockRecord {
                buffer: record_buffer.clone(),
                range: buffer.anchor_before(block_start)..buffer.anchor_before(new_end),
                kind: rendered.kind,
                visible: rendered.visible(),
                text: new_text,
                gutter,
                inlays,
                styles,
                terminal_newline_supplied_by_excerpt,
                visualizations,
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
            .flat_map(|record| record.inlays.iter())
            .any(InlayRecord::ticks)
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

    fn reset_full_excerpts<V: 'static>(&self, cx: &mut Context<V>) {
        let last = self.buffers.iter().rposition(|turn| turn.composed);
        let entries = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, turn)| turn.composed)
            .map(|(index, turn)| {
                let buffer = turn.buffer.read(cx);
                let end = if Some(index) == last {
                    prompt_gap_excerpt_end(buffer)
                } else {
                    composed_excerpt_end(buffer)
                };
                (
                    transcript_path(turn.start_block),
                    turn.buffer.clone(),
                    vec![Point::zero()..end],
                )
            })
            .collect::<Vec<_>>();
        self.multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(entries, 0, cx);
        });
    }

    fn reset_document_excerpts<V: 'static>(&self, cx: &mut Context<V>) {
        let entries = self
            .buffers
            .iter()
            .filter(|turn| turn.composed)
            .map(|turn| {
                let buffer = turn.buffer.read(cx);
                (
                    transcript_path(turn.start_block),
                    turn.buffer.clone(),
                    vec![Point::zero()..composed_excerpt_end(buffer)],
                )
            })
            .collect::<Vec<_>>();
        self.document_multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(entries, 0, cx);
        });
    }

    /// Aligns the document excerpt with the tail policy. Returns whether the
    /// excerpt range changed, so range-based styling can be fully reapplied.
    fn update_document_excerpt<V: 'static>(&mut self, cx: &mut Context<V>) -> bool {
        let Some(last) = self.buffers.iter().rev().find(|turn| turn.composed) else {
            return self.document_tail.take().is_some();
        };
        let buffer = last.buffer.read(cx);
        let (desired, end) = desired_document_tail(buffer, self.turn_open);
        if self.document_tail == Some(desired) {
            return false;
        }
        self.document_tail = Some(desired);
        let buffer = last.buffer.clone();
        let path = transcript_path(last.start_block);
        self.document_multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(path, buffer, [Point::zero()..end], 0, cx);
        });
        true
    }

    /// Brings every attached editor up to date with the model: changed
    /// highlight classes reapplied per region, gutters when they moved,
    /// inlays and display elisions reconciled. Dead attachments prune here.
    /// A changed document range gets a full style re-apply, while concrete
    /// decorations are retained, removed, or updated by anchor reconciliation.
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
            .flat_map(|record| record.inlays.iter())
            .filter_map(|inlay| inlay.desired(now_ms))
            .collect::<Vec<_>>();
        let desired_visualizations = self
            .records
            .iter()
            .flat_map(|record| record.visualizations.iter().cloned())
            .collect::<Vec<_>>();
        let desired_visualization_ids = desired_visualizations
            .iter()
            .map(|visualization| visualization.id.as_str())
            .collect::<HashSet<_>>();
        self.visualization_cache
            .retain(|id, _| desired_visualization_ids.contains(id.as_str()));

        let Self {
            document_multi_buffer,
            next_inlay_id,
            attachments,
            elisions,
            visualization_client,
            visualization_cache,
            ..
        } = self;
        attachments.retain_mut(|attachment| {
            let Some(editor) = attachment.editor.upgrade() else {
                return false;
            };
            let refresh = document_replaced && attachment.multi_buffer == *document_multi_buffer;
            // Scale rows before anything lays them out: the wrap map sizes a
            // row when it wraps it, and highlights, inlays and folds below
            // all take display snapshots.
            if let Some(ranges) = &gutter_anchor_ranges {
                let snapshot = attachment.multi_buffer.read(cx).snapshot(cx);
                let scales = ranges
                    .iter()
                    .filter(|(class, _)| *class == StyleClass::UserMessage)
                    .filter_map(|(_, range)| excerpt_range(&snapshot, range))
                    .map(|range| (range, crate::style::USER_MESSAGE_SCALE))
                    .collect::<Vec<_>>();
                let display_map = editor.read(cx).display_map.clone();
                display_map.update(cx, |display_map, cx| display_map.set_row_scales(scales, cx));
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
            reconcile_visualizations(
                &desired_visualizations,
                &mut attachment.visualizations,
                visualization_cache,
                visualization_client,
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

fn transcript_path(start_block: usize) -> PathKey {
    PathKey::sorted(start_block as u64)
}

fn composed_excerpt_end(buffer: &Buffer) -> Point {
    debug_assert!(buffer.as_rope().reversed_chars_at(buffer.len()).next() == Some('\n'));
    buffer.offset_to_point(buffer.len() - 1)
}

fn desired_document_tail(buffer: &Buffer, turn_open: bool) -> (DocumentTail, Point) {
    let buffer_id = buffer.remote_id();
    if turn_open {
        (
            DocumentTail::Growing(buffer_id),
            composed_excerpt_end(buffer),
        )
    } else {
        let len = buffer.len();
        let trailing = buffer
            .as_rope()
            .reversed_chars_at(len)
            .take_while(|c| *c == '\n')
            .count();
        let end = buffer.offset_to_point(len - trailing);
        (DocumentTail::Cropped(buffer_id, end), end)
    }
}

fn prompt_gap_excerpt_end(buffer: &Buffer) -> Point {
    let len = buffer.len();
    let trailing = buffer
        .as_rope()
        .reversed_chars_at(len)
        .take_while(|character| *character == '\n')
        .count();
    debug_assert!(trailing > 0);
    buffer.offset_to_point(len - trailing.saturating_sub(1))
}

fn block_record(
    buffer_entity: &Entity<Buffer>,
    buffer: &Buffer,
    start: usize,
    rendered: RenderedBlock,
    terminal_newline_supplied_by_excerpt: bool,
) -> BlockRecord {
    let (span_ranges, inlays, gutter, visualizations) =
        spans_for_rendered(buffer, start, &rendered);
    let text = rendered_text(&rendered);
    let style_end = text
        .len()
        .checked_sub(usize::from(terminal_newline_supplied_by_excerpt))
        .map(|len| start + len);
    let styles = styles_for_rendered(buffer, &rendered, &span_ranges, style_end);
    BlockRecord {
        buffer: buffer_entity.clone(),
        range: buffer.anchor_before(start)
            ..buffer.anchor_before(
                start + text.len() - usize::from(terminal_newline_supplied_by_excerpt),
            ),
        kind: rendered.kind,
        visible: rendered.visible(),
        text,
        gutter,
        inlays,
        styles,
        terminal_newline_supplied_by_excerpt,
        visualizations,
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
    if old.ends_with('\n') && new.ends_with('\n') {
        // Keep the transcript's final newline as a suffix sentinel. Inserts
        // before it move the block's left-biased end anchor; consuming it as
        // a prefix would append after the anchor when streaming a new line.
        prefix = prefix.min(old.len() - 1).min(new.len() - 1);
    }
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

fn spans_for_rendered(buffer: &Buffer, start: usize, rendered: &RenderedBlock) -> PlacedSpans {
    let mut ranges = Vec::with_capacity(rendered.spans.len());
    let mut inlays = Vec::new();
    let mut gutter = None;
    let mut offset = start;
    for (index, span) in rendered.spans.iter().enumerate() {
        let end = offset + span.text.len();
        let range = buffer.anchor_before(offset)..buffer.anchor_before(end);
        if let Some(spec) = rendered
            .inlay
            .as_ref()
            .filter(|spec| spec.span_index == index)
        {
            inlays.push(InlayRecord::new(range.start, spec.content.clone()));
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
    inlays.extend(rendered.table_padding.iter().map(|padding| {
        InlayRecord::text(
            buffer.anchor_before(start + padding.position),
            "\t".repeat(padding.tabs),
        )
    }));
    let visualizations = rendered
        .visualizations
        .iter()
        .map(|visualization| VisualizationAnchor {
            id: visualization.id.clone(),
            rows: visualization.rows,
            range: buffer.anchor_before(start + visualization.range.start)
                ..buffer.anchor_before(start + visualization.range.end),
        })
        .collect();
    (ranges, inlays, gutter, visualizations)
}

fn styles_for_rendered(
    buffer: &Buffer,
    rendered: &RenderedBlock,
    ranges: &[Range<Anchor>],
    source_end: Option<usize>,
) -> Vec<(StyleClass, Range<Anchor>)> {
    rendered
        .spans
        .iter()
        .zip(ranges)
        .filter(|(span, _)| span.class != StyleClass::Default && !span.text.is_empty())
        .filter_map(|(span, range)| {
            let start = range.start.to_offset(buffer);
            let end = source_end.map_or_else(
                || range.end.to_offset(buffer),
                |source_end| range.end.to_offset(buffer).min(source_end),
            );
            (start < end).then(|| (span.class, range.start..buffer.anchor_before(end)))
        })
        .collect()
}

fn reconcile_visualizations<V: 'static>(
    desired: &[VisualizationAnchor],
    placed: &mut Vec<PlacedVisualization>,
    cache: &mut HashMap<String, Entity<Visualization>>,
    client: &VisualizationClient,
    multi_buffer: &Entity<MultiBuffer>,
    editor: &Entity<Editor>,
    cx: &mut Context<V>,
) {
    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let desired_keys = desired
        .iter()
        .filter_map(|desired| {
            visualization_key(&desired.id, desired.rows, &desired.range, &snapshot)
        })
        .collect::<HashSet<_>>();
    let mut removed = collections::HashSet::default();
    placed.retain(|placed| {
        let keep = visualization_key(&placed.id, placed.rows, &placed.source_range, &snapshot)
            .is_some_and(|key| desired_keys.contains(&key));
        if !keep {
            removed.insert(placed.block_id);
        }
        keep
    });
    if !removed.is_empty() {
        editor.update(cx, |editor, cx| editor.remove_blocks(removed, None, cx));
    }

    let mut placed_keys = placed
        .iter()
        .filter_map(|placed| {
            visualization_key(&placed.id, placed.rows, &placed.source_range, &snapshot)
        })
        .collect::<HashSet<_>>();
    for desired in desired {
        let Some(key) = visualization_key(&desired.id, desired.rows, &desired.range, &snapshot)
        else {
            continue;
        };
        if !placed_keys.insert(key) {
            continue;
        }
        let Some(start) = snapshot.anchor_in_excerpt(desired.range.start) else {
            continue;
        };
        let Some(end) = snapshot.anchor_in_excerpt(desired.range.end) else {
            continue;
        };
        let view = match cache.get(&desired.id) {
            Some(view) => view.clone(),
            None => {
                let view = cx.new(|_| Visualization::new(desired.id.clone(), client.clone()));
                cache.insert(desired.id.clone(), view.clone());
                view
            }
        };
        let render_view = view.clone();
        let ids = editor.update(cx, |editor, cx| {
            editor.insert_blocks(
                [BlockProperties {
                    // Preserve the reference in the buffer for copy/search,
                    // but replace its whole source line in the display map.
                    placement: BlockPlacement::Replace(start..=end),
                    height: Some(desired.rows),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |_| render_view.clone().into_any_element()),
                    priority: 0,
                }],
                None,
                cx,
            )
        });
        if let Some(block_id) = ids.into_iter().next() {
            placed.push(PlacedVisualization {
                id: desired.id.clone(),
                rows: desired.rows,
                source_range: desired.range.clone(),
                block_id,
            });
        }
    }
}

fn visualization_key(
    id: &str,
    rows: u32,
    range: &Range<Anchor>,
    snapshot: &multi_buffer::MultiBufferSnapshot,
) -> Option<(String, u32, usize, usize)> {
    let start = snapshot
        .anchor_in_excerpt(range.start)?
        .to_offset(snapshot)
        .0;
    let end = snapshot.anchor_in_excerpt(range.end)?.to_offset(snapshot).0;
    Some((id.to_owned(), rows, start, end))
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
    fn rendered_text_edit_preserves_terminal_newline_sentinel() {
        let edit = rendered_text_edit("line\n", "line\nnext\n").expect("edit");
        assert_eq!(edit.old_range, 4..4);
        assert_eq!(edit.inserted, "\nnext");
    }

    #[test]
    fn rendered_text_edit_is_utf8_boundary_safe() {
        let edit = rendered_text_edit("a🙂c", "a🙂bc").expect("edit");
        assert_eq!(edit.old_range, "a🙂".len().."a🙂".len());
        assert_eq!(edit.inserted, "b");
    }
}
