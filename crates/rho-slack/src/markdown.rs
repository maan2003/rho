//! A Slack message as markdown, which is what a document of blocks is
//! written in.
//!
//! The conversation surface is a transcript of blocks, one a message, and
//! the transcript reads markdown: it parses the markup, conceals it, and
//! renders links, code and quotes itself. Slack speaks `mrkdwn`, which is
//! the same ideas in different characters, so this is the one place the two
//! are told apart — the ids, links and lists are resolved by the same
//! renderer either way, and only the markers differ.
//!
//! Everything here is a pure function of the payload and whatever names are
//! known, so a message re-renders correctly once a late roster fills a gap.

use serde_json::Value;

use crate::block::{Emphasis, Flavour, Names, emphasis};
use crate::types::{Attachment, FileSummary};

/// What the sender said, as markdown.
pub fn body(
    blocks: &[Value],
    text: &str,
    attachments: &[Attachment],
    files: &[FileSummary],
    names: &dyn Names,
) -> String {
    parts(blocks, text, attachments, files, names).0
}

/// What the sender said, and the lines that hang under it: an attachment's
/// card, a bot's fields, a file. The split is the transcript's, which puts
/// the said part in the turn and the rest in the gutter beside it.
pub fn parts(
    blocks: &[Value],
    text: &str,
    attachments: &[Attachment],
    files: &[FileSummary],
    names: &dyn Names,
) -> (String, Vec<String>) {
    crate::block::render_parts_as(Flavour::Markdown, blocks, text, attachments, files, names)
}

/// A plain Slack body rewritten with markdown's markers.
///
/// Code spans pass through: a backtick means the same thing in both, and
/// what is inside one is not formatting in either.
pub(crate) fn remark(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        out.push_str(&marked(&rest[..open]));
        match rest[open + 1..].find('`') {
            Some(close) => {
                let end = open + 1 + close + 1;
                out.push_str(&rest[open..end]);
                rest = &rest[end..];
            }
            None => {
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(&marked(rest));
    out
}

/// One run of text with no code in it: every emphasised stretch rewritten,
/// everything else escaped.
fn marked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (kind, range) in emphasis(text) {
        out.push_str(&escape(&text[at..range.start]));
        let marker = match kind {
            Emphasis::Bold => "**",
            Emphasis::Italic => "*",
            Emphasis::Struck => "~~",
        };
        out.push_str(marker);
        out.push_str(&escape(&text[range.start + 1..range.end - 1]));
        out.push_str(marker);
        at = range.end;
    }
    out.push_str(&escape(&text[at..]));
    out
}

/// Text that is text, kept as text: the characters markdown would read as
/// markup are escaped, and the ones it only reads as markup between words
/// are left alone, so `snake_case` and `a~b` still say what they said.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '*' | '`' | '[' | ']') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::NoNames;
    use crate::types::{ChannelId, UserId};

    struct Roster;

    impl Names for Roster {
        fn user(&self, id: &UserId) -> Option<String> {
            (id.0 == "U1").then(|| "ada".to_owned())
        }

        fn channel(&self, id: &ChannelId) -> Option<String> {
            (id.0 == "C1").then(|| "design".to_owned())
        }
    }

    fn said(text: &str) -> String {
        body(&[], text, &[], &[], &Roster)
    }

    /// Slack's markers say the same things as markdown's, and this is where
    /// they are translated. A reader sees bold text either way; the parse
    /// behind the transcript only understands one of them.
    #[test]
    fn slacks_emphasis_becomes_markdowns() {
        assert_eq!(said("*bold* and _italic_"), "**bold** and *italic*");
        assert_eq!(said("~struck~ out"), "~~struck~~ out");
        assert_eq!(said("`code *stays*`"), "`code *stays*`");
        assert_eq!(
            said("```\nfenced *stays*\n```"),
            "```\nfenced *stays*\n```",
            "a fence is two code spans and an empty one between them, and \
             nothing in it is markup"
        );
    }

    /// A name with an underscore in it is a name. The conversion is only
    /// allowed where Slack itself would have shown emphasis, which is what
    /// keeps `snake_case` off the italic list.
    #[test]
    fn what_is_not_a_marker_is_left_alone() {
        assert_eq!(
            said("call snake_case_thing here"),
            "call snake_case_thing here"
        );
        assert_eq!(said("2 * 3 * 4"), "2 \\* 3 \\* 4");
        assert_eq!(said("a [b] c"), "a \\[b\\] c");
    }

    /// The address travels in the text, because a document of markdown
    /// carries its own links.
    #[test]
    fn a_link_becomes_a_markdown_link() {
        assert_eq!(
            said("see <https://example.com|the notes>"),
            "see [the notes](https://example.com)"
        );
        assert_eq!(said("<https://example.com>"), "<https://example.com>");
    }

    /// Ids never survive, in either flavour: the mention is a name.
    #[test]
    fn a_mention_is_a_name_and_never_an_id() {
        assert_eq!(said("<@U1> in <#C1>"), "@ada in #design");
        assert_eq!(
            body(&[], "<@U9>", &[], &[], &NoNames),
            "@someone",
            "an id the roster has not filled yet reads as the handle Slack \
             itself would show"
        );
    }

    /// Block Kit carries its emphasis as flags rather than characters, and
    /// they are written out in the flavour asked for.
    #[test]
    fn rich_text_styles_are_written_as_markdown() {
        let blocks = vec![serde_json::json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_section",
                "elements": [
                    {"type": "text", "text": "look at ", "style": {}},
                    {"type": "text", "text": "this", "style": {"bold": true}},
                    {"type": "text", "text": " and "},
                    {"type": "link", "url": "https://example.com", "text": "that"},
                ],
            }],
        })];
        assert_eq!(
            body(&blocks, "", &[], &[], &Roster),
            "look at **this** and [that](https://example.com)"
        );
    }

    /// A quote, a list and a fence are the transcript's own shapes, so they
    /// come across as markdown writes them.
    #[test]
    fn quotes_lists_and_fences_come_across() {
        let blocks = vec![serde_json::json!({
            "type": "rich_text",
            "elements": [
                {
                    "type": "rich_text_quote",
                    "elements": [{"type": "text", "text": "as you said"}],
                },
                {
                    "type": "rich_text_list",
                    "style": "bullet",
                    "elements": [{
                        "type": "rich_text_section",
                        "elements": [{"type": "text", "text": "one"}],
                    }],
                },
            ],
        })];
        assert_eq!(body(&blocks, "", &[], &[], &Roster), "> as you said\n- one");
    }
}
