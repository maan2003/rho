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
//! A screen opens at the cost of what it draws, so history is lazy in
//! both senses: the blocks above the opening tail are neither rendered nor
//! composed into the multibuffer until a reader asks for them, by scrolling
//! into them or by a verb that needs the whole transcript. The blocks
//! themselves stay whole in memory — they are the fold's, shared by
//! pointer — and `records` and `buffers` cover `blocks[uncomposed..]`.
//!
//! Highlights are bucketed per [`StyleClass`] into two editor highlight keys
//! each, split at the start of the live turn (after the last user message) —
//! history ranges change at most once per turn; live-turn ranges are small,
//! so per-streaming-event churn stays bounded. The boundary is derived from
//! the block list itself; moving it re-buckets highlights without touching
//! the buffer.

pub mod elisions;
mod gap;
mod inlays;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use editor::Editor;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
pub use elisions::HistoryFold;
use elisions::{ElisionState, ElisionSync};
use gpui::{AppContext as _, Context, Entity, IntoElement as _, Reservation, WeakEntity};
use inlays::{InlayRecord, PlacedInlay};
use language::{Buffer, Point};
use multi_buffer::{MultiBuffer, PathKey, ToOffset as _};
use rho_hosts::connection::VisualizationClient;
use rho_ui_proto::AgentId;
use rho_window::highlights::{apply_class_highlights, excerpt_range};
use rho_window::style::{Region, StyleClass};
use rho_window::visualization::Visualization;
use text::{Anchor, Buffer as TextBuffer, ToOffset as _};

use crate::render::elision::ElisionPlan;
use crate::render::{
    BlockKind, RenderedBlock, block_kind, block_visible, render_block_with_agent_labels,
};
use crate::state::{UiAgentState, UiBlock};
use crate::store::{FrameSummary, IncrementalUpdate};

mod store;

pub use store::{FrameChange, TranscriptFrame, Transcripts};

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
    /// Every block of the transcript, shared with the fold that made them:
    /// a clone copies pointers, never text. The model keeps them so the
    /// history a reader asks for is composed without asking for it again.
    blocks: Vec<Arc<UiBlock>>,
    /// Whether each block renders to anything, kept beside `blocks` so an
    /// elision refresh never re-renders history to find out.
    visible: Vec<bool>,
    /// How many blocks the gap still holds, read by the marker's own
    /// render so the count falls as the gap closes without the block being
    /// written again.
    gap_remaining: Arc<AtomicUsize>,
    /// Blocks composed at the top for a reader who asked for the top:
    /// `blocks[..head]`, and the first `head` records. Zero until a reader
    /// asks for the top, which is the state the transcript opens in.
    head: usize,
    /// The end of the gap: `blocks[head..uncomposed]` is composed nowhere.
    /// `records` and `buffers` cover the head and then `blocks[uncomposed..]`,
    /// so composition grows from both ends and the gap closes in the middle.
    uncomposed: usize,
    /// What other agents' messages are labelled, kept so history composed
    /// later reads the same as history composed at open.
    agent_labels: HashMap<AgentId, String>,
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
    /// The row that says the gap is still composing, and the block it sits
    /// above, so it is moved only when that block changes.
    gap_marker: Option<(CustomBlockId, usize)>,
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
    visible: Vec<bool>,
    /// The first block the opening tail composes; everything before it is
    /// history the reader has not asked for yet.
    first_block: usize,
    agent_labels: HashMap<AgentId, String>,
    chunks: Vec<PreparedChunk>,
}

struct PreparedChunk {
    start_block: usize,
    markdown: bool,
    rendered: Vec<RenderedBlock>,
    terminal_record: Option<usize>,
    text: String,
    /// Where each of `rendered` sits in `text`, from the same walk that
    /// built it.
    spans: Vec<Range<usize>>,
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
            blocks: Vec::new(),
            visible: Vec::new(),
            gap_remaining: Arc::new(AtomicUsize::new(0)),
            head: 0,
            uncomposed: 0,
            agent_labels: HashMap::new(),
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
    ///
    /// Only the tail is rendered: a screen opens at the cost of what it
    /// draws, and history above the opening tail is rendered when the
    /// reader asks for it.
    pub(crate) fn prepare_initial(
        state: UiAgentState,
        now_ms: u64,
        agent_labels: HashMap<AgentId, String>,
    ) -> PreparedInitialTranscript {
        let label = |id: AgentId| agent_labels.get(&id).cloned().unwrap_or_default();
        let visible = state
            .blocks
            .iter()
            .map(|b| block_visible(b))
            .collect::<Vec<_>>();
        let first_block = tail_start(
            &state.blocks,
            state.blocks.len(),
            OPENING_ROWS,
            now_ms,
            &label,
        );
        let chunks = render_chunks(
            &state.blocks,
            &visible,
            first_block..state.blocks.len(),
            now_ms,
            &label,
        );
        PreparedInitialTranscript {
            state,
            visible,
            first_block,
            agent_labels,
            chunks,
        }
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
    ) {
        debug_assert!(self.records.is_empty());
        debug_assert!(self.buffers.is_empty());
        debug_assert_eq!(prepared.chunks.len(), text_buffers.len());

        self.turn_open = crate::store::turn_open(prepared.state.status);
        self.blocks = prepared.state.blocks;
        self.visible = prepared.visible;
        self.head = 0;
        self.uncomposed = prepared.first_block;
        self.agent_labels = prepared.agent_labels;
        let mut installed = Vec::with_capacity(prepared.chunks.len());
        for (chunk, (reservation, text_buffer)) in prepared.chunks.into_iter().zip(text_buffers) {
            let buffer = cx.insert_entity(reservation, |cx| {
                let mut buffer = Buffer::build(text_buffer, None, language::Capability::Read);
                if chunk.markdown {
                    rho_window::markdown::configure_buffer(&mut buffer, cx);
                }
                buffer
            });
            installed.push((chunk, buffer));
        }

        let mut gutters_changed = false;
        for (chunk, buffer) in installed {
            {
                let snapshot = buffer.read(cx);
                let spans = chunk.spans;
                for (record_index, (rendered, span)) in
                    chunk.rendered.into_iter().zip(spans).enumerate()
                {
                    let record = block_record(
                        &buffer,
                        snapshot,
                        span,
                        rendered,
                        chunk.terminal_record == Some(record_index),
                    );
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
        self.refresh_elision_plans(self.uncomposed);
        let history = classes_in(&self.records[..self.turn_boundary]);
        let live = classes_in(&self.records[self.turn_boundary..]);
        self.apply_to_attachments(now_ms, &history, &live, gutters_changed, cx);
        self.warm_first_screen(cx);
        cx.notify();
    }

    /// Parses the syntax of the buffers the window is about to draw.
    ///
    /// Everywhere else the editor does this for itself, on an excerpt
    /// change, a display-map change or a scroll — but it parses what it
    /// can see, and at open it has not laid out yet and can see nothing.
    /// So the first screen is warmed here, and only the first screen: a
    /// window's rows counted back from the tail, which is where a
    /// transcript opens. What is above them is parsed when the reader
    /// scrolls to it, by the element that draws it.
    fn warm_first_screen<V: 'static>(&mut self, cx: &mut Context<V>) {
        let mut rows = 0;
        let mut screen = Vec::new();
        for turn in self.buffers.iter().rev() {
            if rows >= WINDOW_ROWS {
                break;
            }
            // Counted whether or not it has a language: a buffer with no
            // syntax still takes up the screen the bound is written in.
            rows += turn.buffer.read(cx).max_point().row as usize + 1;
            if turn.buffer.read(cx).language().is_some() {
                screen.push(turn.buffer.clone());
            }
        }
        for buffer in screen {
            buffer.update(cx, |buffer, cx| {
                buffer.ensure_syntax_parsed(cx);
            });
        }
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
            gap_marker: None,
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
        let Some(first_changed_block) = summary.first_changed_block else {
            // Status alone can close the turn; the document tail follows,
            // and a replaced excerpt triggers the full re-apply inside.
            let empty = HashSet::new();
            self.apply_to_attachments(now_ms, &empty, &empty, false, cx);
            return;
        };
        self.remember_blocks(state, first_changed_block, agent_label);

        if first_changed_block < self.uncomposed {
            // The change is under what is composed, so what is composed no
            // longer describes the transcript: the screen opens again on
            // its tail, at the cost of what it draws. A head a reader asked
            // for goes with it — the blocks it holds are the ones that
            // changed.
            self.recompose(now_ms, cx);
            return;
        }
        let first_changed = self
            .record_of(first_changed_block)
            .expect("a block at or after the gap is composed");

        if let Some(incremental) = summary.incremental
            && self.try_incremental_sync(first_changed, incremental, now_ms, cx)
        {
            return;
        }

        let requested_start = first_changed.min(self.records.len());
        let start = self.rebuild_start(requested_start);

        let mut prev_kind = last_visible_kind(&self.records[..start]);
        let label = |id| self.label(id);
        let rendered_blocks = self
            .blocks
            .get(self.block_of(start)..)
            .unwrap_or(&[])
            .iter()
            .map(|block| {
                let block = render_block_with_agent_labels(block, prev_kind, now_ms, &label);
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

        self.refresh_elision_plans(self.block_of(start));
        self.apply_to_attachments(now_ms, &changed_history, &changed_live, gutters_changed, cx);
        cx.notify();
    }

    /// Where a suffix rebuild has to start, in record space: a buffer is
    /// replaced whole, so a change inside one starts at that buffer.
    fn rebuild_start(&self, requested: usize) -> usize {
        if let Some(record) = self.records.get(requested) {
            return self
                .buffers
                .iter()
                .find(|turn| turn.buffer == record.buffer)
                .map_or(requested, |turn| {
                    self.record_of(turn.start_block).unwrap_or(requested)
                });
        }
        let next_is_response = self
            .blocks
            .get(self.block_of(requested))
            .is_some_and(|block| matches!(block_kind(block), BlockKind::Response { .. }));
        if next_is_response
            && self
                .records
                .last()
                .is_some_and(|record| matches!(record.kind, BlockKind::Response { .. }))
            && let Some(turn) = self.buffers.last()
        {
            return self.record_of(turn.start_block).unwrap_or(requested);
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
        let start_block = self.block_of(start);
        let first_removed = self
            .buffers
            .iter()
            .position(|turn| turn.start_block >= start_block)
            .unwrap_or(self.buffers.len());
        let removed = self.buffers.split_off(first_removed);
        let removed_buffers = removed
            .into_iter()
            .map(|turn| (transcript_path(turn.start_block), turn.buffer))
            .collect::<Vec<_>>();

        let mut chunks: Vec<(usize, bool, Vec<RenderedBlock>)> = Vec::new();
        let mut rows = 0;
        let mut fence_open = false;
        for (offset, rendered) in rendered_blocks.into_iter().enumerate() {
            let block_index = start_block + offset;
            let markdown = rendered.markdown;
            let block_rows = rendered_rows(&rendered);
            if starts_chunk(
                chunks.last().map(|(_, markdown, _)| *markdown),
                markdown,
                rows,
                block_rows,
                fence_open,
            ) {
                chunks.push((block_index, markdown, Vec::new()));
                rows = 0;
            }
            rows += block_rows;
            fence_open = leaves_fence_open(&rendered);
            chunks.last_mut().unwrap().2.push(rendered);
        }

        let mut prepared = Vec::with_capacity(chunks.len());
        for (start_block, markdown, rendered) in chunks {
            let terminal_record = rendered.iter().rposition(RenderedBlock::visible);
            let (text, spans) = chunk_text_and_spans(&rendered);
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(&text, cx);
                if markdown {
                    rho_window::markdown::configure_buffer(&mut buffer, cx);
                }
                buffer.set_capability(language::Capability::Read, cx);
                buffer
            });
            prepared.push((start_block, rendered, spans, terminal_record, buffer));
        }

        for (start_block, rendered, spans, terminal_record, buffer) in prepared {
            {
                let snapshot = buffer.read(cx);
                for (record_index, (rendered, span)) in rendered.into_iter().zip(spans).enumerate()
                {
                    let record = block_record(
                        &buffer,
                        snapshot,
                        span,
                        rendered,
                        terminal_record == Some(record_index),
                    );
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
        self.reset_rebuilt_excerpts(first_removed, old_last_composed, removed_buffers, cx);
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

    /// `first_changed` and the returned index are in record space.
    fn try_incremental_sync<V: 'static>(
        &mut self,
        first_changed: usize,
        incremental: IncrementalUpdate,
        now_ms: u64,
        cx: &mut Context<V>,
    ) -> bool {
        let block_index = match incremental {
            IncrementalUpdate::AssistantText { index }
            | IncrementalUpdate::ReasoningText { index }
            | IncrementalUpdate::Tool { index } => index,
        };
        let Some(index) = block_index.checked_sub(self.uncomposed) else {
            return false;
        };
        if index != first_changed || index >= self.records.len() {
            return false;
        }
        self.resplice_block(index, block_index, now_ms, cx)
    }

    /// Re-renders one composed block in place: the record's text is edited
    /// to what the block now says, and its anchors, styles, inlays and
    /// gutter follow. Everything either side of it keeps its own. Answers
    /// `false` when the block no longer renders to the same shape, which
    /// no in-place edit can carry.
    fn resplice_block<V: 'static>(
        &mut self,
        index: usize,
        block_index: usize,
        now_ms: u64,
        cx: &mut Context<V>,
    ) -> bool {
        let prev_kind = last_visible_kind(&self.records[..index]);
        let Some(block) = self.blocks.get(block_index) else {
            return false;
        };
        let rendered =
            render_block_with_agent_labels(block, prev_kind, now_ms, &|id| self.label(id));
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

            // The block's extent after the edit is where its own anchors
            // now are: the edit moved them, and nothing re-derives them
            // from the rendered string's length. A streaming block's end is
            // right-biased, so text arriving before the trailing newline
            // lands inside the block rather than after it.
            let range = self.records[index].range.clone();
            let block_end = range.end.to_offset(buffer);
            let (span_ranges, inlays, gutter, visualizations) =
                spans_for_rendered(buffer, block_start, &rendered);
            let styles = styles_for_rendered(buffer, &rendered, &span_ranges, Some(block_end));
            let new_relative_styles = relative_style_ranges(buffer, block_start, &styles);
            changed.extend(changed_style_classes(
                &old_relative_styles,
                &new_relative_styles,
            ));

            gutters_changed = self.records[index].gutter.is_some() || gutter.is_some();
            self.records[index] = BlockRecord {
                buffer: record_buffer.clone(),
                range,
                kind: rendered.kind,
                visible: rendered.visible(),
                text: new_text,
                gutter,
                inlays,
                styles,
                terminal_newline_supplied_by_excerpt,
                visualizations,
            };
            // Every record of this buffer, not only the edited one: an edit
            // at a block boundary moves the bytes of one record and the
            // anchors of the next.
            #[cfg(debug_assertions)]
            for record in self.records.iter().filter(|r| r.buffer == record_buffer) {
                assert_record_names_its_text(record, buffer);
            }
        });

        let empty = HashSet::new();
        let (changed_history, changed_live) = if live_region {
            (&empty, &changed)
        } else {
            (&changed, &empty)
        };
        self.refresh_elision_plans(block_index);
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

    fn refresh_elision_plans(&mut self, first_changed_block: usize) {
        let Self {
            records,
            elisions,
            blocks,
            visible,
            head,
            uncomposed,
            turn_open,
            ..
        } = self;
        let (head, uncomposed) = (*head, *uncomposed);
        let record_of = |block: usize| {
            if block < head {
                Some(block)
            } else if block >= uncomposed {
                Some(head + (block - uncomposed))
            } else {
                None
            }
        };
        elisions.refresh(blocks, first_changed_block, visible, *turn_open, |plan| {
            plan_anchor_range(records, &record_of, plan)
        });
    }

    /// The label another agent's messages are rendered under.
    fn label(&self, id: AgentId) -> String {
        self.agent_labels.get(&id).cloned().unwrap_or_default()
    }

    /// Keeps the model's own block list current at the cost of what
    /// changed. Blocks are pointers into the fold that made them, so this
    /// copies no text.
    fn remember_blocks(
        &mut self,
        state: &UiAgentState,
        first_changed: usize,
        agent_label: &impl Fn(AgentId) -> String,
    ) {
        let first_changed = first_changed.min(self.blocks.len()).min(state.blocks.len());
        self.blocks.truncate(first_changed);
        self.visible.truncate(first_changed);
        for block in &state.blocks[first_changed..] {
            self.visible.push(block_visible(block));
            if let UiBlock::AgentMessage { sender, .. }
            | UiBlock::QueuedMessage {
                sender: Some(sender),
                ..
            } = &**block
            {
                self.agent_labels.insert(*sender, agent_label(*sender));
            }
            self.blocks.push(block.clone());
        }
    }

    /// How many blocks are rendered and composed nowhere: the gap between
    /// the head and the tail.
    pub fn uncomposed_blocks(&self) -> usize {
        self.uncomposed - self.head
    }

    pub fn has_uncomposed(&self) -> bool {
        self.uncomposed > self.head
    }

    /// How many leading blocks are composed for a reader at the top.
    pub fn head_blocks(&self) -> usize {
        self.head
    }

    /// The block the gap marker sits above, in the first attachment: what
    /// the reader is told is still on its way, and `None` when nothing is.
    pub fn gap_marker_block(&self) -> Option<usize> {
        self.attachments
            .first()
            .and_then(|attachment| attachment.gap_marker)
            .map(|(_, block)| block)
    }

    /// Whether a block is composed anywhere: in the head a reader asked
    /// for, or in the tail the transcript opened on.
    pub fn is_composed(&self, block: usize) -> bool {
        block < self.head || block >= self.uncomposed
    }

    /// The record covering a block, or `None` while the block is in the
    /// gap. One block renders to one record, so the head's blocks and its
    /// records are the same run.
    fn record_of(&self, block: usize) -> Option<usize> {
        if block < self.head {
            Some(block)
        } else if block >= self.uncomposed {
            Some(self.head + (block - self.uncomposed))
        } else {
            None
        }
    }

    /// The block a record covers.
    fn block_of(&self, record: usize) -> usize {
        if record < self.head {
            record
        } else {
            self.uncomposed + (record - self.head)
        }
    }

    /// Renders and composes the history immediately above the tail — at
    /// least `rows` more rows of it — and answers whether any gap is left.
    /// What a reader moving up into history asks for.
    pub fn compose_history<V: 'static>(
        &mut self,
        rows: usize,
        now_ms: u64,
        cx: &mut Context<V>,
    ) -> bool {
        if !self.has_uncomposed() {
            return false;
        }
        let from = {
            let label = |id| self.label(id);
            tail_start(&self.blocks, self.uncomposed, rows, now_ms, &label).max(self.head)
        };
        self.compose_range(from..self.uncomposed, FillEdge::Tail, now_ms, cx);
        self.has_uncomposed()
    }

    /// Renders and composes the top of the transcript — at least `rows`
    /// rows of it, or what is left of the gap — and answers whether any
    /// gap is left. What a reader who asked for the top gets first, and the
    /// end the gap then closes from while they read down it.
    ///
    /// A reader who asks for the top waits for a screen, not for the
    /// transcript: the blocks between the top and the tail are composed
    /// behind them, and nothing above or below them is laid out again.
    pub fn compose_head<V: 'static>(
        &mut self,
        rows: usize,
        now_ms: u64,
        cx: &mut Context<V>,
    ) -> bool {
        if !self.has_uncomposed() {
            return false;
        }
        let end = {
            let label = |id| self.label(id);
            head_end(
                &self.blocks,
                self.head,
                self.uncomposed,
                rows,
                now_ms,
                &label,
            )
        };
        self.compose_range(self.head..end, FillEdge::Head, now_ms, cx);
        self.has_uncomposed()
    }

    /// Composes a range of blocks into one of the two composed runs and
    /// puts its excerpts in place. Everything either side keeps its ids,
    /// its anchors and its layout: composing costs the rows it composes.
    fn compose_range<V: 'static>(
        &mut self,
        range: Range<usize>,
        edge: FillEdge,
        now_ms: u64,
        cx: &mut Context<V>,
    ) {
        let chunks = {
            let label = |id| self.label(id);
            render_chunks(&self.blocks, &self.visible, range.clone(), now_ms, &label)
        };
        let mut gutters_changed = false;
        let (new_buffers, new_records) = Self::build_chunks(chunks, &mut gutters_changed, cx);
        let added_buffers = new_buffers.len();
        let added_records = new_records.len();
        // Both runs meet at the gap, so both fills splice there: the head's
        // records end where the tail's begin.
        let first_record = self.head;
        let first_buffer = self
            .buffers
            .partition_point(|turn| turn.start_block < self.head);
        self.buffers.splice(first_buffer..first_buffer, new_buffers);
        self.records.splice(first_record..first_record, new_records);
        match edge {
            FillEdge::Head => self.head = range.end,
            FillEdge::Tail => self.uncomposed = range.start,
        }
        if self.head == self.uncomposed {
            // The gap is closed, so the two runs are one run from the first
            // block, which is the state the model started in.
            self.head = 0;
            self.uncomposed = 0;
        }
        let added_records = first_record..first_record + added_records;
        let added_buffers = first_buffer..first_buffer + added_buffers;

        let buffer_ids = self.buffers[added_buffers.clone()]
            .iter()
            .filter(|turn| turn.composed)
            .map(|turn| turn.buffer.read(cx).remote_id())
            .collect::<Vec<_>>();
        for attachment in &self.attachments {
            if let Some(editor) = attachment.editor.upgrade() {
                let buffer_ids = buffer_ids.clone();
                editor.update(cx, |editor, cx| {
                    editor.disable_headers_for_buffers(buffer_ids, cx)
                });
            }
        }

        let last_composed = self.buffers.iter().rposition(|turn| turn.composed);
        let mut document_tail = None;
        let turn_open = self.turn_open;
        let entries = self.buffers[added_buffers.clone()]
            .iter()
            .enumerate()
            .map(|(index, turn)| (index + first_buffer, turn))
            .filter(|(_, turn)| turn.composed)
            .map(|(index, turn)| {
                let buffer = turn.buffer.read(cx);
                let full_end = if Some(index) == last_composed {
                    prompt_gap_excerpt_end(buffer)
                } else {
                    composed_excerpt_end(buffer)
                };
                let document_end = if Some(index) == last_composed {
                    let (tail, end) = desired_document_tail(buffer, turn_open);
                    document_tail = Some(tail);
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
        if document_tail.is_some() {
            self.document_tail = document_tail;
        }
        self.multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(
                entries
                    .iter()
                    .map(|(path, buffer, end, _)| {
                        (path.clone(), buffer.clone(), vec![Point::zero()..*end])
                    })
                    .collect::<Vec<_>>(),
                0,
                cx,
            );
        });
        self.document_multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(
                entries
                    .iter()
                    .map(|(path, buffer, _, end)| {
                        (path.clone(), buffer.clone(), vec![Point::zero()..*end])
                    })
                    .collect::<Vec<_>>(),
                0,
                cx,
            );
        });

        self.turn_boundary = turn_boundary(&self.records);
        let boundary = self
            .turn_boundary
            .clamp(added_records.start, added_records.end);
        let changed_history = classes_in(&self.records[added_records.start..boundary]);
        let changed_live = classes_in(&self.records[boundary..added_records.end]);
        self.refresh_elision_plans(range.start);
        self.apply_to_attachments(now_ms, &changed_history, &changed_live, gutters_changed, cx);
        cx.notify();
    }

    /// Opens the transcript again on its tail, dropping everything that was
    /// composed. A change under the composed tail rewrites history the
    /// model holds no records for, and reopening costs what it draws.
    fn recompose<V: 'static>(&mut self, now_ms: u64, cx: &mut Context<V>) {
        let removed = self
            .buffers
            .drain(..)
            .map(|turn| (transcript_path(turn.start_block), turn.buffer))
            .collect::<Vec<_>>();
        self.records.clear();
        self.head = 0;
        self.uncomposed = self.blocks.len();
        self.turn_boundary = 0;
        self.elisions = ElisionSync::default();
        self.document_tail = None;
        for multi_buffer in [
            self.multi_buffer.clone(),
            self.document_multi_buffer.clone(),
        ] {
            let removed = removed.clone();
            multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.set_excerpts_for_paths(
                    removed
                        .into_iter()
                        .map(|(path, buffer)| (path, buffer, Vec::new())),
                    0,
                    cx,
                );
            });
        }
        self.compose_history(OPENING_ROWS, now_ms, cx);
        let history = classes_in(&self.records[..self.turn_boundary]);
        let live = classes_in(&self.records[self.turn_boundary..]);
        self.apply_to_attachments(now_ms, &history, &live, true, cx);
        cx.notify();
    }

    /// Builds the buffers and records for prepared chunks.
    fn build_chunks<V: 'static>(
        chunks: Vec<PreparedChunk>,
        gutters_changed: &mut bool,
        cx: &mut Context<V>,
    ) -> (Vec<TranscriptBuffer>, Vec<BlockRecord>) {
        let mut prepared = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(&chunk.text, cx);
                if chunk.markdown {
                    rho_window::markdown::configure_buffer(&mut buffer, cx);
                }
                buffer.set_capability(language::Capability::Read, cx);
                buffer
            });
            prepared.push((chunk, buffer));
        }
        let mut buffers = Vec::with_capacity(prepared.len());
        let mut records = Vec::new();
        for (chunk, buffer) in prepared {
            {
                let snapshot = buffer.read(cx);
                let spans = chunk.spans;
                for (record_index, (rendered, span)) in
                    chunk.rendered.into_iter().zip(spans).enumerate()
                {
                    let record = block_record(
                        &buffer,
                        snapshot,
                        span,
                        rendered,
                        chunk.terminal_record == Some(record_index),
                    );
                    *gutters_changed |= record.gutter.is_some();
                    records.push(record);
                }
            }
            let composed = !buffer.read(cx).is_empty();
            buffers.push(TranscriptBuffer {
                start_block: chunk.start_block,
                composed,
                buffer,
            });
        }
        (buffers, records)
    }

    /// Where a point in an attached editor sits in the store: which block,
    /// and how far into it. A buffer offset does not survive a transcript
    /// whose history is composed on demand; this does. Costs the buffer it
    /// lands in, not the transcript.
    pub fn store_point(&self, anchor: &multi_buffer::Anchor, cx: &gpui::App) -> Option<StorePoint> {
        let buffer_id = anchor.buffer_id()?;
        let text_anchor = anchor.raw_text_anchor()?;
        let turn = self
            .buffers
            .iter()
            .find(|turn| turn.buffer.read(cx).remote_id() == buffer_id)?;
        let buffer = turn.buffer.read(cx);
        let offset = text_anchor.to_offset(buffer);
        let mut found = None;
        for (index, record) in self
            .records
            .iter()
            .enumerate()
            .skip(self.record_of(turn.start_block)?)
        {
            if record.buffer != turn.buffer {
                break;
            }
            let start = record.range.start.to_offset(buffer);
            if start > offset {
                break;
            }
            found = Some(StorePoint {
                block: self.block_of(index),
                offset: offset - start,
            });
        }
        found
    }

    /// The buffer anchor a store point names, if its block is composed.
    pub fn place_store_point(&self, point: StorePoint, cx: &gpui::App) -> Option<text::Anchor> {
        let record = self.records.get(self.record_of(point.block)?)?;
        let buffer = record.buffer.read(cx);
        let start = record.range.start.to_offset(buffer);
        let end = record.range.end.to_offset(buffer);
        Some(buffer.anchor_before((start + point.offset).min(end)))
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

        // Where the gap is, for the row that says so: above the tail's
        // first block, which stays put while the head grows towards it.
        let gap_at = (self.head > 0)
            .then(|| Some((self.records.get(self.head)?.range.start, self.uncomposed)))
            .flatten();
        self.gap_remaining
            .store(self.uncomposed - self.head, Ordering::Relaxed);

        let Self {
            document_multi_buffer,
            next_inlay_id,
            attachments,
            elisions,
            visualization_client,
            visualization_cache,
            gap_remaining,
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
                    .map(|range| (range, rho_window::style::USER_MESSAGE_SCALE))
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
                        rho_window::style::user_prompt_gutter_color,
                        cx,
                    );
                    editor.highlight_gutter::<AgentMessageGutter>(
                        agent_ranges,
                        rho_window::style::agent_message_gutter_color,
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
            gap::reconcile_marker(
                &mut attachment.gap_marker,
                &attachment.multi_buffer,
                gap_at,
                gap_remaining,
                &editor,
                cx,
            );
            true
        });
    }
}

/// How many rows the tail composes when a transcript opens: enough to
/// fill a window twice over, so opening and the first page of scrolling
/// draw without composing anything more.
pub const OPENING_ROWS: usize = 200;

/// The most rows a chunk carries, and so the most a buffer holds.
///
/// A buffer is replaced whole — `rebuild_start` pulls a change back to the
/// start of the buffer that holds it — so without a cap a block arriving at
/// the end of a long run of one kind rewrites the whole run: forty finished
/// calls and one more makes forty-one records, for one block. The cap makes
/// that rebuild cost the cap instead of the document, and a cap under a
/// window means the worst one is still smaller than a screen.
///
/// A block is never split, so a single block longer than this is its own
/// chunk and the cap is a floor rather than a ceiling for it.
const MAX_CHUNK_ROWS: usize = 32;

/// A window's rows, which is what the opening tail is two of by the line
/// above. What the open parses, since at open there is no laid-out editor
/// to say which rows those are.
const WINDOW_ROWS: usize = OPENING_ROWS / 2;

/// Which end of the gap a composition step closes.
///
/// The reader names it, the way they name the rows a width change wraps
/// first: a reader at the top is read downward from the head, and a reader
/// moving up out of the tail is served from the tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillEdge {
    /// Downward from the top, which is where a reader who asked for the top
    /// is about to read.
    Head,
    /// Upward from the tail, which is where a reader scrolling back into
    /// history is about to read.
    Tail,
}

/// A point in the transcript as the store sees it: which block, and how
/// far into that block's rendered text. What a surface remembers when the
/// reader leaves it, so returning places the point where it was even when
/// the block it names has to be composed again first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorePoint {
    pub block: usize,
    pub offset: usize,
}

/// The first block of the last `rows` rows of `blocks[..end]`, never fewer
/// than one block. Rendering a block to count its rows is cheap beside
/// laying it out, and what this bounds is what gets laid out.
fn tail_start(
    blocks: &[Arc<UiBlock>],
    end: usize,
    rows: usize,
    now_ms: u64,
    label: &impl Fn(AgentId) -> String,
) -> usize {
    let mut counted = 0;
    let mut start = end;
    while start > 0 && counted < rows {
        start -= 1;
        let rendered = render_block_with_agent_labels(&blocks[start], None, now_ms, label);
        counted += rendered
            .spans
            .iter()
            .map(|span| span.text.matches('\n').count())
            .sum::<usize>();
    }
    start
}

/// The block after the first `rows` rows of `blocks[start..end]`, never
/// fewer than one block: what a reader who asked for the top is given, and
/// the unit the gap closes in behind them. The mirror of [`tail_start`],
/// and as cheap: rendering a block to count its rows costs nothing beside
/// laying it out.
fn head_end(
    blocks: &[Arc<UiBlock>],
    start: usize,
    end: usize,
    rows: usize,
    now_ms: u64,
    label: &impl Fn(AgentId) -> String,
) -> usize {
    let mut counted = 0;
    let mut at = start;
    while at < end && counted < rows {
        let rendered = render_block_with_agent_labels(&blocks[at], None, now_ms, label);
        counted += rendered
            .spans
            .iter()
            .map(|span| span.text.matches('\n').count())
            .sum::<usize>();
        at += 1;
    }
    at
}

/// Renders `range` into per-buffer chunks, split where the syntax changes.
/// The separator the first block carries is the one it would have carried
/// with all of history above it, so composing that history later leaves
/// every chunk's text exactly as it is.
fn render_chunks(
    blocks: &[Arc<UiBlock>],
    visible: &[bool],
    range: Range<usize>,
    now_ms: u64,
    label: &impl Fn(AgentId) -> String,
) -> Vec<PreparedChunk> {
    let mut prev = visible[..range.start]
        .iter()
        .rposition(|visible| *visible)
        .map(|index| block_kind(&blocks[index]));
    let mut chunks: Vec<(usize, bool, Vec<RenderedBlock>)> = Vec::new();
    let mut rows = 0;
    let mut fence_open = false;
    for index in range {
        let rendered = render_block_with_agent_labels(&blocks[index], prev, now_ms, label);
        if rendered.visible() {
            prev = Some(rendered.kind);
        }
        let markdown = rendered.markdown;
        let block_rows = rendered_rows(&rendered);
        if starts_chunk(
            chunks.last().map(|(_, markdown, _)| *markdown),
            markdown,
            rows,
            block_rows,
            fence_open,
        ) {
            chunks.push((index, markdown, Vec::new()));
            rows = 0;
        }
        rows += block_rows;
        fence_open = leaves_fence_open(&rendered);
        chunks.last_mut().unwrap().2.push(rendered);
    }
    chunks
        .into_iter()
        .map(|(start_block, markdown, rendered)| {
            let (text, spans) = chunk_text_and_spans(&rendered);
            PreparedChunk {
                start_block,
                markdown,
                terminal_record: rendered.iter().rposition(RenderedBlock::visible),
                text,
                spans,
                rendered,
            }
        })
        .collect()
}

/// The one walk over a chunk's blocks: the text its buffer is built from,
/// and the byte span each block takes in that text. Both come out of the
/// same pass, so nothing downstream has to re-measure a rendered string to
/// find out where a block sits.
/// The rows a rendered block takes, counted the way the buffer will hold
/// them: one per newline in its own text.
fn rendered_rows(rendered: &RenderedBlock) -> usize {
    rendered
        .spans
        .iter()
        .map(|span| span.text.matches('\n').count())
        .sum()
}

/// Whether a block starts a new chunk: a different language, a chunk
/// already at the cap, or a chunk whose last block left a code fence open.
/// `rows` is the rows the open chunk holds and is reset by the caller when
/// a chunk starts.
fn starts_chunk(
    current: Option<bool>,
    markdown: bool,
    rows: usize,
    block_rows: usize,
    fence_open: bool,
) -> bool {
    match current {
        None => true,
        Some(current) => {
            current != markdown || fence_open || (rows > 0 && rows + block_rows > MAX_CHUNK_ROWS)
        }
    }
}

/// Whether a block's own text ends inside a fenced code block. A blank line
/// closes every other markdown construct and every block ends with one, so
/// a fence is the only markup that can reach past the block that opened it.
/// It must not: the next block is another turn, or the user's own words,
/// and a stray fence would draw all of it as code.
fn leaves_fence_open(rendered: &RenderedBlock) -> bool {
    rendered
        .spans
        .iter()
        .flat_map(|span| span.text.lines())
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("```") || line.starts_with("~~~")
        })
        .count()
        % 2
        == 1
}

fn chunk_text_and_spans(rendered: &[RenderedBlock]) -> (String, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut spans = Vec::with_capacity(rendered.len());
    for rendered in rendered {
        let start = text.len();
        text.push_str(&rendered_text(rendered));
        spans.push(start..text.len());
    }
    (text, spans)
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

fn plan_anchor_range(
    records: &[BlockRecord],
    record_of: &impl Fn(usize) -> Option<usize>,
    plan: &ElisionPlan,
) -> Option<Range<Anchor>> {
    let start = records.get(record_of(plan.start_block)?)?.range.start;
    let end = records.get(record_of(plan.end_block)?)?.range.end;
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
    span: Range<usize>,
    rendered: RenderedBlock,
    terminal_newline_supplied_by_excerpt: bool,
) -> BlockRecord {
    let start = span.start;
    let (span_ranges, inlays, gutter, visualizations) =
        spans_for_rendered(buffer, start, &rendered);
    let text = rendered_text(&rendered);
    // The block's last byte is the newline the excerpt supplies for the
    // terminal record, and that byte belongs to the excerpt, not the block.
    let end = span.end - usize::from(terminal_newline_supplied_by_excerpt);
    let styles = styles_for_rendered(buffer, &rendered, &span_ranges, Some(end));
    BlockRecord {
        buffer: buffer_entity.clone(),
        range: buffer.anchor_before(start)
            ..block_end_anchor(buffer, end, terminal_newline_supplied_by_excerpt),
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

/// A record's anchors name its own text, and nothing else. An elision's
/// fold end is a record's end anchor, so a record whose end is a byte off
/// hides the wrong rows; the assertion is here rather than in a test
/// because the sites that can break it are the sites that build records.
#[cfg(debug_assertions)]
fn assert_record_names_its_text(record: &BlockRecord, buffer: &Buffer) {
    let start = record.range.start.to_offset(buffer);
    let end = record.range.end.to_offset(buffer);
    let named = buffer.text_for_range(start..end).collect::<String>();
    let expected = &record.text
        [..record.text.len() - usize::from(record.terminal_newline_supplied_by_excerpt)];
    debug_assert_eq!(
        named, expected,
        "record's anchors name {start}..{end}, which is not its own text"
    );
}

/// A block's end, and which way it leans.
///
/// Only the block a turn is streaming grows at its own end, and it is the
/// terminal one: its last byte is the newline the excerpt supplies, so its
/// end anchor sits just before that newline and a second line arrives
/// exactly there. Left-biased, it would stay put and leave the new line
/// outside the block it belongs to, so the terminal block's end leans
/// right.
///
/// Every other block's end is the next block's start, the same offset in
/// the same buffer. A block that is not last must lean left, or the next
/// block's first line - which arrives at exactly that offset - is swallowed
/// by the block above it.
fn block_end_anchor(buffer: &Buffer, end: usize, streaming: bool) -> Anchor {
    if streaming {
        buffer.anchor_after(end)
    } else {
        buffer.anchor_before(end)
    }
}

fn rendered_text(rendered: &RenderedBlock) -> String {
    let text: String = rendered
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect();
    // The streaming edit keeps a block's trailing newline as a sentinel so
    // an append lands inside the block and never at the next block's start.
    // That is what lets a record's anchors carry its extent through an
    // edit, and it holds only while every visible block ends with one.
    debug_assert!(
        text.is_empty() || text.ends_with('\n'),
        "a visible block's rendered text must end with a newline: {text:?}"
    );
    text
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
