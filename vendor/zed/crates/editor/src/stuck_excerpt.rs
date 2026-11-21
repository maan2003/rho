//! An edge projection of one excerpt of the editor's own MultiBuffer.
//! Layout uses real display rows; the document, anchors, search and
//! selections never include a second copy of the excerpt.

use std::ops::Range;

use gpui::{App, Context, Hsla, SharedString};
use multi_buffer::{Anchor, PathKey, ToPoint as _};

use crate::display_map::{DisplaySnapshot, ToDisplayPoint as _};
use crate::{DisplayRow, Editor, RowExt as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StuckEdge {
    Top,
    Bottom,
}

impl StuckEdge {
    pub(crate) fn is_outside(self, start: f64, end: f64, scroll: f64, viewport: f64) -> bool {
        match self {
            Self::Top => end <= scroll,
            Self::Bottom => start >= scroll + viewport,
        }
    }
}

pub(crate) struct StuckExcerpt {
    path: PathKey,
    pub(crate) edge: StuckEdge,
    pub(crate) background: fn(&App) -> Hsla,
    /// Offset within the excerpt, in vertical display units.
    offset: f64,
    followed_head: Option<Anchor>,
    pub(crate) label: SharedString,
    pub(crate) pending: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct StuckExcerptRows {
    pub(crate) rows: Range<DisplayRow>,
    pub(crate) top_y: f64,
    pub(crate) height: f64,
    pub(crate) max_offset: f64,
}

impl Editor {
    /// Project one single-excerpt path onto this edge when none of its
    /// display rows is visible in the ordinary viewport.
    /// The path remains designated when its excerpt is replaced.
    pub fn set_stuck_excerpt(
        &mut self,
        path: PathKey,
        edge: StuckEdge,
        pending_label: SharedString,
        background: fn(&App) -> Hsla,
        cx: &mut Context<Self>,
    ) {
        self.stuck_excerpt = Some(StuckExcerpt {
            path,
            edge,
            background,
            offset: 0.0,
            followed_head: None,
            label: pending_label,
            pending: false,
        });
        cx.notify();
    }

    /// Have a short end-aligned label in the stuck excerpt until the
    /// excerpt comes back inline. The label is outside the document.
    pub fn mark_stuck_excerpt(&mut self, cx: &mut Context<Self>) {
        if let Some(excerpt) = &mut self.stuck_excerpt {
            excerpt.pending = true;
            cx.notify();
        }
    }

    /// Whether this editor needs its lower edge kept above surrounding UI.
    pub fn keeps_bottom_visible(&self) -> bool {
        self.stuck_excerpt
            .as_ref()
            .is_some_and(|state| state.edge == StuckEdge::Bottom)
    }

    pub fn stuck_excerpt_is_visible(&self) -> bool {
        self.last_position_map
            .as_ref()
            .is_some_and(|map| map.stuck_excerpt.is_some())
    }

    /// Preserve a point in the excerpt on viewport (rather than point)
    /// motions, even when the excerpt is currently inline.
    pub fn point_in_stuck_excerpt(&self, cx: &App) -> bool {
        self.stuck_excerpt.as_ref().is_some_and(|excerpt| {
            let buffer = self.buffer.read(cx).snapshot(cx);
            let Some(range) = buffer.single_excerpt_range(&excerpt.path) else {
                return false;
            };
            let point = self.selections.newest_anchor().head().to_point(&buffer);
            (range.start.to_point(&buffer)..=range.end.to_point(&buffer)).contains(&point)
        })
    }

    pub(crate) fn stuck_excerpt_range(
        &self,
        snapshot: &DisplaySnapshot,
    ) -> Option<Range<DisplayRow>> {
        let state = self.stuck_excerpt.as_ref()?;
        let range = snapshot
            .buffer_snapshot()
            .single_excerpt_range(&state.path)?;
        Some(
            range.start.to_display_point(snapshot).row()
                ..range.end.to_display_point(snapshot).row().next_row(),
        )
    }

    pub(crate) fn scroll_stuck_excerpt(&mut self, delta: f64, max: f64, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.stuck_excerpt {
            state.offset = (state.offset + delta).clamp(0.0, max);
            cx.notify();
        }
    }

    pub(crate) fn layout_stuck_excerpt_rows(
        &mut self,
        snapshot: &DisplaySnapshot,
        scroll_y: f64,
        viewport: f64,
        label_rows: f64,
    ) -> Option<StuckExcerptRows> {
        let rows = self.stuck_excerpt_range(snapshot)?;
        let start_y = snapshot.row_y(rows.start.as_f64());
        let end_y = snapshot.row_y(rows.end.as_f64());
        let state = self.stuck_excerpt.as_mut()?;
        let Some((height, max_offset)) =
            panel_size(state.edge, start_y, end_y, scroll_y, viewport, label_rows)
        else {
            state.pending = false;
            state.followed_head = None;
            state.offset = 0.0;
            return None;
        };

        let anchor = self.selections.newest_anchor().head();
        if state.followed_head != Some(anchor) {
            state.followed_head = Some(anchor);
            let head = anchor.to_display_point(snapshot).row();
            if rows.contains(&head) {
                let head_top = snapshot.row_y(head.as_f64()) - start_y;
                let head_end = snapshot.row_y(head.next_row().as_f64()) - start_y;
                state.offset = state
                    .offset
                    .clamp(head_end - height, head_top.max(head_end - height));
            }
        }
        state.offset = state.offset.clamp(0.0, max_offset);
        let top_y = start_y + state.offset;
        let first = DisplayRow(snapshot.row_at_y(top_y).floor() as u32).max(rows.start);
        let last = DisplayRow(snapshot.row_at_y(top_y + height).ceil() as u32).min(rows.end);
        Some(StuckExcerptRows {
            rows: first..last,
            top_y,
            height,
            max_offset,
        })
    }
}

fn panel_size(
    edge: StuckEdge,
    start: f64,
    end: f64,
    scroll: f64,
    viewport: f64,
    label_rows: f64,
) -> Option<(f64, f64)> {
    let height = (end - start).min((viewport / 2.0).floor() - label_rows);
    (edge.is_outside(start, end, scroll, viewport) && height > 0.0)
        .then_some((height, end - start - height))
}

#[cfg(test)]
mod tests {
    use super::{StuckEdge, panel_size};

    #[test]
    fn the_same_excerpt_sticks_only_below_the_view_and_caps_at_half() {
        assert_eq!(
            panel_size(StuckEdge::Bottom, 45.0, 49.0, 14.0, 31.0, 0.0),
            Some((4.0, 0.0))
        );
        assert_eq!(
            panel_size(StuckEdge::Bottom, 45.0, 49.0, 14.01, 31.0, 0.0),
            None
        );
        assert_eq!(
            panel_size(StuckEdge::Bottom, 45.0, 71.0, 14.0, 31.0, 0.0),
            Some((15.0, 11.0))
        );
    }

    #[test]
    fn a_top_excerpt_sticks_only_when_its_last_row_has_left_the_view() {
        let top = StuckEdge::Top;
        assert_eq!(panel_size(top, 0.0, 5.0, 4.99, 19.0, 1.0), None);
        assert_eq!(panel_size(top, 0.0, 5.0, 5.0, 19.0, 1.0), Some((5.0, 0.0)));
        assert_eq!(
            panel_size(top, 0.0, 21.0, 23.0, 19.0, 1.0),
            Some((8.0, 13.0))
        );
    }
}
