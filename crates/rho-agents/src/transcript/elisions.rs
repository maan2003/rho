//! Applies elision plans to editors as folds.
//!
//! Plans and their anchor-resolved specs are model state, cached per turn:
//! a refresh recomputes only from the changed turn onward. Which folds an
//! editor actually carries is per-attachment [`ElisionState`], diffed
//! against the model's specs — fold open/closed state stays per-editor
//! (vim's per-window folds, not emacs' buffer-level invisibility).
//!
//! Folds rather than display elisions, which is the whole point of this
//! module's second life: a display elision is a block-map construct and the
//! block map sits *above* the wrap map, so an elided turn was a turn that
//! had already been wrapped and then hidden. A fold is below the wrap, so
//! an elided turn leaves the wrap's input — and the block map's — entirely.
//! The tail an elision keeps visible is the fold map's own
//! [`ElisionPolicy::Tail`].

use std::ops::Range;
use std::sync::Arc;

use editor::Editor;
use editor::display_map::{CaretRest, Crease, ElisionPolicy};
use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Entity};
use multi_buffer::MultiBuffer;
use rho_window::highlights::excerpt_range;
use text::Anchor;
use ui::{Icon, IconName, IconSize, div};

use crate::render::elision::{ElisionPlan, elision_label, elision_plans_from, turn_start_index};
use crate::state::UiBlock;

/// What one fold looks like, independent of its editor identity.
#[derive(Clone, PartialEq)]
struct ElisionSpec {
    range: Range<Anchor>,
    tool_count: usize,
    tail_rows: u32,
}

/// One editor's live folds, reconciled against the model's specs: what the
/// editor was last told to fold, and the crease id each fold left behind. A
/// crease is what lets a reader close an elision again after opening it —
/// the fold is gone once it is open, and the crease is what `z c` finds.
#[derive(Default)]
pub struct ElisionState {
    active: Vec<(ElisionSpec, editor::display_map::CreaseId)>,
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

    /// Reconciles one editor's folds with the model's specs. The two lists
    /// are in document order and a change touches a run in the middle of
    /// them — composing history adds a prefix, a new turn adds a suffix, an
    /// edit rewrites one turn — so the common prefix and suffix are skipped
    /// and only the run between them reaches the editor.
    pub fn apply<V: 'static>(
        &self,
        state: &mut ElisionState,
        multi_buffer: &Entity<MultiBuffer>,
        editor: &Entity<Editor>,
        cx: &mut Context<V>,
    ) {
        let common = state
            .active
            .iter()
            .zip(&self.specs)
            .take_while(|((active, _), spec)| active == *spec)
            .count();
        let tail = state.active[common..]
            .iter()
            .rev()
            .zip(self.specs[common..].iter().rev())
            .take_while(|((active, _), spec)| active == *spec)
            .count();
        let stale = &state.active[common..state.active.len() - tail];
        let fresh = &self.specs[common..self.specs.len() - tail];
        if stale.is_empty() && fresh.is_empty() {
            return;
        }

        let snapshot = multi_buffer.read(cx).snapshot(cx);
        let unfold = stale
            .iter()
            .filter_map(|(spec, _)| excerpt_range(&snapshot, &spec.range))
            .collect::<Vec<_>>();
        let uncrease = stale.iter().map(|(_, id)| *id).collect::<Vec<_>>();
        let fold = fresh
            .iter()
            .filter_map(|spec| {
                let range = excerpt_range(&snapshot, &spec.range)?;
                let label = elision_label(spec.tool_count);
                Some(
                    Crease::simple(
                        range,
                        editor::FoldPlaceholder {
                            render: Arc::new(move |_, _, cx| render_elision(&label, cx)),
                            constrain_width: false,
                            merge_adjacent: false,
                            type_tag: Some(std::any::TypeId::of::<HistoryFold>()),
                            collapsed_text: None,
                            caret_rest: CaretRest::Boundary,
                        },
                    )
                    .with_elision_policy(ElisionPolicy::Tail {
                        rows: spec.tail_rows,
                    }),
                )
            })
            .collect::<Vec<_>>();

        let fresh_ids = editor.update(cx, |editor, cx| {
            if !uncrease.is_empty() {
                editor.remove_creases(uncrease, cx);
            }
            let ids = editor.insert_creases(fold.clone(), cx);
            editor.display_map.update(cx, |display_map, cx| {
                if !unfold.is_empty() {
                    display_map.remove_folds_with_type(
                        unfold,
                        std::any::TypeId::of::<HistoryFold>(),
                        cx,
                    );
                }
                if !fold.is_empty() {
                    display_map.fold(fold, cx);
                }
            });
            cx.notify();
            ids
        });

        // The specs that resolved to a range are the ones that became folds,
        // in order, so the ids line up with them; a spec whose anchors no
        // longer resolve is carried with no crease of its own and is
        // reconciled again the next time its turn changes.
        let mut fresh_ids = fresh_ids.into_iter();
        let mut active = state.active[..common].to_vec();
        active.extend(fresh.iter().filter_map(|spec| {
            let id = fresh_ids.next()?;
            Some((spec.clone(), id))
        }));
        active.extend_from_slice(&state.active[state.active.len() - tail..]);
        state.active = active;
    }
}

/// The tag that tells this module's folds from every other fold in the
/// buffer — the concealment folds a markdown line carries, above all, which
/// live inside the ranges these cover and must survive them. Public because
/// a reader of the editor (a test, a screen that counts what is elided)
/// needs the same tag to find them.
pub enum HistoryFold {}

/// The row a folded turn leaves behind: the same chevron and count the
/// reader saw when this was a block, drawn now as the fold's placeholder.
fn render_elision(label: &str, cx: &mut App) -> AnyElement {
    let text_color = rho_window::style::hint_color(cx);
    div()
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
        .into_any_element()
}
