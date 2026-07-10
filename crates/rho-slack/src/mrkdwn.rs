//! Convert agent-authored Markdown to Slack mrkdwn.
//!
//! Slack does not render standard Markdown: `**bold**` shows literal
//! asterisks (mrkdwn bold is `*bold*`), `[label](url)` stays raw (mrkdwn
//! links are `<url|label>`), and `# headers` don't exist (rendered as a bold
//! line). `<` and `&` must be entity-escaped or Slack treats them as control
//! sequences; `>` is left alone so blockquotes keep working. Code spans and
//! fenced blocks are preserved verbatim (minus the required escaping).

pub fn to_mrkdwn(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    // Alternate between prose and ```fenced``` regions; fences pass through
    // untransformed.
    while let Some(start) = rest.find("```") {
        convert_prose(&mut out, &rest[..start]);
        let after = &rest[start + 3..];
        match after.find("```") {
            Some(end) => {
                escape_into(&mut out, &rest[start..start + 3 + end + 3]);
                rest = &after[end + 3..];
            }
            None => {
                escape_into(&mut out, &rest[start..]);
                return out;
            }
        }
    }
    convert_prose(&mut out, rest);
    out
}

fn convert_prose(out: &mut String, text: &str) {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        convert_line(out, line);
    }
}

fn convert_line(out: &mut String, line: &str) {
    // `# header` → a bold line.
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        out.push('*');
        convert_inline(out, line[hashes + 1..].trim());
        out.push('*');
        return;
    }
    convert_inline(out, line);
}

fn convert_inline(out: &mut String, line: &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // `inline code` passes through verbatim.
            b'`' => match line[i + 1..].find('`') {
                Some(end) => {
                    escape_into(out, &line[i..i + end + 2]);
                    i += end + 2;
                }
                None => {
                    out.push('`');
                    i += 1;
                }
            },
            b'[' => match parse_link(&line[i..]) {
                Some((label, url, consumed)) => {
                    out.push('<');
                    out.push_str(url);
                    out.push('|');
                    escape_into(out, label);
                    out.push('>');
                    i += consumed;
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            // `**bold**` → `*bold*`, `~~strike~~` → `~strike~`.
            b'*' if bytes.get(i + 1) == Some(&b'*') => {
                out.push('*');
                i += 2;
            }
            b'~' if bytes.get(i + 1) == Some(&b'~') => {
                out.push('~');
                i += 2;
            }
            _ => {
                let ch = line[i..].chars().next().expect("in-bounds char");
                escape_char(out, ch);
                i += ch.len_utf8();
            }
        }
    }
}

/// `[label](http…)` starting at the front of `s`; returns (label, url,
/// bytes consumed).
fn parse_link(s: &str) -> Option<(&str, &str, usize)> {
    let close = s.find(']')?;
    let label = &s[1..close];
    let after = &s[close + 1..];
    let url = after.strip_prefix('(')?;
    let end = url.find(')')?;
    let url = &url[..end];
    if label.contains('[') || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    Some((label, url, close + 1 + 1 + end + 1))
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        escape_char(out, ch);
    }
}

fn escape_char(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        _ => out.push(ch),
    }
}

#[cfg(test)]
mod tests {
    use super::to_mrkdwn;

    #[test]
    fn bold_links_headers_and_strike() {
        assert_eq!(to_mrkdwn("**bold** and ~~gone~~"), "*bold* and ~gone~");
        assert_eq!(
            to_mrkdwn("see [the docs](https://example.com/a?b=1)"),
            "see <https://example.com/a?b=1|the docs>"
        );
        assert_eq!(to_mrkdwn("## Heading **x**\nbody"), "*Heading *x**\nbody");
    }

    #[test]
    fn code_is_preserved_and_control_chars_escaped() {
        assert_eq!(to_mrkdwn("run `a ** b` now"), "run `a ** b` now");
        assert_eq!(
            to_mrkdwn("```\n**not bold** [x](https://y)\n```"),
            "```\n**not bold** [x](https://y)\n```"
        );
        assert_eq!(to_mrkdwn("a < b && c"), "a &lt; b &amp;&amp; c");
        // '>' survives so blockquotes keep rendering.
        assert_eq!(to_mrkdwn("> quoted"), "> quoted");
    }

    #[test]
    fn non_links_pass_through() {
        assert_eq!(to_mrkdwn("array[0] (note)"), "array[0] (note)");
        assert_eq!(to_mrkdwn("[label](not a url)"), "[label](not a url)");
    }
}
