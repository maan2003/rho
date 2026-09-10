//! Non-buffer transcript text: running-tool durations and queue labels.
//!
//! Each carrier block anchors an empty span in the transcript; the content
//! renders as a custom inlay there. The model only records position and
//! content; which inlays an editor actually shows is per-attachment state,
//! reconciled against the desired list so the once-per-second duration tick
//! never edits the buffer and unchanged inlays are never replaced.

use std::collections::HashSet;

use editor::{Editor, Inlay};
use gpui::{Context, Entity};
use language::InlayId;
use multi_buffer::MultiBuffer;
use text::{Anchor, BufferId};

use crate::render::{InlayContent, format_running_duration};

pub struct InlayRecord {
    position: Anchor,
    content: InlayContent,
}

/// One inlay an editor currently shows; per-attachment because inlay ids
/// live in the editor's id space.
pub struct PlacedInlay {
    id: usize,
    position: Anchor,
    text: String,
}

impl InlayRecord {
    pub fn new(position: Anchor, content: InlayContent) -> Self {
        Self { position, content }
    }

    pub fn text(position: Anchor, text: String) -> Self {
        Self {
            position,
            content: InlayContent::Text(text),
        }
    }

    pub fn ticks(&self) -> bool {
        matches!(&self.content, InlayContent::RunningDuration { .. })
    }

    pub fn desired(&self, now_ms: u64) -> Option<(Anchor, String)> {
        let text = match &self.content {
            InlayContent::RunningDuration { started_at_ms } => {
                format_running_duration(*started_at_ms, now_ms)
            }
            InlayContent::Label(label) => (*label).to_owned(),
            InlayContent::Text(text) => text.clone(),
        };
        (!text.is_empty()).then_some((self.position, text))
    }
}

/// Reconciles one editor's placed inlays with the desired list: stale ones
/// (position or text no longer wanted) are removed, missing ones inserted.
/// `next_id` allocates from the model's shared counter — ids only need to
/// be unique within each editor, so one counter serves all attachments.
///
/// `scope`, when given, names the buffers the desired list covers: inlays
/// placed in any other buffer are left exactly as they are. That is what
/// lets a caller that changed one chunk pass only that chunk's inlays
/// without the rest of the document's being read as unwanted.
pub fn reconcile_inlays<V: 'static>(
    desired: &[(Anchor, String)],
    placed: &mut Vec<PlacedInlay>,
    scope: Option<&HashSet<BufferId>>,
    next_id: &mut usize,
    multi_buffer: &Entity<MultiBuffer>,
    editor: &Entity<Editor>,
    cx: &mut Context<V>,
) {
    let mut stale = Vec::new();
    let mut kept = Vec::new();
    for inlay in placed.drain(..) {
        if scope.is_some_and(|scope| !scope.contains(&inlay.position.buffer_id)) {
            kept.push(inlay);
            continue;
        }
        let wanted = desired
            .iter()
            .any(|(position, text)| *position == inlay.position && *text == inlay.text);
        if wanted {
            kept.push(inlay);
        } else {
            stale.push(InlayId::Custom(inlay.id));
        }
    }

    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let mut insert = Vec::new();
    for (position, text) in desired {
        if kept
            .iter()
            .any(|inlay| inlay.position == *position && inlay.text == *text)
        {
            continue;
        }
        let Some(anchor) = snapshot.anchor_in_excerpt(*position) else {
            continue;
        };
        let id = *next_id;
        *next_id += 1;
        insert.push(Inlay::custom(id, anchor, text.as_str()));
        kept.push(PlacedInlay {
            id,
            position: *position,
            text: text.clone(),
        });
    }
    *placed = kept;

    if stale.is_empty() && insert.is_empty() {
        return;
    }
    editor.update(cx, |editor, cx| {
        editor.splice_inlays(&stale, insert, cx);
    });
}
