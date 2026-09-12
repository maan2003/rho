//! Shortcodes to glyphs.
//!
//! Slack sends `:thumbsup:` and every Slack client shows 👍. Rho does the
//! same, with two exceptions the reader would otherwise be lied to about:
//! a workspace's custom emoji has no glyph anywhere but Slack, so it stays a
//! shortcode, and code keeps whatever was typed in it.

use std::ops::Range;

/// Replaces every standard shortcode with its glyph. Unknown shortcodes,
/// which is what a custom emoji looks like from here, are left alone.
pub fn render(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (range, literal) in scan(text) {
        out.push_str(&text[cursor..range.start]);
        let name = &text[range.start + 1..range.end - 1];
        match (literal, emojis::get_by_shortcode(name)) {
            (false, Some(emoji)) => out.push_str(emoji.as_str()),
            _ => out.push_str(&text[range.clone()]),
        }
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// The `:name:` tokens still standing in rendered text: the custom emoji a
/// workspace defined, which the reader sees as a shortcode and which the UI
/// mutes so it does not read as a word.
pub fn shortcodes(text: &str) -> Vec<Range<usize>> {
    scan(text)
        .into_iter()
        .filter(|(range, literal)| {
            !literal && emojis::get_by_shortcode(&text[range.start + 1..range.end - 1]).is_none()
        })
        .map(|(range, _)| range)
        .collect()
}

/// Every `:name:` in `text`, flagged with whether it sits inside code, where
/// Slack leaves it as typed.
fn scan(text: &str) -> Vec<(Range<usize>, bool)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    let mut in_code = false;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            // A fence and a span both toggle the same way: what matters is
            // only whether the scanner is inside code right now.
            in_code = !in_code;
            index += 1;
            continue;
        }
        if bytes[index] != b':' {
            index += 1;
            continue;
        }
        let name_start = index + 1;
        let Some(offset) = bytes[name_start..]
            .iter()
            .position(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-' | b'+'))
        else {
            break;
        };
        let end = name_start + offset;
        if end > name_start && bytes[end] == b':' {
            found.push((index..end + 1, in_code));
            index = end + 1;
            continue;
        }
        index += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_shortcodes_become_glyphs_and_custom_ones_do_not() {
        assert_eq!(render("nice :thumbsup:"), "nice 👍");
        assert_eq!(render("morning :wave: :sweat_smile:"), "morning 👋 😅");
        assert_eq!(
            render("hi :forrest_gump_wave:"),
            "hi :forrest_gump_wave:",
            "a workspace emoji has no glyph outside Slack"
        );
        assert_eq!(render("a 10:30 start"), "a 10:30 start");
    }

    #[test]
    fn code_keeps_what_was_typed_in_it() {
        assert_eq!(render("`:thumbsup:`"), "`:thumbsup:`");
        assert_eq!(
            render("```\nprintln!(\":wave:\")\n```"),
            "```\nprintln!(\":wave:\")\n```"
        );
        assert_eq!(render("`code` then :wave:"), "`code` then 👋");
    }

    #[test]
    fn only_the_custom_shortcodes_are_offered_for_muting() {
        let text = render("👋 :forrest_gump_wave: and `:wave:`");
        let ranges = shortcodes(&text);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].clone()], ":forrest_gump_wave:");
    }
}
