//! Which byte ranges of a markdown message are pure markup punctuation.
//!
//! The transcript buffer always holds the model's original text; these ranges
//! are hidden at the display layer only (zero-width folds), so `**bold**`
//! reads as `bold` while copying, searching and anchoring still see the
//! markdown source.
//!
//! A block parse locates heading markers and the inline spans, and one inline
//! parse per span locates the emphasis and code-span delimiters. Restricting
//! the inline pass matters - parsing the whole message as inline would find
//! "delimiters" inside fenced code blocks and hide characters that are
//! content. The trees are handed back to the caller rather than dropped:
//! highlighting wants the same trees, and the markdown grammars parse at
//! about 5MB/s, so a second parse of a message costs more than everything
//! else the transcript does with it.

use std::cell::RefCell;
use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

thread_local! {
    static BLOCK_PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
    static INLINE_PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

/// One message's markdown trees: the block parse, and one inline parse per
/// inline span, each with the span it covers.
pub struct Trees {
    block: Option<Tree>,
    inline: Vec<(tree_sitter::Range, Tree)>,
}

impl Trees {
    pub fn block(&self) -> Option<&Tree> {
        self.block.as_ref()
    }

    /// The inline trees, each with the byte range of the text it covers.
    pub fn inline(&self) -> impl Iterator<Item = (&tree_sitter::Range, &Tree)> {
        self.inline.iter().map(|(span, tree)| (span, tree))
    }
}

/// Parses `text` as markdown: one block pass, then one inline pass per
/// inline span the block pass found.
pub fn parse(text: &str) -> Trees {
    let mut spans = Vec::new();
    let block = with_parser(&BLOCK_PARSER, tree_sitter_md::LANGUAGE.into(), |parser| {
        parser.parse(text, None)
    })
    .flatten();
    if let Some(tree) = &block {
        for_each_node(tree, |node| {
            if node.kind() == "inline" {
                spans.push(node.range());
            }
        });
    }

    let mut inline = Vec::with_capacity(spans.len());
    if !spans.is_empty() {
        with_parser(
            &INLINE_PARSER,
            tree_sitter_md::INLINE_LANGUAGE.into(),
            |parser| {
                // One parse per span, as the grammar's own injection expects:
                // the inline scanner reads a span as a self-contained line of
                // text, and a run of spans parsed together loses delimiters.
                for span in spans {
                    if parser
                        .set_included_ranges(std::slice::from_ref(&span))
                        .is_err()
                    {
                        continue;
                    }
                    if let Some(tree) = parser.parse(text, None) {
                        inline.push((span, tree));
                    }
                }
                parser.set_included_ranges(&[]).ok();
            },
        );
    }
    Trees { block, inline }
}

/// The markup ranges of an already-parsed message, sorted and
/// non-overlapping.
pub fn concealed_ranges_of(text: &str, trees: &Trees) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    if let Some(block) = &trees.block {
        for_each_node(block, |node| {
            if is_heading_marker(node.kind()) {
                ranges.push(node.start_byte()..marker_end(text, node.end_byte()));
            }
        });
    }
    for (_, tree) in &trees.inline {
        for_each_node(tree, |node| {
            if matches!(node.kind(), "emphasis_delimiter" | "code_span_delimiter") {
                ranges.push(node.byte_range());
            }
        });
    }
    ranges.sort_by_key(|range| range.start);
    merge_adjacent(ranges)
}

/// Markup ranges to conceal, parsing `text` for them alone. Rendering
/// highlights the same trees and so goes through [`parse`] itself.
#[cfg(test)]
pub fn concealed_ranges(text: &str) -> Vec<Range<usize>> {
    concealed_ranges_of(text, &parse(text))
}

/// Heading markers swallow the blank that separates them from the heading
/// text, so `## Title` conceals to `Title` rather than ` Title`.
fn marker_end(text: &str, end: usize) -> usize {
    text[end..]
        .find(|character| character != ' ' && character != '\t')
        .map_or(text.len(), |offset| end + offset)
}

fn is_heading_marker(kind: &str) -> bool {
    matches!(
        kind,
        "atx_h1_marker"
            | "atx_h2_marker"
            | "atx_h3_marker"
            | "atx_h4_marker"
            | "atx_h5_marker"
            | "atx_h6_marker"
    )
}

/// Concealed ranges become one fold each; merging the runs that markdown
/// emits as separate tokens (`**` parses as two delimiters) keeps that count
/// down.
fn merge_adjacent(ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

fn for_each_node(tree: &Tree, mut body: impl FnMut(Node<'_>)) {
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
        body(node);
    }
}

/// Runs `body` with a parser for `language`, reused across calls: parser
/// construction allocates, and messages re-parse on every streaming delta.
fn with_parser<R>(
    slot: &'static std::thread::LocalKey<RefCell<Option<Parser>>>,
    language: tree_sitter::Language,
    body: impl FnOnce(&mut Parser) -> R,
) -> Option<R> {
    slot.with(|slot| {
        let mut slot = slot.borrow_mut();
        let parser = match slot.as_mut() {
            Some(parser) => parser,
            None => {
                let mut parser = Parser::new();
                if parser.set_language(&language).is_err() {
                    return None;
                }
                slot.insert(parser)
            }
        };
        Some(body(parser))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concealed(text: &str) -> String {
        let mut visible = String::new();
        let mut cursor = 0;
        for range in concealed_ranges(text) {
            visible.push_str(&text[cursor..range.start]);
            cursor = range.end;
        }
        visible.push_str(&text[cursor..]);
        visible
    }

    #[test]
    fn emphasis_delimiters_are_concealed() {
        assert_eq!(
            concealed("**FOO** and *bar* and _baz_"),
            "FOO and bar and baz"
        );
    }

    #[test]
    fn code_span_delimiters_are_concealed() {
        assert_eq!(concealed("use `cargo test` here"), "use cargo test here");
    }

    #[test]
    fn heading_markers_take_their_trailing_space() {
        assert_eq!(concealed("## Title\n\nbody\n"), "Title\n\nbody\n");
    }

    #[test]
    fn heading_and_inline_markup_conceal_together() {
        assert_eq!(
            concealed("## Heading\n\n**bold** and `code`.\n"),
            "Heading\n\nbold and code.\n"
        );
    }

    #[test]
    fn fenced_code_keeps_its_content() {
        let text = "```rust\nlet x = **y;\n```\n";
        assert_eq!(concealed(text), text);
    }

    #[test]
    fn indented_code_keeps_its_content() {
        let text = "text\n\n    a ** b\n";
        assert_eq!(concealed(text), text);
    }

    #[test]
    fn unclosed_emphasis_stays_visible() {
        assert_eq!(concealed("**partial"), "**partial");
    }

    #[test]
    fn strong_emphasis_conceals_as_one_range_per_side() {
        assert_eq!(concealed_ranges("**FOO**"), vec![0..2, 5..7]);
    }
}
