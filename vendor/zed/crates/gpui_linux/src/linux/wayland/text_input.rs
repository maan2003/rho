use gpui::{PlatformInputHandler, UTF16Selection};
use std::ops::Range;

/// Wayland v3 events are double-buffered until `done`, in protocol order.
#[derive(Default)]
pub(super) struct ImeBatch {
    pub commit: Option<String>,
    pub preedit: Option<(String, i32, i32)>,
    pub delete: Option<(u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SurroundingText {
    pub text: String,
    pub cursor: i32,
    pub anchor: i32,
}

fn utf16_to_byte(text: &str, offset: usize) -> Option<usize> {
    let mut utf16 = 0;
    for (byte, ch) in text.char_indices() {
        if utf16 == offset {
            return Some(byte);
        }
        utf16 += ch.len_utf16();
    }
    (utf16 == offset).then_some(text.len())
}

fn byte_to_utf16(text: &str, byte: usize) -> Option<usize> {
    text.get(..byte).map(|prefix| prefix.encode_utf16().count())
}

impl SurroundingText {
    fn from_text(
        mut text: String,
        range: Range<usize>,
        selection: UTF16Selection,
        marked: Option<Range<usize>>,
    ) -> Option<Self> {
        let mut start = selection.range.start.checked_sub(range.start)?;
        let mut end = selection.range.end.checked_sub(range.start)?;
        if let Some(marked) = marked {
            let a = marked.start.checked_sub(range.start)?;
            let b = marked.end.checked_sub(range.start)?;
            let bytes = utf16_to_byte(&text, a)?..utf16_to_byte(&text, b)?;
            text.replace_range(bytes, "");
            // The cursor and anchor inside preedit refer to its insertion point.
            let adjust = |offset: usize| {
                if offset > b {
                    offset - (b - a)
                } else if offset >= a {
                    a
                } else {
                    offset
                }
            };
            start = adjust(start);
            end = adjust(end);
        }
        let start = utf16_to_byte(&text, start)?;
        let end = utf16_to_byte(&text, end)?;
        if end - start > 4000 {
            return None;
        }
        // Keep the whole selection and use the remaining byte budget for context.
        let mut left = start.saturating_sub((4000 - (end - start)) / 2);
        while !text.is_char_boundary(left) {
            left += 1;
        }
        let mut right = (left + 4000).min(text.len());
        while !text.is_char_boundary(right) {
            right -= 1;
        }
        // Near the end, spend otherwise unused capacity on preceding context.
        left = right.saturating_sub(4000).min(left);
        while !text.is_char_boundary(left) {
            left += 1;
        }
        if right < end {
            return None;
        }
        let (cursor, anchor) = if selection.reversed {
            (start, end)
        } else {
            (end, start)
        };
        Some(Self {
            text: text[left..right].into(),
            cursor: (cursor - left) as i32,
            anchor: (anchor - left) as i32,
        })
    }

    pub fn read(handler: &mut PlatformInputHandler) -> Option<Self> {
        let selection = handler.selected_text_range(false)?;
        let marked = handler.marked_text_range();
        let start = selection
            .range
            .start
            .min(marked.as_ref().map_or(usize::MAX, |r| r.start));
        let end = selection
            .range
            .end
            .max(marked.as_ref().map_or(0, |r| r.end));
        // UTF-16 units are at most three UTF-8 bytes, so this bounded query
        // obtains enough context without asking an editor for the whole buffer.
        let requested = start.saturating_sub(2000)
            ..end
                .saturating_add(2000)
                .min(handler.text_length_utf16().unwrap_or(usize::MAX));
        let mut adjusted = None;
        let text = handler.text_for_range(requested.clone(), &mut adjusted)?;
        Self::from_text(text, adjusted.unwrap_or(requested), selection, marked)
    }

    fn deletion_lengths(&self, before: u32, after: u32) -> Option<(usize, usize)> {
        let start = self.cursor.min(self.anchor) as usize;
        let end = self.cursor.max(self.anchor) as usize;
        let before_start = start.checked_sub(before as usize)?;
        let after_end = end.checked_add(after as usize)?;
        Some((
            byte_to_utf16(&self.text, start)? - byte_to_utf16(&self.text, before_start)?,
            byte_to_utf16(&self.text, after_end)? - byte_to_utf16(&self.text, end)?,
        ))
    }
}

impl ImeBatch {
    pub fn apply(self, handler: &mut PlatformInputHandler) {
        handler.begin_input_method_batch();
        handler.set_ime_cursor_visible(true);
        if handler.marked_text_range().is_some() {
            handler.replace_and_mark_text_in_range(None, "", None);
        }
        if let Some((before, after)) = self.delete {
            if let Some(lengths) =
                SurroundingText::read(handler).and_then(|s| s.deletion_lengths(before, after))
            {
                handler.delete_surrounding_text(lengths.0, lengths.1);
            }
        }
        if let Some(text) = self.commit {
            handler.replace_text_in_range(None, &text);
        }
        if let Some((text, begin, end)) = self.preedit {
            let visible = begin >= 0 && end >= 0;
            let selection = if visible {
                byte_to_utf16(&text, begin as usize)
                    .zip(byte_to_utf16(&text, end as usize))
                    .map(|(begin, end)| begin..end)
            } else {
                None
            };
            handler.replace_and_mark_text_in_range(None, &text, selection);
            handler.set_ime_cursor_visible(visible);
        }
        handler.end_input_method_batch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrounding_preserves_reversed_selection_and_excludes_preedit() {
        let surrounding = SurroundingText::from_text(
            "α😀選択xyz終".into(),
            20..31,
            UTF16Selection {
                range: 23..25,
                reversed: true,
            },
            None,
        )
        .unwrap();
        assert_eq!(surrounding.cursor, 6);
        assert_eq!(surrounding.anchor, 12);
        assert_eq!(surrounding.deletion_lengths(4, 3), Some((2, 3)));
        assert_eq!(surrounding.deletion_lengths(3, 3), None); // inside emoji
        let surrounding = SurroundingText::from_text(
            "α😀仮入力xyz終".into(),
            20..32,
            UTF16Selection {
                range: 24..24,
                reversed: false,
            },
            Some(23..26),
        )
        .unwrap();
        assert_eq!(surrounding.text, "α😀xyz終");
        assert_eq!((surrounding.cursor, surrounding.anchor), (6, 6));
        assert_eq!(surrounding.deletion_lengths(6, 6), Some((3, 4)));
    }

    #[test]
    fn surrounding_budget_keeps_selection_and_unicode_boundaries() {
        let text = format!("{}😀中{}", "α".repeat(2200), "終".repeat(2200));
        let s = SurroundingText::from_text(
            text,
            0..4403,
            UTF16Selection {
                range: 2200..2203,
                reversed: false,
            },
            None,
        )
        .unwrap();
        assert!(s.text.len() <= 4000);
        assert_eq!(&s.text[s.anchor as usize..s.cursor as usize], "😀中");
        assert_eq!(s.deletion_lengths(2, 3), Some((1, 1)));
        assert_eq!(byte_to_utf16("a😀中", 5), Some(3));
        assert_eq!(byte_to_utf16("a😀中", 3), None);
        assert_eq!(utf16_to_byte("a😀中", 2), None);
    }
}
