//! Source-backed Markdown decorations for Slack messages.
//! Syntax, rather than punctuation guesses, distinguishes lists/code from
//! prose.

use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Default)]
pub(super) struct TextLayout {
    pub lists: Vec<(Range<usize>, u32)>,
    pub bullets: Vec<Range<usize>>,
    pub code: Vec<Range<usize>>,
    pub code_blocks: Vec<Range<usize>>,
    pub underlines: Vec<Range<usize>>,
    pub mentions: Vec<Range<usize>>,
    pub concealed: Vec<Range<usize>>,
    pub paragraph_gaps: Vec<usize>,
}

impl TextLayout {
    pub fn from_snapshot(snapshot: &language::BufferSnapshot, messages: &[Range<usize>]) -> Self {
        let source = snapshot.text();
        let mut layout = Self::default();
        let mut protected = Vec::new();
        let mut tags = Vec::new();
        let mut lists = BTreeMap::new();
        for layer in snapshot.syntax_layers() {
            let mut nodes = vec![layer.node()];
            while let Some(node) = nodes.pop() {
                let range = node.byte_range();
                match node.kind() {
                    "fenced_code_block" | "indented_code_block" => {
                        layout.code_blocks.push(range.clone());
                        protected.push(range);
                        continue;
                    }
                    "code_span" => {
                        protected.push(range.clone());
                        layout.code.push(range);
                        continue;
                    }
                    "html_tag" => tags.push(range),
                    "backslash_escape" => layout.concealed.push(range.start..range.start + 1),
                    "list_item" => {
                        let mut cursor = node.walk();
                        if let Some(marker) = node
                            .named_children(&mut cursor)
                            .find(|child| child.kind().starts_with("list_marker_"))
                        {
                            let marker_text = &source[marker.byte_range()];
                            let marker_end = marker.start_byte() + marker_text.trim_end().len();
                            let mut body = marker_end;
                            while source
                                .as_bytes()
                                .get(body)
                                .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
                            {
                                body += 1;
                            }
                            let row_start = source[..marker.start_byte()]
                                .rfind('\n')
                                .map_or(0, |at| at + 1);
                            let indent = source[row_start..body].chars().count() as u32;
                            if matches!(
                                marker.kind(),
                                "list_marker_minus" | "list_marker_plus" | "list_marker_star"
                            ) {
                                layout.bullets.push(marker.start_byte()..marker_end);
                            }
                            // Markdown permits lazy list continuation, but a Slack
                            // message boundary always ends the preceding list.
                            let message = messages
                                .partition_point(|range| range.start <= marker.start_byte());
                            let end = message.checked_sub(1).map_or(node.end_byte(), |ix| {
                                node.end_byte().min(messages[ix].end)
                            });
                            let mut offset = row_start;
                            for line in source[row_start..end].split_inclusive('\n') {
                                // Slack source newlines are author-entered, not soft
                                // wraps. An unindented following paragraph is not a
                                // lazy Markdown continuation of this list item.
                                if offset != row_start
                                    && !line.trim().is_empty()
                                    && line
                                        .chars()
                                        .take_while(|ch| *ch == ' ' || *ch == '\t')
                                        .count()
                                        < indent as usize
                                {
                                    break;
                                }
                                let end = offset + line.trim_end_matches('\n').len();
                                lists.insert(offset, (offset..end, indent));
                                offset += line.len();
                            }
                        }
                    }
                    _ => {}
                }
                let mut cursor = node.walk();
                let children = node.named_children(&mut cursor).collect::<Vec<_>>();
                nodes.extend(children.into_iter().rev());
            }
        }
        protected.sort_by_key(|range| range.start);
        let mut code_ranges: Vec<Range<usize>> = Vec::new();
        for range in protected {
            if let Some(last) = code_ranges.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                code_ranges.push(range);
            }
        }
        let in_code = |offset| {
            let ix = code_ranges.partition_point(|range| range.start <= offset);
            ix > 0 && offset < code_ranges[ix - 1].end
        };
        layout.lists = lists
            .into_values()
            .filter(|(range, _)| !in_code(range.start))
            .collect();

        tags.sort_by_key(|range| range.start);
        let mut underline = Vec::new();
        let mut mention = Vec::new();
        for tag in tags {
            match &source[tag.clone()] {
                "<u>" => underline.push(tag),
                "<mark>" => mention.push(tag),
                "</u>" => {
                    if let Some(open) = underline.pop() {
                        layout.underlines.push(open.end..tag.start);
                        layout.concealed.extend([open, tag]);
                    }
                }
                "</mark>" => {
                    if let Some(open) = mention.pop() {
                        layout.mentions.push(open.end..tag.start);
                        layout.concealed.extend([open, tag]);
                    }
                }
                _ => {}
            }
        }
        // A paragraph break is half a line, without modifying the source or
        // collapsing intentional whitespace inside code.
        let mut offset = 0;
        let mut blank = None;
        for line in source.split_inclusive('\n') {
            let end = offset + line.len();
            if offset > 0 && line.trim().is_empty() && !in_code(offset) {
                blank.get_or_insert(offset);
            } else if let Some(start) = blank.take() {
                layout.concealed.push(start..offset);
                layout.paragraph_gaps.push(start - 1);
            }
            offset = end;
        }
        layout
    }
}
