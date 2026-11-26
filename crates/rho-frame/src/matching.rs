//! Matching typed words against names: where a word hits a name (a whole
//! word, a word's start, or inside a word), and which display bytes to
//! mark. `Places`, `Commands` and the text search share it.

use std::ops::Range;

use memchr::memmem::Finder;

use crate::text_search::TextQuery;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Hit {
    WholeWord,
    WordStart,
    InsideWord,
}

pub(crate) fn ordered_starts(name: &str, terms: &[Finder<'_>]) -> bool {
    let mut end = 0;
    for term in terms {
        let mut from = end;
        let at = loop {
            let Some(offset) = term.find(&name.as_bytes()[from..]) else {
                return false;
            };
            let at = from + offset;
            if word_start(name, at) {
                break at;
            }
            from = at + 1;
        };
        end = at + term.needle().len();
    }
    true
}

fn word_start(text: &str, at: usize) -> bool {
    match at.checked_sub(1).map(|last| text.as_bytes()[last]) {
        None => true,
        Some(byte) if byte.is_ascii() => !byte.is_ascii_alphanumeric(),
        Some(_) => !text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric),
    }
}

fn word_end(text: &str, at: usize) -> bool {
    match text.as_bytes().get(at).copied() {
        None => true,
        Some(byte) if byte.is_ascii() => !byte.is_ascii_alphanumeric(),
        Some(_) => !text[at..].chars().next().is_some_and(char::is_alphanumeric),
    }
}

pub(crate) fn best_hit(text: &str, term: &Finder<'_>) -> Option<(Hit, Range<usize>)> {
    let mut best = None;
    let mut from = 0;
    while let Some(offset) = term.find(&text.as_bytes()[from..]) {
        let start = from + offset;
        let end = start + term.needle().len();
        let quality = if !word_start(text, start) {
            Hit::InsideWord
        } else if word_end(text, end) {
            return Some((Hit::WholeWord, start..end));
        } else {
            Hit::WordStart
        };
        if best.as_ref().is_none_or(|(old, _)| quality < *old) {
            best = Some((quality, start..end));
        }
        // Both strings are UTF-8, so a nonempty needle never begins in a
        // continuation byte even when we advance only one byte.
        from = start + 1;
    }
    best
}

pub(crate) fn merge_marks(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut kept = 0;
    for index in 0..ranges.len() {
        if kept != 0 && ranges[index].start <= ranges[kept - 1].end {
            ranges[kept - 1].end = ranges[kept - 1].end.max(ranges[index].end);
        } else {
            ranges.swap(kept, index);
            kept += 1;
        }
    }
    ranges.truncate(kept);
}

// For length-changing folds, map every folded byte to the display character.
// ASCII and length-preserving Unicode need no offset table.
pub(crate) struct Folded {
    pub(crate) text: String,
    display: Vec<Range<usize>>,
    ascii_hits: [u128; 3],
}

impl Folded {
    pub(crate) fn new(text: &str) -> Self {
        let mut folded = Self::for_display(text);
        folded.ascii_hits = ascii_hits(&folded.text);
        folded
    }

    pub(crate) fn for_display(text: &str) -> Self {
        if text.is_ascii() {
            let text = text.to_ascii_lowercase();
            return Self {
                text,
                display: Vec::new(),
                ascii_hits: [0; 3],
            };
        }
        let lower = text.to_lowercase();
        let preserving = text.chars().all(|character| {
            let mut folded = character.to_lowercase();
            folded
                .next()
                .is_some_and(|lower| lower.len_utf8() == character.len_utf8())
                && folded.next().is_none()
        });
        let mut display = Vec::new();
        if !preserving {
            for (offset, character) in text.char_indices() {
                let count = character.to_lowercase().map(char::len_utf8).sum();
                display.extend(std::iter::repeat_n(
                    offset..offset + character.len_utf8(),
                    count,
                ));
            }
        }
        Self {
            text: lower,
            display,
            ascii_hits: [0; 3],
        }
    }

    pub(crate) fn quality(&self, byte: u8) -> Option<Hit> {
        let bit = 1 << byte;
        if self.ascii_hits[Hit::WholeWord as usize] & bit != 0 {
            Some(Hit::WholeWord)
        } else if self.ascii_hits[Hit::WordStart as usize] & bit != 0 {
            Some(Hit::WordStart)
        } else if self.ascii_hits[Hit::InsideWord as usize] & bit != 0 {
            Some(Hit::InsideWord)
        } else {
            None
        }
    }

    pub(crate) fn display(&self, range: Range<usize>) -> Range<usize> {
        if self.display.is_empty() {
            range
        } else {
            self.display[range.start].start..self.display[range.end - 1].end
        }
    }
}

fn ascii_hits(text: &str) -> [u128; 3] {
    let mut hits = [0; 3];
    for (at, byte) in text.bytes().enumerate().filter(|(_, byte)| byte.is_ascii()) {
        let quality = if !word_start(text, at) {
            Hit::InsideWord
        } else if word_end(text, at + 1) {
            Hit::WholeWord
        } else {
            Hit::WordStart
        };
        hits[quality as usize] |= 1 << byte;
    }
    hits
}

pub(crate) fn text_marks(line: &str, query: &TextQuery<'_>) -> Vec<Range<usize>> {
    let folded = Folded::for_display(line);
    let mut marks: Vec<_> = query
        .terms
        .iter()
        .filter_map(|term| {
            let (_, range) = best_hit(&folded.text, term)?;
            Some(folded.display(range))
        })
        .collect();
    merge_marks(&mut marks);
    marks
}
