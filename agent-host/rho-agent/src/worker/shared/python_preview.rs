//! Recover the literal prefix of a live Python `human.send` call.
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
        ("mcp__py__exec", ArgumentsFormat::Json) => {
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
    let string_end = start
        .parent()
        .filter(|parent| parent.kind() == "string")
        .and_then(|parent| {
            let mut cursor = parent.walk();
            parent
                .children(&mut cursor)
                .find(|child| child.kind() == "string_end")
        });
    let content_end = string_end.map_or(source.len(), |end| end.start_byte());
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
        format!("{closing})")
    };
    let repaired = format!("{prefix}{suffix}");
    let tree = parser.parse(&repaired, None)?;
    let string = send_string(tree.root_node(), &repaired, Some(prefix.len()), start_at)?;
    let completed = send_string(original.root_node(), source, None, start_at)
        .and_then(|arg| arg.parent()?.parent())
        .map(|call| call.end_byte());
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

fn last_send(node: Node<'_>, source: &str) -> Option<usize> {
    let mut latest = (node.kind() == "attribute" && &source[node.byte_range()] == "human.send")
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
        if i == bytes.len() {
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
                    .find(|child| child.kind() == "string_end")
            });
        let content_end = end.map_or(source.len(), |end| end.start_byte());
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
            .is_some_and(|callee| &source[callee.byte_range()] == "human.send")
    {
        let args = node.child_by_field_name("arguments")?;
        if args.named_child_count() == 1 {
            let arg = args.named_child(0)?;
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
                "import os\nx = {'a': [1, 2]}\nhuman.send(\"Hel",
                Some("Hel"),
            ),
            (
                "def f():\n    return {'a': 'human.send(\\\"fake\\\")'}\nhuman.send('real",
                Some("real"),
            ),
            ("human.send('old')\nhuman.send('new", Some("new")),
            (
                "await human.reply()\nfor item in items:\n    print(item)\nhuman.send(\"After work",
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
            "human.send(value)",
            "human.send('old')\nhuman.send(name + 'suffix",
            "# human.send('comment",
            "print('human.send(\\\"string')",
            "x =\nhuman.send('invalid prior line", // The tree makes this an assignment RHS.
            "async def work():\n    x = await other(3)\n    if x:\n        human.send(\"nested",
            "if False:\n    human.send('untaken",
            "if False: human.send('inline branch",
            "for item in items:\n    human.send('loop body",
            "def f():\n    human.send('function body",
            "class C:\n    human.send('class body",
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
            ("human.send('Hi ' + name", Some("Hi ")),
            ("human.send('Hi ' +", Some("Hi ")),
            ("human.send('Hi ' + 'there'", Some("Hi there")),
            ("human.send('Hi ' + 'the", Some("Hi the")),
            ("human.send('' + 'Hello", Some("Hello")),
            ("human.send('Hi ' + '\\x0", Some("Hi ")),
            ("human.send('Hi ' + f'{name}", Some("Hi ")),
            ("human.send('Hi ' + b'bytes", Some("Hi ")),
            ("human.send('Hi ' + 'there' + name", Some("Hi there")),
            ("human.send('Hi ' + name + 'later'", Some("Hi ")),
            ("human.send('Hi ' + str(name)", Some("Hi ")),
            ("human.send(name + 'suffix'", None),
            ("human.send('Hi ' * 0", None),
            ("human.send('Hi ' + name)", Some("Hi ")),
            ("human.send('Hi ' + 'there' + name)", Some("Hi there")),
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
            ("human.send(\"foo\" \"bar", Some("foobar")),
            ("human.send(\"foo\"\n \"bar", Some("foobar")),
            ("human.send(\"foo\" f\"bar {name", Some("foobar ")),
            ("human.send(\"foo\" r\"\\n", Some("foo\\n")),
            ("human.send(\"foo\" + \"bar\" \"baz", Some("foobarbaz")),
            ("human.send(\"foo\" \"bar\")", Some("foobar")),
            ("human.send(\"foo\" name", None),
            ("human.send(\"foo\" b\"bar", None),
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
            ("human.send(\"Line\\nnext\\x21", Some("Line\nnext!")),
            ("human.send('''α\nbeta", Some("α\nbeta")),
            ("human.send(\"pending\\u00", Some("pending")),
            ("human.send(\"pending\\", Some("pending")),
            ("human.send(f\"Hello {{name}}", Some("Hello {name}")),
            ("human.send(f\"Hello {name", Some("Hello ")),
            ("human.send(f\"Hello {", Some("Hello ")),
            ("human.send(f\"Hello {{name}} {name", Some("Hello {name} ")),
            ("human.send(f\"Hello {name} later", Some("Hello ")),
            ("human.send(f\"Hello {name} later\"", Some("Hello ")),
            ("print(1)\nhuman.send(f\"Hello {name", Some("Hello ")),
            ("human.send(f\"Hello {name} later\")", Some("Hello ")),
            ("human.send(f\"Hello {name} later\")\nx = 2", Some("Hello ")),
        ] {
            assert_eq!(
                preview(&mut parser, source).as_deref(),
                expected,
                "{source:?}"
            );
        }
    }
}
