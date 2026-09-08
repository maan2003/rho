//! A keyed, incremental transcript over one buffer.
//!
//! Items sit in display order, each keyed by the caller (Slack: conversation
//! plus timestamp) and each owning an anchored range in the buffer, its
//! styles, its below-line blocks and its per-line metadata. Every operation
//! (insert a run before or after a key, replace one item, remove one) edits
//! only the range it names, so anchors outside it — the cursor, the scroll
//! anchor, other items' highlights and blocks — survive untouched. Nothing
//! here knows about Slack, agents or GitHub: day rules and gap lines are
//! ordinary items under caller-chosen keys.
//!
//! Highlights are bucketed: items are grouped into runs of [`BUCKET`], each
//! bucket has its own highlight key per class, and an edit re-sends only the
//! buckets it touched. Bucket numbers come from a counter, not from the item
//! index, so prepending a page numbers the new buckets afresh and leaves
//! every existing bucket alone.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::Arc;

use editor::display_map::{
    BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock,
};
use editor::{Editor, HighlightKey};
use gpui::{App, Entity, HighlightStyle, Hsla, WeakEntity};
use language::{Buffer, Point};
use multi_buffer::MultiBuffer;
use text::{Anchor, ToOffset as _, ToPoint as _};

/// How many items share one highlight key per class.
pub const BUCKET: usize = 64;

/// A style class the caller paints with. Each class owns one highlight key
/// per bucket, so a class's ranges are re-sent a bucket at a time.
pub trait Style: Copy + Eq + Hash + 'static {
    /// A key unique to this class and bucket, and to this transcript.
    fn highlight_key(self, bucket: u32) -> HighlightKey;
    fn highlight_style(self, cx: &App) -> HighlightStyle;
    /// Classes that tint a range rather than colour its text (an unfurl's
    /// card) answer with their own key and colour. The key must not collide
    /// with any `highlight_key`.
    fn background(self, _bucket: u32, _cx: &App) -> Option<(HighlightKey, Hsla)> {
        None
    }
}

/// Something drawn under a line of an item: an image, an unfurl.
#[derive(Clone)]
pub struct BlockSpec {
    /// Which line of the item's own text the block sits under.
    pub line: u32,
    pub height: u32,
    pub render: RenderBlock,
    pub priority: usize,
}

impl BlockSpec {
    fn same(&self, other: &Self) -> bool {
        self.line == other.line
            && self.height == other.height
            && self.priority == other.priority
            && Arc::ptr_eq(&self.render, &other.render)
    }
}

/// One rendered item, with offsets relative to its own text: the caller
/// never computes a buffer offset.
pub struct Item<K, C, M> {
    pub key: K,
    /// The item's whole text. A trailing newline is supplied if missing:
    /// items are whole lines.
    pub text: String,
    pub styles: Vec<(C, Range<usize>)>,
    /// Ranges painted as a background tint, in the same class space.
    pub backgrounds: Vec<(C, Range<usize>)>,
    pub blocks: Vec<BlockSpec>,
    /// One entry per line of `text`, for whatever the surface reads back
    /// under the cursor (Slack: the thread and the file a line offers).
    pub lines: Vec<M>,
    /// The run of the item's text the gutter bar marks, if any: the same
    /// bar the agent transcript draws beside a message, which is how a
    /// surface says "this is hung off the words" without spending a column
    /// of the text on it.
    pub gutter: Option<Range<usize>>,
}

impl<K, C, M> Item<K, C, M> {
    pub fn new(key: K, text: impl Into<String>) -> Self {
        let mut text = text.into();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Self {
            key,
            text,
            styles: Vec::new(),
            backgrounds: Vec::new(),
            blocks: Vec::new(),
            lines: Vec::new(),
            gutter: None,
        }
    }

    pub fn with_styles(mut self, styles: Vec<(C, Range<usize>)>) -> Self {
        self.styles = styles;
        self
    }

    pub fn with_backgrounds(mut self, backgrounds: Vec<(C, Range<usize>)>) -> Self {
        self.backgrounds = backgrounds;
        self
    }

    pub fn with_blocks(mut self, blocks: Vec<BlockSpec>) -> Self {
        self.blocks = blocks;
        self
    }

    pub fn with_lines(mut self, lines: Vec<M>) -> Self {
        self.lines = lines;
        self
    }

    pub fn with_gutter(mut self, gutter: Range<usize>) -> Self {
        self.gutter = Some(gutter);
        self
    }

    fn normalized(mut self) -> Self {
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self
    }
}

struct Record<K, C, M> {
    key: K,
    text: String,
    range: Range<Anchor>,
    styles: Vec<(C, Range<usize>)>,
    backgrounds: Vec<(C, Range<usize>)>,
    anchored: Vec<(C, Range<Anchor>)>,
    anchored_backgrounds: Vec<(C, Range<Anchor>)>,
    blocks: Vec<BlockSpec>,
    lines: Vec<M>,
    gutter: Option<Range<usize>>,
    anchored_gutter: Option<Range<Anchor>>,
    bucket: u32,
}

/// One editor showing this transcript. Blocks live in the editor's own id
/// space and anchors resolve through its multibuffer, so both are per
/// attachment.
struct Attachment {
    editor: WeakEntity<Editor>,
    multi_buffer: Entity<MultiBuffer>,
    blocks: Vec<(usize, CustomBlockId)>,
}

/// The ranges each class paints in one bucket.
type Classes<C> = HashMap<C, Vec<Range<Anchor>>>;

/// The gutter bar's owner: one marker type, one bar, so two surfaces in
/// one editor cannot overwrite each other's. A transcript that never marks
/// a gutter takes the default and never touches one.
pub struct NoGutter;

pub struct Transcript<K, C, M, G = NoGutter> {
    buffer: Entity<Buffer>,
    items: Vec<Record<K, C, M>>,
    index: HashMap<K, usize>,
    attachments: Vec<Attachment>,
    /// Classes each bucket currently paints, so a bucket that loses a class
    /// clears it instead of leaving stale ranges behind.
    painted: HashMap<u32, HashSet<C>>,
    /// Which items are in each bucket. Repainting a bucket needs its
    /// members and nothing else, and a bucket holds at most [`BUCKET`] of
    /// them, so this is what keeps a paint off the length of the
    /// transcript.
    members: HashMap<u32, HashSet<K>>,
    next_bucket: u32,
    /// Buckets re-sent by the last operation. Diagnostics, and what the
    /// tests assert on.
    last_painted: Vec<u32>,
    /// What colour the gutter bar is, from the host. Without one no item's
    /// gutter is drawn, which is the state of every surface that does not
    /// ask for one.
    gutter: Option<fn(&App) -> Hsla>,
    marker: PhantomData<G>,
}

impl<K, C, M, G> Transcript<K, C, M, G>
where
    K: Clone + Eq + Hash,
    C: Style,
    M: Clone + PartialEq,
    G: 'static,
{
    pub fn new(buffer: Entity<Buffer>) -> Self {
        Self {
            buffer,
            items: Vec::new(),
            index: HashMap::new(),
            attachments: Vec::new(),
            painted: HashMap::new(),
            members: HashMap::new(),
            next_bucket: 0,
            last_painted: Vec::new(),
            gutter: None,
            marker: PhantomData,
        }
    }

    /// Draws the gutter bar in the host's colour. Until this is called an
    /// item's `gutter` range is remembered and not drawn, so a host with no
    /// bar of its own costs nothing.
    pub fn set_gutter(&mut self, colour: fn(&App) -> Hsla) {
        self.gutter = Some(colour);
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    pub fn first_key(&self) -> Option<&K> {
        self.items.first().map(|item| &item.key)
    }

    pub fn last_key(&self) -> Option<&K> {
        self.items.last().map(|item| &item.key)
    }

    /// Double-ended, so a caller looking for the last key of a kind walks
    /// back from the end rather than over everything drawn.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> {
        self.items.iter().map(|item| &item.key)
    }

    /// Buckets whose highlights the last operation re-sent.
    pub fn last_painted_buckets(&self) -> &[u32] {
        &self.last_painted
    }

    /// The key just before this one in display order, which is how a
    /// caller finds the day rule that heads a message.
    pub fn key_before(&self, key: &K) -> Option<&K> {
        let index = *self.index.get(key)?;
        self.items.get(index.checked_sub(1)?).map(|item| &item.key)
    }

    pub fn key_after(&self, key: &K) -> Option<&K> {
        let index = *self.index.get(key)?;
        self.items.get(index + 1).map(|item| &item.key)
    }

    pub fn range_of(&self, key: &K) -> Option<Range<Anchor>> {
        let index = *self.index.get(key)?;
        Some(self.items[index].range.clone())
    }

    /// The key of the item covering `offset`, by binary search over the
    /// items' anchors.
    pub fn key_at(&self, offset: usize, cx: &App) -> Option<&K> {
        let snapshot = self.buffer.read(cx).snapshot();
        let found = self
            .items
            .binary_search_by(|item| {
                let start = item.range.start.to_offset(&snapshot);
                let end = item.range.end.to_offset(&snapshot);
                if offset < start {
                    std::cmp::Ordering::Greater
                } else if offset > end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        Some(&self.items[found].key)
    }

    /// The metadata the caller attached to buffer row `row`.
    /// The key of the item covering a row, for a caller that knows where
    /// the cursor is rather than what it is on.
    pub fn key_at_row(&self, row: u32, cx: &App) -> Option<&K> {
        let snapshot = self.buffer.read(cx).snapshot();
        let offset = snapshot.point_to_offset(text::Point::new(row, 0));
        self.key_at(offset, cx)
    }

    pub fn line_meta(&self, row: u32, cx: &App) -> Option<&M> {
        let snapshot = self.buffer.read(cx).snapshot();
        let found = self
            .items
            .binary_search_by(|item| {
                let start = item.range.start.to_point(&snapshot).row;
                let end = item.range.end.to_point(&snapshot).row;
                if row < start {
                    std::cmp::Ordering::Greater
                } else if row > end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        let item = &self.items[found];
        let start = item.range.start.to_point(&snapshot).row;
        item.lines.get(row.saturating_sub(start) as usize)
    }

    /// Inserts a run before `before`, or at the end when it is `None`.
    pub fn insert_before(&mut self, before: Option<&K>, items: Vec<Item<K, C, M>>, cx: &mut App) {
        let at = match before {
            Some(key) => match self.index.get(key) {
                Some(index) => *index,
                None => return,
            },
            None => self.items.len(),
        };
        self.splice(at..at, items, cx);
    }

    /// Inserts a run after `after`, or at the start when it is `None`.
    pub fn insert_after(&mut self, after: Option<&K>, items: Vec<Item<K, C, M>>, cx: &mut App) {
        let at = match after {
            Some(key) => match self.index.get(key) {
                Some(index) => *index + 1,
                None => return,
            },
            None => 0,
        };
        self.splice(at..at, items, cx);
    }

    /// Replaces one item. An item that renders identically costs nothing:
    /// no buffer edit, no highlights re-sent. Reactions and reply counts
    /// arrive often enough that this is the common case.
    pub fn replace(&mut self, key: &K, item: Item<K, C, M>, cx: &mut App) -> bool {
        let Some(&at) = self.index.get(key) else {
            return false;
        };
        let item = item.normalized();
        if self.items[at].same_as(&item) {
            self.last_painted.clear();
            return true;
        }
        self.splice(at..at + 1, vec![item], cx);
        true
    }

    pub fn remove(&mut self, key: &K, cx: &mut App) -> bool {
        let Some(&at) = self.index.get(key) else {
            return false;
        };
        self.splice(at..at + 1, Vec::new(), cx);
        true
    }

    pub fn clear(&mut self, cx: &mut App) {
        let all = 0..self.items.len();
        self.splice(all, Vec::new(), cx);
    }

    /// The one place the buffer is edited: replaces the items in `range`
    /// with `items` in a single edit covering exactly their text.
    fn splice(&mut self, range: Range<usize>, items: Vec<Item<K, C, M>>, cx: &mut App) {
        let items = items.into_iter().map(Item::normalized).collect::<Vec<_>>();
        let snapshot = self.buffer.read(cx).snapshot();
        let start = match self.items.get(range.start) {
            Some(item) => item.range.start.to_offset(&snapshot),
            None => snapshot.len(),
        };
        let end = match range
            .end
            .checked_sub(1)
            .and_then(|last| self.items.get(last))
        {
            Some(item) if !range.is_empty() => item.range.end.to_offset(&snapshot),
            _ => start,
        };
        drop(snapshot);

        let text = items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<String>();
        if !(text.is_empty() && start == end) {
            self.buffer.update(cx, |buffer, cx| {
                buffer.edit([(start..end, text)], None, cx);
            });
        }

        let buckets = self.buckets_for(&range, items.len());
        // Taken before the records go: the bar is drawn against the editor,
        // so what leaves has to be named to be unmarked.
        let dropped = self.items[range.clone()]
            .iter()
            .filter_map(|item| item.anchored_gutter.clone())
            .collect::<Vec<_>>();
        let gone = self
            .items
            .splice(range.clone(), Vec::new())
            .collect::<Vec<_>>();
        debug_assert_eq!(gone.len(), range.len());
        for item in &gone {
            if let Some(members) = self.members.get_mut(&item.bucket) {
                members.remove(&item.key);
            }
        }

        let snapshot = self.buffer.read(cx).snapshot();
        let mut offset = start;
        let mut fresh = Vec::with_capacity(items.len());
        for (item, bucket) in items.into_iter().zip(buckets.iter().copied()) {
            let item_start = offset;
            offset += item.text.len();
            // Right bias on the start, left on the end, so a neighbouring
            // insert never bleeds into a style.
            let anchor = |spans: &[(C, Range<usize>)]| {
                spans
                    .iter()
                    .map(|(class, span)| {
                        let start = (item_start + span.start).min(offset);
                        let end = (item_start + span.end).min(offset);
                        (
                            *class,
                            snapshot.anchor_after(start)..snapshot.anchor_before(end),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let anchored = anchor(&item.styles);
            let anchored_backgrounds = anchor(&item.backgrounds);
            let anchored_gutter = item.gutter.clone().map(|span| {
                let start = (item_start + span.start).min(offset);
                let end = (item_start + span.end).min(offset);
                snapshot.anchor_after(start)..snapshot.anchor_before(end)
            });
            fresh.push(Record {
                key: item.key,
                text: item.text,
                range: snapshot.anchor_after(item_start)..snapshot.anchor_before(offset),
                styles: item.styles,
                backgrounds: item.backgrounds,
                anchored,
                anchored_backgrounds,
                blocks: item.blocks,
                lines: item.lines,
                gutter: item.gutter,
                anchored_gutter,
                bucket,
            });
        }
        drop(snapshot);
        let inserted = fresh.len();
        let at = range.start;
        for item in &fresh {
            self.members
                .entry(item.bucket)
                .or_default()
                .insert(item.key.clone());
        }
        self.items.splice(at..at, fresh);
        self.reindex(at);
        self.mark_gutter(dropped, at..at + inserted, cx);

        let mut touched = buckets.into_iter().collect::<HashSet<_>>();
        // Removing an item leaves its bucket short: repaint it too, so the
        // ranges the buffer no longer has stop being sent.
        for item in self.items.get(at..at + inserted).into_iter().flatten() {
            touched.insert(item.bucket);
        }
        if !range.is_empty() {
            for neighbour in [at.checked_sub(1), Some(at + inserted)]
                .into_iter()
                .flatten()
            {
                if let Some(item) = self.items.get(neighbour) {
                    touched.insert(item.bucket);
                }
            }
        }
        let mut touched = touched.into_iter().collect::<Vec<_>>();
        touched.sort_unstable();
        self.last_painted = touched.clone();
        self.paint(&touched, cx);
        self.place_blocks(cx);
    }

    /// Bucket numbers for a run. Appending extends the last bucket while it
    /// has room; anything else takes fresh numbers, so no existing item ever
    /// changes bucket and no untouched bucket is re-sent.
    fn buckets_for(&mut self, range: &Range<usize>, count: usize) -> Vec<u32> {
        if count == 0 {
            return Vec::new();
        }
        if range.len() == 1 && count == 1 {
            // A replacement keeps the item's own bucket.
            return vec![self.items[range.start].bucket];
        }
        let mut buckets = Vec::with_capacity(count);
        let appending = range.start == self.items.len();
        let last = appending.then(|| self.items.last()).flatten();
        let (mut current, mut room) = match last {
            Some(last) => (
                last.bucket,
                BUCKET.saturating_sub(self.bucket_len(last.bucket)),
            ),
            None => {
                let bucket = self.next_bucket;
                self.next_bucket += 1;
                (bucket, BUCKET)
            }
        };
        for _ in 0..count {
            if room == 0 {
                current = self.next_bucket;
                self.next_bucket += 1;
                room = BUCKET;
            }
            room -= 1;
            buckets.push(current);
        }
        buckets
    }

    fn bucket_len(&self, bucket: u32) -> usize {
        self.members.get(&bucket).map_or(0, HashSet::len)
    }

    fn reindex(&mut self, from: usize) {
        if from == 0 {
            self.index.clear();
        } else {
            self.index.retain(|_, index| *index < from);
        }
        for (index, item) in self.items.iter().enumerate().skip(from) {
            self.index.insert(item.key.clone(), index);
        }
    }

    /// Re-sends the highlights of the given buckets, and only those.
    fn paint(&mut self, buckets: &[u32], cx: &mut App) {
        if buckets.is_empty() || self.attachments.is_empty() {
            return;
        }
        for bucket in buckets {
            let (mut text, mut background): (Classes<C>, Classes<C>) = Default::default();
            for index in self.rows_in(*bucket) {
                let item = &self.items[index];
                for (class, range) in &item.anchored {
                    text.entry(*class).or_default().push(range.clone());
                }
                for (class, range) in &item.anchored_backgrounds {
                    background.entry(*class).or_default().push(range.clone());
                }
            }
            // A class the bucket has lost still has ranges in the editor, so
            // it is re-sent empty rather than left behind.
            let present = text
                .keys()
                .chain(background.keys())
                .copied()
                .collect::<HashSet<_>>();
            let stale = self
                .painted
                .get(bucket)
                .map(|painted| painted.difference(&present).copied().collect::<Vec<_>>())
                .unwrap_or_default();
            self.painted.insert(*bucket, present);
            let mut text = text.into_iter().collect::<Vec<_>>();
            let mut background = background.into_iter().collect::<Vec<_>>();
            for class in stale {
                text.push((class, Vec::new()));
                background.push((class, Vec::new()));
            }
            self.apply(*bucket, text, background, cx);
        }
    }

    /// Unmarks what left and marks what arrived, a range at a time. The
    /// editor keeps one sorted list per marker type, so this costs what it
    /// touched and never re-sends a bar that did not move.
    fn mark_gutter(&mut self, dropped: Vec<Range<Anchor>>, inserted: Range<usize>, cx: &mut App) {
        let Some(colour) = self.gutter else {
            return;
        };
        let added = self
            .items
            .get(inserted)
            .into_iter()
            .flatten()
            .filter_map(|item| item.anchored_gutter.clone())
            .collect::<Vec<_>>();
        if dropped.is_empty() && added.is_empty() {
            return;
        }
        self.attachments.retain(|attachment| {
            let Some(editor) = attachment.editor.upgrade() else {
                return false;
            };
            let snapshot = attachment.multi_buffer.read(cx).snapshot(cx);
            let resolve = |range: &Range<Anchor>| {
                Some(
                    snapshot.anchor_in_excerpt(range.start)?
                        ..snapshot.anchor_in_excerpt(range.end)?,
                )
            };
            let dropped = dropped.iter().filter_map(resolve).collect::<Vec<_>>();
            let added = added.iter().filter_map(resolve).collect::<Vec<_>>();
            editor.update(cx, |editor, cx| {
                if !dropped.is_empty() {
                    editor.remove_gutter_highlights::<G>(dropped, cx);
                }
                for range in added {
                    editor.insert_gutter_highlight::<G>(range, colour, cx);
                }
            });
            true
        });
    }

    /// Where a bucket's items are, in display order. The order matters:
    /// the editor is given a bucket's ranges as one list, and a list out of
    /// order is a list it has to sort.
    fn rows_in(&self, bucket: u32) -> Vec<usize> {
        let mut rows = self
            .members
            .get(&bucket)
            .into_iter()
            .flatten()
            .filter_map(|key| self.index.get(key).copied())
            .collect::<Vec<_>>();
        rows.sort_unstable();
        rows
    }

    fn apply(
        &mut self,
        bucket: u32,
        text: Vec<(C, Vec<Range<Anchor>>)>,
        background: Vec<(C, Vec<Range<Anchor>>)>,
        cx: &mut App,
    ) {
        self.attachments.retain(|attachment| {
            let Some(editor) = attachment.editor.upgrade() else {
                return false;
            };
            let snapshot = attachment.multi_buffer.read(cx).snapshot(cx);
            let resolve = |updates: &Vec<(C, Vec<Range<Anchor>>)>| {
                updates
                    .iter()
                    .map(|(class, ranges)| {
                        let ranges = ranges
                            .iter()
                            .filter_map(|range| {
                                Some(
                                    snapshot.anchor_in_excerpt(range.start)?
                                        ..snapshot.anchor_in_excerpt(range.end)?,
                                )
                            })
                            .collect::<Vec<_>>();
                        (*class, ranges)
                    })
                    .collect::<Vec<_>>()
            };
            let text = resolve(&text);
            let background = resolve(&background);
            editor.update(cx, |editor, cx| {
                for (class, ranges) in text {
                    let style = class.highlight_style(cx);
                    editor.highlight_text(class.highlight_key(bucket), ranges, style, cx);
                }
                for (class, ranges) in background {
                    let Some((key, color)) = class.background(bucket, cx) else {
                        continue;
                    };
                    editor.highlight_background(key, &ranges, move |_, _| color, cx);
                }
            });
            true
        });
    }

    /// Blocks are reconciled per attachment against the items that own them.
    /// Only items whose specs changed lose and regain their block ids.
    fn place_blocks(&mut self, cx: &mut App) {
        let wanted = self
            .items
            .iter()
            .enumerate()
            .flat_map(|(index, item)| item.blocks.iter().map(move |block| (index, block)))
            .collect::<Vec<_>>();
        let snapshot = self.buffer.read(cx).snapshot();
        let placements = wanted
            .iter()
            .map(|(index, block)| {
                let row = self.items[*index].range.start.to_point(&snapshot).row + block.line;
                let point = Point::new(row.min(snapshot.max_point().row), 0);
                (*index, (*block).clone(), snapshot.anchor_after(point))
            })
            .collect::<Vec<_>>();
        drop(snapshot);

        let mut attachments = std::mem::take(&mut self.attachments);
        attachments.retain_mut(|attachment| {
            let Some(editor) = attachment.editor.upgrade() else {
                return false;
            };
            let previous = std::mem::take(&mut attachment.blocks);
            if !previous.is_empty() {
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(
                        previous.into_iter().map(|(_, id)| id).collect(),
                        None,
                        cx,
                    );
                });
            }
            let snapshot = attachment.multi_buffer.read(cx).snapshot(cx);
            let mut owners = Vec::new();
            let properties = placements
                .iter()
                .filter_map(|(index, block, anchor)| {
                    let anchor = snapshot.anchor_in_excerpt(*anchor)?;
                    owners.push(*index);
                    Some(BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(block.height),
                        style: BlockStyle::Fixed,
                        render: block.render.clone(),
                        priority: block.priority,
                    })
                })
                .collect::<Vec<_>>();
            if !properties.is_empty() {
                let ids =
                    editor.update(cx, |editor, cx| editor.insert_blocks(properties, None, cx));
                attachment.blocks = owners.into_iter().zip(ids).collect();
            }
            true
        });
        self.attachments = attachments;
    }

    /// Attaches an editor over a multibuffer holding this transcript's
    /// buffer, bringing it up to date. Dropped editors prune themselves.
    pub fn attach(&mut self, editor: &Entity<Editor>, cx: &mut App) {
        let multi_buffer = editor.read(cx).buffer().clone();
        self.attachments.push(Attachment {
            editor: editor.downgrade(),
            multi_buffer,
            blocks: Vec::new(),
        });
        let buckets = self
            .items
            .iter()
            .map(|item| item.bucket)
            .collect::<HashSet<_>>();
        let mut buckets = buckets.into_iter().collect::<Vec<_>>();
        buckets.sort_unstable();
        self.painted.clear();
        self.paint(&buckets, cx);
        self.mark_gutter(Vec::new(), 0..self.items.len(), cx);
        self.place_blocks(cx);
    }
}

impl<K, C, M> Record<K, C, M>
where
    C: PartialEq,
    M: PartialEq,
{
    fn same_as(&self, item: &Item<K, C, M>) -> bool {
        self.text == item.text
            && self.styles == item.styles
            && self.backgrounds == item.backgrounds
            && self.lines == item.lines
            && self.gutter == item.gutter
            && self.blocks.len() == item.blocks.len()
            && self
                .blocks
                .iter()
                .zip(&item.blocks)
                .all(|(mine, theirs)| mine.same(theirs))
    }
}

#[cfg(test)]
mod tests;
