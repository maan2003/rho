//! Shared helpers for resolving transcript-buffer anchors into the editor's
//! multibuffer space and applying per-class text highlights.

use std::ops::Range;

use editor::Editor;
use gpui::{Context, Entity};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot};
use text::Anchor;

use crate::style::{Region, StyleClass};

/// Maps a buffer anchor range into the editor multibuffer, if the excerpt
/// still exists.
pub fn excerpt_range(
    snapshot: &MultiBufferSnapshot,
    range: &Range<Anchor>,
) -> Option<Range<multi_buffer::Anchor>> {
    Some(snapshot.anchor_in_excerpt(range.start)?..snapshot.anchor_in_excerpt(range.end)?)
}

/// Applies per-class highlights for `region`. An empty range list clears the
/// class's previous highlights.
pub fn apply_class_highlights<'a, V: 'static>(
    editor: &Entity<Editor>,
    multi_buffer: &Entity<MultiBuffer>,
    region: Region,
    styles: impl IntoIterator<Item = (StyleClass, &'a [Range<Anchor>])>,
    cx: &mut Context<V>,
) {
    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let updates = styles
        .into_iter()
        .map(|(class, ranges)| {
            let ranges = ranges
                .iter()
                .filter_map(|range| excerpt_range(&snapshot, range))
                .collect::<Vec<_>>();
            (class, ranges)
        })
        .collect::<Vec<_>>();
    editor.update(cx, |editor, cx| {
        for (class, ranges) in updates {
            editor.highlight_text(class.highlight_key(region), ranges, class.resolve(cx), cx);
        }
    });
}
