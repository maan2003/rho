use crate::display_map::inlay_map::InlayChunk;

use super::{
    ElisionPolicy, Highlights,
    inlay_map::{InlayBufferRows, InlayChunks, InlayEdit, InlayOffset, InlayPoint, InlaySnapshot},
};
use collections::HashMap;
use gpui::{AnyElement, App, ElementId, HighlightStyle, Pixels, SharedString, Stateful, Window};
use language::InlayId;
use language::{Edit, HighlightId, LanguageAwareStyling, Point};
use multi_buffer::{
    Anchor, AnchorRangeExt, MBTextSummary, MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot,
    RowInfo, ToOffset,
};
use std::{
    any::TypeId,
    cmp::{self, Ordering},
    fmt, iter,
    ops::{Add, AddAssign, Deref, DerefMut, Range, Sub, SubAssign},
    sync::Arc,
    usize,
};
use sum_tree::{Bias, Cursor, Dimensions, FilterCursor, SumTree, Summary, TreeMap};
use ui::IntoElement as _;
use util::post_inc;

/// Where an empty caret may rest at this fold's boundaries. Both ends of
/// a fold map to the same display position, so without a constraint the
/// caret can sit on either buffer side of concealed text — invisible
/// states that only differ once the user types. One-sided rest is
/// emacs's point adjustment for invisible regions: the caret is moved
/// to the allowed boundary whenever selections change.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum CaretRest {
    #[default]
    Any,
    /// A caret landing on this fold's end or interior snaps to its start.
    Start,
    /// A caret landing on this fold's start or interior snaps to its end.
    End,
    /// A caret landing strictly inside snaps to the start; both
    /// boundaries stay restable. This is vim's closed-fold convention:
    /// motions into the fold show the cursor on the fold line, while
    /// line-wise edits still address either edge.
    Boundary,
}

#[derive(Clone)]
pub struct FoldPlaceholder {
    /// Creates an element to represent this fold's placeholder.
    pub render: Arc<dyn Send + Sync + Fn(FoldId, Range<Anchor>, &mut App) -> AnyElement>,
    /// If true, the element is constrained to the shaped width of an ellipsis.
    pub constrain_width: bool,
    /// If true, merges the fold with an adjacent one.
    pub merge_adjacent: bool,
    /// Category of the fold. Useful for carefully removing from overlapping folds.
    pub type_tag: Option<TypeId>,
    /// Text provided by the language server to display in place of the folded range.
    /// When set, this is used instead of the default "⋯" ellipsis.
    /// Empty text conceals: the folded range takes no display columns and
    /// produces no chunk at all. See [`FoldPlaceholder::concealed`].
    pub collapsed_text: Option<SharedString>,
    /// Which boundary an empty caret may rest on.
    pub caret_rest: CaretRest,
}

impl Default for FoldPlaceholder {
    fn default() -> Self {
        Self {
            render: Arc::new(|_, _, _| gpui::Empty.into_any_element()),
            constrain_width: true,
            merge_adjacent: true,
            type_tag: None,
            collapsed_text: None,
            caret_rest: CaretRest::Any,
        }
    }
}

impl FoldPlaceholder {
    fn equivalent_for_replacement(&self, other: &Self) -> bool {
        self.type_tag == other.type_tag
            && self.constrain_width == other.constrain_width
            && self.merge_adjacent == other.merge_adjacent
            && self.collapsed_text == other.collapsed_text
            && self.caret_rest == other.caret_rest
            && (Arc::ptr_eq(&self.render, &other.render)
                || self.is_concealed() && other.is_concealed())
    }

    /// Returns a styled `Div` container with the standard fold‐placeholder
    /// look (background, hover, active, rounded corners, full size).
    /// Callers add children and event handlers on top.
    pub fn fold_element(fold_id: FoldId, cx: &App) -> Stateful<gpui::Div> {
        use gpui::{InteractiveElement as _, StatefulInteractiveElement as _, Styled as _};
        use settings::Settings as _;
        use theme::ActiveTheme as _;
        use theme_settings::ThemeSettings;
        let settings = ThemeSettings::get_global(cx);
        gpui::div()
            .id(fold_id)
            .font(settings.buffer_font.clone())
            .text_color(cx.theme().colors().text_placeholder)
            .bg(cx.theme().colors().ghost_element_background)
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .active(|style| style.bg(cx.theme().colors().ghost_element_active))
            .rounded_xs()
            .size_full()
    }

    /// Whether this placeholder conceals: displays nothing in place of the
    /// folded text. Concealed folds are decoration rather than something the
    /// reader folded, so unfold commands leave them alone.
    pub fn is_concealed(&self) -> bool {
        self.collapsed_text
            .as_ref()
            .is_some_and(|text| text.is_empty())
    }

    /// A fold that hides its range outright, with no placeholder standing in
    /// for it: the buffer text stays as it is (selections, copy and search
    /// still see it) while the display skips it. Suited to markup that only
    /// carries styling, like the `**` around bold markdown.
    pub fn concealed(type_tag: TypeId) -> Self {
        Self {
            render: Arc::new(|_, _, _| gpui::Empty.into_any_element()),
            constrain_width: true,
            // Concealed ranges are placed and replaced individually; merging
            // them would fold the text between two adjacent ones.
            merge_adjacent: false,
            type_tag: Some(type_tag),
            collapsed_text: Some(SharedString::default()),
            caret_rest: CaretRest::Any,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> Self {
        Self {
            render: Arc::new(|_id, _range, _cx| gpui::Empty.into_any_element()),
            constrain_width: true,
            merge_adjacent: true,
            type_tag: None,
            collapsed_text: None,
            caret_rest: CaretRest::Any,
        }
    }
}

impl fmt::Debug for FoldPlaceholder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FoldPlaceholder")
            .field("constrain_width", &self.constrain_width)
            .field("collapsed_text", &self.collapsed_text)
            .finish()
    }
}

impl Eq for FoldPlaceholder {}

impl PartialEq for FoldPlaceholder {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.render, &other.render)
            && self.constrain_width == other.constrain_width
            && self.collapsed_text == other.collapsed_text
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct FoldPoint(pub Point);

impl FoldPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(Point::new(row, column))
    }

    pub fn row(self) -> u32 {
        self.0.row
    }

    pub fn column(self) -> u32 {
        self.0.column
    }

    pub fn row_mut(&mut self) -> &mut u32 {
        &mut self.0.row
    }

    #[cfg(test)]
    pub fn column_mut(&mut self) -> &mut u32 {
        &mut self.0.column
    }

    #[ztracing::instrument(skip_all)]
    pub fn to_inlay_point(self, snapshot: &FoldSnapshot) -> InlayPoint {
        let (start, _, _) = snapshot
            .transforms
            .find::<Dimensions<FoldPoint, InlayPoint>, _>((), &self, Bias::Right);
        let overshoot = self.0 - start.0.0;
        InlayPoint(start.1.0 + overshoot)
    }

    #[ztracing::instrument(skip_all)]
    pub fn to_offset(self, snapshot: &FoldSnapshot) -> FoldOffset {
        let (start, _, item) = snapshot
            .transforms
            .find::<Dimensions<FoldPoint, TransformSummary>, _>((), &self, Bias::Right);
        let overshoot = self.0 - start.1.output.lines;
        let mut offset = start.1.output.len;
        if !overshoot.is_zero() {
            let transform = item.expect("display point out of range");
            assert!(transform.placeholder.is_none());
            let end_inlay_offset = snapshot
                .inlay_snapshot
                .to_offset(InlayPoint(start.1.input.lines + overshoot));
            offset += end_inlay_offset.0 - start.1.input.len;
        }
        FoldOffset(offset)
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for FoldPoint {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += &summary.output.lines;
    }
}

pub(crate) struct FoldMapWriter<'a>(&'a mut FoldMap);

pub(crate) trait FoldInput<T> {
    fn into_parts(self) -> (Range<T>, FoldPlaceholder, ElisionPolicy);
}

impl<T> FoldInput<T> for (Range<T>, FoldPlaceholder) {
    fn into_parts(self) -> (Range<T>, FoldPlaceholder, ElisionPolicy) {
        (self.0, self.1, ElisionPolicy::Hidden)
    }
}

impl<T> FoldInput<T> for (Range<T>, FoldPlaceholder, ElisionPolicy) {
    fn into_parts(self) -> (Range<T>, FoldPlaceholder, ElisionPolicy) {
        self
    }
}

impl FoldMapWriter<'_> {
    #[ztracing::instrument(skip_all)]
    pub(crate) fn fold<T: ToOffset, I: FoldInput<T>>(
        &mut self,
        ranges: impl IntoIterator<Item = I>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let mut edits = Vec::new();
        let mut folds = Vec::new();
        let snapshot = self.0.snapshot.inlay_snapshot.clone();
        for input in ranges.into_iter() {
            let (range, fold_text, elision_policy) = input.into_parts();
            let buffer = &snapshot.buffer;
            let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);

            // Ignore any empty ranges.
            if range.start == range.end {
                continue;
            }

            let fold_range = buffer.anchor_after(range.start)..buffer.anchor_before(range.end);
            folds.push(Fold {
                id: FoldId(post_inc(&mut self.0.next_fold_id.0)),
                range: FoldRange(fold_range),
                placeholder: fold_text,
                elision_policy,
            });

            let inlay_range =
                snapshot.to_inlay_offset(range.start)..snapshot.to_inlay_offset(range.end);
            edits.push(InlayEdit {
                old: inlay_range.clone(),
                new: inlay_range,
            });
        }

        let buffer = &snapshot.buffer;
        folds.sort_unstable_by(|a, b| sum_tree::SeekTarget::cmp(&a.range, &b.range, buffer));

        self.0.snapshot.folds = {
            let mut new_tree = SumTree::new(buffer);
            let mut cursor = self.0.snapshot.folds.cursor::<FoldRange>(buffer);
            // Folds with no existing fold between them are built as one
            // bulk run rather than appended one at a time: concealing a
            // transcript's markup inserts thousands in a single batch, and
            // a tree built from a run costs a pass instead of a merge per
            // fold.
            let mut run = Vec::new();
            for fold in folds {
                self.0.snapshot.fold_metadata_by_id.insert(
                    fold.id,
                    FoldMetadata {
                        range: fold.range.clone(),
                        width: None,
                    },
                );
                let preceding = cursor.slice(&fold.range, Bias::Right);
                if !preceding.is_empty() {
                    new_tree.extend(run.drain(..), buffer);
                    new_tree.append(preceding, buffer);
                }
                run.push(fold);
            }
            new_tree.extend(run, buffer);
            new_tree.append(cursor.suffix(), buffer);
            new_tree
        };

        let edits = consolidate_inlay_edits(edits);
        let edits = self.0.sync(snapshot.clone(), edits);
        (self.0.snapshot.clone(), edits)
    }

    /// Removes any folds with the given ranges.
    #[ztracing::instrument(skip_all)]
    pub(crate) fn remove_folds<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        type_id: TypeId,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        self.remove_folds_with(
            ranges,
            |fold| fold.placeholder.type_tag == Some(type_id),
            false,
        )
    }

    /// Replaces folds carrying `type_id`, preserving existing fold identities
    /// wherever the desired range and placeholder are unchanged.
    pub(crate) fn replace_folds_with_type<T: ToOffset, I: FoldInput<T>>(
        &mut self,
        type_id: TypeId,
        ranges: impl IntoIterator<Item = I>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let snapshot = self.0.snapshot.inlay_snapshot.clone();
        let buffer = &snapshot.buffer;
        let mut desired = Vec::new();
        for input in ranges {
            let (range, placeholder, elision_policy) = input.into_parts();
            let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);
            if range.is_empty() {
                continue;
            }
            let anchors = buffer.anchor_after(range.start)..buffer.anchor_before(range.end);
            desired.push((range, FoldRange(anchors), placeholder, elision_policy));
        }

        let mut existing_by_range: HashMap<_, Vec<Fold>> = HashMap::default();
        let mut folds = Vec::new();
        let mut cursor = self.0.snapshot.folds.cursor::<FoldRange>(buffer);
        cursor.next();
        while let Some(fold) = cursor.item() {
            if fold.placeholder.type_tag == Some(type_id) {
                existing_by_range
                    .entry((
                        fold.range.start.to_offset(buffer),
                        fold.range.end.to_offset(buffer),
                    ))
                    .or_default()
                    .push(fold.clone());
            } else {
                folds.push(fold.clone());
            }
            cursor.next();
        }
        drop(cursor);

        let mut edits = Vec::new();
        for (range, anchors, placeholder, elision_policy) in desired {
            let key = (range.start, range.end);
            let existing = existing_by_range.get_mut(&key).and_then(|folds| {
                folds
                    .iter()
                    .position(|fold| {
                        fold.elision_policy == elision_policy
                            && fold.placeholder.equivalent_for_replacement(&placeholder)
                    })
                    .map(|ix| folds.swap_remove(ix))
            });
            if let Some(existing) = existing {
                folds.push(existing);
            } else {
                let fold = Fold {
                    id: FoldId(post_inc(&mut self.0.next_fold_id.0)),
                    range: anchors,
                    placeholder,
                    elision_policy,
                };
                self.0.snapshot.fold_metadata_by_id.insert(
                    fold.id,
                    FoldMetadata {
                        range: fold.range.clone(),
                        width: None,
                    },
                );
                let range =
                    snapshot.to_inlay_offset(range.start)..snapshot.to_inlay_offset(range.end);
                edits.push(InlayEdit {
                    old: range.clone(),
                    new: range,
                });
                folds.push(fold);
            }
        }

        for removed in existing_by_range.into_values().flatten() {
            let range = removed.range.start.to_offset(buffer)..removed.range.end.to_offset(buffer);
            if !range.is_empty() {
                let range =
                    snapshot.to_inlay_offset(range.start)..snapshot.to_inlay_offset(range.end);
                edits.push(InlayEdit {
                    old: range.clone(),
                    new: range,
                });
            }
            self.0.snapshot.fold_metadata_by_id.remove(&removed.id);
        }

        folds.sort_unstable_by(|a, b| sum_tree::SeekTarget::cmp(&a.range, &b.range, buffer));
        self.0.snapshot.folds = SumTree::from_iter(folds, buffer);
        let edits = consolidate_inlay_edits(edits);
        let edits = self.0.sync(snapshot, edits);
        (self.0.snapshot.clone(), edits)
    }

    /// Removes any folds whose ranges intersect the given ranges. Concealed
    /// folds stay: they hide markup rather than content, so unfolding a
    /// region is not a request to reveal them.
    #[ztracing::instrument(skip_all)]
    pub(crate) fn unfold_intersecting<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        inclusive: bool,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        self.remove_folds_with(ranges, |fold| !fold.placeholder.is_concealed(), inclusive)
    }

    /// Removes any folds that intersect the given ranges and for which the given predicate
    /// returns true.
    #[ztracing::instrument(skip_all)]
    fn remove_folds_with<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        should_unfold: impl Fn(&Fold) -> bool,
        inclusive: bool,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let mut edits = Vec::new();
        let mut fold_ixs_to_delete = Vec::new();
        let snapshot = self.0.snapshot.inlay_snapshot.clone();
        let buffer = &snapshot.buffer;
        for range in ranges.into_iter() {
            let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);
            let mut folds_cursor =
                intersecting_folds(&snapshot, &self.0.snapshot.folds, range.clone(), inclusive);
            while let Some(fold) = folds_cursor.item() {
                let offset_range =
                    fold.range.start.to_offset(buffer)..fold.range.end.to_offset(buffer);
                if should_unfold(fold) {
                    if offset_range.end > offset_range.start {
                        let inlay_range = snapshot.to_inlay_offset(offset_range.start)
                            ..snapshot.to_inlay_offset(offset_range.end);
                        edits.push(InlayEdit {
                            old: inlay_range.clone(),
                            new: inlay_range,
                        });
                    }
                    fold_ixs_to_delete.push(*folds_cursor.start());
                    self.0.snapshot.fold_metadata_by_id.remove(&fold.id);
                }
                folds_cursor.next();
            }
        }

        fold_ixs_to_delete.sort_unstable();
        fold_ixs_to_delete.dedup();

        self.0.snapshot.folds = {
            let mut cursor = self.0.snapshot.folds.cursor::<MultiBufferOffset>(buffer);
            let mut folds = SumTree::new(buffer);
            for fold_ix in fold_ixs_to_delete {
                folds.append(cursor.slice(&fold_ix, Bias::Right), buffer);
                cursor.next();
            }
            folds.append(cursor.suffix(), buffer);
            folds
        };

        let edits = consolidate_inlay_edits(edits);
        let edits = self.0.sync(snapshot.clone(), edits);
        (self.0.snapshot.clone(), edits)
    }

    #[ztracing::instrument(skip_all)]
    pub(crate) fn update_fold_widths(
        &mut self,
        new_widths: impl IntoIterator<Item = (ChunkRendererId, Pixels)>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let mut edits = Vec::new();
        let inlay_snapshot = self.0.snapshot.inlay_snapshot.clone();
        let buffer = &inlay_snapshot.buffer;

        for (id, new_width) in new_widths {
            let ChunkRendererId::Fold(id) = id else {
                continue;
            };
            if let Some(metadata) = self.0.snapshot.fold_metadata_by_id.get(&id).cloned()
                && Some(new_width) != metadata.width
            {
                let buffer_start = metadata.range.start.to_offset(buffer);
                let buffer_end = metadata.range.end.to_offset(buffer);
                let inlay_range = inlay_snapshot.to_inlay_offset(buffer_start)
                    ..inlay_snapshot.to_inlay_offset(buffer_end);
                edits.push(InlayEdit {
                    old: inlay_range.clone(),
                    new: inlay_range.clone(),
                });

                self.0.snapshot.fold_metadata_by_id.insert(
                    id,
                    FoldMetadata {
                        range: metadata.range,
                        width: Some(new_width),
                    },
                );
            }
        }

        let edits = consolidate_inlay_edits(edits);
        let edits = self.0.sync(inlay_snapshot, edits);
        (self.0.snapshot.clone(), edits)
    }
}

/// Decides where the fold indicators should be; also tracks parts of a source file that are currently folded.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct FoldMap {
    snapshot: FoldSnapshot,
    next_fold_id: FoldId,
    /// Every widened edit that stopped describing a range, in the words of
    /// [`widening_violation`].
    ///
    /// The widening loops in `sync` are where an edit is made wrong; the
    /// seek that walks over it is where the panic happens, and the two are
    /// far enough apart that the message names the wrong layer. Both faults
    /// found on the rig this week — an unsigned underflow at a fold
    /// beginning at offset zero, and an edit left behind a cursor that
    /// stepped over it — are visible here, at the point the edit is built,
    /// before anything is asked to seek anywhere.
    #[cfg(feature = "wrap-test-support")]
    widening_violations: Vec<String>,
    /// Every sync whose emitted edits did not account for the change in
    /// this map's own output extent.
    ///
    /// The layer-by-layer accounting question, asked of the fold map at
    /// its own output: does this snapshot's extent equal the last one's
    /// plus the net of the edits handed over with it. A sync that says
    /// the document grew by six bytes over an output that did not grow is
    /// telling the layers above about a document that does not exist, and
    /// they find out later and further away - as `display point out of
    /// range` in `FoldPoint::to_offset`, reached from `BlockMap::sync`,
    /// or as rows a snapshot claims and the chunks do not yield.
    #[cfg(feature = "wrap-test-support")]
    accounting_violations: Vec<String>,
    /// How many times the two ends of an edit met at one buffer offset at
    /// the end of one and the same fold - the shape that is allowed.
    ///
    /// Kept so that an empty violation record means something. Without it,
    /// a document whose ends converge legitimately and one whose ends never
    /// converge at all are the same silence.
    #[cfg(feature = "wrap-test-support")]
    end_convergences: usize,
}

/// Whether a widened edit still describes a range of a document that exists.
///
/// Deliberately narrow. It does not know what the right answer is, only
/// what cannot be one: a range whose start is past its own end, or whose
/// end is past everything there is. Both are unreachable in a document and
/// both are one subtraction away in the loops that build them.
#[cfg(feature = "wrap-test-support")]
pub fn widening_violation(
    side: &str,
    range: std::ops::Range<usize>,
    extent: usize,
    stage: &str,
) -> Option<String> {
    if range.start > range.end {
        return Some(format!(
            "{stage}: the {side} side was widened to {}..{}, which starts after it ends; an unsigned subtraction took more than the side had in front of it",
            range.start, range.end
        ));
    }
    if range.end > extent {
        return Some(format!(
            "{stage}: the {side} side was widened to {}..{}, past the {extent} bytes the tree has",
            range.start, range.end
        ));
    }
    None
}

impl FoldMap {
    /// Every widened edit this map built that did not describe a range,
    /// and forgets them. Empty is the answer a healthy sync gives.
    #[cfg(feature = "wrap-test-support")]
    pub fn take_widening_violations(&mut self) -> Vec<String> {
        std::mem::take(&mut self.widening_violations)
    }

    /// Every sync whose edits did not account for its own output extent,
    /// and forgets them. Empty is the answer a healthy sync gives.
    #[cfg(feature = "wrap-test-support")]
    pub fn take_accounting_violations(&mut self) -> Vec<String> {
        std::mem::take(&mut self.accounting_violations)
    }

    /// How many edits had both ends widened to the end of one and the same
    /// fold, and forgets the count. A test asserting the violation record
    /// is empty asserts this is not zero beside it, or it has not shown
    /// that the shape is legitimate - only that it did not occur.
    #[cfg(feature = "wrap-test-support")]
    pub fn take_end_convergences(&mut self) -> usize {
        std::mem::take(&mut self.end_convergences)
    }

    #[ztracing::instrument(skip_all)]
    pub fn new(inlay_snapshot: InlaySnapshot) -> (Self, FoldSnapshot) {
        let this = Self {
            #[cfg(feature = "wrap-test-support")]
            widening_violations: Vec::new(),
            #[cfg(feature = "wrap-test-support")]
            accounting_violations: Vec::new(),
            #[cfg(feature = "wrap-test-support")]
            end_convergences: 0,
            snapshot: FoldSnapshot {
                folds: SumTree::new(&inlay_snapshot.buffer),
                transforms: SumTree::from_item(
                    Transform {
                        summary: TransformSummary {
                            input: inlay_snapshot.text_summary(),
                            output: inlay_snapshot.text_summary(),
                        },
                        placeholder: None,
                    },
                    (),
                ),
                inlay_snapshot: inlay_snapshot,
                version: 0,
                fold_metadata_by_id: TreeMap::default(),
            },
            next_fold_id: FoldId::default(),
        };
        let snapshot = this.snapshot.clone();
        (this, snapshot)
    }

    #[ztracing::instrument(skip_all)]
    pub fn read(
        &mut self,
        inlay_snapshot: InlaySnapshot,
        edits: Vec<InlayEdit>,
    ) -> (FoldSnapshot, Vec<FoldEdit>) {
        let edits = self.sync(inlay_snapshot, edits);
        self.check_invariants();
        (self.snapshot.clone(), edits)
    }

    #[ztracing::instrument(skip_all)]
    pub(crate) fn write(
        &mut self,
        inlay_snapshot: InlaySnapshot,
        edits: Vec<InlayEdit>,
    ) -> (FoldMapWriter<'_>, FoldSnapshot, Vec<FoldEdit>) {
        let (snapshot, edits) = self.read(inlay_snapshot, edits);
        (FoldMapWriter(self), snapshot, edits)
    }

    #[ztracing::instrument(skip_all)]
    fn check_invariants(&self) {
        // `cfg!(test)` alone never fires from rho: this crate is vendored and
        // is not a workspace member, so its own tests do not run here and the
        // invariant that would have caught a fold tree out of step with its
        // inlay snapshot was dead code for us. The feature lets rho's tests
        // run it.
        if cfg!(test) || cfg!(feature = "wrap-test-support") {
            assert_eq!(
                self.snapshot.transforms.summary().input.len,
                self.snapshot.inlay_snapshot.len().0,
                "transform tree does not match inlay snapshot's length"
            );

            let mut prev_transform_isomorphic = false;
            for transform in self.snapshot.transforms.iter() {
                if !transform.is_fold() && prev_transform_isomorphic {
                    panic!(
                        "found adjacent isomorphic transforms: {:?}",
                        self.snapshot.transforms.items(())
                    );
                }
                prev_transform_isomorphic = !transform.is_fold();
            }

            let mut folds = self.snapshot.folds.iter().peekable();
            while let Some(fold) = folds.next() {
                if let Some(next_fold) = folds.peek() {
                    let comparison = fold.range.cmp(&next_fold.range, self.snapshot.buffer());
                    assert!(comparison.is_le());
                }
            }
        }
    }

    #[ztracing::instrument(skip_all)]
    fn sync(
        &mut self,
        inlay_snapshot: InlaySnapshot,
        inlay_edits: Vec<InlayEdit>,
    ) -> Vec<FoldEdit> {
        let mut profile =
            gpui::profiler::EditorTimingGuard::new(gpui::profiler::EditorTimingKind::FoldMapSync);
        let old_rows = if profile.is_enabled() {
            profile.touched_rows(
                inlay_edits
                    .iter()
                    .map(|edit| {
                        let old_start = self.snapshot.inlay_snapshot.to_point(edit.old.start).row();
                        let old_end = self.snapshot.inlay_snapshot.to_point(edit.old.end).row();
                        let new_start = inlay_snapshot.to_point(edit.new.start).row();
                        let new_end = inlay_snapshot.to_point(edit.new.end).row();
                        u64::from((old_end - old_start).max(new_end - new_start) + 1)
                    })
                    .sum(),
            );
            let input_start = inlay_edits
                .iter()
                .map(|edit| inlay_snapshot.to_point(edit.new.start).row())
                .min()
                .unwrap_or(0) as u64;
            let input_end = inlay_edits
                .iter()
                .map(|edit| inlay_snapshot.to_point(edit.new.end).row())
                .max()
                .unwrap_or(0) as u64;
            let input_rows = if inlay_edits.is_empty() {
                0
            } else {
                input_end.saturating_sub(input_start) + 1
            };
            profile.input(inlay_edits.len(), input_start, input_rows);
            u64::from(self.snapshot.max_point().row()) + 1
        } else {
            0
        };
        let mut walked_items = 0_u64;
        let fold_edits = if inlay_edits.is_empty() {
            if self.snapshot.inlay_snapshot.version != inlay_snapshot.version {
                self.snapshot.version += 1;
            }
            self.snapshot.inlay_snapshot = inlay_snapshot;
            Vec::new()
        } else {
            // The snapshot the old tree is written against. Lengths on the
            // two sides are only comparable through the buffer, so both
            // snapshots have to be in hand to convert either way.
            let old_inlay_snapshot = self.snapshot.inlay_snapshot.clone();

            // A retained fold can straddle the start of a later edit in the
            // batch. Widen that edit before either cursor starts walking, so
            // overlapping widened edits are coalesced and the fold is rebuilt
            // once from its beginning rather than appended at its old extent.
            let mut normalized_edits: Vec<InlayEdit> = Vec::with_capacity(inlay_edits.len());
            for mut edit in inlay_edits {
                let (old_transform_start, _, old_transform) = self
                    .snapshot
                    .transforms
                    .find::<InlayOffset, _>((), &edit.old.start, Bias::Left);
                let transform_prefix = edit.old.start - old_transform_start;
                let mut scan_old_start = edit.old.start;
                let mut scan_new_start = edit.new.start;
                if old_transform.is_some_and(|transform| !transform.is_fold())
                    && transform_prefix <= edit.new.start.0.0
                {
                    scan_new_start -= transform_prefix;
                    scan_old_start = old_transform_start;
                }
                loop {
                    let old_start = old_inlay_snapshot.to_buffer_offset(scan_old_start);
                    let new_start = inlay_snapshot.to_buffer_offset(scan_new_start);
                    let mut folds = intersecting_folds(
                        &old_inlay_snapshot,
                        &self.snapshot.folds,
                        old_start..old_inlay_snapshot.buffer.len(),
                        true,
                    );
                    let mut containing = None;
                    while let Some(fold) = folds.item() {
                        let old_buffer_start =
                            fold.range.start.to_offset(&old_inlay_snapshot.buffer);
                        let old_buffer_end = fold.range.end.to_offset(&old_inlay_snapshot.buffer);
                        let new_buffer_start = fold.range.start.to_offset(&inlay_snapshot.buffer);
                        let new_buffer_end = fold.range.end.to_offset(&inlay_snapshot.buffer);
                        if old_buffer_start > old_start && new_buffer_start > new_start {
                            break;
                        }
                        let old_range = old_inlay_snapshot.to_inlay_offset(old_buffer_start)
                            ..old_inlay_snapshot.to_inlay_offset(old_buffer_end);
                        let new_range = inlay_snapshot.to_inlay_offset(new_buffer_start)
                            ..inlay_snapshot.to_inlay_offset(new_buffer_end);
                        if (old_range.start < scan_old_start && scan_old_start <= old_range.end)
                            || (new_range.start < scan_new_start && scan_new_start <= new_range.end)
                        {
                            containing = Some((old_buffer_start, new_buffer_start));
                            break;
                        }
                        folds.next();
                    }
                    let Some((old_buffer_start, new_buffer_start)) = containing else {
                        break;
                    };
                    edit.old.start = edit.old.start.min(widen_start_over_inlays(
                        &old_inlay_snapshot,
                        old_buffer_start,
                    ));
                    edit.new.start = edit
                        .new
                        .start
                        .min(widen_start_over_inlays(&inlay_snapshot, new_buffer_start));
                    scan_old_start = edit.old.start;
                    scan_new_start = edit.new.start;
                }
                while normalized_edits.last().is_some_and(|previous| {
                    edit.old.start <= previous.old.end || edit.new.start <= previous.new.end
                }) {
                    let previous = normalized_edits.pop().unwrap();
                    edit.old.start = edit.old.start.min(previous.old.start);
                    edit.old.end = edit.old.end.max(previous.old.end);
                    edit.new.start = edit.new.start.min(previous.new.start);
                    edit.new.end = edit.new.end.max(previous.new.end);
                }
                normalized_edits.push(edit);
            }
            let mut inlay_edits_iter = normalized_edits.iter().cloned().peekable();

            let mut new_transforms = SumTree::<Transform>::default();
            let mut cursor = self.snapshot.transforms.cursor::<InlayOffset>(());
            cursor.seek(&InlayOffset(MultiBufferOffset(0)), Bias::Right);

            while let Some(mut edit) = inlay_edits_iter.next() {
                if let Some(item) = cursor.item()
                    && !item.is_fold()
                {
                    new_transforms.update_last(
                        |transform| {
                            if !transform.is_fold() {
                                transform.summary.add_summary(&item.summary, ());
                                cursor.next();
                            }
                        },
                        (),
                    );
                }
                new_transforms.append(cursor.slice(&edit.old.start, Bias::Left), ());
                let prefix = edit.old.start - *cursor.start();
                if prefix <= edit.new.start.0.0 {
                    edit.new.start -= prefix;
                } else {
                    let boundary = old_inlay_snapshot.to_buffer_offset(*cursor.start());
                    edit.new.start = widen_start_over_inlays(&inlay_snapshot, boundary);
                }
                edit.old.start = *cursor.start();

                cursor.seek(&edit.old.end, Bias::Right);
                cursor.next();

                let mut delta = edit.new_len() as isize - edit.old_len() as isize;
                delta += absorb_edits_behind_the_cursor(
                    &mut edit,
                    &mut cursor,
                    &mut inlay_edits_iter,
                    true,
                );
                edit.new.end = InlayOffset(MultiBufferOffset(
                    ((edit.new.start + edit.old_len()).0.0 as isize + delta) as usize,
                ));
                let edit_start = inlay_snapshot.to_buffer_offset(edit.new.start);
                let mut folds_cursor = intersecting_folds(
                    &inlay_snapshot,
                    &self.snapshot.folds,
                    edit_start..inlay_snapshot.buffer.len(),
                    false,
                );
                let folds_cursor_work = std::cell::Cell::new(folds_cursor.walked_items());

                let mut folds = iter::from_fn({
                    let inlay_snapshot = &inlay_snapshot;
                    let folds_cursor_work = &folds_cursor_work;
                    move || {
                        let item = folds_cursor.item().map(|fold| {
                            let buffer_start = fold.range.start.to_offset(&inlay_snapshot.buffer);
                            let buffer_end = fold.range.end.to_offset(&inlay_snapshot.buffer);
                            (
                                fold.clone(),
                                inlay_snapshot.to_inlay_offset(buffer_start)
                                    ..inlay_snapshot.to_inlay_offset(buffer_end),
                            )
                        });
                        folds_cursor.next();
                        folds_cursor_work.set(folds_cursor.walked_items());
                        item
                    }
                })
                .peekable();

                // Round again when the folds emitted below reach past the
                // end of the edit; see the overshoot at the end of the loop.
                loop {
                    while folds
                        .peek()
                        .is_some_and(|(_, fold_range)| fold_range.start < edit.new.end)
                    {
                        let (fold, mut fold_range) = folds.next().unwrap();
                        let sum = new_transforms.summary();

                        // Historical anchors at the edit boundary can make the
                        // intersection cursor include a fold already copied in full.
                        if fold_range.end.0 <= sum.input.len {
                            continue;
                        }
                        assert!(fold_range.start.0 >= sum.input.len);

                        while folds.peek().is_some_and(|(next_fold, next_fold_range)| {
                            next_fold_range.start < fold_range.end
                                || (next_fold_range.start == fold_range.end
                                    && fold.elision_policy == ElisionPolicy::Hidden
                                    && next_fold.elision_policy == ElisionPolicy::Hidden
                                    && fold.placeholder.merge_adjacent
                                    && next_fold.placeholder.merge_adjacent)
                        }) {
                            let (_, next_fold_range) = folds.next().unwrap();
                            if next_fold_range.end > fold_range.end {
                                fold_range.end = next_fold_range.end;
                            }
                        }

                        if fold_range.start.0 > sum.input.len {
                            let text_summary = inlay_snapshot.text_summary_for_range(
                                InlayOffset(sum.input.len)..fold_range.start,
                            );
                            push_isomorphic(&mut new_transforms, text_summary);
                        }

                        let (placeholder_range, visible_tail_range) =
                            elided_ranges(&inlay_snapshot, fold_range, fold.elision_policy);

                        if let Some(fold_range) = placeholder_range {
                            const ELLIPSIS: &str = "⋯";

                            let placeholder_text: SharedString = fold
                                .placeholder
                                .collapsed_text
                                .clone()
                                .unwrap_or_else(|| ELLIPSIS.into());
                            let chars_bitmap = placeholder_text
                                .char_indices()
                                .fold(0u128, |bitmap, (idx, _)| {
                                    bitmap | 1u128.unbounded_shl(idx as u32)
                                });

                            let fold_id = fold.id;
                            new_transforms.push(
                                Transform {
                                    summary: TransformSummary {
                                        output: MBTextSummary::from(placeholder_text.as_ref()),
                                        input: inlay_snapshot.text_summary_for_range(
                                            fold_range.start..fold_range.end,
                                        ),
                                    },
                                    placeholder: Some(TransformPlaceholder {
                                        text: placeholder_text,
                                        chars: chars_bitmap,
                                        renderer: ChunkRenderer {
                                            id: ChunkRendererId::Fold(fold.id),
                                            render: Arc::new(move |cx| {
                                                (fold.placeholder.render)(
                                                    fold_id,
                                                    fold.range.0.clone(),
                                                    cx.context,
                                                )
                                            }),
                                            constrain_width: fold.placeholder.constrain_width,
                                            measured_width: self.snapshot.fold_width(&fold_id),
                                        },
                                    }),
                                },
                                (),
                            );
                        }

                        if let Some(tail_range) = visible_tail_range {
                            let text_summary = inlay_snapshot.text_summary_for_range(tail_range);
                            push_isomorphic(&mut new_transforms, text_summary);
                        }
                    }

                    // A fold that starts inside the edit can end well after it,
                    // and it is emitted whole, so the tree can now describe
                    // input the cursor has not walked past. Past the edit both
                    // sides are the same text at a constant shift, so that
                    // input sits the same distance ahead of the cursor in the
                    // old tree. Leave the cursor where it is and the suffix
                    // appended below repeats those bytes: the tree then claims
                    // a longer document than the inlay snapshot under it, and
                    // rows resolve to offsets that are not theirs.
                    //
                    // Zed does not meet this because a hidden fold covers the
                    // same anchored range before and after, so stepping past
                    // the old fold transform steps past the right amount.
                    // `ElisionPolicy::Tail` is ours and splits one fold into a
                    // placeholder over the head and a visible tail, by rows, so
                    // the extent of what is emitted moves as the text under it
                    // does and stops matching the old transform.
                    let sum = new_transforms.summary();
                    if sum.input.len <= edit.new.end.0 {
                        break;
                    }
                    // How much further the new tree reached than the
                    // edit's end, and where in the old tree that is. Both
                    // steps go through the buffer: `sum.input.len` and
                    // `edit.new.end` are new-side inlay offsets and the
                    // cursor stands in the old tree, and an inlay is bytes
                    // one side has and the other does not, so adding a
                    // new-side length to an old-side offset is only right
                    // when no inlay lies between them. Buffer bytes past
                    // the edit are the same bytes on both sides, so the
                    // distance is measured there and laid off from the
                    // cursor's own buffer position.
                    let reached = inlay_snapshot.to_buffer_offset(InlayOffset(sum.input.len));
                    let edit_end = inlay_snapshot.to_buffer_offset(edit.new.end);
                    let overshoot = reached.0 - edit_end.0;
                    let covered_buffer = MultiBufferOffset(
                        old_inlay_snapshot.to_buffer_offset(*cursor.start()).0 + overshoot,
                    );
                    let covered = widen_end_over_inlays(&old_inlay_snapshot, covered_buffer);
                    cursor.seek_forward(&covered, Bias::Right);
                    if cursor.item().is_some_and(|item| !item.is_fold())
                        && *cursor.start() != covered
                    {
                        // The new tree ends part way through a plain run of
                        // text. A fold has no meaningful half, but text
                        // does: emit the part of this transform the new
                        // tree has not described yet, from the new
                        // snapshot, and step past it. The edit is not
                        // touched. Widening it to the end of this transform
                        // instead - which is what this did before - makes
                        // an edit that reaches the end of the document
                        // whenever the run does, and an edit that size is
                        // O(rows composed so far) on every batch that gets
                        // here, which is the cost rule broken rather than
                        // exceeded.
                        let remainder =
                            old_inlay_snapshot.to_buffer_offset(cursor.end()).0 - covered_buffer.0;
                        let remainder_end = widen_end_over_inlays(
                            &inlay_snapshot,
                            MultiBufferOffset(reached.0 + remainder),
                        );
                        if remainder_end.0 > sum.input.len {
                            let text_summary = inlay_snapshot
                                .text_summary_for_range(InlayOffset(sum.input.len)..remainder_end);
                            push_isomorphic(&mut new_transforms, text_summary);
                        }
                        // The edit has to name everything the new tree
                        // re-described, or the maps above are never told
                        // that part of their document changed - but that is
                        // `covered`, the extent of what was re-described,
                        // and not the end of the transform it landed in.
                        if covered > edit.old.end {
                            edit.old.end = covered;
                        }
                        cursor.next();
                        delta += absorb_edits_behind_the_cursor(
                            &mut edit,
                            &mut cursor,
                            &mut inlay_edits_iter,
                            false,
                        );
                        edit.new.end = InlayOffset(MultiBufferOffset(
                            ((edit.new.start + edit.old_len()).0.0 as isize + delta) as usize,
                        ));
                        break;
                    }
                    if *cursor.start() == covered {
                        // The old tree has a boundary where the new one now
                        // ends - the usual case, since a fold ends at the
                        // same anchor on both sides - and the suffix starts
                        // there.
                        let absorbed = absorb_edits_behind_the_cursor(
                            &mut edit,
                            &mut cursor,
                            &mut inlay_edits_iter,
                            true,
                        );
                        delta += absorbed;
                        edit.new.end = InlayOffset(MultiBufferOffset(
                            ((edit.new.start + edit.old_len()).0.0 as isize + delta) as usize,
                        ));
                        break;
                    }
                    // The new tree ends inside an old transform. It cannot
                    // be appended whole and it cannot be split - a fold
                    // transform has no meaningful half - so step over it and
                    // go round again with the edit widened to its end.
                    // Whatever of it the new tree does not already describe
                    // is emitted from the new snapshot, by the folds above
                    // if a fold still covers it and as text below if none
                    // does. The cursor only ever moves forward, so this
                    // ends.
                    cursor.next();
                    let absorbed = absorb_edits_behind_the_cursor(
                        &mut edit,
                        &mut cursor,
                        &mut inlay_edits_iter,
                        true,
                    );
                    delta += absorbed;
                    edit.new.end = InlayOffset(MultiBufferOffset(
                        ((edit.new.start + edit.old_len()).0.0 as isize + delta) as usize,
                    ));
                }

                let sum = new_transforms.summary();
                if sum.input.len < edit.new.end.0 {
                    let text_summary = inlay_snapshot
                        .text_summary_for_range(InlayOffset(sum.input.len)..edit.new.end);
                    push_isomorphic(&mut new_transforms, text_summary);
                }
                drop(folds);
                walked_items = walked_items.saturating_add(folds_cursor_work.get());
            }

            new_transforms.append(cursor.suffix(), ());
            walked_items = walked_items.saturating_add(cursor.walked_items());
            if new_transforms.is_empty() {
                let text_summary = inlay_snapshot.text_summary();
                push_isomorphic(&mut new_transforms, text_summary);
            }

            drop(cursor);

            let mut fold_edits = Vec::with_capacity(normalized_edits.len());
            #[cfg(feature = "wrap-test-support")]
            let mut widening_violations = Vec::new();
            #[cfg(feature = "wrap-test-support")]
            let mut accounting_violations = Vec::new();
            #[cfg(feature = "wrap-test-support")]
            let mut end_convergences = 0usize;
            // Taken from the trees before the cursors shadow them: what a
            // widened edit is checked against is the whole of each side,
            // and a cursor only knows where it is standing.
            #[cfg(feature = "wrap-test-support")]
            let old_extent = self.snapshot.transforms.summary().output.len.0;
            #[cfg(feature = "wrap-test-support")]
            let new_extent = new_transforms.summary().output.len.0;
            {
                let mut old_transforms = self
                    .snapshot
                    .transforms
                    .cursor::<Dimensions<InlayOffset, FoldOffset>>(());
                let mut new_transforms =
                    new_transforms.cursor::<Dimensions<InlayOffset, FoldOffset>>(());
                for mut edit in normalized_edits {
                    // An edit landing inside a fold is widened to the fold,
                    // which has to be re-emitted whole. Both sides of the
                    // edit name the same boundary in the text - the bytes
                    // before an edit are the same bytes before and after it
                    // - so widening one side widens the other by the same
                    // inlay bytes. Widen either side alone and the edit
                    // stops describing one document: the map above adds up
                    // the rows it names and gets a total the snapshot
                    // beside it does not have.
                    //
                    // The folds are not the same on both sides - that is
                    // what the edit is often about - so moving one side
                    // can land the other in a fold of its own, and this
                    // repeats until neither side is inside one. It walks
                    // the folds the edit touches and no others.
                    // How far the new side's start sits from the old
                    // side's, in the coordinate they share, before any
                    // widening. This is not zero in general: within a
                    // batch, an edit's new side carries the shift of every
                    // edit before it. What the widening must not do is
                    // change it.
                    #[cfg(feature = "wrap-test-support")]
                    let unwidened_start = (edit.old.start, edit.new.start);
                    #[cfg(feature = "wrap-test-support")]
                    let start_skew = inlay_snapshot.to_buffer_offset(edit.new.start).0 as isize
                        - old_inlay_snapshot.to_buffer_offset(edit.old.start).0 as isize;
                    loop {
                        old_transforms.seek(&edit.old.start, Bias::Left);
                        let old_fold_start = old_transforms
                            .item()
                            .is_some_and(|t| t.is_fold())
                            .then(|| old_inlay_snapshot.to_buffer_offset(old_transforms.start().0));
                        new_transforms.seek(&edit.new.start, Bias::Left);
                        let new_fold_start = new_transforms
                            .item()
                            .is_some_and(|t| t.is_fold())
                            .then(|| inlay_snapshot.to_buffer_offset(new_transforms.start().0));
                        // The boundary the edit is being moved back to,
                        // named once, in the coordinate both sides share.
                        // Whichever side is inside a fold names the fold's
                        // start; if both are, the earlier of the two wins,
                        // because the later side is inside that fold too
                        // once it gets there.
                        let target = match (old_fold_start, new_fold_start) {
                            (Some(old), Some(new)) => old.min(new),
                            (Some(side), None) | (None, Some(side)) => side,
                            (None, None) => break,
                        };
                        // Said back on each side in its own coordinates.
                        // The two sides can differ here, and only here:
                        // by the inlay bytes standing in front of the
                        // boundary on that side. Stepping back over an
                        // adjacent inlay brings it inside the widened
                        // edit, which is where it belongs.
                        let old_start = widen_start_over_inlays(&old_inlay_snapshot, target);
                        let new_start = widen_start_over_inlays(&inlay_snapshot, target);
                        if old_start >= edit.old.start && new_start >= edit.new.start {
                            break;
                        }
                        edit.old.start = edit.old.start.min(old_start);
                        edit.new.start = edit.new.start.min(new_start);
                    }
                    // Both sides moved back to one boundary, so the gap
                    // between them is the one they started with. That is
                    // the invariant the layers above depend on: they add
                    // up the rows the edit names and expect the total the
                    // snapshot beside them has. Sides that moved by
                    // different amounts describe two different documents,
                    // and that is the fault this records.
                    #[cfg(feature = "wrap-test-support")]
                    {
                        let skew = inlay_snapshot.to_buffer_offset(edit.new.start).0 as isize
                            - old_inlay_snapshot.to_buffer_offset(edit.old.start).0 as isize;
                        if skew != start_skew {
                            widening_violations.push(format!(
                                "fold widening: the two sides of the start stood {start_skew} apart in the buffer and stand {skew} apart after widening; they did not move back to the same boundary"
                            ));
                        }
                        // An inlay standing on a widened start belongs
                        // inside the widened edit, whatever its own bias:
                        // the direction of travel decides, and a start
                        // travels backwards. A byte just before the start
                        // that maps to the same buffer offset as the start
                        // is inlay text left outside.
                        for (side, start, unwidened, snapshot) in [
                            (
                                "old",
                                edit.old.start,
                                unwidened_start.0,
                                &old_inlay_snapshot,
                            ),
                            ("new", edit.new.start, unwidened_start.1, &inlay_snapshot),
                        ] {
                            // Only a side the widening moved. An inlay
                            // beside a start that never moved is where the
                            // inlay map put the edit, and the edit already
                            // covers it; this is about a boundary chosen
                            // here.
                            if start < unwidened && start.0.0 > 0 {
                                let before = InlayOffset(MultiBufferOffset(start.0.0 - 1));
                                if snapshot.to_buffer_offset(before)
                                    == snapshot.to_buffer_offset(start)
                                {
                                    widening_violations.push(format!(
                                        "fold widening: an inlay stands on the widened {side} start at {} and was left outside the edit",
                                        start.0.0
                                    ));
                                }
                            }
                        }
                    }
                    let old_start =
                        old_transforms.start().1.0 + (edit.old.start - old_transforms.start().0);
                    let new_start =
                        new_transforms.start().1.0 + (edit.new.start - new_transforms.start().0);

                    #[cfg(feature = "wrap-test-support")]
                    let unwidened_end = (edit.old.end, edit.new.end);
                    // The last fold each end was widened out of, as a
                    // range of the buffer, which is the only coordinate in
                    // which the two sides' folds can be compared.
                    #[cfg(feature = "wrap-test-support")]
                    let mut widened_out_of: (
                        Option<std::ops::Range<MultiBufferOffset>>,
                        Option<std::ops::Range<MultiBufferOffset>>,
                    ) = (None, None);
                    loop {
                        old_transforms.seek_forward(&edit.old.end, Bias::Right);
                        let old_delta = if old_transforms.item().is_some_and(|t| t.is_fold()) {
                            #[cfg(feature = "wrap-test-support")]
                            {
                                widened_out_of.0 = Some(
                                    old_inlay_snapshot.to_buffer_offset(old_transforms.start().0)
                                        ..old_inlay_snapshot
                                            .to_buffer_offset(old_transforms.end().0),
                                );
                            }
                            old_transforms.end().0.0.0 - edit.old.end.0.0
                        } else {
                            0
                        };
                        new_transforms.seek_forward(&edit.new.end, Bias::Right);
                        let new_delta = if new_transforms.item().is_some_and(|t| t.is_fold()) {
                            #[cfg(feature = "wrap-test-support")]
                            {
                                widened_out_of.1 = Some(
                                    inlay_snapshot.to_buffer_offset(new_transforms.start().0)
                                        ..inlay_snapshot.to_buffer_offset(new_transforms.end().0),
                                );
                            }
                            new_transforms.end().0.0.0 - edit.new.end.0.0
                        } else {
                            0
                        };
                        let delta = old_delta.max(new_delta);
                        if delta == 0 {
                            break;
                        }
                        edit.old.end.0.0 += delta;
                        edit.new.end.0.0 += delta;
                    }
                    // Whether the two ends met, and whether they had a
                    // right to.
                    //
                    // Two ends inside one fold converge on one buffer
                    // offset legitimately: the fold is re-emitted whole, so
                    // both ends are moved to its end and that end is the
                    // same text on both sides. Two ends that converge for
                    // any other reason - two different folds whose ends
                    // happen to land on one offset, or a common step
                    // carrying one end further than its own fold needed -
                    // are the end's version of the inverted range the
                    // common step used to hide at the start.
                    //
                    // The legitimate case is counted and not only the
                    // faulty one recorded. Silence on its own cannot tell
                    // a document whose ends met legitimately from one whose
                    // ends never met at all, and without the count "it
                    // never fired" would be partly a statement about the
                    // documents rather than about the rule. eng-8gpr's
                    // point, from having been caught by their own record.
                    #[cfg(feature = "wrap-test-support")]
                    if edit.old.end > unwidened_end.0 || edit.new.end > unwidened_end.1 {
                        let old_buffer = old_inlay_snapshot.to_buffer_offset(edit.old.end);
                        let new_buffer = inlay_snapshot.to_buffer_offset(edit.new.end);
                        if old_buffer == new_buffer {
                            match (&widened_out_of.0, &widened_out_of.1) {
                                (Some(old), Some(new)) if old == new => {
                                    end_convergences += 1;
                                }
                                (old, new) => {
                                    accounting_violations.push(format!(
                                        "fold end convergence: the two ends met at buffer offset {}, and the old side was widened out of {:?} while the new side was widened out of {:?}; ends only have a right to meet at the end of one fold",
                                        old_buffer.0,
                                        old.as_ref().map(|r| r.start.0..r.end.0),
                                        new.as_ref().map(|r| r.start.0..r.end.0),
                                    ));
                                }
                            }
                        }
                    }
                    // The mirror at the end: an end travels forwards, so
                    // a byte just after it mapping to the same buffer
                    // offset is inlay text left outside.
                    #[cfg(feature = "wrap-test-support")]
                    for (side, end, unwidened, snapshot) in [
                        ("old", edit.old.end, unwidened_end.0, &old_inlay_snapshot),
                        ("new", edit.new.end, unwidened_end.1, &inlay_snapshot),
                    ] {
                        if end > unwidened && end < snapshot.len() {
                            let after = InlayOffset(MultiBufferOffset(end.0.0 + 1));
                            if snapshot.to_buffer_offset(after) == snapshot.to_buffer_offset(end) {
                                widening_violations.push(format!(
                                    "fold widening: an inlay stands on the widened {side} end at {} and was left outside the edit",
                                    end.0.0
                                ));
                            }
                        }
                    }
                    // Common-step widening can carry one side past its input
                    // snapshot when the other side's fold is longer. Map the
                    // reachable endpoint, not an overshoot beyond the document.
                    edit.old.end = edit.old.end.min(old_inlay_snapshot.len());
                    edit.new.end = edit.new.end.min(inlay_snapshot.len());
                    old_transforms.seek(&edit.old.end, Bias::Right);
                    new_transforms.seek(&edit.new.end, Bias::Right);
                    let old_end =
                        old_transforms.start().1.0 + (edit.old.end - old_transforms.start().0);
                    let new_end =
                        new_transforms.start().1.0 + (edit.new.end - new_transforms.start().0);

                    #[cfg(feature = "wrap-test-support")]
                    {
                        widening_violations.extend(widening_violation(
                            "old",
                            old_start.0..old_end.0,
                            old_extent,
                            "fold widening",
                        ));
                        widening_violations.extend(widening_violation(
                            "new",
                            new_start.0..new_end.0,
                            new_extent,
                            "fold widening",
                        ));
                    }
                    fold_edits.push(FoldEdit {
                        old: FoldOffset(old_start)..FoldOffset(old_end),
                        new: FoldOffset(new_start)..FoldOffset(new_end),
                    });
                }

                fold_edits = consolidate_fold_edits(fold_edits);

                // The new start of each edit is its old start shifted by the edits
                // before it. Compute that only after consolidation: widening can make
                // otherwise disjoint input edits overlap in fold output, where applying
                // either raw edit's full delta to the other would double-count it.
                let mut output_delta = 0isize;
                for edit in &mut fold_edits {
                    edit.new.start = FoldOffset(MultiBufferOffset(
                        (edit.old.start.0.0 as isize + output_delta)
                            .try_into()
                            .expect("consolidated fold edits cannot move a start before zero"),
                    ));
                    output_delta += edit.new_len() as isize - edit.old_len() as isize;
                }

                // Asked after consolidation, because consolidation is
                // part of what is handed over and a fault introduced
                // there would be invisible before it.
                #[cfg(feature = "wrap-test-support")]
                {
                    let net: isize = fold_edits
                        .iter()
                        .map(|edit| {
                            (edit.new.end.0 - edit.new.start.0) as isize
                                - (edit.old.end.0 - edit.old.start.0) as isize
                        })
                        .sum();
                    let accounted = old_extent as isize + net;
                    if accounted != new_extent as isize {
                        accounting_violations.push(format!(
                            "fold output accounting: the old output was {old_extent} bytes and the {} edit(s) handed over net {net}, which is {accounted}; the new output is {new_extent}",
                            fold_edits.len()
                        ));
                    }
                }
                walked_items = walked_items
                    .saturating_add(old_transforms.walked_items())
                    .saturating_add(new_transforms.walked_items());
            }

            self.snapshot.transforms = new_transforms;
            self.snapshot.inlay_snapshot = inlay_snapshot;
            self.snapshot.version += 1;
            #[cfg(feature = "wrap-test-support")]
            self.widening_violations.extend(widening_violations);
            #[cfg(feature = "wrap-test-support")]
            self.accounting_violations.extend(accounting_violations);
            #[cfg(feature = "wrap-test-support")]
            {
                self.end_convergences += end_convergences;
            }
            fold_edits
        };
        profile.walked_items(walked_items);
        if profile.is_enabled() {
            let output_start = fold_edits
                .iter()
                .map(|edit| edit.new.start.to_point(&self.snapshot).row())
                .min()
                .unwrap_or(0) as u64;
            let output_end = fold_edits
                .iter()
                .map(|edit| edit.new.end.to_point(&self.snapshot).row())
                .max()
                .unwrap_or(0) as u64;
            let output_rows = if fold_edits.is_empty() {
                0
            } else {
                output_end.saturating_sub(output_start) + 1
            };
            profile.output(fold_edits.len(), output_start, output_rows);
        }
        profile.state(
            old_rows,
            u64::from(self.snapshot.max_point().row()) + 1,
            0,
            0,
        );
        fold_edits
    }
}

#[derive(Clone)]
pub struct FoldSnapshot {
    pub inlay_snapshot: InlaySnapshot,
    transforms: SumTree<Transform>,
    folds: SumTree<Fold>,
    fold_metadata_by_id: TreeMap<FoldId, FoldMetadata>,
    pub version: usize,
}

impl Deref for FoldSnapshot {
    type Target = InlaySnapshot;

    fn deref(&self) -> &Self::Target {
        &self.inlay_snapshot
    }
}

impl FoldSnapshot {
    pub fn buffer(&self) -> &MultiBufferSnapshot {
        &self.inlay_snapshot.buffer
    }

    #[ztracing::instrument(skip_all)]
    fn fold_width(&self, fold_id: &FoldId) -> Option<Pixels> {
        self.fold_metadata_by_id.get(fold_id)?.width
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.chunks(
            FoldOffset(MultiBufferOffset(0))..self.len(),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Highlights::default(),
        )
        .map(|c| c.text)
        .collect()
    }

    #[cfg(test)]
    pub fn fold_count(&self) -> usize {
        self.folds.items(&self.inlay_snapshot.buffer).len()
    }

    #[inline(always)]
    pub fn has_folds(&self) -> bool {
        !self.folds.is_empty()
    }

    #[ztracing::instrument(skip_all)]
    pub fn text_summary_for_range(&self, range: Range<FoldPoint>) -> MBTextSummary {
        let mut summary = MBTextSummary::default();

        let mut cursor = self
            .transforms
            .cursor::<Dimensions<FoldPoint, InlayPoint>>(());
        cursor.seek(&range.start, Bias::Right);
        if let Some(transform) = cursor.item() {
            let start_in_transform = range.start.0 - cursor.start().0.0;
            let end_in_transform = cmp::min(range.end, cursor.end().0).0 - cursor.start().0.0;
            if let Some(placeholder) = transform.placeholder.as_ref() {
                summary = MBTextSummary::from(
                    &placeholder.text.as_ref()
                        [start_in_transform.column as usize..end_in_transform.column as usize],
                );
            } else {
                let inlay_start = self
                    .inlay_snapshot
                    .to_offset(InlayPoint(cursor.start().1.0 + start_in_transform));
                let inlay_end = self
                    .inlay_snapshot
                    .to_offset(InlayPoint(cursor.start().1.0 + end_in_transform));
                summary = self
                    .inlay_snapshot
                    .text_summary_for_range(inlay_start..inlay_end);
            }
        }

        if range.end > cursor.end().0 {
            cursor.next();
            summary += cursor
                .summary::<_, TransformSummary>(&range.end, Bias::Right)
                .output;
            if let Some(transform) = cursor.item() {
                let end_in_transform = range.end.0 - cursor.start().0.0;
                if let Some(placeholder) = transform.placeholder.as_ref() {
                    summary += MBTextSummary::from(
                        &placeholder.text.as_ref()[..end_in_transform.column as usize],
                    );
                } else {
                    let inlay_start = self.inlay_snapshot.to_offset(cursor.start().1);
                    let inlay_end = self
                        .inlay_snapshot
                        .to_offset(InlayPoint(cursor.start().1.0 + end_in_transform));
                    summary += self
                        .inlay_snapshot
                        .text_summary_for_range(inlay_start..inlay_end);
                }
            }
        }

        summary
    }

    #[ztracing::instrument(skip_all)]
    pub fn to_fold_point(&self, point: InlayPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &point, Bias::Right);
        if item.is_some_and(|t| t.is_fold()) {
            if bias == Bias::Left || point == start.0 {
                start.1
            } else {
                end.1
            }
        } else {
            let overshoot = point.0 - start.0.0;
            FoldPoint(cmp::min(start.1.0 + overshoot, end.1.0))
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn fold_point_cursor(&self) -> FoldPointCursor<'_> {
        let cursor = self
            .transforms
            .cursor::<Dimensions<InlayPoint, FoldPoint>>(());
        FoldPointCursor { cursor }
    }

    #[ztracing::instrument(skip_all)]
    pub fn len(&self) -> FoldOffset {
        FoldOffset(self.transforms.summary().output.len)
    }

    #[ztracing::instrument(skip_all)]
    pub fn line_len(&self, row: u32) -> u32 {
        let line_start = FoldPoint::new(row, 0).to_offset(self).0;
        let line_end = if row >= self.max_point().row() {
            self.len().0
        } else {
            FoldPoint::new(row + 1, 0).to_offset(self).0 - 1
        };
        (line_end - line_start) as u32
    }

    #[ztracing::instrument(skip_all)]
    pub fn row_infos(&self, start_row: u32) -> FoldRows<'_> {
        if start_row > self.transforms.summary().output.lines.row {
            panic!("invalid display row {}", start_row);
        }

        let fold_point = FoldPoint::new(start_row, 0);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<FoldPoint, InlayPoint>>(());
        cursor.seek(&fold_point, Bias::Left);

        let overshoot = fold_point.0 - cursor.start().0.0;
        let inlay_point = InlayPoint(cursor.start().1.0 + overshoot);
        let input_rows = self.inlay_snapshot.row_infos(inlay_point.row());

        FoldRows {
            fold_point,
            input_rows,
            cursor,
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn max_point(&self) -> FoldPoint {
        FoldPoint(self.transforms.summary().output.lines)
    }

    #[cfg(test)]
    pub fn longest_row(&self) -> u32 {
        self.transforms.summary().output.longest_row
    }

    #[ztracing::instrument(skip_all)]
    pub fn folds_in_range<T>(&self, range: Range<T>) -> impl Iterator<Item = &Fold>
    where
        T: ToOffset,
    {
        let buffer = &self.inlay_snapshot.buffer;
        let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);
        let mut folds = intersecting_folds(&self.inlay_snapshot, &self.folds, range, false);
        iter::from_fn(move || {
            let item = folds.item();
            folds.next();
            item
        })
    }

    #[ztracing::instrument(skip_all)]
    pub fn intersects_fold<T>(&self, offset: T) -> bool
    where
        T: ToOffset,
    {
        let buffer_offset = offset.to_offset(&self.inlay_snapshot.buffer);
        let inlay_offset = self.inlay_snapshot.to_inlay_offset(buffer_offset);
        let (_, _, item) = self
            .transforms
            .find::<InlayOffset, _>((), &inlay_offset, Bias::Right);
        item.is_some_and(|t| t.placeholder.is_some())
    }

    #[ztracing::instrument(skip_all)]
    pub fn is_line_folded(&self, buffer_row: MultiBufferRow) -> bool {
        let mut inlay_point = self
            .inlay_snapshot
            .to_inlay_point(Point::new(buffer_row.0, 0));
        let mut cursor = self.transforms.cursor::<InlayPoint>(());
        cursor.seek(&inlay_point, Bias::Right);
        loop {
            match cursor.item() {
                Some(transform) => {
                    let buffer_point = self.inlay_snapshot.to_buffer_point(inlay_point);
                    if buffer_point.row != buffer_row.0 {
                        return false;
                    } else if transform.is_fold() && !transform.conceals() {
                        // Concealed markup is not a folded line: the row
                        // reads whole, and there is nothing to unfold.
                        return true;
                    }
                }
                None => return false,
            }

            if cursor.end().row() == inlay_point.row() {
                cursor.next();
            } else {
                inlay_point.0 += Point::new(1, 0);
                cursor.seek(&inlay_point, Bias::Right);
            }
        }
    }

    #[ztracing::instrument(skip_all)]
    pub(crate) fn chunks<'a>(
        &'a self,
        range: Range<FoldOffset>,
        language_aware: LanguageAwareStyling,
        highlights: Highlights<'a>,
    ) -> FoldChunks<'a> {
        let mut transform_cursor = self
            .transforms
            .cursor::<Dimensions<FoldOffset, InlayOffset>>(());
        transform_cursor.seek(&range.start, Bias::Right);

        let inlay_start = {
            let overshoot = range.start - transform_cursor.start().0;
            transform_cursor.start().1 + overshoot
        };

        let transform_end = transform_cursor.end();

        let inlay_end = if transform_cursor
            .item()
            .is_none_or(|transform| transform.is_fold())
        {
            inlay_start
        } else if range.end < transform_end.0 {
            let overshoot = range.end - transform_cursor.start().0;
            transform_cursor.start().1 + overshoot
        } else {
            transform_end.1
        };

        FoldChunks {
            transform_cursor,
            inlay_chunks: self.inlay_snapshot.chunks(
                inlay_start..inlay_end,
                language_aware,
                highlights,
            ),
            inlay_chunk: None,
            inlay_offset: inlay_start,
            output_offset: range.start,
            max_output_offset: range.end,
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn chars_at(&self, start: FoldPoint) -> impl '_ + Iterator<Item = char> {
        self.chunks(
            start.to_offset(self)..self.len(),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Highlights::default(),
        )
        .flat_map(|chunk| chunk.text.chars())
    }

    #[ztracing::instrument(skip_all)]
    pub fn chunks_at(&self, start: FoldPoint) -> FoldChunks<'_> {
        self.chunks(
            start.to_offset(self)..self.len(),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Highlights::default(),
        )
    }

    #[cfg(test)]
    #[ztracing::instrument(skip_all)]
    pub fn clip_offset(&self, offset: FoldOffset, bias: Bias) -> FoldOffset {
        if offset > self.len() {
            self.len()
        } else {
            self.clip_point(offset.to_point(self), bias).to_offset(self)
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn clip_point(&self, point: FoldPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<FoldPoint, InlayPoint>, _>((), &point, Bias::Right);
        if let Some(transform) = item {
            let transform_start = start.0.0;
            if transform.placeholder.is_some() {
                if point.0 == transform_start || matches!(bias, Bias::Left) {
                    FoldPoint(transform_start)
                } else {
                    FoldPoint(end.0.0)
                }
            } else {
                let overshoot = InlayPoint(point.0 - transform_start);
                let inlay_point = start.1 + overshoot;
                let clipped_inlay_point = self.inlay_snapshot.clip_point(inlay_point, bias);
                FoldPoint(start.0.0 + (clipped_inlay_point - start.1).0)
            }
        } else {
            FoldPoint(self.transforms.summary().output.lines)
        }
    }
}

pub struct FoldPointCursor<'transforms> {
    cursor: Cursor<'transforms, 'static, Transform, Dimensions<InlayPoint, FoldPoint>>,
}

impl FoldPointCursor<'_> {
    pub(crate) fn walked_items(&self) -> u64 {
        self.cursor.walked_items()
    }

    /// Resets the cursor to the start so it can seek backward again.
    pub fn reset(&mut self) {
        self.cursor.reset();
    }

    #[ztracing::instrument(skip_all)]
    pub fn map(&mut self, point: InlayPoint, bias: Bias) -> FoldPoint {
        let cursor = &mut self.cursor;
        if cursor.did_seek() {
            cursor.seek_forward(&point, Bias::Right);
        } else {
            cursor.seek(&point, Bias::Right);
        }
        if cursor.item().is_some_and(|t| t.is_fold()) {
            if bias == Bias::Left || point == cursor.start().0 {
                cursor.start().1
            } else {
                cursor.end().1
            }
        } else {
            let overshoot = point.0 - cursor.start().0.0;
            FoldPoint(cmp::min(cursor.start().1.0 + overshoot, cursor.end().1.0))
        }
    }
}

fn push_isomorphic(transforms: &mut SumTree<Transform>, summary: MBTextSummary) {
    let mut did_merge = false;
    transforms.update_last(
        |last| {
            if !last.is_fold() {
                last.summary.input += summary;
                last.summary.output += summary;
                did_merge = true;
            }
        },
        (),
    );
    if !did_merge {
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary,
                    output: summary,
                },
                placeholder: None,
            },
            (),
        )
    }
}

/// The inlay offset of a buffer boundary, on the side of any inlay sitting
/// there that a *start* being widened backwards wants: in front of it, so
/// the inlay falls inside the widened edit.
///
/// `InlaySnapshot::to_inlay_offset` cannot answer this on its own. It
/// resolves the ambiguity at an inlay by the inlay's own bias - stepping
/// over left-biased ones and stopping in front of right-biased ones - so
/// which side you get is a property of the inlay rather than of what you
/// are doing with it. A widening has a direction of travel and the choice
/// belongs to that. An inlay left straddling the edge of an edit is a side
/// naming bytes the other side does not, which is the fault this whole
/// function was rewritten to make impossible.
fn widen_start_over_inlays(snapshot: &InlaySnapshot, buffer: MultiBufferOffset) -> InlayOffset {
    let offset = snapshot.to_inlay_offset(buffer);
    let mut start = offset;
    while start > InlayOffset(MultiBufferOffset(0)) {
        let before = InlayOffset(MultiBufferOffset(start.0.0 - 1));
        if snapshot.to_buffer_offset(before) != buffer {
            break;
        }
        start = before;
    }
    start
}

/// The same boundary on the side an *end* being widened forwards wants:
/// past any inlay sitting there, so the inlay falls inside the widened
/// edit rather than beyond its end.
fn widen_end_over_inlays(snapshot: &InlaySnapshot, buffer: MultiBufferOffset) -> InlayOffset {
    let offset = snapshot.to_inlay_offset(buffer);
    let mut end = offset;
    let limit = snapshot.len();
    while end < limit {
        let after = InlayOffset(MultiBufferOffset(end.0.0 + 1));
        if snapshot.to_buffer_offset(after) != buffer {
            break;
        }
        end = after;
    }
    end
}

/// Grow an edit's end to the boundary its cursor now stands on, and take in
/// the later edits that boundary has moved past.
///
/// The rule this keeps, and the one both faults in this function broke: the
/// cursor never passes an offset a later edit still names, and the two sides
/// of an edit always name the same bytes. An edit's end moves forward for two
/// reasons - the old tree's boundary lies past it, or the folds emitted for
/// it reach past it - and in both cases the edits behind the new end can no
/// longer be sliced to, because the cursor is already past them. They are
/// taken into this edit instead, with their lengths, so that the one edit
/// that survives describes everything the cursor walked over.
///
/// It moves the old end only, and returns the length the edits it took in
/// added or removed. The new end is the caller's, because the two callers
/// know it from different places: the one that widens to a boundary in the
/// old tree derives it from the running delta, and the one that widens
/// because the emitted folds reached past the edit derives it from how far
/// the new tree already reaches. What they must both do is move the new end
/// by what this returns.
fn absorb_edits_behind_the_cursor<I>(
    edit: &mut InlayEdit,
    cursor: &mut sum_tree::Cursor<'_, '_, Transform, InlayOffset>,
    edits: &mut iter::Peekable<I>,
    widen_to_cursor: bool,
) -> isize
where
    I: Iterator<Item = InlayEdit>,
{
    let mut absorbed = 0;
    loop {
        // Where the cursor has reached is what decides which edits can no
        // longer be sliced to. How far this edit is widened is a separate
        // question: when a fold was taken whole the edit has to name all of
        // it, and when a run of text was split the edit only has to name
        // what the edits behind the cursor named, which is the difference
        // between O(edit) and O(document).
        let reach = *cursor.start();
        if widen_to_cursor {
            edit.old.end = reach;
        }

        let Some(next_edit) = edits.peek() else {
            break;
        };
        if next_edit.old.start > reach {
            break;
        }

        let next_edit = edits.next().expect("peeked");
        absorbed += next_edit.new_len() as isize - next_edit.old_len() as isize;

        if next_edit.old.end > edit.old.end {
            edit.old.end = next_edit.old.end;
        }
        if next_edit.old.end >= reach {
            cursor.seek_forward(&edit.old.end, Bias::Right);
            cursor.next();
        }
    }

    absorbed
}

fn elided_ranges(
    inlay_snapshot: &InlaySnapshot,
    fold_range: Range<InlayOffset>,
    elision_policy: ElisionPolicy,
) -> (Option<Range<InlayOffset>>, Option<Range<InlayOffset>>) {
    if fold_range.start >= fold_range.end {
        return (None, None);
    }

    match elision_policy {
        ElisionPolicy::Visible => (None, Some(fold_range)),
        ElisionPolicy::Hidden => (Some(fold_range), None),
        ElisionPolicy::Tail { rows } => {
            if rows == 0 {
                return (Some(fold_range), None);
            }

            let start_point = inlay_snapshot.to_point(fold_range.start);
            let end_point = inlay_snapshot.to_point(fold_range.end);
            let tail_start_row = end_point
                .row()
                .saturating_sub(rows.saturating_sub(1))
                .max(start_point.row());
            // The elided head stops at the end of the row above the tail,
            // not at the tail's first column: the newline between them has
            // to survive, or the tail's first row is not a row at all — it
            // is drawn on the end of the placeholder's row, and a policy
            // that promises `rows` visible rows delivers `rows - 1` of them
            // plus a fragment.
            let tail_start = if tail_start_row == 0 {
                fold_range.start
            } else {
                inlay_snapshot
                    .to_offset(InlayPoint(Point::new(
                        tail_start_row - 1,
                        inlay_snapshot.line_len(tail_start_row - 1),
                    )))
                    .max(fold_range.start)
                    .min(fold_range.end)
            };

            if tail_start <= fold_range.start {
                (None, Some(fold_range))
            } else if tail_start >= fold_range.end {
                (Some(fold_range), None)
            } else {
                (
                    Some(fold_range.start..tail_start),
                    Some(tail_start..fold_range.end),
                )
            }
        }
    }
}

fn intersecting_folds<'a>(
    inlay_snapshot: &'a InlaySnapshot,
    folds: &'a SumTree<Fold>,
    range: Range<MultiBufferOffset>,
    inclusive: bool,
) -> FilterCursor<'a, 'a, impl 'a + FnMut(&FoldSummary) -> bool, Fold, MultiBufferOffset> {
    let buffer = &inlay_snapshot.buffer;
    let start = buffer.anchor_before(range.start.to_offset(buffer));
    let end = buffer.anchor_after(range.end.to_offset(buffer));
    let mut cursor = folds.filter::<_, MultiBufferOffset>(buffer, move |summary| {
        let start_cmp = start.cmp(&summary.max_end, buffer);
        let end_cmp = end.cmp(&summary.min_start, buffer);

        if inclusive {
            start_cmp <= Ordering::Equal && end_cmp >= Ordering::Equal
        } else {
            start_cmp == Ordering::Less && end_cmp == Ordering::Greater
        }
    });
    cursor.next();
    cursor
}

/// Edits closer together than this are merged into one. Every edit costs
/// each downstream map a pass of its own, so folding a run of markup is
/// cheaper as one edit spanning the run than as one edit per fold, and the
/// text swept up in between is re-examined either way.
const EDIT_PROXIMITY: usize = 256;

fn consolidate_inlay_edits(mut edits: Vec<InlayEdit>) -> Vec<InlayEdit> {
    edits.sort_unstable_by(|a, b| {
        a.old
            .start
            .cmp(&b.old.start)
            .then_with(|| b.old.end.cmp(&a.old.end))
    });

    let _old_alloc_ptr = edits.as_ptr();
    let mut inlay_edits = edits.into_iter();

    if let Some(mut first_edit) = inlay_edits.next() {
        // This code relies on reusing allocations from the Vec<_> - at the time of writing .flatten() prevents them.
        #[allow(clippy::filter_map_identity)]
        let mut v: Vec<_> = inlay_edits
            .scan(&mut first_edit, |prev_edit, edit| {
                if prev_edit.old.end.0.0 + EDIT_PROXIMITY >= edit.old.start.0.0 {
                    prev_edit.old.end = prev_edit.old.end.max(edit.old.end);
                    prev_edit.new.start = prev_edit.new.start.min(edit.new.start);
                    prev_edit.new.end = prev_edit.new.end.max(edit.new.end);
                    Some(None) // Skip this edit, it's merged
                } else {
                    let prev = std::mem::replace(*prev_edit, edit);
                    Some(Some(prev)) // Yield the previous edit
                }
            })
            .filter_map(|x| x)
            .collect();
        v.push(first_edit.clone());
        debug_assert_eq!(_old_alloc_ptr, v.as_ptr(), "Inlay edits were reallocated");
        v
    } else {
        vec![]
    }
}

fn consolidate_fold_edits(mut edits: Vec<FoldEdit>) -> Vec<FoldEdit> {
    edits.sort_unstable_by(|a, b| {
        a.old
            .start
            .cmp(&b.old.start)
            .then_with(|| b.old.end.cmp(&a.old.end))
    });
    let _old_alloc_ptr = edits.as_ptr();
    let mut fold_edits = edits.into_iter();

    if let Some(mut first_edit) = fold_edits.next() {
        // This code relies on reusing allocations from the Vec<_> - at the time of writing .flatten() prevents them.
        #[allow(clippy::filter_map_identity)]
        let mut v: Vec<_> = fold_edits
            .scan(&mut first_edit, |prev_edit, edit| {
                if prev_edit.old.end.0.0 + EDIT_PROXIMITY >= edit.old.start.0.0 {
                    prev_edit.old.end = prev_edit.old.end.max(edit.old.end);
                    prev_edit.new.start = prev_edit.new.start.min(edit.new.start);
                    prev_edit.new.end = prev_edit.new.end.max(edit.new.end);
                    Some(None) // Skip this edit, it's merged
                } else {
                    let prev = std::mem::replace(*prev_edit, edit);
                    Some(Some(prev)) // Yield the previous edit
                }
            })
            .filter_map(|x| x)
            .collect();
        v.push(first_edit.clone());
        v
    } else {
        vec![]
    }
}

#[derive(Clone, Debug, Default)]
struct Transform {
    summary: TransformSummary,
    placeholder: Option<TransformPlaceholder>,
}

#[derive(Clone, Debug)]
struct TransformPlaceholder {
    text: SharedString,
    chars: u128,
    renderer: ChunkRenderer,
}

impl Transform {
    fn is_fold(&self) -> bool {
        self.placeholder.is_some()
    }

    /// Whether this fold displays nothing at all in place of its text.
    fn conceals(&self) -> bool {
        self.placeholder
            .as_ref()
            .is_some_and(|placeholder| placeholder.text.is_empty())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
    output: MBTextSummary,
    input: MBTextSummary,
}

impl sum_tree::Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        self.summary.clone()
    }
}

impl sum_tree::ContextLessSummary for TransformSummary {
    fn zero() -> Self {
        Default::default()
    }

    fn add_summary(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Ord, PartialOrd, Hash)]
pub struct FoldId(pub(super) usize);

impl From<FoldId> for ElementId {
    fn from(val: FoldId) -> Self {
        val.0.into()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fold {
    pub id: FoldId,
    pub range: FoldRange,
    pub placeholder: FoldPlaceholder,
    pub elision_policy: ElisionPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldRange(pub(crate) Range<Anchor>);

impl Deref for FoldRange {
    type Target = Range<Anchor>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FoldRange {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Default for FoldRange {
    fn default() -> Self {
        Self(Anchor::Min..Anchor::Max)
    }
}

#[derive(Clone, Debug)]
struct FoldMetadata {
    range: FoldRange,
    width: Option<Pixels>,
}

impl sum_tree::Item for Fold {
    type Summary = FoldSummary;

    fn summary(&self, _cx: &MultiBufferSnapshot) -> Self::Summary {
        FoldSummary {
            start: self.range.start,
            end: self.range.end,
            min_start: self.range.start,
            max_end: self.range.end,
            count: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FoldSummary {
    start: Anchor,
    end: Anchor,
    min_start: Anchor,
    max_end: Anchor,
    count: usize,
}

impl Default for FoldSummary {
    fn default() -> Self {
        Self {
            start: Anchor::Min,
            end: Anchor::Max,
            min_start: Anchor::Max,
            max_end: Anchor::Min,
            count: 0,
        }
    }
}

impl sum_tree::Summary for FoldSummary {
    type Context<'a> = &'a MultiBufferSnapshot;

    fn zero(_cx: &MultiBufferSnapshot) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, other: &Self, buffer: Self::Context<'_>) {
        if other.min_start.cmp(&self.min_start, buffer) == Ordering::Less {
            self.min_start = other.min_start;
        }
        if other.max_end.cmp(&self.max_end, buffer) == Ordering::Greater {
            self.max_end = other.max_end;
        }

        #[cfg(debug_assertions)]
        {
            let start_comparison = self.start.cmp(&other.start, buffer);
            assert!(start_comparison <= Ordering::Equal);
            if start_comparison == Ordering::Equal {
                assert!(self.end.cmp(&other.end, buffer) >= Ordering::Equal);
            }
        }

        self.start = other.start;
        self.end = other.end;
        self.count += other.count;
    }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for FoldRange {
    fn zero(_cx: &MultiBufferSnapshot) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a FoldSummary, _: &MultiBufferSnapshot) {
        self.0.start = summary.start;
        self.0.end = summary.end;
    }
}

impl sum_tree::SeekTarget<'_, FoldSummary, FoldRange> for FoldRange {
    fn cmp(&self, other: &Self, buffer: &MultiBufferSnapshot) -> Ordering {
        AnchorRangeExt::cmp(&self.0, &other.0, buffer)
    }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for MultiBufferOffset {
    fn zero(_cx: &MultiBufferSnapshot) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a FoldSummary, _: &MultiBufferSnapshot) {
        *self += summary.count;
    }
}

#[derive(Clone)]
pub struct FoldRows<'a> {
    cursor: Cursor<'a, 'static, Transform, Dimensions<FoldPoint, InlayPoint>>,
    input_rows: InlayBufferRows<'a>,
    fold_point: FoldPoint,
}

impl FoldRows<'_> {
    #[ztracing::instrument(skip_all)]
    pub(crate) fn seek(&mut self, row: u32) {
        let fold_point = FoldPoint::new(row, 0);
        self.cursor.seek(&fold_point, Bias::Left);
        let overshoot = fold_point.0 - self.cursor.start().0.0;
        let inlay_point = InlayPoint(self.cursor.start().1.0 + overshoot);
        self.input_rows.seek(inlay_point.row());
        self.fold_point = fold_point;
    }
}

impl Iterator for FoldRows<'_> {
    type Item = RowInfo;

    #[ztracing::instrument(skip_all)]
    fn next(&mut self) -> Option<Self::Item> {
        let mut traversed_fold = false;
        while self.fold_point > self.cursor.end().0 {
            self.cursor.next();
            traversed_fold = true;
            if self.cursor.item().is_none() {
                break;
            }
        }

        if self.cursor.item().is_some() {
            if traversed_fold {
                self.input_rows.seek(self.cursor.start().1.0.row);
                self.input_rows.next();
            }
            *self.fold_point.row_mut() += 1;
            self.input_rows.next()
        } else {
            None
        }
    }
}

/// A chunk of a buffer's text, along with its syntax highlight and
/// diagnostic status.
#[derive(Clone, Debug, Default)]
pub struct Chunk<'a> {
    /// The text of the chunk.
    pub text: &'a str,
    /// The syntax highlighting style of the chunk.
    pub syntax_highlight_id: Option<HighlightId>,
    /// The highlight style that has been applied to this chunk in
    /// the editor.
    pub highlight_style: Option<HighlightStyle>,
    /// The severity of diagnostic associated with this chunk, if any.
    pub diagnostic_severity: Option<language::DiagnosticSeverity>,
    /// Whether this chunk of text is marked as unnecessary.
    pub is_unnecessary: bool,
    /// Whether this chunk of text should be underlined.
    pub underline: bool,
    /// Whether this chunk of text was originally a tab character.
    pub is_tab: bool,
    /// Whether this chunk of text was originally a tab character.
    pub is_inlay: bool,
    /// An optional recipe for how the chunk should be presented.
    pub renderer: Option<ChunkRenderer>,
    /// Bitmap of tab character locations in chunk
    pub tabs: u128,
    /// Bitmap of character locations in chunk
    pub chars: u128,
    /// Bitmap of newline locations in chunk
    pub newlines: u128,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChunkRendererId {
    Fold(FoldId),
    Inlay(InlayId),
}

/// A recipe for how the chunk should be presented.
#[derive(Clone)]
pub struct ChunkRenderer {
    /// The id of the renderer associated with this chunk.
    pub id: ChunkRendererId,
    /// Creates a custom element to represent this chunk.
    pub render: Arc<dyn Send + Sync + Fn(&mut ChunkRendererContext) -> AnyElement>,
    /// If true, the element is constrained to the shaped width of the text.
    pub constrain_width: bool,
    /// The width of the element, as measured during the last layout pass.
    ///
    /// This is None if the element has not been laid out yet.
    pub measured_width: Option<Pixels>,
}

pub struct ChunkRendererContext<'a, 'b> {
    pub window: &'a mut Window,
    pub context: &'b mut App,
    pub max_width: Pixels,
}

impl fmt::Debug for ChunkRenderer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ChunkRenderer")
            .field("constrain_width", &self.constrain_width)
            .finish()
    }
}

impl Deref for ChunkRendererContext<'_, '_> {
    type Target = App;

    fn deref(&self) -> &Self::Target {
        self.context
    }
}

impl DerefMut for ChunkRendererContext<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.context
    }
}

pub struct FoldChunks<'a> {
    transform_cursor: Cursor<'a, 'static, Transform, Dimensions<FoldOffset, InlayOffset>>,
    inlay_chunks: InlayChunks<'a>,
    inlay_chunk: Option<(InlayOffset, InlayChunk<'a>)>,
    inlay_offset: InlayOffset,
    output_offset: FoldOffset,
    max_output_offset: FoldOffset,
}

impl FoldChunks<'_> {
    #[ztracing::instrument(skip_all)]
    pub(crate) fn seek(&mut self, range: Range<FoldOffset>) {
        self.transform_cursor.seek(&range.start, Bias::Right);

        let inlay_start = {
            let overshoot = range.start - self.transform_cursor.start().0;
            self.transform_cursor.start().1 + overshoot
        };

        let transform_end = self.transform_cursor.end();

        let inlay_end = if self
            .transform_cursor
            .item()
            .is_none_or(|transform| transform.is_fold())
        {
            inlay_start
        } else if range.end < transform_end.0 {
            let overshoot = range.end - self.transform_cursor.start().0;
            self.transform_cursor.start().1 + overshoot
        } else {
            transform_end.1
        };

        self.inlay_chunks.seek(inlay_start..inlay_end);
        self.inlay_chunk = None;
        self.inlay_offset = inlay_start;
        self.output_offset = range.start;
        self.max_output_offset = range.end;
    }
}

impl<'a> Iterator for FoldChunks<'a> {
    type Item = Chunk<'a>;

    #[ztracing::instrument(skip_all)]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.output_offset >= self.max_output_offset {
                return None;
            }

            let transform = self.transform_cursor.item()?;

            // If we're in a fold, then return the fold's display text and
            // advance the transform and buffer cursors to the end of the fold.
            let Some(placeholder) = transform.placeholder.as_ref() else {
                break;
            };
            self.inlay_chunk.take();
            self.inlay_offset += InlayOffset(transform.summary.input.len);

            while self.inlay_offset >= self.transform_cursor.end().1
                && self.transform_cursor.item().is_some()
            {
                self.transform_cursor.next();
            }

            self.output_offset.0 += placeholder.text.len();
            // A concealing fold displays as nothing at all: it consumes its
            // input but contributes no chunk, so nothing downstream has to
            // handle an empty one.
            if placeholder.text.is_empty() {
                continue;
            }
            return Some(Chunk {
                text: &placeholder.text,
                chars: placeholder.chars,
                renderer: Some(placeholder.renderer.clone()),
                ..Default::default()
            });
        }

        // When we reach a non-fold region, seek the underlying text
        // chunk iterator to the next unfolded range.
        if self.inlay_offset == self.transform_cursor.start().1
            && self.inlay_chunks.offset() != self.inlay_offset
        {
            let transform_start = self.transform_cursor.start();
            let transform_end = self.transform_cursor.end();
            let inlay_end = if self.max_output_offset < transform_end.0 {
                let overshoot = self.max_output_offset - transform_start.0;
                transform_start.1 + overshoot
            } else {
                transform_end.1
            };

            self.inlay_chunks.seek(self.inlay_offset..inlay_end);
        }

        // Retrieve a chunk from the current location in the buffer.
        if self.inlay_chunk.is_none() {
            let chunk_offset = self.inlay_chunks.offset();
            self.inlay_chunk = self.inlay_chunks.next().map(|chunk| (chunk_offset, chunk));
        }

        // Otherwise, take a chunk from the buffer's text.
        if let Some((buffer_chunk_start, mut inlay_chunk)) = self.inlay_chunk.clone() {
            let chunk = &mut inlay_chunk.chunk;
            let buffer_chunk_end = buffer_chunk_start + chunk.text.len();
            let transform_end = self.transform_cursor.end().1;
            let chunk_end = buffer_chunk_end.min(transform_end);

            let bit_start = self.inlay_offset - buffer_chunk_start;
            let bit_end = chunk_end - buffer_chunk_start;
            chunk.text = &chunk.text[bit_start..bit_end];

            let bit_end = chunk_end - buffer_chunk_start;
            let mask = 1u128.unbounded_shl(bit_end as u32).wrapping_sub(1);

            chunk.tabs = (chunk.tabs >> bit_start) & mask;
            chunk.chars = (chunk.chars >> bit_start) & mask;
            chunk.newlines = (chunk.newlines >> bit_start) & mask;

            if chunk_end == transform_end {
                self.transform_cursor.next();
            } else if chunk_end == buffer_chunk_end {
                self.inlay_chunk.take();
            }

            self.inlay_offset = chunk_end;
            self.output_offset.0 += chunk.text.len();
            return Some(Chunk {
                text: chunk.text,
                tabs: chunk.tabs,
                chars: chunk.chars,
                newlines: chunk.newlines,
                syntax_highlight_id: chunk.syntax_highlight_id,
                highlight_style: chunk.highlight_style,
                diagnostic_severity: chunk.diagnostic_severity,
                is_unnecessary: chunk.is_unnecessary,
                is_tab: chunk.is_tab,
                is_inlay: chunk.is_inlay,
                underline: chunk.underline,
                renderer: inlay_chunk.renderer,
            });
        }

        None
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct FoldOffset(pub MultiBufferOffset);

impl FoldOffset {
    #[ztracing::instrument(skip_all)]
    pub fn to_point(self, snapshot: &FoldSnapshot) -> FoldPoint {
        let (start, _, item) = snapshot
            .transforms
            .find::<Dimensions<FoldOffset, TransformSummary>, _>((), &self, Bias::Right);
        let overshoot = if item.is_none_or(|t| t.is_fold()) {
            Point::new(0, (self.0 - start.0.0) as u32)
        } else {
            let inlay_offset = start.1.input.len + (self - start.0);
            let inlay_point = snapshot.inlay_snapshot.to_point(InlayOffset(inlay_offset));
            inlay_point.0 - start.1.input.lines
        };
        FoldPoint(start.1.output.lines + overshoot)
    }

    #[cfg(test)]
    #[ztracing::instrument(skip_all)]
    pub fn to_inlay_offset(self, snapshot: &FoldSnapshot) -> InlayOffset {
        let (start, _, _) = snapshot
            .transforms
            .find::<Dimensions<FoldOffset, InlayOffset>, _>((), &self, Bias::Right);
        let overshoot = self - start.0;
        InlayOffset(start.1.0 + overshoot)
    }
}

impl Add for FoldOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for FoldOffset {
    type Output = <MultiBufferOffset as Sub>::Output;

    fn sub(self, rhs: Self) -> Self::Output {
        self.0 - rhs.0
    }
}

impl<T> SubAssign<T> for FoldOffset
where
    MultiBufferOffset: SubAssign<T>,
{
    fn sub_assign(&mut self, rhs: T) {
        self.0 -= rhs;
    }
}

impl<T> Add<T> for FoldOffset
where
    MultiBufferOffset: Add<T, Output = MultiBufferOffset>,
{
    type Output = Self;

    fn add(self, rhs: T) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl AddAssign for FoldOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl<T> AddAssign<T> for FoldOffset
where
    MultiBufferOffset: AddAssign<T>,
{
    fn add_assign(&mut self, rhs: T) {
        self.0 += rhs;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for FoldOffset {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += summary.output.len;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayPoint {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += &summary.input.lines;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayOffset {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += summary.input.len;
    }
}

pub type FoldEdit = Edit<FoldOffset>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MultiBuffer, ToPoint, display_map::inlay_map::InlayMap};
    use Bias::{Left, Right};
    use collections::HashSet;
    use rand::prelude::*;
    use settings::SettingsStore;
    use std::{env, mem};
    use text::Patch;
    use util::RandomCharIter;
    use util::test::sample_text;

    #[gpui::test]
    fn test_concealed_folds(cx: &mut gpui::App) {
        init_test(cx);
        struct ConcealTag;
        let buffer = MultiBuffer::build_simple("**bold** text\n", cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let placeholder = FoldPlaceholder::concealed(TypeId::of::<ConcealTag>());
        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        let (snapshot, _) = writer.fold(vec![
            (Point::new(0, 0)..Point::new(0, 2), placeholder.clone()),
            (Point::new(0, 6)..Point::new(0, 8), placeholder.clone()),
        ]);
        assert_eq!(snapshot.text(), "bold text\n");
        // The markup is display-only: it neither folds its line nor answers
        // to an unfold.
        assert!(!snapshot.is_line_folded(MultiBufferRow(0)));

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.unfold_intersecting(Some(Point::new(0, 0)..Point::new(0, 13)), true);
        let (snapshot, _) = map.read(inlay_snapshot.clone(), vec![]);
        assert_eq!(snapshot.text(), "bold text\n");

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.remove_folds(
            Some(Point::new(0, 0)..Point::new(0, 13)),
            TypeId::of::<ConcealTag>(),
        );
        let (snapshot, _) = map.read(inlay_snapshot, vec![]);
        assert_eq!(snapshot.text(), "**bold** text\n");
    }

    #[gpui::test]
    fn test_replace_concealed_folds_preserves_unchanged_ids(cx: &mut gpui::App) {
        init_test(cx);
        struct ConcealTag;
        let type_id = TypeId::of::<ConcealTag>();
        let buffer = MultiBuffer::build_simple("**bold** text\n", cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        let (snapshot, _) = writer.replace_folds_with_type(
            type_id,
            [
                (
                    Point::new(0, 0)..Point::new(0, 2),
                    FoldPlaceholder::concealed(type_id),
                ),
                (
                    Point::new(0, 6)..Point::new(0, 8),
                    FoldPlaceholder::concealed(type_id),
                ),
            ],
        );
        let initial_ids = snapshot
            .folds_in_range(Point::new(0, 0)..Point::new(1, 0))
            .map(|fold| fold.id)
            .collect::<Vec<_>>();

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        let (snapshot, edits) = writer.replace_folds_with_type(
            type_id,
            [
                (
                    Point::new(0, 0)..Point::new(0, 2),
                    FoldPlaceholder::concealed(type_id),
                ),
                (
                    Point::new(0, 6)..Point::new(0, 8),
                    FoldPlaceholder::concealed(type_id),
                ),
            ],
        );
        assert!(edits.is_empty());
        assert_eq!(
            snapshot
                .folds_in_range(Point::new(0, 0)..Point::new(1, 0))
                .map(|fold| fold.id)
                .collect::<Vec<_>>(),
            initial_ids
        );

        let (mut writer, _, _) = map.write(inlay_snapshot, vec![]);
        let (snapshot, _) = writer.replace_folds_with_type(
            type_id,
            [
                (
                    Point::new(0, 0)..Point::new(0, 2),
                    FoldPlaceholder::concealed(type_id),
                ),
                (
                    Point::new(0, 7)..Point::new(0, 8),
                    FoldPlaceholder::concealed(type_id),
                ),
            ],
        );
        let replacement_ids = snapshot
            .folds_in_range(Point::new(0, 0)..Point::new(1, 0))
            .map(|fold| fold.id)
            .collect::<Vec<_>>();
        assert_eq!(replacement_ids[0], initial_ids[0]);
        assert_ne!(replacement_ids[1], initial_ids[1]);
    }

    #[gpui::test]
    fn test_basic_folds(cx: &mut gpui::App) {
        init_test(cx);
        let buffer = MultiBuffer::build_simple(&sample_text(5, 6, 'a'), cx);
        let subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot, vec![]);
        let (snapshot2, edits) = writer.fold(vec![
            (Point::new(0, 2)..Point::new(2, 2), FoldPlaceholder::test()),
            (Point::new(2, 4)..Point::new(4, 1), FoldPlaceholder::test()),
        ]);
        assert_eq!(snapshot2.text(), "aa⋯cc⋯eeeee");
        // Rho merges nearby fold edits so transcript markup batches make one downstream pass.
        assert_eq!(
            edits,
            &[FoldEdit {
                old: FoldOffset(MultiBufferOffset(2))..FoldOffset(MultiBufferOffset(29)),
                new: FoldOffset(MultiBufferOffset(2))..FoldOffset(MultiBufferOffset(10)),
            }]
        );

        let buffer_snapshot = buffer.update(cx, |buffer, cx| {
            buffer.edit(
                vec![
                    (Point::new(0, 0)..Point::new(0, 1), "123"),
                    (Point::new(2, 3)..Point::new(2, 3), "123"),
                ],
                None,
                cx,
            );
            buffer.snapshot(cx)
        });

        let (inlay_snapshot, inlay_edits) =
            inlay_map.sync(buffer_snapshot, subscription.consume().into_inner());
        let (snapshot3, edits) = map.read(inlay_snapshot, inlay_edits);
        assert_eq!(snapshot3.text(), "123a⋯c123c⋯eeeee");
        assert_eq!(
            edits,
            &[FoldEdit {
                old: FoldOffset(MultiBufferOffset(0))..FoldOffset(MultiBufferOffset(6)),
                new: FoldOffset(MultiBufferOffset(0))..FoldOffset(MultiBufferOffset(11)),
            }]
        );

        let buffer_snapshot = buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(2, 6)..Point::new(4, 3), "456")], None, cx);
            buffer.snapshot(cx)
        });
        let (inlay_snapshot, inlay_edits) =
            inlay_map.sync(buffer_snapshot, subscription.consume().into_inner());
        let (snapshot4, _) = map.read(inlay_snapshot.clone(), inlay_edits);
        assert_eq!(snapshot4.text(), "123a⋯c123456eee");

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.unfold_intersecting(Some(Point::new(0, 4)..Point::new(0, 4)), false);
        let (snapshot5, _) = map.read(inlay_snapshot.clone(), vec![]);
        assert_eq!(snapshot5.text(), "123a⋯c123456eee");

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.unfold_intersecting(Some(Point::new(0, 4)..Point::new(0, 4)), true);
        let (snapshot6, _) = map.read(inlay_snapshot, vec![]);
        assert_eq!(snapshot6.text(), "123aaaaa\nbbbbbb\nccc123456eee");
    }

    #[gpui::test]
    fn test_fold_spanning_excerpt_boundaries(cx: &mut gpui::App) {
        init_test(cx);
        let buffer = MultiBuffer::build_multi(
            [
                ("parent\n", vec![Point::new(0, 0)..Point::new(1, 0)]),
                ("child\n", vec![Point::new(0, 0)..Point::new(1, 0)]),
            ],
            cx,
        );
        let subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        // Composition separates excerpts by a row so their buffer boundaries stay distinct.
        assert_eq!(buffer_snapshot.text(), "parent\n\nchild\n");
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot, vec![]);
        let (snapshot, _) = writer.fold(vec![(
            Point::new(0, 6)..Point::new(3, 0),
            FoldPlaceholder::test(),
        )]);
        assert_eq!(snapshot.text(), "parent⋯");

        let buffer_snapshot = buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 0)..Point::new(1, 0), "new ")], None, cx);
            buffer.snapshot(cx)
        });
        let (inlay_snapshot, inlay_edits) =
            inlay_map.sync(buffer_snapshot, subscription.consume().into_inner());
        let (snapshot, _) = map.read(inlay_snapshot, inlay_edits);
        assert_eq!(snapshot.text(), "parent⋯");
    }

    #[gpui::test]
    fn test_tail_elision(cx: &mut gpui::App) {
        init_test(cx);
        let buffer = MultiBuffer::build_simple("one\ntwo\nthree\nfour\nfive", cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.fold(vec![(
            Point::new(0, 0)..Point::new(4, 4),
            FoldPlaceholder::test(),
            ElisionPolicy::Tail { rows: 2 },
        )]);

        let (snapshot, _) = map.read(inlay_snapshot, vec![]);
        // The placeholder keeps its own row: the newline above the tail is
        // outside the elided head, so "four" starts a row rather than
        // running on from the fold.
        assert_eq!(snapshot.text(), "⋯\nfour\nfive");
    }

    #[gpui::test]
    fn test_adjacent_folds(cx: &mut gpui::App) {
        init_test(cx);
        let buffer = MultiBuffer::build_simple("abcdefghijkl", cx);
        let subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);

        {
            let mut map = FoldMap::new(inlay_snapshot.clone()).0;

            let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
            writer.fold(vec![(
                MultiBufferOffset(5)..MultiBufferOffset(8),
                FoldPlaceholder::test(),
            )]);
            let (snapshot, _) = map.read(inlay_snapshot.clone(), vec![]);
            assert_eq!(snapshot.text(), "abcde⋯ijkl");

            // Create an fold adjacent to the start of the first fold.
            let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
            writer.fold(vec![
                (
                    MultiBufferOffset(0)..MultiBufferOffset(1),
                    FoldPlaceholder::test(),
                ),
                (
                    MultiBufferOffset(2)..MultiBufferOffset(5),
                    FoldPlaceholder::test(),
                ),
            ]);
            let (snapshot, _) = map.read(inlay_snapshot.clone(), vec![]);
            assert_eq!(snapshot.text(), "⋯b⋯ijkl");

            // Create an fold adjacent to the end of the first fold.
            let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
            writer.fold(vec![
                (
                    MultiBufferOffset(11)..MultiBufferOffset(11),
                    FoldPlaceholder::test(),
                ),
                (
                    MultiBufferOffset(8)..MultiBufferOffset(10),
                    FoldPlaceholder::test(),
                ),
            ]);
            let (snapshot, _) = map.read(inlay_snapshot.clone(), vec![]);
            assert_eq!(snapshot.text(), "⋯b⋯kl");
        }

        {
            let mut map = FoldMap::new(inlay_snapshot.clone()).0;

            // Create two adjacent folds.
            let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
            writer.fold(vec![
                (
                    MultiBufferOffset(0)..MultiBufferOffset(2),
                    FoldPlaceholder::test(),
                ),
                (
                    MultiBufferOffset(2)..MultiBufferOffset(5),
                    FoldPlaceholder::test(),
                ),
            ]);
            let (snapshot, _) = map.read(inlay_snapshot, vec![]);
            assert_eq!(snapshot.text(), "⋯fghijkl");

            // Edit within one of the folds.
            let buffer_snapshot = buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    [(MultiBufferOffset(0)..MultiBufferOffset(1), "12345")],
                    None,
                    cx,
                );
                buffer.snapshot(cx)
            });
            let (inlay_snapshot, inlay_edits) =
                inlay_map.sync(buffer_snapshot, subscription.consume().into_inner());
            let (snapshot, _) = map.read(inlay_snapshot, inlay_edits);
            assert_eq!(snapshot.text(), "12345⋯fghijkl");
        }
    }

    #[gpui::test]
    fn test_overlapping_folds(cx: &mut gpui::App) {
        let buffer = MultiBuffer::build_simple(&sample_text(5, 6, 'a'), cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;
        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.fold(vec![
            (Point::new(0, 2)..Point::new(2, 2), FoldPlaceholder::test()),
            (Point::new(0, 4)..Point::new(1, 0), FoldPlaceholder::test()),
            (Point::new(1, 2)..Point::new(3, 2), FoldPlaceholder::test()),
            (Point::new(3, 1)..Point::new(4, 1), FoldPlaceholder::test()),
        ]);
        let (snapshot, _) = map.read(inlay_snapshot, vec![]);
        assert_eq!(snapshot.text(), "aa⋯eeeee");
    }

    #[gpui::test]
    fn test_merging_folds_via_edit(cx: &mut gpui::App) {
        init_test(cx);
        let buffer = MultiBuffer::build_simple(&sample_text(5, 6, 'a'), cx);
        let subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.fold(vec![
            (Point::new(0, 2)..Point::new(2, 2), FoldPlaceholder::test()),
            (Point::new(3, 1)..Point::new(4, 1), FoldPlaceholder::test()),
        ]);
        let (snapshot, _) = map.read(inlay_snapshot, vec![]);
        assert_eq!(snapshot.text(), "aa⋯cccc\nd⋯eeeee");

        let buffer_snapshot = buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(2, 2)..Point::new(3, 1), "")], None, cx);
            buffer.snapshot(cx)
        });
        let (inlay_snapshot, inlay_edits) =
            inlay_map.sync(buffer_snapshot, subscription.consume().into_inner());
        let (snapshot, _) = map.read(inlay_snapshot, inlay_edits);
        assert_eq!(snapshot.text(), "aa⋯eeeee");
    }

    #[gpui::test]
    fn test_folds_in_range(cx: &mut gpui::App) {
        let buffer = MultiBuffer::build_simple(&sample_text(5, 6, 'a'), cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.fold(vec![
            (Point::new(0, 2)..Point::new(2, 2), FoldPlaceholder::test()),
            (Point::new(0, 4)..Point::new(1, 0), FoldPlaceholder::test()),
            (Point::new(1, 2)..Point::new(3, 2), FoldPlaceholder::test()),
            (Point::new(3, 1)..Point::new(4, 1), FoldPlaceholder::test()),
        ]);
        let (snapshot, _) = map.read(inlay_snapshot, vec![]);
        let fold_ranges = snapshot
            .folds_in_range(Point::new(1, 0)..Point::new(1, 3))
            .map(|fold| {
                fold.range.start.to_point(&buffer_snapshot)
                    ..fold.range.end.to_point(&buffer_snapshot)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fold_ranges,
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(1, 2)..Point::new(3, 2)
            ]
        );
    }

    #[gpui::test(iterations = 100)]
    fn test_random_folds(cx: &mut gpui::App, mut rng: StdRng) {
        init_test(cx);
        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);

        let len = rng.random_range(0..10);
        let text = RandomCharIter::new(&mut rng).take(len).collect::<String>();
        let buffer = if rng.random() {
            MultiBuffer::build_simple(&text, cx)
        } else {
            MultiBuffer::build_random(&mut rng, cx)
        };
        let mut buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut initial_snapshot, _) = map.read(inlay_snapshot, vec![]);
        let mut snapshot_edits = Vec::new();

        let mut next_inlay_id = 0;
        for _ in 0..operations {
            log::info!("text: {:?}", buffer_snapshot.text());
            let mut buffer_edits = Vec::new();
            let mut inlay_edits = Vec::new();
            match rng.random_range(0..=100) {
                0..=39 => {
                    snapshot_edits.extend(map.randomly_mutate(&mut rng));
                }
                40..=59 => {
                    let (_, edits) = inlay_map.randomly_mutate(&mut next_inlay_id, &mut rng);
                    inlay_edits = edits;
                }
                _ => buffer.update(cx, |buffer, cx| {
                    let subscription = buffer.subscribe();
                    let edit_count = rng.random_range(1..=5);
                    buffer.randomly_mutate(&mut rng, edit_count, cx);
                    buffer_snapshot = buffer.snapshot(cx);
                    let edits = subscription.consume().into_inner();
                    log::info!("editing {:?}", edits);
                    buffer_edits.extend(edits);
                }),
            };

            let (inlay_snapshot, new_inlay_edits) =
                inlay_map.sync(buffer_snapshot.clone(), buffer_edits);
            log::info!("inlay text {:?}", inlay_snapshot.text());

            let inlay_edits = Patch::new(inlay_edits)
                .compose(new_inlay_edits)
                .into_inner();
            let (snapshot, edits) = map.read(inlay_snapshot.clone(), inlay_edits);
            snapshot_edits.push((snapshot.clone(), edits));

            let mut expected_text: String = inlay_snapshot.text().to_string();
            for fold_range in map.merged_folds().into_iter().rev() {
                let fold_inlay_start = inlay_snapshot.to_inlay_offset(fold_range.start);
                let fold_inlay_end = inlay_snapshot.to_inlay_offset(fold_range.end);
                expected_text.replace_range(fold_inlay_start.0.0..fold_inlay_end.0.0, "⋯");
            }

            assert_eq!(snapshot.text(), expected_text);
            log::info!(
                "fold text {:?} ({} lines)",
                expected_text,
                expected_text.matches('\n').count() + 1
            );

            let mut prev_row = 0;
            let mut expected_buffer_rows = Vec::new();
            for fold_range in map.merged_folds() {
                let fold_start = inlay_snapshot
                    .to_point(inlay_snapshot.to_inlay_offset(fold_range.start))
                    .row();
                let fold_end = inlay_snapshot
                    .to_point(inlay_snapshot.to_inlay_offset(fold_range.end))
                    .row();
                expected_buffer_rows.extend(
                    inlay_snapshot
                        .row_infos(prev_row)
                        .take((1 + fold_start - prev_row) as usize),
                );
                prev_row = 1 + fold_end;
            }
            expected_buffer_rows.extend(inlay_snapshot.row_infos(prev_row));

            assert_eq!(
                expected_buffer_rows.len(),
                expected_text.matches('\n').count() + 1,
                "wrong expected buffer rows {:?}. text: {:?}",
                expected_buffer_rows,
                expected_text
            );

            for (output_row, line) in expected_text.split('\n').enumerate() {
                let line_len = snapshot.line_len(output_row as u32);
                assert_eq!(line_len, line.len() as u32);
            }

            let longest_row = snapshot.longest_row();
            let longest_char_column = expected_text
                .split('\n')
                .nth(longest_row as usize)
                .unwrap()
                .chars()
                .count();
            let mut fold_point = FoldPoint::new(0, 0);
            let mut fold_offset = FoldOffset(MultiBufferOffset(0));
            let mut char_column = 0;
            for c in expected_text.chars() {
                let inlay_point = fold_point.to_inlay_point(&snapshot);
                let inlay_offset = fold_offset.to_inlay_offset(&snapshot);
                assert_eq!(
                    snapshot.to_fold_point(inlay_point, Right),
                    fold_point,
                    "{:?} -> fold point",
                    inlay_point,
                );
                assert_eq!(
                    inlay_snapshot.to_offset(inlay_point),
                    inlay_offset,
                    "inlay_snapshot.to_offset({:?})",
                    inlay_point,
                );
                assert_eq!(
                    fold_point.to_offset(&snapshot),
                    fold_offset,
                    "fold_point.to_offset({:?})",
                    fold_point,
                );

                if c == '\n' {
                    *fold_point.row_mut() += 1;
                    *fold_point.column_mut() = 0;
                    char_column = 0;
                } else {
                    *fold_point.column_mut() += c.len_utf8() as u32;
                    char_column += 1;
                }
                fold_offset.0 += c.len_utf8();
                if char_column > longest_char_column {
                    panic!(
                        "invalid longest row {:?} (chars {}), found row {:?} (chars: {})",
                        longest_row,
                        longest_char_column,
                        fold_point.row(),
                        char_column
                    );
                }
            }

            for _ in 0..5 {
                let mut start = snapshot.clip_offset(
                    FoldOffset(rng.random_range(MultiBufferOffset(0)..=snapshot.len().0)),
                    Bias::Left,
                );
                let mut end = snapshot.clip_offset(
                    FoldOffset(rng.random_range(MultiBufferOffset(0)..=snapshot.len().0)),
                    Bias::Right,
                );
                if start > end {
                    mem::swap(&mut start, &mut end);
                }

                let text = &expected_text[start.0.0..end.0.0];
                assert_eq!(
                    snapshot
                        .chunks(
                            start..end,
                            LanguageAwareStyling {
                                tree_sitter: false,
                                diagnostics: false,
                            },
                            Highlights::default()
                        )
                        .map(|c| c.text)
                        .collect::<String>(),
                    text,
                );
            }

            let mut fold_row = 0;
            while fold_row < expected_buffer_rows.len() as u32 {
                assert_eq!(
                    snapshot.row_infos(fold_row).collect::<Vec<_>>(),
                    expected_buffer_rows[(fold_row as usize)..],
                    "wrong buffer rows starting at fold row {}",
                    fold_row,
                );
                fold_row += 1;
            }

            let folded_buffer_rows = map
                .merged_folds()
                .iter()
                .flat_map(|fold_range| {
                    let start_row = fold_range.start.to_point(&buffer_snapshot).row;
                    let end = fold_range.end.to_point(&buffer_snapshot);
                    if end.column == 0 {
                        start_row..end.row
                    } else {
                        start_row..end.row + 1
                    }
                })
                .collect::<HashSet<_>>();
            for row in 0..=buffer_snapshot.max_point().row {
                assert_eq!(
                    snapshot.is_line_folded(MultiBufferRow(row)),
                    folded_buffer_rows.contains(&row),
                    "expected buffer row {}{} to be folded",
                    row,
                    if folded_buffer_rows.contains(&row) {
                        ""
                    } else {
                        " not"
                    }
                );
            }

            for _ in 0..5 {
                let end = buffer_snapshot.clip_offset(
                    rng.random_range(MultiBufferOffset(0)..=buffer_snapshot.len()),
                    Right,
                );
                let start =
                    buffer_snapshot.clip_offset(rng.random_range(MultiBufferOffset(0)..=end), Left);
                let expected_folds = map
                    .snapshot
                    .folds
                    .items(&buffer_snapshot)
                    .into_iter()
                    .filter(|fold| {
                        let start = buffer_snapshot.anchor_before(start);
                        let end = buffer_snapshot.anchor_after(end);
                        start.cmp(&fold.range.end, &buffer_snapshot) == Ordering::Less
                            && end.cmp(&fold.range.start, &buffer_snapshot) == Ordering::Greater
                    })
                    .collect::<Vec<_>>();

                assert_eq!(
                    snapshot
                        .folds_in_range(start..end)
                        .cloned()
                        .collect::<Vec<_>>(),
                    expected_folds
                );
            }

            let text = snapshot.text();
            for _ in 0..5 {
                let start_row = rng.random_range(0..=snapshot.max_point().row());
                let start_column = rng.random_range(0..=snapshot.line_len(start_row));
                let end_row = rng.random_range(0..=snapshot.max_point().row());
                let end_column = rng.random_range(0..=snapshot.line_len(end_row));
                let mut start =
                    snapshot.clip_point(FoldPoint::new(start_row, start_column), Bias::Left);
                let mut end = snapshot.clip_point(FoldPoint::new(end_row, end_column), Bias::Right);
                if start > end {
                    mem::swap(&mut start, &mut end);
                }

                let lines = start..end;
                let bytes = start.to_offset(&snapshot)..end.to_offset(&snapshot);
                assert_eq!(
                    snapshot.text_summary_for_range(lines),
                    MBTextSummary::from(&text[bytes.start.0.0..bytes.end.0.0])
                )
            }

            let mut text = initial_snapshot.text();
            for (snapshot, edits) in snapshot_edits.drain(..) {
                let new_text = snapshot.text();
                for edit in edits {
                    let old_bytes = edit.new.start.0.0..edit.new.start.0.0 + edit.old_len();
                    let new_bytes = edit.new.start.0.0..edit.new.end.0.0;
                    text.replace_range(old_bytes, &new_text[new_bytes]);
                }

                assert_eq!(text, new_text);
                initial_snapshot = snapshot;
            }
        }
    }

    #[gpui::test]
    fn test_buffer_rows(cx: &mut gpui::App) {
        let text = sample_text(6, 6, 'a') + "\n";
        let buffer = MultiBuffer::build_simple(&text, cx);

        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let mut map = FoldMap::new(inlay_snapshot.clone()).0;

        let (mut writer, _, _) = map.write(inlay_snapshot.clone(), vec![]);
        writer.fold(vec![
            (Point::new(0, 2)..Point::new(2, 2), FoldPlaceholder::test()),
            (Point::new(3, 1)..Point::new(4, 1), FoldPlaceholder::test()),
        ]);

        let (snapshot, _) = map.read(inlay_snapshot, vec![]);
        assert_eq!(snapshot.text(), "aa⋯cccc\nd⋯eeeee\nffffff\n");
        assert_eq!(
            snapshot
                .row_infos(0)
                .map(|info| info.buffer_row)
                .collect::<Vec<_>>(),
            [Some(0), Some(3), Some(5), Some(6)]
        );
        assert_eq!(
            snapshot
                .row_infos(3)
                .map(|info| info.buffer_row)
                .collect::<Vec<_>>(),
            [Some(6)]
        );
    }

    #[gpui::test(iterations = 100)]
    fn test_random_chunk_bitmaps(cx: &mut gpui::App, mut rng: StdRng) {
        init_test(cx);

        // Generate random buffer using existing test infrastructure
        let text_len = rng.random_range(0..10000);
        let buffer = if rng.random() {
            let text = RandomCharIter::new(&mut rng)
                .take(text_len)
                .collect::<String>();
            MultiBuffer::build_simple(&text, cx)
        } else {
            MultiBuffer::build_random(&mut rng, cx)
        };
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        // Perform random mutations
        let mutation_count = rng.random_range(1..10);
        for _ in 0..mutation_count {
            fold_map.randomly_mutate(&mut rng);
        }

        let (snapshot, _) = fold_map.read(inlay_snapshot, vec![]);

        // Get all chunks and verify their bitmaps
        let chunks = snapshot.chunks(
            FoldOffset(MultiBufferOffset(0))..FoldOffset(snapshot.len().0),
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            Highlights::default(),
        );

        for chunk in chunks {
            let chunk_text = chunk.text;
            let chars_bitmap = chunk.chars;
            let tabs_bitmap = chunk.tabs;

            // Check empty chunks have empty bitmaps
            if chunk_text.is_empty() {
                assert_eq!(
                    chars_bitmap, 0,
                    "Empty chunk should have empty chars bitmap"
                );
                assert_eq!(tabs_bitmap, 0, "Empty chunk should have empty tabs bitmap");
                continue;
            }

            // Verify that chunk text doesn't exceed 128 bytes
            assert!(
                chunk_text.len() <= 128,
                "Chunk text length {} exceeds 128 bytes",
                chunk_text.len()
            );

            // Verify chars bitmap
            let char_indices = chunk_text
                .char_indices()
                .map(|(i, _)| i)
                .collect::<Vec<_>>();

            for byte_idx in 0..chunk_text.len() {
                let should_have_bit = char_indices.contains(&byte_idx);
                let has_bit = chars_bitmap & (1 << byte_idx) != 0;

                if has_bit != should_have_bit {
                    eprintln!("Chunk text bytes: {:?}", chunk_text.as_bytes());
                    eprintln!("Char indices: {:?}", char_indices);
                    eprintln!("Chars bitmap: {:#b}", chars_bitmap);
                    assert_eq!(
                        has_bit, should_have_bit,
                        "Chars bitmap mismatch at byte index {} in chunk {:?}. Expected bit: {}, Got bit: {}",
                        byte_idx, chunk_text, should_have_bit, has_bit
                    );
                }
            }

            // Verify tabs bitmap
            for (byte_idx, byte) in chunk_text.bytes().enumerate() {
                let is_tab = byte == b'\t';
                let has_bit = tabs_bitmap & (1 << byte_idx) != 0;

                assert_eq!(
                    has_bit, is_tab,
                    "Tabs bitmap mismatch at byte index {} in chunk {:?}. Byte: {:?}, Expected bit: {}, Got bit: {}",
                    byte_idx, chunk_text, byte as char, is_tab, has_bit
                );
            }
        }
    }

    fn init_test(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
    }

    impl FoldMap {
        fn merged_folds(&self) -> Vec<Range<MultiBufferOffset>> {
            let inlay_snapshot = self.snapshot.inlay_snapshot.clone();
            let buffer = &inlay_snapshot.buffer;
            let mut folds = self.snapshot.folds.items(buffer);
            // Ensure sorting doesn't change how folds get merged and displayed.
            folds.sort_by(|a, b| a.range.cmp(&b.range, buffer));
            let mut folds = folds
                .iter()
                .map(|fold| fold.range.start.to_offset(buffer)..fold.range.end.to_offset(buffer))
                .peekable();

            let mut merged_folds = Vec::new();
            while let Some(mut fold_range) = folds.next() {
                while let Some(next_range) = folds.peek() {
                    if fold_range.end >= next_range.start {
                        if next_range.end > fold_range.end {
                            fold_range.end = next_range.end;
                        }
                        folds.next();
                    } else {
                        break;
                    }
                }
                if fold_range.end > fold_range.start {
                    merged_folds.push(fold_range);
                }
            }
            merged_folds
        }

        pub fn randomly_mutate(
            &mut self,
            rng: &mut impl Rng,
        ) -> Vec<(FoldSnapshot, Vec<FoldEdit>)> {
            let mut snapshot_edits = Vec::new();
            match rng.random_range(0..=100) {
                0..=39 if !self.snapshot.folds.is_empty() => {
                    let inlay_snapshot = self.snapshot.inlay_snapshot.clone();
                    let buffer = &inlay_snapshot.buffer;
                    let mut to_unfold = Vec::new();
                    for _ in 0..rng.random_range(1..=3) {
                        let end = buffer.clip_offset(
                            rng.random_range(MultiBufferOffset(0)..=buffer.len()),
                            Right,
                        );
                        let start =
                            buffer.clip_offset(rng.random_range(MultiBufferOffset(0)..=end), Left);
                        to_unfold.push(start..end);
                    }
                    let inclusive = rng.random();
                    log::info!("unfolding {:?} (inclusive: {})", to_unfold, inclusive);
                    let (mut writer, snapshot, edits) = self.write(inlay_snapshot, vec![]);
                    snapshot_edits.push((snapshot, edits));
                    let (snapshot, edits) = writer.unfold_intersecting(to_unfold, inclusive);
                    snapshot_edits.push((snapshot, edits));
                }
                _ => {
                    let inlay_snapshot = self.snapshot.inlay_snapshot.clone();
                    let buffer = &inlay_snapshot.buffer;
                    let mut to_fold = Vec::new();
                    for _ in 0..rng.random_range(1..=2) {
                        let end = buffer.clip_offset(
                            rng.random_range(MultiBufferOffset(0)..=buffer.len()),
                            Right,
                        );
                        let start =
                            buffer.clip_offset(rng.random_range(MultiBufferOffset(0)..=end), Left);
                        to_fold.push((start..end, FoldPlaceholder::test()));
                    }
                    log::info!("folding {:?}", to_fold);
                    let (mut writer, snapshot, edits) = self.write(inlay_snapshot, vec![]);
                    snapshot_edits.push((snapshot, edits));
                    let (snapshot, edits) = writer.fold(to_fold);
                    snapshot_edits.push((snapshot, edits));
                }
            }
            snapshot_edits
        }
    }
}
