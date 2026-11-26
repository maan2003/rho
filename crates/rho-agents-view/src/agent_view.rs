//! One model per agent: its transcript, the buffer role. Editors are the
//! window role: each surface showing the agent builds its editor over the
//! shared multibuffer via [`AgentModel::build_editor`], with its own
//! cursor, scroll, and folds. The model reconciles every attached editor
//! when content or chrome changes, so the model persists for the session
//! while editors come and go with surfaces.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use editor::display_map::ToDisplayPoint as _;
use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, EntityId, Subscription, Task, WeakEntity, Window};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_agents_client::now_ms;
use rho_agents_client::state::{UiAgentState, UiBlock};
use rho_agents_client::store::FrameSummary;
use rho_window::style::{Region, StyleClass};
use text::{Buffer as TextBuffer, BufferId, ReplicaId};

use crate::render::{Label, Labels};
use crate::transcript::{FillEdge, StorePoint, TranscriptModel};

/// How much history one composition step takes on, in buffer rows.
///
/// Sized to the frame budget rather than to a round number. A step runs on
/// the main thread between two frames, and what it costs is display rows,
/// not the buffer rows counted here: on the desk snapshot at 1280 logical
/// columns a buffer row of a transcript is about 3.4 display rows once it
/// is wrapped, so 400 buffer rows was a step of some 1,400 rows of wrap and
/// a frame well over the 8 ms budget while the reader was already reading
/// at the top. At 40 the median step is 136 display rows and 3.2 ms, and
/// the whole fill draws at p99 7.9 ms. The landing note carries the
/// measurement and what remains over budget: a single block larger than the
/// step, which composition cannot yet divide.
const HISTORY_CHUNK_ROWS: usize = 40;

/// How close to the top of what is composed the reader comes before the
/// next screenful of history is composed.
const PRELOAD_ROWS: f64 = 40.0;

/// The word above the brief.
const BRIEF_LABEL: &str = "brief";

/// What the reader has asked the transcript's history for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HistoryWant {
    /// Enough to keep scrolling up into.
    Screenful,
    /// Down to the block the reader is going to.
    ToBlock(usize),
    /// All of it: the reader asked for something only the whole
    /// transcript answers.
    All,
}

impl HistoryWant {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::ToBlock(one), Self::ToBlock(other)) => Self::ToBlock(one.min(other)),
            (Self::ToBlock(block), _) | (_, Self::ToBlock(block)) => Self::ToBlock(block),
            _ => Self::Screenful,
        }
    }

    /// Whether the transcript has composed what this asked for.
    fn met_by(self, transcript: &TranscriptModel) -> bool {
        match self {
            Self::Screenful => true,
            Self::ToBlock(block) => transcript.is_composed(block),
            Self::All => !transcript.has_uncomposed(),
        }
    }

    /// The block this wants composed, if it names one.
    fn block(self) -> Option<usize> {
        match self {
            Self::ToBlock(block) => Some(block),
            Self::Screenful | Self::All => None,
        }
    }
}

pub struct AgentModel {
    transcript: TranscriptModel,
    /// The document multibuffer: the transcript cropped flush when the
    /// turn is closed. Every editor over this model reads it.
    document_multi_buffer: Entity<MultiBuffer>,
    initial_load_started: bool,
    initial_load_ready: bool,
    initial_load: Option<Task<()>>,
    /// What moved while the bulk load composed, synced once it is ready.
    pending_sync: Option<FrameSummary>,
    /// What history the reader has asked for and has not been given yet.
    history_want: Option<HistoryWant>,
    /// Whether a composition loop is running; it composes a chunk, lets
    /// the window draw, and comes back.
    composing: bool,
    history_task: Option<Task<()>>,
    /// A point waiting for the history that holds it to be composed.
    pending_point: Option<(WeakEntity<Editor>, StorePoint)>,
    /// Where the point was when an editor over this model last moved it,
    /// as a store position: what returning to the surface goes back to.
    remembered_point: Option<StorePoint>,
    /// The idea the agent works from, in full above its transcript.
    brief: Option<Entity<Buffer>>,
    /// Editors currently displaying this agent, weakly held: surfaces own
    /// their editors; the model only reconciles whoever is still alive.
    editors: Vec<WeakEntity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

/// What an agent's screen says to whoever opened it. The screen knows
/// its own state; who cares about it is not its business.
pub enum AgentModelEvent {
    /// The transcript is composed and the screen is ready to draw.
    Loaded,
    /// The history the reader asked for is composed; nothing is waiting.
    HistoryComposed,
    /// Rebuilt chunks invalidated a point; restore its logical step.
    RestorePoints(Vec<(EntityId, StorePoint)>),
}

impl gpui::EventEmitter<AgentModelEvent> for AgentModel {}

impl AgentModel {
    pub fn new(
        visualization_client: rho_agents_client::remote::AgentsLink,
        cx: &mut Context<Self>,
    ) -> Self {
        let multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
        let document_multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
        let transcript = TranscriptModel::new(
            multi_buffer.clone(),
            document_multi_buffer.clone(),
            visualization_client,
        );
        Self {
            transcript,
            document_multi_buffer,
            initial_load_started: false,
            initial_load_ready: false,
            initial_load: None,
            pending_sync: None,
            history_want: None,
            composing: false,
            history_task: None,
            pending_point: None,
            remembered_point: None,
            brief: None,
            editors: Vec::new(),
            _subscriptions: Vec::new(),
        }
    }

    pub fn initial_load_started(&self) -> bool {
        self.initial_load_started
    }

    pub fn initial_load_ready(&self) -> bool {
        self.initial_load_ready
    }

    /// Starts the one asynchronous bulk load. Normal streamed updates
    /// remain synchronous after this reaches Ready.
    pub(crate) fn start_initial_load(
        &mut self,
        state: UiAgentState,
        labels: Labels,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) {
        if self.initial_load_started {
            return;
        }
        self.initial_load_started = true;
        let background = cx.background_executor().clone();
        self.initial_load = Some(cx.spawn(async move |this, cx| {
            let mut prepared = background
                .spawn(async move { TranscriptModel::prepare_initial(state, now_ms, labels) })
                .await;
            let reservations = (0..prepared.buffer_count())
                .map(|_| cx.reserve_entity::<Buffer>())
                .collect::<Vec<_>>();
            let buffer_ids = reservations
                .iter()
                .map(|reservation| BufferId::from(reservation.entity_id().as_non_zero_u64()))
                .collect::<Vec<_>>();
            let texts = prepared.take_texts();
            let text_buffers = background
                .spawn(async move {
                    buffer_ids
                        .into_iter()
                        .zip(texts)
                        .map(|(buffer_id, text)| TextBuffer::new(ReplicaId::LOCAL, buffer_id, text))
                        .collect::<Vec<_>>()
                })
                .await;
            let text_buffers = reservations.into_iter().zip(text_buffers).collect();
            if this
                .update(cx, |this, cx| {
                    this.transcript
                        .install_initial(prepared, text_buffers, now_ms, cx);
                })
                .is_err()
            {
                return;
            }
            if this
                .update(cx, |this, _| {
                    this.initial_load_ready = true;
                })
                .is_err()
            {
                return;
            }
            // Whoever opened this agent is told the transcript is ready;
            // the screen does not know who that is.
            let _ = this.update(cx, |_, cx| cx.emit(AgentModelEvent::Loaded));
        }));
    }

    /// Builds an editor over the shared multibuffer — own cursor,
    /// scroll, and folds — fully caught up with the model.
    pub fn build_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let multi_buffer = self.document_multi_buffer.clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.disable_bracket_colorization(cx);
            editor.set_read_only(true);
            editor.set_autoscroll_pin(multi_buffer::Anchor::Max, AutoscrollStrategy::Bottom, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections
                    .select_anchor_ranges([multi_buffer::Anchor::Max..multi_buffer::Anchor::Max])
            });
            editor
        });

        self.transcript.attach(&editor, now_ms(), cx);
        // History is composed as the reader moves into it, and the point
        // is remembered where the store can name it, so a surface opened
        // again comes back to the block it was left on.
        self._subscriptions.push(cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &editor::EditorEvent, window, cx| match event {
                editor::EditorEvent::ScrollPositionChanged { .. } => {
                    this.editor_scrolled(&editor.clone(), window, cx);
                }
                editor::EditorEvent::SelectionsChanged { local: true } => {
                    this.remembered_point = this.store_point(&editor.clone(), cx);
                }
                _ => {}
            },
        ));
        self.style_brief(&editor, cx);
        self.editors.push(editor.downgrade());
        if let Some(point) = self.remembered_point {
            self.go_to_store_point(&editor, point, window, cx);
        }
        editor
    }

    /// Shows `brief`, the idea the agent works from, whole at the top of
    /// the document: it is where the work starts, so it reads before the
    /// first step.
    pub(crate) fn set_brief(&mut self, brief: Option<&str>, cx: &mut Context<Self>) {
        let text = brief.map(|brief| format!("{BRIEF_LABEL}\n{}\n", brief.trim_end()));
        if self.brief.as_ref().map(|buffer| buffer.read(cx).text()) == text {
            return;
        }
        let buffer = text.map(|text| cx.new(|cx| Buffer::local(text, cx)));
        self.document_multi_buffer
            .update(cx, |document, cx| match &buffer {
                Some(buffer) => {
                    let end = buffer.read(cx).max_point();
                    document.set_excerpts_for_path(
                        PathKey::min(),
                        buffer.clone(),
                        [Point::zero()..end],
                        0,
                        cx,
                    );
                }
                None => document.remove_excerpts_for_paths(vec![PathKey::min()], cx),
            });
        self.brief = buffer;
        for editor in self.live_editors() {
            self.style_brief(&editor, cx);
        }
    }

    /// The brief's label reads quieter than the idea under it.
    fn style_brief(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let label = self.brief.as_ref().and_then(|buffer| {
            let buffer = buffer.read(cx);
            let range = buffer.anchor_before(Point::zero())
                ..buffer.anchor_after(Point::new(0, BRIEF_LABEL.len() as u32));
            let snapshot = self.document_multi_buffer.read(cx).snapshot(cx);
            rho_window::highlights::excerpt_range(&snapshot, &range)
        });
        let class = StyleClass::SystemInfo;
        let style = class.resolve(cx);
        editor.update(cx, |editor, cx| {
            editor.highlight_text(
                class.highlight_key(Region::System),
                label.into_iter().collect(),
                style,
                cx,
            );
        });
    }

    /// How many display rows the brief takes above the transcript.
    fn brief_rows(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> f64 {
        let Some(buffer) = &self.brief else {
            return 0.0;
        };
        let rows = buffer.read(cx).max_point().row;
        editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            Point::new(rows + 1, 0).to_display_point(&display).row().0 as f64
        })
    }

    pub(crate) fn unresolved_mail(&self) -> impl Iterator<Item = (usize, Label)> + '_ {
        self.transcript.unresolved_mail()
    }

    pub(crate) fn current_errors(&self) -> u32 {
        self.transcript.current_errors()
    }

    pub(crate) fn newest_step(&self) -> Option<StorePoint> {
        self.transcript.newest_step()
    }

    pub(crate) fn errors(
        &self,
    ) -> impl DoubleEndedIterator<Item = (StorePoint, u32, Option<u64>)> + '_ {
        self.transcript.error_points()
    }

    pub(crate) fn mail_point(&self, mail: &rho_agent_types::Message) -> Option<StorePoint> {
        self.transcript.mail_point(mail)
    }

    /// How much of the transcript is composed nowhere yet, in blocks.
    pub fn uncomposed_blocks(&self) -> usize {
        self.transcript.uncomposed_blocks()
    }

    /// How much of the transcript is composed at the top, in blocks: what a
    /// reader who asked for the top is reading while the gap is still open.
    pub fn head_blocks(&self) -> usize {
        self.transcript.head_blocks()
    }

    /// The blocks the editor shows for visualization refs, by block id:
    /// per-attachment editor state the buffer text does not show.
    pub fn visualization_blocks(&self) -> Vec<editor::display_map::CustomBlockId> {
        self.transcript.visualization_blocks()
    }

    /// The block the gap marker sits above, if the reader is being told
    /// that a middle is still on its way.
    pub fn gap_marker_block(&self) -> Option<usize> {
        self.transcript.gap_marker_block()
    }

    /// Asks for more of the transcript's history. Composition runs off the
    /// frame loop, a chunk at a time, so the window keeps drawing while it
    /// catches up and the point lands when what holds it exists.
    pub fn request_history(
        &mut self,
        want: HistoryWant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history_want = Some(match self.history_want {
            Some(current) => current.merge(want),
            None => want,
        });
        if self.composing {
            return;
        }
        self.composing = true;
        // The first chunk is composed in the frame that asked for it. A
        // jump that draws what was composed before it and replaces it on
        // the next frame is a flicker, and one chunk is bounded work. The
        // rest of history still catches up off the frame loop.
        if !self.compose_step(window, cx) {
            return;
        }
        self.history_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(more) = this.update_in(cx, |this, window, cx| this.compose_step(window, cx))
                else {
                    return;
                };
                if !more {
                    return;
                }
                // Off the frame loop: a step, then back to the window,
                // which draws while the rest of history catches up.
                cx.background_executor().spawn(async {}).await;
            }
        }));
    }

    /// Which end of the gap the next step closes.
    ///
    /// What the reader asked for while they are still waiting for it — the
    /// end nearest the block they named, which for the top of the
    /// transcript is the top — and where they are reading once they are
    /// not. A reader who has reached the gap is served their own edge, not
    /// the one the last step happened to be closing.
    fn fill_edge(&mut self, want: Option<HistoryWant>, cx: &mut Context<Self>) -> FillEdge {
        let head = self.transcript.head_blocks();
        if let Some(block) = want.and_then(HistoryWant::block)
            && !self.transcript.is_composed(block)
        {
            let from_head = block.saturating_sub(head);
            let from_tail = self
                .transcript
                .uncomposed_blocks()
                .saturating_sub(from_head);
            return if from_head <= from_tail {
                FillEdge::Head
            } else {
                FillEdge::Tail
            };
        }
        if head == 0 {
            return FillEdge::Tail;
        }
        let reading = self
            .live_editors()
            .first()
            .and_then(|editor| {
                self.transcript
                    .store_point(&editor.read(cx).selections.newest_anchor().head(), cx)
            })
            .map(|point| point.block);
        match reading {
            // The point is in the prompt or below the head: the reader is
            // at the bottom, and the gap closes towards them.
            Some(block) if block >= head => FillEdge::Tail,
            None => FillEdge::Tail,
            Some(_) => FillEdge::Head,
        }
    }

    /// One composition step. Answers whether another is wanted.
    fn compose_step(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let want = self.history_want;
        let more = match self.fill_edge(want, cx) {
            FillEdge::Head => self
                .transcript
                .compose_head(HISTORY_CHUNK_ROWS, now_ms(), cx),
            FillEdge::Tail => self
                .transcript
                .compose_history(HISTORY_CHUNK_ROWS, now_ms(), cx),
        };
        if let Some(want) = want {
            if more && !want.met_by(&self.transcript) {
                return true;
            }
            self.history_want = None;
            if let Some((editor, point)) = self.pending_point.take()
                && let Some(editor) = editor.upgrade()
            {
                self.place_point(&editor, point, window, cx);
            }
            cx.emit(AgentModelEvent::HistoryComposed);
        }
        // A gap between the head and the tail is history that is on its way
        // — the reader is told so in the buffer — and it closes whether or
        // not anything is still waiting for it. History that was never
        // asked for is not a gap: it stays uncomposed until it is.
        if more && self.transcript.head_blocks() > 0 {
            return true;
        }
        self.composing = false;
        false
    }

    /// Puts the point at a store position, composing whatever history is
    /// needed to place it. The point lands when the block it names exists.
    pub fn go_to_store_point(
        &mut self,
        editor: &Entity<Editor>,
        point: StorePoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Before the bulk load there is nothing to compose towards: the
        // point waits for the load ([`Self::place_pending_point`]).
        if !self.initial_load_ready && self.initial_load_started {
            self.pending_point = Some((editor.downgrade(), point));
            return;
        }
        if self.place_point(editor, point, window, cx) {
            return;
        }
        self.pending_point = Some((editor.downgrade(), point));
        self.request_history(HistoryWant::ToBlock(point.block), window, cx);
    }

    /// Places a point asked for before the bulk load, once it is in.
    pub fn place_pending_point(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((editor, point)) = self.pending_point.take()
            && let Some(editor) = editor.upgrade()
        {
            self.go_to_store_point(&editor, point, window, cx);
        }
    }

    /// Where the point of an editor over this model is, as the store sees
    /// it. `None` when the point is in the prompt rather than the
    /// transcript.
    pub fn store_point(&self, editor: &Entity<Editor>, cx: &App) -> Option<StorePoint> {
        let head = editor.read(cx).selections.newest_anchor().head();
        self.transcript.store_point(&head, cx)
    }

    /// The point this model's editors last had in the transcript.
    pub fn remembered_point(&self) -> Option<StorePoint> {
        self.remembered_point
    }

    /// Places the point, answering whether the block it names was composed.
    fn place_point(
        &mut self,
        editor: &Entity<Editor>,
        point: StorePoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(anchor) = self.transcript.place_store_point(point, cx) else {
            return false;
        };
        let Some(anchor) = self
            .document_multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(anchor)
        else {
            return false;
        };
        editor.update(cx, |editor, cx| {
            // The point the reader asked for outranks the pin that keeps
            // a live transcript at its tail.
            editor.clear_autoscroll_pin(cx);
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
        true
    }

    /// Composes the next screenful of history when the reader comes close
    /// to the top of what is composed.
    fn editor_scrolled(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.transcript.has_uncomposed() || self.composing {
            return;
        }
        // Measured from where the transcript starts, below the brief.
        let top = editor.update(cx, |editor, cx| editor.scroll_position(cx).y)
            - self.brief_rows(editor, cx);
        if top > PRELOAD_ROWS {
            return;
        }
        self.request_history(HistoryWant::Screenful, window, cx);
    }

    /// The editors still alive, pruning dropped ones.
    fn live_editors(&mut self) -> Vec<Entity<Editor>> {
        self.editors.retain(|editor| editor.upgrade().is_some());
        self.editors
            .iter()
            .filter_map(|editor| editor.upgrade())
            .collect()
    }

    /// Shows what moved in the agent's state. Until the bulk load is
    /// ready it is held, and the first sync after `Loaded` (with
    /// [`FrameSummary::nothing`] if nothing else moved) shows it.
    pub(crate) fn sync(
        &mut self,
        state: &UiAgentState,
        summary: FrameSummary,
        now_ms: u64,
        labels: &impl Fn(HashSet<Label>, &gpui::App) -> Labels,
        cx: &mut Context<Self>,
    ) {
        let summary = self
            .pending_sync
            .take()
            .map_or(summary, |pending| pending.merge(summary));
        if !self.initial_load_ready {
            self.pending_sync = Some(summary);
            return;
        }
        let rewrite = summary.first_changed_block == Some(0);
        let points = if summary.first_changed_block.is_some() {
            self.live_editors()
                .into_iter()
                .filter_map(|editor| self.store_point(&editor, cx).map(|point| (editor, point)))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        // A rewind can remove blocks and shift the indices of delivered mail.
        // A surviving block keeps its Arc, even when the rebuilt text buffer
        // no longer owns the editor's old anchor.
        let remapped = if rewrite && !points.is_empty() {
            let survivors = state
                .blocks
                .iter()
                .enumerate()
                .map(|(index, block)| (Arc::as_ptr(block), index))
                .collect::<HashMap<_, _>>();
            points
                .iter()
                .map(|(_, point)| {
                    remap_point(*point, &survivors, |at| self.transcript.block_at(at))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        self.transcript.sync(state, summary, now_ms, labels, cx);
        let changed = points
            .into_iter()
            .enumerate()
            .filter_map(|(index, (editor, old))| {
                let point = remapped.get(index).copied().unwrap_or(Some(old))?;
                (self.store_point(&editor, cx) != Some(point))
                    .then_some((editor.entity_id(), point))
            })
            .collect::<Vec<_>>();
        if !changed.is_empty() {
            cx.emit(AgentModelEvent::RestorePoints(changed));
        }
        cx.notify();
    }

    pub fn tick_timers(&mut self, now_ms: u64, cx: &mut Context<Self>) {
        self.transcript.tick_timers(now_ms, cx);
    }

    pub fn has_timers(&self) -> bool {
        self.transcript.has_timers()
    }
}

/// If the step went away, keep the point on the nearest surviving step before
/// it.
fn remap_point<'a>(
    point: StorePoint,
    survivors: &HashMap<*const UiBlock, usize>,
    old: impl Fn(usize) -> Option<&'a Arc<UiBlock>>,
) -> Option<StorePoint> {
    (0..=point.block).rev().find_map(|block| {
        let old = old(block)?;
        if !crate::render::block_visible(old) {
            return None;
        }
        let index = *survivors.get(&Arc::as_ptr(old))?;
        Some(StorePoint {
            block: index,
            offset: if block == point.block {
                point.offset
            } else {
                0
            },
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewind_preserves_the_surviving_step_not_the_old_index() {
        let row = |text: &str| Arc::new(UiBlock::UserMessage { text: text.into() });
        let old = [
            row("origin"),
            row("removed"),
            row("delivered"),
            row("removed"),
        ];
        let survivors = HashMap::from([(Arc::as_ptr(&old[0]), 0), (Arc::as_ptr(&old[2]), 1)]);
        let remap = |block| {
            remap_point(StorePoint { block, offset: 3 }, &survivors, |at| {
                old.get(at)
            })
        };
        assert_eq!(
            remap(2),
            Some(StorePoint {
                block: 1,
                offset: 3
            })
        );
        assert_eq!(
            remap(3),
            Some(StorePoint {
                block: 1,
                offset: 0
            })
        );
    }
}
