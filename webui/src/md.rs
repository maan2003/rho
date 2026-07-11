//! Markdown-lite: enough for agent transcripts (fences, inline code, bold,
//! headings, list items, https links) without pulling in a full parser.
//! Escapes first, so model output can never inject markup.

pub fn render(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 64);
    let mut in_fence = false;
    let mut in_list = false;
    for line in source.lines() {
        if line.trim_start().starts_with("```") {
            close_list(&mut out, &mut in_list);
            if in_fence {
                out.push_str("</code></pre>");
            } else {
                out.push_str("<pre class=\"code\"><code>");
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(&escape(line));
            out.push('\n');
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(heading) = trimmed.strip_prefix('#') {
            close_list(&mut out, &mut in_list);
            let heading = heading.trim_start_matches('#').trim();
            out.push_str("<h4>");
            out.push_str(&inline(heading));
            out.push_str("</h4>");
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            if !in_list {
                out.push_str("<ul>");
                in_list = true;
            }
            out.push_str("<li>");
            out.push_str(&inline(item));
            out.push_str("</li>");
        } else if trimmed.is_empty() {
            close_list(&mut out, &mut in_list);
        } else {
            close_list(&mut out, &mut in_list);
            out.push_str("<p>");
            out.push_str(&inline(line));
            out.push_str("</p>");
        }
    }
    if in_fence {
        out.push_str("</code></pre>");
    }
    close_list(&mut out, &mut in_list);
    out
}

fn close_list(out: &mut String, in_list: &mut bool) {
    if *in_list {
        out.push_str("</ul>");
        *in_list = false;
    }
}

fn inline(text: &str) -> String {
    let escaped = escape(text);
    let mut out = String::with_capacity(escaped.len());
    // Alternate outside/inside inline code; markup only applies outside.
    for (index, part) in escaped.split('`').enumerate() {
        if index % 2 == 1 {
            out.push_str("<code>");
            out.push_str(part);
            out.push_str("</code>");
        } else {
            out.push_str(&links(&bold(part)));
        }
    }
    out
}

fn bold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, part) in text.split("**").enumerate() {
        if index % 2 == 1 {
            out.push_str("<strong>");
            out.push_str(part);
            out.push_str("</strong>");
        } else {
            out.push_str(part);
        }
    }
    out
}

/// `[text](https://…)` only; other schemes stay literal text.
fn links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some((label, url, tail)) = parse_link(&rest[open..]) else {
            out.push_str(&rest[..=open]);
            rest = &rest[open + 1..];
            continue;
        };
        out.push_str(&rest[..open]);
        out.push_str("<a href=\"");
        out.push_str(url);
        out.push_str("\" target=\"_blank\" rel=\"noopener noreferrer\">");
        out.push_str(label);
        out.push_str("</a>");
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn parse_link(text: &str) -> Option<(&str, &str, &str)> {
    let inner = text.strip_prefix('[')?;
    let close = inner.find(']')?;
    let (label, after) = inner.split_at(close);
    let after = after.strip_prefix("](")?;
    let end = after.find(')')?;
    let (url, tail) = after.split_at(end);
    if !url.starts_with("https://") || url.contains('"') {
        return None;
    }
    Some((label, url, &tail[1..]))
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_before_formatting() {
        let html = render("<script>alert(1)</script> **bold** `code`");
        assert!(!html.contains("<script>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn fences_and_links() {
        let html =
            render("```rust\nlet x = 1;\n```\n[docs](https://example.com) [bad](javascript:x)");
        assert!(html.contains("<pre class=\"code\"><code>let x = 1;"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(!html.contains("href=\"javascript:"));
    }
}
