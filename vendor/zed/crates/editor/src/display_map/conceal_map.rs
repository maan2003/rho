use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset};
use std::ops::Range;

/// Syntax-derived source ranges that are omitted from the visible text stream.
///
/// The ranges stay anchored in the multibuffer's source coordinate space. The
/// inlay layer composes them before inserting view-local text, so concealment
/// remains independent of inlay churn while downstream maps only see visible
/// source text.
#[derive(Default)]
pub(crate) struct ConcealMap {
    ranges: Vec<Range<Anchor>>,
    resolved_ranges: Vec<Range<MultiBufferOffset>>,
    edit_count: usize,
}

impl ConcealMap {
    pub(crate) fn ranges(&self, snapshot: &MultiBufferSnapshot) -> Vec<Range<MultiBufferOffset>> {
        if self.edit_count == snapshot.edit_count() {
            return self.resolved_ranges.clone();
        }
        Self::resolve(&self.ranges, snapshot)
    }

    fn resolve(
        ranges: &[Range<Anchor>],
        snapshot: &MultiBufferSnapshot,
    ) -> Vec<Range<MultiBufferOffset>> {
        let mut resolved = Vec::<Range<MultiBufferOffset>>::with_capacity(ranges.len());
        for range in ranges {
            let start = range.start.to_offset(snapshot);
            let end = range.end.to_offset(snapshot);
            if start >= end {
                continue;
            }
            if let Some(previous) = resolved.last_mut()
                && start <= previous.end
            {
                previous.end = previous.end.max(end);
            } else {
                resolved.push(start..end);
            }
        }
        resolved
    }

    pub(crate) fn sync(&mut self, snapshot: &MultiBufferSnapshot) {
        self.resolved_ranges = Self::resolve(&self.ranges, snapshot);
        self.edit_count = snapshot.edit_count();
    }

    pub(crate) fn sync_from(&mut self, start: MultiBufferOffset, snapshot: &MultiBufferSnapshot) {
        let prefix_len = self
            .resolved_ranges
            .partition_point(|range| range.end <= start);
        let mut resolved = self.resolved_ranges[..prefix_len].to_vec();
        for range in Self::resolve(&self.ranges[prefix_len..], snapshot) {
            if let Some(previous) = resolved.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
            } else {
                resolved.push(range);
            }
        }
        self.resolved_ranges = resolved;
        self.edit_count = snapshot.edit_count();
    }

    pub(crate) fn matches(
        &self,
        ranges: &[Range<MultiBufferOffset>],
        snapshot: &MultiBufferSnapshot,
    ) -> bool {
        self.edit_count == snapshot.edit_count() && self.resolved_ranges == ranges
    }

    pub(crate) fn replace(
        &mut self,
        ranges: Vec<Range<MultiBufferOffset>>,
        snapshot: &MultiBufferSnapshot,
    ) {
        let common_prefix = self
            .resolved_ranges
            .iter()
            .zip(&ranges)
            .take_while(|(old, new)| old == new)
            .count();
        let common_suffix = self.resolved_ranges[common_prefix..]
            .iter()
            .rev()
            .zip(ranges[common_prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let old_changed_end = self.ranges.len() - common_suffix;
        let new_changed_end = ranges.len() - common_suffix;
        let replacement = ranges[common_prefix..new_changed_end]
            .iter()
            .map(|range| snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end))
            .collect::<Vec<_>>();
        self.ranges
            .splice(common_prefix..old_changed_end, replacement);
        self.resolved_ranges = ranges;
        self.edit_count = snapshot.edit_count();
    }
}
