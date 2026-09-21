//! Shortcodes to glyphs.
//!
//! Slack sends `:thumbsup:` and every Slack client shows 👍. Rho does the
//! same, with two exceptions the reader would otherwise be lied to about:
//! a workspace's custom emoji has no glyph anywhere but Slack, so it stays a
//! shortcode, and code keeps whatever was typed in it.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

const SLACK_SHORTCODE_DATA: &str = include_str!("../assets/iamcal-emoji-data/slack-shortcodes.tsv");

static SLACK_SHORTCODES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    SLACK_SHORTCODE_DATA
        .lines()
        .map(|line| {
            line.split_once('\t')
                .expect("generated Slack shortcode row contains a tab")
        })
        .collect()
});

struct StandardEmoji {
    glyph: &'static str,
    emoji: Option<&'static emojis::Emoji>,
}

fn standard_emoji(name: &str) -> Option<StandardEmoji> {
    // Modifiers are meaningful only after a modifiable base. Leaving a
    // standalone modifier as a shortcode also preserves custom-emoji lookup.
    if skin_tone(name).is_some() {
        return None;
    }

    if let Some(&glyph) = SLACK_SHORTCODES.get(name) {
        return Some(StandardEmoji {
            glyph,
            emoji: emojis::get(glyph),
        });
    }

    emojis::get_by_shortcode(name).map(|emoji| StandardEmoji {
        glyph: emoji.as_str(),
        emoji: Some(emoji),
    })
}

/// Replaces every standard shortcode with its glyph. Unknown shortcodes,
/// which is what a custom emoji looks like from here, are left alone.
pub fn render(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut tokens = scan(text).into_iter().peekable();
    while let Some((range, literal)) = tokens.next() {
        out.push_str(&text[cursor..range.start]);
        let name = &text[range.start + 1..range.end - 1];
        cursor = range.end;
        match (literal, standard_emoji(name)) {
            (false, Some(standard)) => {
                let mut glyph = standard.glyph;
                if let Some((tone_range, false)) = tokens.peek()
                    && tone_range.start == cursor
                    && let Some(tone) = skin_tone(&text[tone_range.start + 1..tone_range.end - 1])
                    && let Some(toned) = standard.emoji.and_then(|emoji| emoji.with_skin_tone(tone))
                {
                    glyph = toned.as_str();
                    cursor = tone_range.end;
                    tokens.next();
                }
                out.push_str(glyph);
            }
            _ => out.push_str(&text[range]),
        }
    }
    out.push_str(&text[cursor..]);
    out
}

fn skin_tone(name: &str) -> Option<emojis::SkinTone> {
    use emojis::SkinTone;
    match name {
        "skin-tone-2" => Some(SkinTone::Light),
        "skin-tone-3" => Some(SkinTone::MediumLight),
        "skin-tone-4" => Some(SkinTone::Medium),
        "skin-tone-5" => Some(SkinTone::MediumDark),
        "skin-tone-6" => Some(SkinTone::Dark),
        _ => None,
    }
}

/// The `:name:` tokens still standing in rendered text: the custom emoji a
/// workspace defined, which the reader sees as a shortcode and which the UI
/// mutes so it does not read as a word.
pub fn shortcodes(text: &str) -> Vec<Range<usize>> {
    scan(text)
        .into_iter()
        .filter(|(range, literal)| {
            !literal && standard_emoji(&text[range.start + 1..range.end - 1]).is_none()
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
    fn slack_skin_tones_modify_the_whole_emoji_sequence() {
        assert_eq!(
            render(
                ":thumbsup::skin-tone-2: :wave::skin-tone-3: :thumbsup::skin-tone-4: :wave::skin-tone-5: :thumbsup::skin-tone-6:"
            ),
            "👍🏻 👋🏼 👍🏽 👋🏾 👍🏿"
        );
        assert_eq!(render(":woman_technologist::skin-tone-4:"), "👩🏽‍💻");
        assert_eq!(render("👩🏽‍💻 👍🏿"), "👩🏽‍💻 👍🏿");
        assert_eq!(
            render("`:thumbsup::skin-tone-4:`"),
            "`:thumbsup::skin-tone-4:`"
        );
        assert_eq!(render(":wave: :skin-tone-4:"), "👋 :skin-tone-4:");
        assert_eq!(render(":heart::skin-tone-4:"), "❤️:skin-tone-4:");
        assert_eq!(render(":custom::skin-tone-4:"), ":custom::skin-tone-4:");
        assert_eq!(render(":thumbsup::skin-tone-7:"), "👍:skin-tone-7:");
    }

    #[test]
    fn slack_aliases_use_qualified_unicode() {
        assert_eq!(
            render(":thinking_face: :rolling_on_the_floor_laughing:"),
            "🤔 🤣"
        );
        assert_eq!(
            render(":skull_and_crossbones: :white_frowning_face:"),
            "☠️ ☹️",
            "text-default symbols retain Slack's emoji-presentation selector"
        );
        assert_eq!(SLACK_SHORTCODES.len(), 1972);
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
