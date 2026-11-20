//! One model per agent: its transcript, the buffer role. Editors are the
//! window role: each surface showing the agent builds its editor over the
//! shared multibuffer via [`AgentModel::build_editor`], with its own
//! cursor, scroll, and folds. The model reconciles every attached editor
//! when content or chrome changes, so the model persists for the session
//! while editors come and go with surfaces.

use std::collections::HashMap;

use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Subscription, Task, WeakEntity, Window};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use rho_agent_types::AgentId;
use rho_agents_client::now_ms;
use rho_agents_client::state::UiAgentState;
use rho_agents_client::store::FrameSummary;
use text::{Buffer as TextBuffer, BufferId, ReplicaId};

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
    pub fn start_initial_load(
        &mut self,
        state: UiAgentState,
        agent_labels: HashMap<AgentId, String>,
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
                .spawn(async move { TranscriptModel::prepare_initial(state, now_ms, agent_labels) })
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
        self.editors.push(editor.downgrade());
        if let Some(point) = self.remembered_point {
            self.go_to_store_point(&editor, point, window, cx);
        }
        editor
    }

    pub(crate) fn errors(
        &self,
    ) -> impl DoubleEndedIterator<Item = (StorePoint, u32, Option<u64>)> + '_ {
        self.transcript.error_points()
    }

    pub(crate) fn mail_point(&self, mail: &rho_agent_types::Message) -> Option<StorePoint> {
        self.transcript.mail_point(mail)
    }

    pub(crate) fn tool_output_ranges(
        &self,
        cx: &App,
    ) -> Vec<(String, bool, std::ops::Range<multi_buffer::Anchor>)> {
        self.transcript.tool_output_ranges(cx)
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
        let top = editor.update(cx, |editor, cx| editor.scroll_position(cx).y);
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
    pub fn sync(
        &mut self,
        state: &UiAgentState,
        summary: FrameSummary,
        now_ms: u64,
        agent_label: &impl Fn(rho_agent_types::AgentId) -> String,
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
        self.transcript
            .sync(state, summary, now_ms, agent_label, cx);
        cx.notify();
    }

    pub fn tick_timers(&mut self, now_ms: u64, cx: &mut Context<Self>) {
        self.transcript.tick_timers(now_ms, cx);
    }

    pub fn has_timers(&self) -> bool {
        self.transcript.has_timers()
    }
}
