//! Hides markdown markup in editors as zero-width inline folds.
//!
//! The buffer keeps the markdown source; only the display map skips the
//! markup, so selections, copy and search still see `**bold**`. Folds live in
//! the editor's fold map, so which ones an editor carries is per-attachment
//! state, diffed against the model's anchor list — the same split the
//! transcript uses for inlays and display elisions.
//!
//! Everything here is bounded by the changed tail. A transcript accumulates
//! thousands of concealed ranges, and a streaming delta must not pay for the
//! ones it did not touch: records before the changed block keep their
//! anchors, so the model reports how many ranges that leaves untouched and
//! attachments resolve and diff only what follows.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::ops::Range;

use editor::Editor;
use editor::display_map::{Crease, FoldPlaceholder};
use gpui::{Context, Entity};
use multi_buffer::{MultiBuffer, MultiBufferOffset, ToOffset as _};
use text::Anchor;

use crate::highlights::excerpt_range;

/// Fold type tag: a refresh removes exactly the transcript's concealment
/// folds and leaves any other fold in the editor alone.
struct MarkdownConcealment;

/// The concealment folds one editor currently carries, by the index of the
/// model range each covers. A range outside this editor's excerpt has no
/// entry, and neither does one the backfill has not reached.
///
/// Folds are compared by the position they cover, not by anchor identity: a
/// re-render of the streaming block mints fresh anchors for markup that has
/// not moved, and refolding those every delta is the one cost this whole
/// module has to avoid.
pub struct ConcealState {
    applied: BTreeMap<usize, Range<MultiBufferOffset>>,
    /// The lowest model range this editor has concealed down to: everything
    /// from here to the end of the transcript is folded, everything before it
    /// is still to come. A fresh editor has concealed nothing, which the
    /// first pass reads as "start at the end" once it knows how many ranges
    /// there are.
    concealed_from: usize,
}

impl Default for ConcealState {
    fn default() -> Self {
        Self {
            applied: BTreeMap::new(),
            concealed_from: usize::MAX,
        }
    }
}

impl ConcealState {
    /// Whether history is still waiting to be concealed.
    pub fn backfilling(&self) -> bool {
        self.concealed_from > 0
    }
}

/// Concealment folds applied in one pass. A transcript carries thousands of
/// them and each costs the display map a fold of its own, so a first view
/// conceals what it opens on - the tail - and the history behind it follows
/// over the next few frames rather than in the frame the user is waiting on.
///
/// Sized against the pause between passes: a pass of this many costs about
/// two milliseconds, which leaves the frames it lands in room for the work
/// they were already doing.
const CONCEAL_BUDGET: usize = 256;

/// The model's concealed ranges: every record's markup, in buffer order.
#[derive(Default)]
pub struct ConcealSync {
    ranges: Vec<Range<Anchor>>,
    /// Leading ranges that the last refresh left untouched.
    unchanged: usize,
}

impl ConcealSync {
    /// Replaces the ranges from the changed record onward. `unchanged` counts
    /// the ranges of the records before it, whose anchors still stand.
    pub fn refresh<'a>(&mut self, unchanged: usize, tail: impl Iterator<Item = &'a Range<Anchor>>) {
        self.ranges.truncate(unchanged);
        self.ranges.extend(tail.cloned());
        self.unchanged = unchanged;
    }

    /// Reconciles one editor's concealment folds with the model's ranges.
    ///
    /// The concealed region runs from [`ConcealState::concealed_from`] to the
    /// end of the transcript; each pass reconciles it and extends it by a
    /// budget's worth of older ranges. Inside the region only the changed
    /// tail is resolved and diffed, and within that a common prefix stays
    /// untouched. Stale folds go by removing everything of this type from the
    /// divergence point onward, which also collects folds whose text the
    /// rewrite deleted. `refresh` redoes the editor wholesale, for an
    /// attachment whose excerpt was replaced under it.
    pub fn apply<V: 'static>(
        &self,
        state: &mut ConcealState,
        refresh: bool,
        backfill: bool,
        multi_buffer: &Entity<MultiBuffer>,
        editor: &Entity<Editor>,
        cx: &mut Context<V>,
    ) {
        // An editor that has concealed nothing yet hides its tail in this
        // pass: it is about to be shown, and the tail is what it opens on.
        let opening = refresh || state.concealed_from == usize::MAX;
        if refresh {
            state.applied.clear();
            state.concealed_from = self.ranges.len();
        }
        // A range the model dropped takes its fold with it, and one whose
        // anchors were re-minted is checked below.
        state.applied.split_off(&self.ranges.len());
        state.concealed_from = state.concealed_from.min(self.ranges.len());

        // The tail the last refresh re-rendered, plus - when this pass is a
        // backfill - a budget of the history behind it. A sync leaves the
        // history to the backfill: the frame that carries a delta has its own
        // work to do.
        let checked = self.unchanged.max(state.concealed_from);
        let backfill = if backfill || opening {
            state.concealed_from.saturating_sub(CONCEAL_BUDGET)
        } else {
            state.concealed_from
        };
        if !refresh && backfill == state.concealed_from && checked >= self.ranges.len() {
            return;
        }

        let snapshot = multi_buffer.read(cx).snapshot(cx);
        let resolve = |range: &Range<Anchor>| {
            let range = excerpt_range(&snapshot, range)?;
            Some(range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot))
        };

        // Ranges whose fold no longer covers what the model says it should:
        // everything from the first of them is refolded.
        let mut diverged = None;
        for index in checked..self.ranges.len() {
            let desired = resolve(&self.ranges[index]);
            if desired.as_ref() != state.applied.get(&index) {
                diverged = Some(index);
                break;
            }
        }
        let stale = diverged.map(|index| state.applied.split_off(&index));
        let first_stale = stale
            .as_ref()
            .and_then(|stale| stale.values().next())
            .map(|range| range.start);

        let mut folds = Vec::new();
        let mut fold = |index: usize, state: &mut ConcealState| {
            if let Some(range) = resolve(&self.ranges[index]) {
                folds.push(range.clone());
                state.applied.insert(index, range);
            }
        };
        for index in backfill..state.concealed_from {
            fold(index, state);
        }
        for index in diverged.unwrap_or(self.ranges.len())..self.ranges.len() {
            fold(index, state);
        }
        state.concealed_from = backfill;

        // One range from the divergence point to the end of the buffer: fold
        // ranges the rewrite emptied no longer intersect their own anchors,
        // but they do intersect this one.
        let removed = (refresh || first_stale.is_some()).then(|| {
            let start = first_stale
                .filter(|_| !refresh)
                .unwrap_or(MultiBufferOffset(0));
            start..snapshot.len()
        });
        if removed.is_none() && folds.is_empty() {
            return;
        }
        let creases = folds
            .into_iter()
            .map(|range| {
                Crease::simple(
                    range,
                    FoldPlaceholder::concealed(TypeId::of::<MarkdownConcealment>()),
                )
            })
            .collect::<Vec<_>>();

        let display_map = editor.read(cx).display_map.clone();
        display_map.update(cx, |display_map, cx| {
            if let Some(removed) = removed {
                display_map.remove_folds_with_type(
                    [removed],
                    TypeId::of::<MarkdownConcealment>(),
                    cx,
                );
            }
            if !creases.is_empty() {
                display_map.fold(creases, cx);
            }
        });
    }
}
