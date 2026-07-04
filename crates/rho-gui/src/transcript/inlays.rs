//! Non-buffer transcript text: running-tool durations and queue labels.
//!
//! Each carrier block anchors an empty span in the transcript; the content
//! renders as a custom inlay there and is replaced only when its text
//! changes, so the once-per-second duration tick never edits the buffer and
//! static labels are inserted exactly once.

use editor::{Editor, Inlay};
use gpui::{Context, Entity};
use multi_buffer::MultiBuffer;
use project::InlayId;
use text::Anchor;

use crate::render::{InlayContent, format_running_duration};

pub struct InlayRecord {
    position: Anchor,
    content: InlayContent,
    inlay: Option<PlacedInlay>,
}

struct PlacedInlay {
    id: usize,
    text: String,
}

impl InlayRecord {
    pub fn new(position: Anchor, content: InlayContent) -> Self {
        Self {
            position,
            content,
            inlay: None,
        }
    }

    pub fn inlay_id(&self) -> Option<InlayId> {
        self.inlay.as_ref().map(|inlay| InlayId::Custom(inlay.id))
    }

    pub fn ticks(&self) -> bool {
        matches!(self.content, InlayContent::RunningDuration { .. })
    }

    fn text(&self, now_ms: u64) -> String {
        match self.content {
            InlayContent::RunningDuration { started_at_ms } => {
                format_running_duration(started_at_ms, now_ms)
            }
            InlayContent::Label(label) => label.to_owned(),
        }
    }
}

/// Reconciles inlays with the current time. `stale` carries ids of inlays
/// whose records were just removed; `next_id` allocates from the editor's
/// custom-inlay id space.
pub fn refresh_inlays<'a, V: 'static>(
    records: impl Iterator<Item = &'a mut InlayRecord>,
    now_ms: u64,
    mut stale: Vec<InlayId>,
    next_id: &mut usize,
    multi_buffer: &Entity<MultiBuffer>,
    editor: &Entity<Editor>,
    cx: &mut Context<V>,
) {
    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let mut insert = Vec::new();
    for record in records {
        let text = record.text(now_ms);
        if matches!(&record.inlay, Some(inlay) if inlay.text == text) {
            continue;
        }
        stale.extend(record.inlay.take().map(|inlay| InlayId::Custom(inlay.id)));
        if text.is_empty() {
            continue;
        }
        let Some(position) = snapshot.anchor_in_excerpt(record.position) else {
            continue;
        };
        let id = *next_id;
        *next_id += 1;
        insert.push(Inlay::custom(id, position, text.as_str()));
        record.inlay = Some(PlacedInlay { id, text });
    }
    if stale.is_empty() && insert.is_empty() {
        return;
    }
    editor.update(cx, |editor, cx| {
        editor.splice_inlays(&stale, insert, cx);
    });
}
