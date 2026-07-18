//! Applies elision plans to editors as display elisions.
//!
//! Plans and their anchor-resolved specs are model state, cached per turn:
//! a refresh recomputes only from the changed turn onward. Which display
//! elisions an editor actually carries is per-attachment [`ElisionState`],
//! diffed positionally against the model's specs — elision ids live in the
//! editor's id space, and fold open/closed state stays per-editor (vim's
//! per-window folds, not emacs' buffer-level invisibility).

use std::ops::Range;
use std::sync::Arc;

use editor::display_map::{BlockContext, BlockStyle};
use editor::{DisplayElisionId, DisplayElisionProperties, Editor};
use gpui::prelude::*;
use gpui::{Context, Entity};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot};
use rho_ui_proto::remote::UiBlock;
use text::Anchor;
use ui::{Icon, IconName, IconSize, div};

use crate::highlights::excerpt_range;
use crate::render::elision::{ElisionPlan, elision_label, elision_plans_from, turn_start_index};

/// What one fold looks like, independent of its editor identity.
#[derive(Clone, PartialEq)]
struct ElisionSpec {
    range: Range<Anchor>,
    tool_count: usize,
    tail_rows: u32,
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

#[derive(Default)]
pub struct ElisionSync {
    plans: Vec<ElisionPlan>,
    specs: Vec<ElisionSpec>,
}

impl ElisionSync {
    /// Recomputes plans and their anchor-resolved specs from the changed
    /// turn onward. `plan_range` resolves a plan to its buffer anchor range.
    pub fn refresh(
        &mut self,
        blocks: &[UiBlock],
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
        blocks: &[UiBlock],
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

    /// Reconciles one editor's display elisions with the model's specs.
    pub fn apply<V: 'static>(
        &self,
        state: &mut ElisionState,
        multi_buffer: &Entity<MultiBuffer>,
        editor: &Entity<Editor>,
        cx: &mut Context<V>,
    ) {
        let specs = self.specs.clone();
        let snapshot = multi_buffer.read(cx).snapshot(cx);
        let mut removed_ids = state.active[specs.len().min(state.active.len())..]
            .iter()
            .map(|elision| elision.id)
            .collect::<rustc_hash::FxHashSet<_>>();
        let mut updates = Vec::new();
        let mut inserted_specs = Vec::new();
        let mut inserted_properties = Vec::new();
        let mut next_active = Vec::new();

        for (index, spec) in specs.into_iter().enumerate() {
            let existing = state.active.get(index);
            if let Some(existing) = existing
                && existing.spec == spec
            {
                next_active.push(ActiveElision {
                    id: existing.id,
                    spec,
                });
                continue;
            }
            let Some(properties) = elision_properties(&snapshot, &spec) else {
                if let Some(existing) = existing {
                    removed_ids.insert(existing.id);
                }
                continue;
            };
            match existing {
                Some(existing) => {
                    updates.push((existing.id, properties));
                    next_active.push(ActiveElision {
                        id: existing.id,
                        spec,
                    });
                }
                None => {
                    inserted_properties.push(properties);
                    inserted_specs.push(spec);
                }
            }
        }

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
