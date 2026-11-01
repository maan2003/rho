//! Recover the literal prefix of a live Python `chat.post(topic, …,
//! mention="user", …)` call: the message to the user the model is writing.
//! While the text streams its mention is not written yet, so any post is
//! shown; a finished post that does not mention the user is not.
//! This is presentation only: the notebook remains the authority on actual
//! sends.

use tree_sitter::{Node, Parser};

pub(crate) fn tool_preview(
    name: &str,
    arguments: &str,
    format: rho_agents_client::protocol::transcript::ArgumentsFormat,
) -> Option<String> {
    use rho_agents_client::protocol::transcript::ArgumentsFormat;
    let source = match (name, format) {
        ("exec", ArgumentsFormat::Text) => arguments.to_owned(),
        ("exec" | "mcp__py__exec", ArgumentsFormat::Json) => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
                value.get("source")?.as_str()?.to_owned()
            } else {
                let mut json = json_stream::JsonStreamParser::new();
                for character in arguments.chars() {
                    json.add_char(character).ok()?;
                }
                json.get_result().get("source")?.as_str()?.to_owned()
            }
        }
        _ => return None,
    };
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    preview(&mut parser, &source)
}

pub(crate) fn preview(parser: &mut Parser, source: &str) -> Option<String> {
    let original = parser.parse(source, None)?;
    let mut starts = Vec::new();
    string_starts(original.root_node(), &mut starts);
    let latest_send = last_send(original.root_node(), source)?;
    for start in starts
        .into_iter()
        .rev()
        .filter(|start| start.start_byte() > latest_send)
    {
        if let Some(result) = preview_from_start(parser, &original, source, start) {
            return result;
        }
    }
    None
}

// `Some(None)` means a matching call has no preview, so an older call
// must not be resurrected as the live draft.
fn preview_from_start(
    parser: &mut Parser,
    original: &tree_sitter::Tree,
    source: &str,
    start: Node<'_>,
) -> Option<Option<String>> {
    let start_at = start.start_byte();
    let delimiter = &source[start.byte_range()];
    let quote = delimiter.bytes().find(|b| matches!(b, b'\'' | b'"'))?;
    if delimiter[..delimiter.find(quote as char)?]
        .to_ascii_lowercase()
        .contains('b')
    {
        return None;
    }
    let closing = if delimiter.as_bytes().ends_with(&[quote, quote, quote]) {
        String::from_utf8(vec![quote; 3]).ok()?
    } else {
        char::from(quote).to_string()
    };
    let is_fstring = delimiter.to_ascii_lowercase().contains('f');
    let is_raw = delimiter.to_ascii_lowercase().contains('r');
    // Recovery may invent a missing terminator at a ')' inside the body.
    // Only a delimiter present in the source closes the literal.
    let string_end = start
        .parent()
        .filter(|parent| parent.kind() == "string")
        .and_then(|parent| {
            let mut cursor = parent.walk();
            parent
                .children(&mut cursor)
                .find(|child| child.kind() == "string_end" && !child.is_missing())
        });
    // An unfinished triple-quoted string may already contain one or two
    // closing quotes. Treat them as a pending delimiter, not body text, and
    // complete only the missing quotes in the repaired parse.
    let pending_quotes = if string_end.is_none() && closing.len() == 3 {
        pending_closing_quotes(source, start.end_byte(), quote)
    } else {
        0
    };
    let content_end = string_end.map_or(source.len() - pending_quotes, |end| end.start_byte());
    let interpolation = is_fstring
        .then(|| first_interpolation(&source[start.end_byte()..content_end]))
        .flatten()
        .map(|offset| start.end_byte() + offset);
    let prefix_end = interpolation
        .or_else(|| string_end.map(|end| end.end_byte()))
        .unwrap_or(source.len());
    let prefix = &source[..prefix_end];
    let ended = interpolation.is_none() && string_end.is_some();
    let suffix = if ended {
        ")".to_owned()
    } else if prefix
        .as_bytes()
        .iter()
        .rev()
        .take_while(|b| **b == b'\\')
        .count()
        % 2
        == 1
    {
        format!("\\{closing})")
    } else {
        // A prefix cut before interpolation does not include the pending
        // closing quotes at the end of the original source.
        let missing = if interpolation.is_some() {
            closing.as_str()
        } else {
            &closing[pending_quotes..]
        };
        format!("{missing})")
    };
    let repaired = format!("{prefix}{suffix}");
    let tree = parser.parse(&repaired, None)?;
    let string = send_string(tree.root_node(), &repaired, Some(prefix.len()), start_at)?;
    let completed_call = send_string(original.root_node(), source, None, start_at)
        .and_then(|arg| arg.parent()?.parent());
    if completed_call.is_some_and(|call| !mentions_user(call, source))
        || (completed_call.is_none() && finished_post_at(original.root_node(), source, start_at))
    {
        return Some(None);
    }
    let completed = completed_call.map(|call| call.end_byte());
    let content_start = string.named_child(0)?.end_byte();
    let first = decode(&source[content_start..content_end], is_fstring, is_raw).unwrap_or_default();
    if interpolation.is_some() || string_end.is_none() {
        return Some((!first.is_empty()).then_some(first));
    }
    Some(
        known_sum(
            original.root_node(),
            &source[..completed.map_or(source.len(), |end| end - 1)],
            string_end?.end_byte(),
            first,
        )
        .filter(|text| !text.is_empty()),
    )
}

fn pending_closing_quotes(source: &str, content_start: usize, quote: u8) -> usize {
    let count = source[content_start..]
        .bytes()
        .rev()
        .take_while(|b| *b == quote)
        .count()
        .min(2);
    let preceding = &source[..source.len() - count];
    if preceding.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 0 {
        count
    } else {
        0
    }
}

/// The notebook's chat tool: a topic, the text, then keywords, among them
/// the mention that reaches the user.
const SEND: &str = "chat.post";

/// Whether a finished post mentions the user: `mention="user"`, or a list
/// or tuple of handles with "user" in it.
fn mentions_user(call: Node<'_>, source: &str) -> bool {
    let Some(args) = call.child_by_field_name("arguments") else {
        return false;
    };
    let is_user = |node: Node<'_>| matches!(&source[node.byte_range()], "\"user\"" | "'user'");
    let mut cursor = args.walk();
    args.named_children(&mut cursor).any(|arg| {
        arg.kind() == "keyword_argument"
            && arg
                .child_by_field_name("name")
                .is_some_and(|name| &source[name.byte_range()] == "mention")
            && arg.child_by_field_name("value").is_some_and(|value| {
                is_user(value)
                    || (matches!(value.kind(), "list" | "tuple") && {
                        let mut cursor = value.walk();
                        value.named_children(&mut cursor).any(is_user)
                    })
            })
    })
}

/// Whether the literal at `start_at` is in a post that is finished but
/// not shaped as a message: a post is a topic, the text, then keywords.
fn finished_post_at(node: Node<'_>, source: &str, start_at: usize) -> bool {
    if node.kind() == "call"
        && !node.has_error()
        && node.byte_range().contains(&start_at)
        && source.as_bytes().get(node.end_byte().saturating_sub(1)) == Some(&b')')
        && node
            .child_by_field_name("function")
            .is_some_and(|callee| &source[callee.byte_range()] == SEND)
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| finished_post_at(child, source, start_at))
}

fn last_send(node: Node<'_>, source: &str) -> Option<usize> {
    let mut latest = (node.kind() == "attribute" && &source[node.byte_range()] == SEND)
        .then_some(node.start_byte());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        latest = latest.max(last_send(child, source));
    }
    latest
}

fn string_starts<'a>(node: Node<'a>, starts: &mut Vec<Node<'a>>) {
    if node.kind() == "string_start" {
        starts.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        string_starts(child, starts);
    }
}

fn first_node<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_node(child, kind) {
            return Some(found);
        }
    }
    None
}

fn start_at(node: Node<'_>, offset: usize) -> Option<Node<'_>> {
    if node.kind() == "string_start" && node.start_byte() == offset {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = start_at(child, offset) {
            return Some(found);
        }
    }
    None
}

fn known_sum(tree: Node<'_>, source: &str, mut i: usize, mut text: String) -> Option<String> {
    let bytes = source.as_bytes();
    loop {
        while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n')) {
            i += 1;
        }
        // The text argument ends at the call's end, or at the comma before
        // its keywords, which are still streaming when the call is not done.
        if i == bytes.len() || bytes[i] == b',' {
            return Some(text);
        }
        let plus = bytes[i] == b'+';
        if plus {
            i += 1;
            while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n')) {
                i += 1;
            }
        }
        let Some(start) = start_at(tree, i) else {
            return plus.then_some(text);
        };
        let delimiter = &source[start.byte_range()];
        let quote = delimiter.bytes().find(|b| matches!(b, b'\'' | b'"'))?;
        if delimiter[..delimiter.find(quote as char)?]
            .to_ascii_lowercase()
            .contains('b')
        {
            return plus.then_some(text);
        }
        let fstring = delimiter.to_ascii_lowercase().contains('f');
        let raw = delimiter.to_ascii_lowercase().contains('r');
        let end = start
            .parent()
            .filter(|node| node.kind() == "string")
            .and_then(|node| {
                let mut cursor = node.walk();
                node.children(&mut cursor)
                    .find(|child| child.kind() == "string_end" && !child.is_missing())
            });
        let pending = if end.is_none() && delimiter.as_bytes().ends_with(&[quote, quote, quote]) {
            pending_closing_quotes(source, start.end_byte(), quote)
        } else {
            0
        };
        let content_end = end.map_or(source.len() - pending, |end| end.start_byte());
        let content = &source[start.end_byte()..content_end];
        let interpolates = fstring && first_interpolation(content).is_some();
        let decoded = decode(content, fstring, raw).unwrap_or_default();
        text.push_str(&decoded);
        if interpolates || end.is_none() {
            return Some(text);
        }
        i = end?.end_byte();
    }
}

fn first_interpolation(content: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'{' | b'}') {
            if bytes.get(i + 1) != Some(&bytes[i]) {
                return Some(i);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

fn send_string<'a>(
    node: Node<'a>,
    source: &str,
    synthetic_boundary: Option<usize>,
    start_at: usize,
) -> Option<Node<'a>> {
    if node.kind() == "call"
        && synthetic_boundary.map_or_else(
            || {
                !node.has_error()
                    && source.as_bytes().get(node.end_byte().saturating_sub(1)) == Some(&b')')
            },
            |end| node.end_byte() > end,
        )
        && node.parent()?.kind() == "expression_statement"
        && node.parent()?.parent()?.kind() == "module"
        && node
            .child_by_field_name("function")
            .is_some_and(|callee| &source[callee.byte_range()] == SEND)
    {
        let args = node.child_by_field_name("arguments")?;
        // A topic, the text, then only keywords (`mention=`, `kind=`).
        let mut cursor = args.walk();
        let keywords_only = args
            .named_children(&mut cursor)
            .skip(2)
            .all(|child| child.kind() == "keyword_argument");
        let topic = args
            .named_child(0)
            .is_some_and(|topic| topic.kind() != "keyword_argument");
        if args.named_child_count() >= 2 && topic && keywords_only {
            let arg = args.named_child(1)?;
            if !arg.has_error() {
                if synthetic_boundary.is_some()
                    && arg.kind() == "string"
                    && arg.named_child(0)?.start_byte() == start_at
                {
                    return Some(arg);
                }
                if synthetic_boundary.is_none()
                    && first_node(arg, "string_start")?.start_byte() == start_at
                {
                    return Some(arg);
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = send_string(child, source, synthetic_boundary, start_at) {
            return Some(found);
        }
    }
    None
}

fn decode(content: &str, fstring: bool, raw: bool) -> Option<String> {
    let bytes = content.as_bytes();
    let (mut i, mut output) = (0, String::new());
    while i < bytes.len() {
        if fstring && matches!(bytes[i], b'{' | b'}') {
            if bytes.get(i + 1) != Some(&bytes[i]) {
                break;
            }
            output.push(bytes[i] as char);
            i += 2;
        } else if bytes[i] == b'\\' {
            let Some(next) = content[i + 1..].chars().next() else {
                break;
            };
            if raw {
                output.push('\\');
                output.push(next);
                i += 1 + next.len_utf8();
            } else {
                i += 1;
                match next {
                    '\n' => i += 1,
                    '\\' | '\'' | '"' => {
                        output.push(next);
                        i += 1;
                    }
                    'n' | 'r' | 't' | 'a' | 'b' | 'f' | 'v' => {
                        output.push(match next {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            'a' => '\x07',
                            'b' => '\x08',
                            'f' => '\x0c',
                            _ => '\x0b',
                        });
                        i += 1;
                    }
                    'x' | 'u' | 'U' => {
                        let digits = match next {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let Some(slice) = bytes.get(i + 1..i + 1 + digits) else {
                            break;
                        };
                        let Ok(slice) = std::str::from_utf8(slice) else {
                            break;
                        };
                        let Ok(value) = u32::from_str_radix(slice, 16) else {
                            break;
                        };
                        let Some(c) = char::from_u32(value) else {
                            break;
                        };
                        output.push(c);
                        i += 1 + digits;
                    }
                    'N' => break, // Python's named Unicode table is deliberately unavailable.
                    '0'..='7' => {
                        let begin = i;
                        while i < (begin + 3).min(bytes.len()) && matches!(bytes[i], b'0'..=b'7') {
                            i += 1;
                        }
                        let value = u32::from_str_radix(&content[begin..i], 8).ok()?;
                        output.push(char::from_u32(value)?);
                    }
                    _ => {
                        output.push('\\');
                        output.push(next);
                        i += next.len_utf8();
                    }
                }
            }
        } else {
            let c = content[i..].chars().next()?;
            output.push(c);
            i += c.len_utf8();
        }
    }
    (!output.is_empty()).then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_prior_python_before_top_level_send() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for (source, expected) in [
            (
                "import os\nx = {'a': [1, 2]}\nchat.post(12, \"Hel",
                Some("Hel"),
            ),
            (
                "def f():\n    return {'a': 'chat.post(12, \\\"fake\\\")'}\nchat.post(12, 'real",
                Some("real"),
            ),
            ("chat.post(12, 'old')\nchat.post(12, 'new", Some("new")),
            (
                "chat.post(12, 'done', mention='user', kind='result')",
                Some("done"),
            ),
            (
                "end_turn()\nfor item in items:\n    print(item)\nchat.post(12, \"After work",
                Some("After work"),
            ),
        ] {
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                expected,
                "{source:?}"
            );
        }
    }

    #[test]
    fn only_the_last_direct_literal_is_a_draft() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for source in [
            "chat.post(12, value)",
            // Finished posts that do not mention the user.
            "chat.post(12, 'to the topic')",
            "chat.post(12, 'old', mention='user', kind='fyi')\nchat.post(12, 'to an agent', mention='eng-abc', kind='ask')",
            "chat.post(12, 'to agents', mention=['eng-a', 'eng-b'], kind='fyi')",
            "chat.post(12, 'to someone', mention=who, kind='fyi')",
            "chat.post(12, 'text', value)",
            "chat.post(12, 'old')\nchat.post(12, name + 'suffix",
            "# chat.post(12, 'comment",
            "print('chat.post(12, \\\"string')",
            "x =\nchat.post(12, 'invalid prior line", // The tree makes this an assignment RHS.
            "async def work():\n    x = await other(3)\n    if x:\n        chat.post(12, \"nested",
            "if False:\n    chat.post(12, 'untaken",
            "if False: chat.post(12, 'inline branch",
            "for item in items:\n    chat.post(12, 'loop body",
            "def f():\n    chat.post(12, 'function body",
            "class C:\n    chat.post(12, 'class body",
        ] {
            assert_eq!(preview(&mut parser, source), None, "{source:?}");
        }
    }

    #[test]
    fn plus_keeps_only_the_known_leading_strings() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for (source, expected) in [
            ("chat.post(12, 'Hi ' + name", Some("Hi ")),
            ("chat.post(12, 'Hi ' +", Some("Hi ")),
            ("chat.post(12, 'Hi ' + 'there'", Some("Hi there")),
            ("chat.post(12, 'Hi ' + 'the", Some("Hi the")),
            ("chat.post(12, '' + 'Hello", Some("Hello")),
            ("chat.post(12, 'Hi ' + '\\x0", Some("Hi ")),
            ("chat.post(12, 'Hi ' + f'{name}", Some("Hi ")),
            ("chat.post(12, 'Hi ' + b'bytes", Some("Hi ")),
            ("chat.post(12, 'Hi ' + 'there' + name", Some("Hi there")),
            ("chat.post(12, 'Hi ' + name + 'later'", Some("Hi ")),
            ("chat.post(12, 'Hi ' + str(name)", Some("Hi ")),
            ("chat.post(12, name + 'suffix'", None),
            ("chat.post(12, 'Hi ' * 0", None),
            (
                "chat.post(12, 'Hi ' + name, mention='user', kind='result')",
                Some("Hi "),
            ),
            (
                "chat.post(12, 'Hi ' + 'there' + name, mention='user', kind='result')",
                Some("Hi there"),
            ),
        ] {
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                expected,
                "{source:?}"
            );
        }
    }

    #[test]
    fn adjacent_literals_are_implicitly_concatenated() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for (source, expected) in [
            ("chat.post(12, \"foo\" \"bar", Some("foobar")),
            ("chat.post(12, \"foo\"\n \"bar", Some("foobar")),
            ("chat.post(12, \"foo\" f\"bar {name", Some("foobar ")),
            ("chat.post(12, \"foo\" r\"\\n", Some("foo\\n")),
            ("chat.post(12, \"foo\" + \"bar\" \"baz", Some("foobarbaz")),
            (
                "chat.post(12, \"foo\" \"bar\", mention='user', kind='result')",
                Some("foobar"),
            ),
            ("chat.post(12, \"foo\" name", None),
            ("chat.post(12, \"foo\" b\"bar", None),
        ] {
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                expected,
                "{source:?}"
            );
        }
    }

    #[test]
    fn quotes_escapes_and_fstring_prefixes() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for (source, expected) in [
            ("chat.post(12, \"Line\\nnext\\x21", Some("Line\nnext!")),
            ("chat.post(12, '''α\nbeta", Some("α\nbeta")),
            ("chat.post(12, \"pending\\u00", Some("pending")),
            ("chat.post(12, \"pending\\", Some("pending")),
            ("chat.post(12, f\"Hello {{name}}", Some("Hello {name}")),
            ("chat.post(12, f\"Hello {name", Some("Hello ")),
            ("chat.post(12, f\"Hello {", Some("Hello ")),
            (
                "chat.post(12, f\"Hello {{name}} {name",
                Some("Hello {name} "),
            ),
            ("chat.post(12, f\"Hello {name} later", Some("Hello ")),
            ("chat.post(12, f\"Hello {name} later\"", Some("Hello ")),
            ("print(1)\nchat.post(12, f\"Hello {name", Some("Hello ")),
            (
                "chat.post(12, f\"Hello {name} later\", mention='user', kind='result')",
                Some("Hello "),
            ),
            (
                "chat.post(12, f\"Hello {name} later\", mention='user', kind='result')\nx = 2",
                Some("Hello "),
            ),
        ] {
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                expected,
                "{source:?}"
            );
        }
    }

    #[test]
    fn all_prefixes_after_visible_text_keep_draft() {
        let cases = [
            "import asyncio\nawait asyncio.sleep(10)\nchat.post(12, \"\"\"The first line.\\nThe second line with a \\\"quote\\\" and α.\"\"\", mention='user', kind='result')",
            "chat.post(12, 'Hello, world!', mention='user', kind='result')",
            "chat.post(12, \"One line\\nThe next!\", mention='user', kind='result')",
            "chat.post(12, 'first' + ' second' + ' third', mention='user', kind='result')",
        ];
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for source in cases {
            let mut seen = false;
            for (i, _) in source
                .char_indices()
                .chain(std::iter::once((source.len(), '\0')))
            {
                let prefix = &source[..i];
                let current = preview(&mut parser, prefix);
                if current.is_some() {
                    seen = true;
                }
                if seen {
                    assert!(current.is_some(), "lost draft at {prefix:?}");
                }
            }
        }
    }
    #[test]
    fn multiline_triple_quoted_prefixes_keep_their_known_text() {
        let cases = [
            (
                "chat.post(12, \"\"\"First line\n\nSecond line with \"quotes\" and α.\"\"\", mention='user', kind='result')",
                "First line\n\nSecond line with \"quotes\" and α.",
            ),
            (
                "chat.post(12, \n    \"\"\"\nFirst line\nSecond line\n\"\"\"\n, mention='user', kind='result')",
                "\nFirst line\nSecond line\n",
            ),
            (
                "chat.post(12, '''First line\nSecond line with 'quotes'.''', mention='user', kind='result')",
                "First line\nSecond line with 'quotes'.",
            ),
            (
                "chat.post(12, f\"\"\"First line\nSecond line {name}!\"\"\", mention='user', kind='result')",
                "First line\nSecond line ",
            ),
            (
                "chat.post(12, r\"\"\"First line\nSecond line\\n\"\"\", mention='user', kind='result')",
                "First line\nSecond line\\n",
            ),
            (
                "chat.post(12, \"\"\"First line\"\"\" + \"\"\"Second line\"\"\", mention='user', kind='result')",
                "First lineSecond line",
            ),
            (
                "chat.post(12, \"\"\"Before (\"A Python\"). After.\"\"\", mention='user', kind='result')",
                "Before (\"A Python\"). After.",
            ),
        ];
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for (source, expected) in cases {
            let mut seen = false;
            for (i, _) in source
                .char_indices()
                .chain(std::iter::once((source.len(), '\0')))
            {
                let prefix = &source[..i];
                let draft = preview(&mut parser, prefix);
                if draft.is_some() {
                    seen = true;
                }
                if seen {
                    assert!(draft.is_some(), "lost draft at {prefix:?}");
                }
                if let Some(draft) = draft {
                    assert!(
                        expected.starts_with(&draft),
                        "incorrect draft {draft:?} at {prefix:?}"
                    );
                }
            }
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                Some(expected),
                "{source:?}"
            );
        }
    }

    #[test]
    fn partially_closed_triple_quote_keeps_body_without_delimiter() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for source in [
            "chat.post(12, \"\"\"Hello\"",
            "chat.post(12, \"\"\"Hello\"\"",
            "chat.post(12, \"\"\"Hello\"\"\", mention='user', kind='result')",
            "chat.post(12, '''Hello'",
            "chat.post(12, '''Hello''",
            "chat.post(12, '''Hello''', mention='user', kind='result')",
        ] {
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                Some("Hello"),
                "{source:?}"
            );
        }
    }

    #[test]
    fn streamed_claude_json_prefix_does_not_withdraw_visible_draft() {
        let source = "chat.post(12, \"\"\"First line (\"A Python\").\nSecond line with \"quotes\".\"\"\", mention='user', kind='result')";
        let arguments = serde_json::json!({"source": source}).to_string();
        let mut visible = false;
        for (i, _) in arguments
            .char_indices()
            .chain(std::iter::once((arguments.len(), '\0')))
        {
            let prefix = &arguments[..i];
            let draft = tool_preview(
                "exec",
                prefix,
                rho_agents_client::protocol::transcript::ArgumentsFormat::Json,
            );
            if draft.is_some() {
                visible = true;
            }
            if visible {
                assert!(draft.is_some(), "lost Claude draft at {prefix:?}");
            }
        }
    }
}
