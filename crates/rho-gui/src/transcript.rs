use std::collections::HashSet;

use editor::{Editor, HighlightKey};
use gpui::{Context, Entity, HighlightStyle};
use language::Buffer;
use multi_buffer::MultiBuffer;
use text::{Anchor, ToOffset as _};

struct HighlightedRange {
    range: std::ops::Range<Anchor>,
    highlight_key: usize,
    style: HighlightStyle,
}

pub(crate) struct InsertedTranscript {
    pub(crate) range: std::ops::Range<Anchor>,
    pub(crate) highlight_keys: Vec<usize>,
    pub(crate) highlight_ranges: Vec<std::ops::Range<Anchor>>,
}

pub(crate) struct Transcript {
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    multi_buffer: Entity<MultiBuffer>,
    end: Anchor,
    ranges: Vec<HighlightedRange>,
    next_highlight_key: usize,
    retired_highlight_keys: Vec<usize>,
}

impl Transcript {
    pub(crate) fn new<T>(
        buffer: Entity<Buffer>,
        editor: Entity<Editor>,
        multi_buffer: Entity<MultiBuffer>,
        cx: &Context<T>,
    ) -> Self {
        let end = buffer.read(cx).anchor_after(0);
        Self {
            buffer,
            editor,
            multi_buffer,
            end,
            ranges: Vec::new(),
            next_highlight_key: 0,
            retired_highlight_keys: Vec::new(),
        }
    }

    pub(crate) fn refresh_highlights<T>(&mut self, cx: &mut Context<T>) {
        self.apply_highlights(cx);
    }

    pub(crate) fn remove_range<T>(&mut self, range: std::ops::Range<Anchor>, cx: &mut Context<T>) {
        self.buffer.update(cx, |buffer, cx| {
            let start = range.start.to_offset(buffer);
            let end = range.end.to_offset(buffer);
            let old_transcript_end = self.end.to_offset(buffer);
            let old_len = end.saturating_sub(start);
            buffer.edit([(start..end, "")], None, cx);
            let new_transcript_end = if old_transcript_end >= end {
                old_transcript_end.saturating_sub(old_len)
            } else if old_transcript_end >= start {
                start
            } else {
                old_transcript_end
            };
            self.end = buffer.anchor_after(new_transcript_end);
        });
        self.apply_highlights(cx);
    }

    pub(crate) fn remove_highlights(&mut self, highlight_keys: Vec<usize>) {
        let highlight_keys = highlight_keys.into_iter().collect::<HashSet<_>>();
        self.ranges
            .retain(|range| !highlight_keys.contains(&range.highlight_key));
        self.retired_highlight_keys.extend(highlight_keys);
    }

    pub(crate) fn range_starts_with<T>(
        &self,
        range: &std::ops::Range<Anchor>,
        character: char,
        cx: &Context<T>,
    ) -> bool {
        let buffer = self.buffer.read(cx);
        let start = range.start.to_offset(buffer);
        let end = range.end.to_offset(buffer);
        buffer_range_starts_with(buffer, start..end, character)
    }

    pub(crate) fn multibuffer_range<T>(
        &self,
        range: std::ops::Range<Anchor>,
        cx: &Context<T>,
    ) -> Option<std::ops::Range<multi_buffer::Anchor>> {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let start = snapshot.anchor_in_excerpt(range.start)?;
        let end = snapshot.anchor_in_excerpt(range.end)?;
        Some(start..end)
    }

    pub(crate) fn range_without_trailing_newlines<T>(
        &self,
        range: &std::ops::Range<Anchor>,
        cx: &Context<T>,
    ) -> std::ops::Range<Anchor> {
        let buffer = self.buffer.read(cx);
        let start = range.start.to_offset(buffer);
        let end = range.end.to_offset(buffer);
        let text = buffer.text_for_range(start..end).collect::<String>();
        let trimmed_len = text.trim_end_matches('\n').len();
        range.start..buffer.anchor_before(start + trimmed_len)
    }

    pub(crate) fn trailing_newlines<T>(&self, cx: &Context<T>) -> usize {
        let buffer = self.buffer.read(cx);
        let transcript_end = self.end.to_offset(buffer);
        buffer
            .text_for_range(0..transcript_end)
            .collect::<String>()
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count()
    }

    pub(crate) fn is_empty<T>(&self, cx: &Context<T>) -> bool {
        self.end.to_offset(&self.buffer.read(cx)) == 0
    }

    pub(crate) fn insert_spans<'a, T>(
        &mut self,
        spans: impl IntoIterator<Item = (&'a str, HighlightStyle)>,
        cx: &mut Context<T>,
    ) -> Option<InsertedTranscript> {
        let spans = spans
            .into_iter()
            .filter(|(text, _)| !text.is_empty())
            .collect::<Vec<_>>();
        if spans.is_empty() {
            return None;
        }
        let text = spans.iter().map(|(text, _)| *text).collect::<String>();
        let buffer = self.buffer.clone();
        let inserted = buffer.update(cx, |buffer, cx| {
            let offset = self.end.to_offset(buffer);
            buffer.edit([(offset..offset, text.as_str())], None, cx);
            let inserted_len = text.len();
            let mut span_start = offset;
            let mut highlight_keys = Vec::new();
            let mut highlight_ranges = Vec::new();
            for (span_text, style) in spans {
                let span_end = span_start + span_text.len();
                let range = buffer.anchor_before(span_start)..buffer.anchor_before(span_end);
                let highlight_key = self.next_highlight_key;
                self.ranges.push(HighlightedRange {
                    range: range.clone(),
                    highlight_key,
                    style,
                });
                highlight_keys.push(highlight_key);
                highlight_ranges.push(range);
                self.next_highlight_key = self.next_highlight_key.saturating_add(1);
                span_start = span_end;
            }
            let end = offset + inserted_len;
            self.end = buffer.anchor_after(end);
            InsertedTranscript {
                range: buffer.anchor_before(offset)..buffer.anchor_before(end),
                highlight_keys,
                highlight_ranges,
            }
        });
        self.apply_highlights(cx);
        Some(inserted)
    }

    pub(crate) fn replace_range_with_spans<'a, T>(
        &mut self,
        range: std::ops::Range<Anchor>,
        spans: impl IntoIterator<Item = (&'a str, HighlightStyle)>,
        cx: &mut Context<T>,
    ) -> Option<InsertedTranscript> {
        let spans = spans
            .into_iter()
            .filter(|(text, _)| !text.is_empty())
            .collect::<Vec<_>>();
        if spans.is_empty() {
            return None;
        }
        let text = spans.iter().map(|(text, _)| *text).collect::<String>();
        let buffer = self.buffer.clone();
        let inserted = buffer.update(cx, |buffer, cx| {
            let start = range.start.to_offset(buffer);
            let end = range.end.to_offset(buffer);
            let old_transcript_end = self.end.to_offset(buffer);
            let old_len = end.saturating_sub(start);
            let new_len = text.len();
            buffer.edit([(start..end, text.as_str())], None, cx);
            let mut span_start = start;
            let mut highlight_keys = Vec::new();
            let mut highlight_ranges = Vec::new();
            for (span_text, style) in spans {
                let span_end = span_start + span_text.len();
                let range = buffer.anchor_before(span_start)..buffer.anchor_before(span_end);
                let highlight_key = self.next_highlight_key;
                self.ranges.push(HighlightedRange {
                    range: range.clone(),
                    highlight_key,
                    style,
                });
                highlight_keys.push(highlight_key);
                highlight_ranges.push(range);
                self.next_highlight_key = self.next_highlight_key.saturating_add(1);
                span_start = span_end;
            }
            let new_transcript_end = if old_transcript_end >= end {
                old_transcript_end - old_len + new_len
            } else if old_transcript_end >= start {
                start + new_len
            } else {
                old_transcript_end
            };
            self.end = buffer.anchor_after(new_transcript_end);
            InsertedTranscript {
                range: buffer.anchor_before(start)..buffer.anchor_before(start + new_len),
                highlight_keys,
                highlight_ranges,
            }
        });
        self.apply_highlights(cx);
        Some(inserted)
    }

    fn apply_highlights<T>(&mut self, cx: &mut Context<T>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut highlights = Vec::new();
        for range in &self.ranges {
            let Some(start) = snapshot.anchor_in_excerpt(range.range.start) else {
                continue;
            };
            let Some(end) = snapshot.anchor_in_excerpt(range.range.end) else {
                continue;
            };
            highlights.push((range.highlight_key, start..end, range.style));
        }

        let retired_highlight_keys = std::mem::take(&mut self.retired_highlight_keys);
        self.editor.update(cx, |editor, cx| {
            for highlight_key in retired_highlight_keys {
                editor.highlight_text(
                    HighlightKey::SyntaxTreeView(highlight_key),
                    Vec::new(),
                    HighlightStyle::default(),
                    cx,
                );
            }
            for (highlight_key, range, style) in highlights {
                editor.highlight_text(
                    HighlightKey::SyntaxTreeView(highlight_key),
                    vec![range],
                    style,
                    cx,
                );
            }
        });
    }
}

pub(crate) fn buffer_range_starts_with(
    buffer: &Buffer,
    range: std::ops::Range<usize>,
    character: char,
) -> bool {
    if range.start >= range.end {
        return false;
    }

    buffer
        .text_for_range(range)
        .next()
        .is_some_and(|text| text.starts_with(character))
}
