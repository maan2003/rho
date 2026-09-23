//! The message log: what rho has said, in one capped buffer with one
//! read-only editor over it.
//!
//! The log is a surface of its own, so it owns its own state — the entries,
//! the buffer they are rendered into, the editor that shows it, and the
//! highlight bookkeeping that keeps a class's ranges together. The host
//! records a line and shows the surface; nothing else about it is the
//! host's business.
//!
//! Two costs are held down here rather than by the caller. Appending is one
//! edit at the end plus at most one at the front, never a rewrite of the
//! log: the buffer is a rolling window over the last [`LOG_CAP`] entries.
//! Anchors survive that, but an evicted line leaves a dead range behind, so
//! after [`REBASE_EVICTIONS`] evictions the buffer is rebuilt once, off the
//! append path, and the scroll position is kept unless the reader was
//! following the tail.

use std::collections::VecDeque;

use collections::HashSet;
use gpui::prelude::*;
use gpui::{Context, Entity, Window};
use rho_window::style::StyleClass;

/// How many entries the log keeps. Older ones are dropped from the front.
pub const LOG_CAP: usize = 4096;
/// How many evictions are taken before the buffer is rebuilt from the log.
pub const REBASE_EVICTIONS: usize = 512;

#[derive(Clone)]
struct Entry {
    timestamp: chrono::DateTime<chrono::FixedOffset>,
    class: StyleClass,
    text: String,
}

#[derive(Default)]
struct Log(VecDeque<Entry>);

impl Log {
    /// Records an entry, and says whether one was dropped to make room.
    fn push(&mut self, entry: Entry) -> bool {
        self.0.push_back(entry);
        if self.0.len() > LOG_CAP {
            self.0.pop_front();
            true
        } else {
            false
        }
    }
}

/// The message log and the surface over it.
pub struct MessageLog {
    log: Log,
    buffer: Entity<language::Buffer>,
    editor: Entity<editor::Editor>,
    /// One range per entry, in the same order, so an eviction drops the
    /// first of each together.
    styles: Vec<(StyleClass, std::ops::Range<text::Anchor>)>,
    /// The rendered length of each entry's line, kept so an eviction knows
    /// how much to cut from the front without rendering it again.
    line_lengths: VecDeque<usize>,
    /// What was highlighted last time, so a class that has just left the
    /// window is cleared rather than left standing.
    applied_classes: HashSet<StyleClass>,
    evictions_since_rebase: usize,
    rebase_scheduled: bool,
}

impl MessageLog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local("", cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = editor::Editor::for_buffer(buffer.clone(), None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            editor.set_autoscroll_pin(
                multi_buffer::Anchor::Max,
                editor::scroll::AutoscrollStrategy::Bottom,
                cx,
            );
            editor
        });
        Self {
            log: Log::default(),
            buffer,
            editor,
            styles: Vec::new(),
            line_lengths: VecDeque::new(),
            applied_classes: HashSet::default(),
            evictions_since_rebase: 0,
            rebase_scheduled: false,
        }
    }

    /// The editor the surface shows. One editor, not one per surface: the
    /// log is a single read-only view of a single buffer.
    pub fn editor(&self) -> &Entity<editor::Editor> {
        &self.editor
    }

    /// Whether the reader is at the tail, so an entry arriving now would
    /// scroll the surface rather than land out of sight.
    pub fn following(&self, cx: &gpui::App) -> bool {
        self.editor.read(cx).has_active_autoscroll_pin()
    }

    /// Records a line. Newlines in `text` are folded to spaces: the log is
    /// one entry per line, and a message that wrapped would otherwise break
    /// the ranges that carry its class.
    pub fn append(&mut self, text: String, class: StyleClass, cx: &mut Context<Self>) {
        let entry = Entry {
            timestamp: chrono::Local::now().fixed_offset(),
            class,
            text: text.lines().collect::<Vec<_>>().join(" "),
        };
        let line = render(&entry);
        let evicted = self.log.push(entry);
        let removed_len = if evicted {
            self.line_lengths
                .pop_front()
                .expect("capped log has a rendered first line")
        } else {
            0
        };
        self.line_lengths.push_back(line.len());
        let range = self.buffer.update(cx, |buffer, cx| {
            let old_len = buffer.len();
            let mut edits = Vec::with_capacity(2);
            if removed_len > 0 {
                edits.push((0..removed_len, ""));
            }
            edits.push((old_len..old_len, line.as_str()));
            buffer.edit(edits, None, cx);
            let start = buffer.len() - line.len();
            buffer.anchor_before(start)..buffer.anchor_before(buffer.len())
        });
        if evicted {
            self.styles.remove(0);
            self.evictions_since_rebase += 1;
        }
        self.styles.push((class, range));
        self.apply_styles(cx);
        if self.evictions_since_rebase >= REBASE_EVICTIONS && !self.rebase_scheduled {
            self.rebase_scheduled = true;
            cx.spawn(async move |this, cx| {
                let _ = this.update_in(cx, |this, window, cx| this.rebase(window, cx));
            })
            .detach();
        }
        cx.notify();
    }

    fn apply_styles(&mut self, cx: &mut Context<Self>) {
        let multi_buffer = self.editor.read(cx).buffer().clone();
        let current_classes = self
            .styles
            .iter()
            .map(|(class, _)| *class)
            .collect::<HashSet<_>>();
        let mut by_class = self
            .applied_classes
            .union(&current_classes)
            .copied()
            .map(|class| (class, Vec::new()))
            .collect::<Vec<(StyleClass, Vec<std::ops::Range<text::Anchor>>)>>();
        for (class, range) in &self.styles {
            by_class
                .iter_mut()
                .find(|(existing, _)| existing == class)
                .expect("current class was seeded")
                .1
                .push(range.clone());
        }
        rho_window::highlights::apply_class_highlights(
            &self.editor,
            &multi_buffer,
            rho_window::style::Region::System,
            by_class
                .iter()
                .map(|(class, ranges)| (*class, ranges.as_slice())),
            cx,
        );
        self.applied_classes = current_classes;
    }

    /// Rebuilds the buffer from the log, off the append path. The dead
    /// ranges left by evictions go with the old buffer, and the reader
    /// keeps their place unless they were following the tail.
    fn rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rebase_scheduled = false;
        self.evictions_since_rebase = 0;
        let scroll_position = self
            .editor
            .update(cx, |editor, cx| editor.scroll_position(cx));
        let following = self.following(cx);
        let rendered = self.log.0.iter().map(render).collect::<String>();
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(rendered, cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let mut offset = 0;
        self.styles = buffer.update(cx, |buffer, _| {
            self.log
                .0
                .iter()
                .zip(&self.line_lengths)
                .map(|(entry, len)| {
                    let start = offset;
                    offset += *len;
                    (
                        entry.class,
                        buffer.anchor_before(start)..buffer.anchor_before(offset),
                    )
                })
                .collect()
        });
        let multi_buffer = self.editor.read(cx).buffer().clone();
        multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                buffer.clone(),
                [language::Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
        });
        self.buffer = buffer;
        self.apply_styles(cx);
        if !following {
            self.editor.update(cx, |editor, cx| {
                editor.set_scroll_position(scroll_position, window, cx);
            });
        }
    }

    /// The text of every entry, oldest first. For a host's tests: the log
    /// is what the host asserts about after it has said something.
    pub fn texts(&self) -> Vec<&str> {
        self.log.0.iter().map(|entry| entry.text.as_str()).collect()
    }

    /// Which buffer the surface is showing, so a host's test can see that a
    /// rebase replaced it.
    pub fn buffer_id(&self) -> gpui::EntityId {
        self.buffer.entity_id()
    }

    /// Records an entry without touching the buffer, for a host's test that
    /// only cares about what the log keeps.
    pub fn append_unrendered(&mut self, text: String) {
        let _ = self.log.push(Entry {
            timestamp: chrono::Local::now().fixed_offset(),
            class: StyleClass::SystemInfo,
            text,
        });
    }

    /// Replaces the log with these entries, rendered in one edit. For a
    /// host's test that needs a full log without paying for it an entry at
    /// a time.
    pub fn seed(
        &mut self,
        entries: impl IntoIterator<Item = (StyleClass, String)>,
        cx: &mut Context<Self>,
    ) {
        self.log = Log::default();
        self.line_lengths.clear();
        let mut rendered = String::new();
        let mut spans = Vec::new();
        for (class, text) in entries {
            let entry = Entry {
                timestamp: chrono::Local::now().fixed_offset(),
                class,
                text,
            };
            let line = render(&entry);
            let start = rendered.len();
            rendered.push_str(&line);
            spans.push((class, start..rendered.len()));
            self.line_lengths.push_back(line.len());
            let _ = self.log.push(entry);
        }
        self.styles = self.buffer.update(cx, |buffer, cx| {
            let old_len = buffer.len();
            buffer.edit([(0..old_len, rendered.as_str())], None, cx);
            spans
                .into_iter()
                .map(|(class, range)| {
                    (
                        class,
                        buffer.anchor_before(range.start)..buffer.anchor_before(range.end),
                    )
                })
                .collect()
        });
        self.apply_styles(cx);
    }
}

/// One entry as one line: the time it landed and what was said.
fn render(entry: &Entry) -> String {
    format!("{}  {}\n", entry.timestamp.format("%H:%M"), entry.text)
}
