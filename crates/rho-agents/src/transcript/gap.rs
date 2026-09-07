//! The row that says the middle of a transcript is still composing.
//!
//! A reader who asks for the top of a long transcript is given the top and
//! not made to wait for everything under it, so for a while the buffer holds
//! two runs with a gap between them. The gap is a fact about the buffer, the
//! same kind of fact as a folded turn, so it is drawn in the buffer: one row
//! where the gap is, saying how much of it is still on its way, gone when it
//! closes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use collections::HashSet;
use editor::Editor;
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity};
use multi_buffer::MultiBuffer;
use text::Anchor;
use ui::{Icon, IconName, IconSize, div};

/// Keeps the marker where the gap is.
///
/// The marker sits above the tail's first block, which does not move while
/// the head grows towards it, so a fill step that serves a reader at the top
/// rewrites no block at all. It is written again only when the block it sits
/// above changes, and the count it draws is read at paint time, so a closing
/// gap counts down without touching the buffer.
pub(super) fn reconcile_marker<V: 'static>(
    marker: &mut Option<(CustomBlockId, usize)>,
    multi_buffer: &Entity<MultiBuffer>,
    gap_at: Option<(Anchor, usize)>,
    remaining: &Arc<AtomicUsize>,
    editor: &Entity<Editor>,
    cx: &mut Context<V>,
) {
    if marker.map(|(_, block)| block) == gap_at.map(|(_, block)| block) {
        return;
    }
    if let Some((id, _)) = marker.take() {
        editor.update(cx, |editor, cx| {
            editor.remove_blocks(HashSet::from_iter([id]), None, cx);
        });
    }
    let Some((anchor, block)) = gap_at else {
        return;
    };
    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let Some(anchor) = snapshot.anchor_in_excerpt(anchor) else {
        return;
    };
    let remaining = remaining.clone();
    let ids = editor.update(cx, |editor, cx| {
        editor.insert_blocks(
            [BlockProperties {
                placement: BlockPlacement::Above(anchor),
                // A starting height the editor resizes to what the element
                // draws: a block with no height is never measured.
                height: Some(1),
                style: BlockStyle::Fixed,
                render: Arc::new(move |cx| render_marker(remaining.load(Ordering::Relaxed), cx)),
                priority: 0,
            }],
            None,
            cx,
        )
    });
    *marker = ids.into_iter().next().map(|id| (id, block));
}

/// The row itself: the chevron a folded turn uses, because it says the same
/// thing — there is more of the transcript here than the screen is showing.
fn render_marker(remaining: usize, cx: &mut BlockContext) -> AnyElement {
    let color = rho_window::style::hint_color(cx);
    div()
        .flex()
        .items_center()
        .gap_1()
        .pr_1()
        .text_color(color)
        .child(
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(color.into()),
        )
        .child(match remaining {
            1 => "1 block still composing".to_owned(),
            count => format!("{count} blocks still composing"),
        })
        .into_any_element()
}
