/// Sparse vertical geometry for display-only gaps following logical rows.
///
/// Logical row coordinates remain unchanged. Each entry stretches one logical
/// row's physical span from one line-height to `1 + gap` line-heights, so a
/// fractional coordinate within that row maps proportionally and the inverse
/// is exact.
#[derive(Clone, Debug, Default)]
pub(crate) struct RowGeometry {
    entries: Vec<Entry>,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    row: u32,
    gap: f64,
    prefix: f64,
    physical_start: f64,
}

impl RowGeometry {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn new(mut gaps: Vec<(u32, f32)>) -> Self {
        gaps.retain(|(_, gap)| gap.is_finite() && *gap > 0.0);
        gaps.sort_unstable_by_key(|(row, _)| *row);

        let mut deduplicated: Vec<(u32, f32)> = Vec::with_capacity(gaps.len());
        for (row, gap) in gaps {
            if let Some((last_row, last_gap)) = deduplicated.last_mut()
                && *last_row == row
            {
                *last_gap = last_gap.max(gap);
            } else {
                deduplicated.push((row, gap));
            }
        }

        let mut prefix = 0.0;
        let entries = deduplicated
            .into_iter()
            .map(|(row, gap)| {
                let entry = Entry {
                    row,
                    gap: gap as f64,
                    prefix,
                    physical_start: row as f64 + prefix,
                };
                prefix += gap as f64;
                entry
            })
            .collect();
        Self { entries }
    }

    pub(crate) fn row_y(&self, row: f64) -> f64 {
        if self.entries.is_empty() || row < 0.0 {
            return row;
        }

        let logical_row = row.floor().min(u32::MAX as f64) as u32;
        let ix = self
            .entries
            .partition_point(|entry| entry.row < logical_row);
        let prefix = ix
            .checked_sub(1)
            .map(|ix| self.entries[ix].prefix + self.entries[ix].gap)
            .unwrap_or(0.0);
        let gap = self
            .entries
            .get(ix)
            .filter(|entry| entry.row == logical_row)
            .map_or(0.0, |entry| entry.gap);
        row + prefix + row.fract() * gap
    }

    pub(crate) fn row_at_y(&self, y: f64) -> f64 {
        if self.entries.is_empty() || y < 0.0 {
            return y;
        }

        let ix = self
            .entries
            .partition_point(|entry| entry.physical_start <= y);
        let Some(entry) = ix.checked_sub(1).map(|ix| self.entries[ix]) else {
            return y;
        };
        let physical_end = entry.physical_start + 1.0 + entry.gap;
        if y <= physical_end {
            entry.row as f64 + (y - entry.physical_start) / (1.0 + entry.gap)
        } else {
            y - entry.prefix - entry.gap
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RowGeometry;

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn identity_without_gaps() {
        let geometry = RowGeometry::default();
        for value in [-2.25, 0.0, 0.75, 3.0, 1000.125] {
            close(geometry.row_y(value), value);
            close(geometry.row_at_y(value), value);
        }
    }

    #[test]
    fn maps_asymmetric_gaps_and_boundaries() {
        let geometry = RowGeometry::new(vec![(1, 0.5), (4, 0.25)]);
        close(geometry.row_y(1.0), 1.0);
        close(geometry.row_y(1.5), 1.75);
        close(geometry.row_y(2.0), 2.5);
        close(geometry.row_y(4.0), 4.5);
        close(geometry.row_y(4.5), 5.125);
        close(geometry.row_y(5.0), 5.75);

        close(geometry.row_at_y(1.0), 1.0);
        close(geometry.row_at_y(1.75), 1.5);
        close(geometry.row_at_y(2.5), 2.0);
        close(geometry.row_at_y(4.5), 4.0);
        close(geometry.row_at_y(5.125), 4.5);
        close(geometry.row_at_y(5.75), 5.0);
    }

    #[test]
    fn forward_and_inverse_are_exact_across_gaps() {
        let geometry = RowGeometry::new(vec![(0, 0.5), (3, 0.2), (9, 0.75)]);
        for value in [0.0, 0.1, 0.999, 1.0, 2.8, 3.25, 4.0, 8.9, 9.5, 10.0, 20.3] {
            close(geometry.row_at_y(geometry.row_y(value)), value);
        }
    }

    #[test]
    fn deduplicates_boundaries_instead_of_piling_gaps() {
        let geometry = RowGeometry::new(vec![(2, 0.25), (2, 0.5), (2, 0.1)]);
        close(geometry.row_y(3.0), 3.5);
        close(geometry.row_at_y(3.5), 3.0);
    }
}
