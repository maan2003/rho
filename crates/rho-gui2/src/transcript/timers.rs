//! Running-tool duration inlays.
//!
//! Each running tool anchors an empty span in the transcript; its duration
//! renders as a custom inlay there and is replaced only when the text
//! changes, so the once-per-second tick never edits the buffer.

use editor::{Editor, Inlay};
use gpui::{Context, Entity};
use multi_buffer::MultiBuffer;
use project::InlayId;
use text::Anchor;

use crate::render::format_running_duration;

pub struct TimerRecord {
    position: Anchor,
    started_at_ms: u64,
    inlay: Option<TimerInlay>,
}

struct TimerInlay {
    id: usize,
    text: String,
}

impl TimerRecord {
    pub fn new(position: Anchor, started_at_ms: u64) -> Self {
        Self {
            position,
            started_at_ms,
            inlay: None,
        }
    }

    pub fn inlay_id(&self) -> Option<InlayId> {
        self.inlay.as_ref().map(|inlay| InlayId::Custom(inlay.id))
    }
}

/// Reconciles duration inlays with the current time. `stale` carries ids of
/// inlays whose timer records were just removed; `next_id` allocates from
/// the editor's custom-inlay id space.
pub fn refresh_timer_inlays<'a, V: 'static>(
    timers: impl Iterator<Item = &'a mut TimerRecord>,
    now_ms: u64,
    mut stale: Vec<InlayId>,
    next_id: &mut usize,
    multi_buffer: &Entity<MultiBuffer>,
    editor: &Entity<Editor>,
    cx: &mut Context<V>,
) {
    let snapshot = multi_buffer.read(cx).snapshot(cx);
    let mut insert = Vec::new();
    for timer in timers {
        let text = format_running_duration(timer.started_at_ms, now_ms);
        if matches!(&timer.inlay, Some(inlay) if inlay.text == text) {
            continue;
        }
        stale.extend(timer.inlay.take().map(|inlay| InlayId::Custom(inlay.id)));
        if text.is_empty() {
            continue;
        }
        let Some(position) = snapshot.anchor_in_excerpt(timer.position) else {
            continue;
        };
        let id = *next_id;
        *next_id += 1;
        insert.push(Inlay::custom(id, position, text.as_str()));
        timer.inlay = Some(TimerInlay { id, text });
    }
    if stale.is_empty() && insert.is_empty() {
        return;
    }
    editor.update(cx, |editor, cx| {
        editor.splice_inlays(&stale, insert, cx);
    });
}
