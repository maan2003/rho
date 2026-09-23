//! Applies elision plans to editors as display elisions.
//!
//! Plans and their anchor-resolved specs are model state, cached per turn:
//! a refresh recomputes only from the changed turn onward. Which display
//! elisions an editor actually carries is per-attachment [`ElisionState`],
//! diffed positionally against the model's specs — elision ids live in the
//! editor's id space, and open/closed state stays per-editor (vim's
//! per-window folds, not emacs' buffer-level invisibility).

use std::ops::Range;
use std::sync::Arc;

use editor::display_map::{BlockContext, BlockStyle};
use editor::{DisplayElisionId, DisplayElisionProperties, Editor};
use gpui::prelude::*;
use gpui::{App, Context, Entity};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot};
use rho_window::highlights::excerpt_range;
use settings::Settings as _;
use text::Anchor;
use theme_settings::ThemeSettings;
use ui::{Icon, IconName, IconSize, div};

use crate::render::elision::{ElisionPlan, elision_label, elision_plans_from, turn_start_index};
use crate::state::UiBlock;

/// What one elision looks like, independent of its editor identity.
#[derive(Clone, Debug, PartialEq)]
pub struct ElisionSpec {
    /// The block the elided run starts at, which is the turn it belongs to
    /// and the only part of a spec that survives the document growing
    /// around it. Plans never overlap, so no two specs share one.
    pub start_block: usize,
    pub range: Range<Anchor>,
    pub tool_count: usize,
    pub tail_rows: u32,
}

struct ActiveElision {
    id: DisplayElisionId,
    spec: ElisionSpec,
}

/// One editor's live display elisions, reconciled against the model's specs.
#[derive(Default)]
pub struct ElisionState {
    active: Vec<ActiveElision>,
}

impl ElisionState {
    /// The specs this editor is carrying elisions for, in document order.
    /// Public because that list is the thing a guard has to read: whether
    /// it names the turns that actually became elisions is the whole of
    /// the pairing below.
    pub fn active_specs(&self) -> impl Iterator<Item = &ElisionSpec> {
        self.active.iter().map(|elision| &elision.spec)
    }
}

#[derive(Default)]
pub struct ElisionSync {
    plans: Vec<ElisionPlan>,
    specs: Vec<ElisionSpec>,
}

impl ElisionSync {
    /// Sets the specs directly, bypassing the plans they normally come
    /// from. Public for guards, which need a spec whose anchors will not
    /// resolve and cannot get one from a plan.
    pub fn set_specs(&mut self, specs: Vec<ElisionSpec>) {
        self.specs = specs;
    }

    /// Recomputes plans and their anchor-resolved specs from the changed
    /// turn onward. `plan_range` resolves a plan to its buffer anchor range.
    pub fn refresh(
        &mut self,
        blocks: &[Arc<UiBlock>],
        first_changed_block: usize,
        visible: &[bool],
        turn_in_progress: bool,
        plan_range: impl Fn(&ElisionPlan) -> Option<Range<Anchor>>,
    ) {
        self.rebuild_plans(blocks, first_changed_block, visible, turn_in_progress);
        self.specs = self
            .plans
            .iter()
            .filter_map(|plan| {
                Some(ElisionSpec {
                    start_block: plan.start_block,
                    range: plan_range(plan)?,
                    tool_count: plan.tool_count,
                    tail_rows: plan.tail_rows,
                })
            })
            .collect();
    }

    /// Plans depend only on their own turn's blocks, so plans for turns
    /// before the change never move; recompute only from the changed turn
    /// onward.
    fn rebuild_plans(
        &mut self,
        blocks: &[Arc<UiBlock>],
        first_changed_block: usize,
        visible: &[bool],
        turn_in_progress: bool,
    ) {
        let mut from_block = turn_start_index(blocks, first_changed_block);
        // A cached plan straddles a turn boundary only when a user message
        // rendered invisible; recompute from the straddling plan's start.
        loop {
            from_block = turn_start_index(blocks, from_block);
            let straddle = self
                .plans
                .iter()
                .find(|plan| plan.start_block < from_block && plan.end_block >= from_block)
                .map(|plan| plan.start_block);
            match straddle {
                Some(start) if start < from_block => from_block = start,
                _ => break,
            }
        }

        let kept = self
            .plans
            .iter()
            .take_while(|plan| plan.end_block < from_block)
            .count();
        self.plans.truncate(kept);
        let last_visible = visible[..from_block.min(visible.len())]
            .iter()
            .rposition(|&visible| visible);
        let carry = match (self.plans.last(), last_visible) {
            (Some(plan), Some(index)) if plan.end_block == index => self.plans.pop(),
            _ => None,
        };
        self.plans.extend(elision_plans_from(
            blocks,
            visible,
            from_block,
            carry,
            turn_in_progress,
        ));
    }

    /// Reconciles one editor's display elisions with the model's specs,
    /// paired by the turn each one elides, so a spec that changed shape
    /// updates the elision already there rather than removing and
    /// re-inserting it.
    ///
    /// The pairing used to be by position, and position does not survive
    /// history arriving. A spec exists only while its anchors resolve, so
    /// composing a page at the head makes several earlier plans resolve at
    /// once and every spec after them shifts down the list. Positionally
    /// that reads as every elision having changed shape, and one page
    /// updated all sixty-one of them, which tells the block map most of
    /// the document moved when nothing did.
    pub fn apply<V: 'static>(
        &self,
        state: &mut ElisionState,
        multi_buffer: &Entity<MultiBuffer>,
        editor: &Entity<Editor>,
        cx: &mut Context<V>,
    ) {
        let specs = self.specs.clone();
        let snapshot = multi_buffer.read(cx).snapshot(cx);
        let mut carried = state
            .active
            .drain(..)
            .map(|elision| (elision.spec.start_block, elision))
            .collect::<rustc_hash::FxHashMap<_, _>>();
        let mut removed_ids = rustc_hash::FxHashSet::default();
        let mut updates = Vec::new();
        let mut inserted_specs = Vec::new();
        let mut inserted_properties = Vec::new();
        let mut next_active = Vec::new();

        for spec in specs {
            // A spec whose anchors do not resolve in this snapshot is not
            // recorded at all: a transcript still composing its history has
            // excerpts that are not in the buffer yet, and leaving the spec
            // out is what makes the next reconcile try it again.
            match carried.remove(&spec.start_block) {
                Some(existing) if existing.spec == spec => next_active.push(existing),
                Some(existing) => match elision_properties(&snapshot, &spec) {
                    Some(properties) => {
                        updates.push((existing.id, properties));
                        next_active.push(ActiveElision {
                            id: existing.id,
                            spec,
                        });
                    }
                    None => {
                        removed_ids.insert(existing.id);
                    }
                },
                None => {
                    if let Some(properties) = elision_properties(&snapshot, &spec) {
                        inserted_properties.push(properties);
                        inserted_specs.push(spec);
                    }
                }
            }
        }

        removed_ids.extend(carried.into_values().map(|elision| elision.id));

        if removed_ids.is_empty() && updates.is_empty() && inserted_properties.is_empty() {
            state.active = next_active;
            return;
        }

        let inserted_ids = editor.update(cx, |editor, cx| {
            if !removed_ids.is_empty() {
                editor.remove_display_elisions(removed_ids, None, cx);
            }
            if !updates.is_empty() {
                editor.update_display_elisions(updates, None, cx);
            }
            editor.insert_display_elisions(inserted_properties, None, cx)
        });
        next_active.extend(
            inserted_ids
                .into_iter()
                .zip(inserted_specs)
                .map(|(id, spec)| ActiveElision { id, spec }),
        );
        state.active = next_active;
    }
}

fn elision_properties(
    snapshot: &MultiBufferSnapshot,
    spec: &ElisionSpec,
) -> Option<DisplayElisionProperties<multi_buffer::Anchor>> {
    let range = excerpt_range(snapshot, &spec.range)?;
    let label = elision_label(spec.tool_count);
    Some(DisplayElisionProperties {
        range,
        tail_rows: spec.tail_rows,
        height: Some(1),
        style: BlockStyle::Flex,
        render: Arc::new(move |cx| render_elision_block(&label, cx).into_any_element()),
        priority: 0,
        type_tag: None,
    })
}

/// The row an elided turn leaves behind: a chevron and the count of what it
/// hides, on a row of its own.
fn render_elision_block(label: &str, cx: &mut BlockContext<'_, '_>) -> impl IntoElement {
    let cursor_color = cx.editor_style.local_player.cursor;
    let selected = cx.selected;
    let anchor_x = cx.anchor_x;
    let line_height = cx.line_height;
    div()
        .block_mouse_except_scroll()
        .pl(anchor_x)
        .h(line_height)
        .flex()
        .items_center()
        .child(
            elision_row(label, cx.app)
                .h(line_height)
                .when(selected, |this| this.bg(cursor_color.opacity(0.22))),
        )
}

/// The elision's row before it is placed, so a guard can read back the face
/// it asked for. Public for that reason only.
///
/// The row stands in for buffer text, so it draws in the buffer's face: a
/// bare `div` here comes out in the window's UI font, a proportional
/// caption in the middle of monospace rows.
pub fn elision_row(label: &str, cx: &mut App) -> ui::Div {
    let text_color = rho_window::style::hint_color(cx);
    let buffer_font = ThemeSettings::get_global(cx).buffer_font.clone();
    div()
        .font(buffer_font)
        .flex()
        .items_center()
        .gap_1()
        .pr_1()
        .text_color(text_color)
        .child(
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(text_color.into()),
        )
        .child(label.to_owned())
}
