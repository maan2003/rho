use std::{cmp::Ordering, ops::Range};

use language::Point;
use multi_buffer::{Anchor, MultiBufferSnapshot, ToPoint};
use sum_tree::{Bias, SeekTarget, SumTree};

use super::tab_map::{TabPoint, TabSnapshot};

/// View-local font scales keyed by anchored source ranges.
///
/// Keeping the anchors in a contextual SumTree lets edits move every range
/// without eagerly resolving the whole collection on each display snapshot.
#[derive(Clone)]
pub(super) struct RowScaleSnapshot {
    ranges: SumTree<RowScale>,
}

#[derive(Clone, Debug)]
struct RowScale {
    range: Range<Anchor>,
    scale: f32,
}

#[derive(Clone, Debug)]
struct RowScaleSummary {
    range: Range<Anchor>,
}

impl Default for RowScaleSummary {
    fn default() -> Self {
        Self {
            range: Anchor::Min..Anchor::Min,
        }
    }
}

impl sum_tree::Summary for RowScaleSummary {
    type Context<'a> = &'a MultiBufferSnapshot;

    fn zero(_: Self::Context<'_>) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self, _: Self::Context<'_>) {
        self.range = other.range.clone();
    }
}

impl sum_tree::Item for RowScale {
    type Summary = RowScaleSummary;

    fn summary(&self, _: &MultiBufferSnapshot) -> Self::Summary {
        RowScaleSummary {
            range: self.range.clone(),
        }
    }
}

impl SeekTarget<'_, RowScaleSummary, RowScaleSummary> for Anchor {
    fn cmp(&self, other: &RowScaleSummary, snapshot: &MultiBufferSnapshot) -> Ordering {
        self.cmp(&other.range.start, snapshot)
    }
}

impl RowScaleSnapshot {
    pub(super) fn new(ranges: Vec<(Range<Anchor>, f32)>, snapshot: &MultiBufferSnapshot) -> Self {
        let mut tree = SumTree::new(snapshot);
        tree.extend(
            ranges
                .into_iter()
                .map(|(range, scale)| RowScale { range, scale }),
            snapshot,
        );
        Self { ranges: tree }
    }

    pub(super) fn scale_for_buffer_row(&self, snapshot: &MultiBufferSnapshot, row: u32) -> f32 {
        if self.ranges.is_empty() {
            return 1.0;
        }
        let row = row.min(snapshot.max_point().row);
        self.scale_for_source_rows(snapshot, row..=row)
    }

    pub(super) fn scale_for_tab_row(&self, snapshot: &TabSnapshot, row: u32) -> f32 {
        if self.ranges.is_empty() {
            return 1.0;
        }
        let max = snapshot.max_point();
        let start = TabPoint::new(row, 0).min(max);
        let end = TabPoint::new(row.saturating_add(1), 0).min(max);
        let start = snapshot.tab_point_to_point(start, Bias::Left);
        let end = snapshot.tab_point_to_point(end, Bias::Right);
        let end_row = if end.column == 0 && end.row > start.row {
            end.row - 1
        } else {
            end.row
        };
        self.scale_for_source_rows(snapshot.buffer_snapshot(), start.row..=end_row)
    }

    fn scale_for_source_rows(
        &self,
        snapshot: &MultiBufferSnapshot,
        rows: std::ops::RangeInclusive<u32>,
    ) -> f32 {
        let start = snapshot.anchor_before(Point::new(*rows.start(), 0));
        let mut cursor = self.ranges.cursor::<RowScaleSummary>(snapshot);
        cursor.seek(&start, Bias::Right);
        cursor.prev();

        if let Some(item) = cursor.item()
            && item.range.end.to_point(snapshot).row >= *rows.start()
        {
            return item.scale;
        }

        cursor.next();
        if let Some(item) = cursor.item()
            && item.range.start.to_point(snapshot).row <= *rows.end()
        {
            return item.scale;
        }

        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::App;
    use multi_buffer::MultiBuffer;

    #[gpui::test]
    fn scales_every_row_touched_by_an_anchor_range(cx: &mut App) {
        let buffer = MultiBuffer::build_simple("zero\none\ntwo\nthree", cx);
        let snapshot = buffer.read(cx).snapshot(cx);
        let scales = RowScaleSnapshot::new(
            vec![(
                snapshot.anchor_before(Point::new(1, 2))..snapshot.anchor_after(Point::new(2, 0)),
                1.5,
            )],
            &snapshot,
        );

        assert_eq!(scales.scale_for_buffer_row(&snapshot, 0), 1.0);
        assert_eq!(scales.scale_for_buffer_row(&snapshot, 1), 1.5);
        assert_eq!(scales.scale_for_buffer_row(&snapshot, 2), 1.5);
        assert_eq!(scales.scale_for_buffer_row(&snapshot, 3), 1.0);
    }
}
