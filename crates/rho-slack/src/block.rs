//! Block Kit rendered to plain text, with the rules ported from
//! `slack-block.el`.
//!
//! The reader wants the message, not the layout: text is what the editor can
//! search and the user can yank. Emphasis keeps Slack's own mrkdwn markers
//! (`*bold*`, `_italic_`, `~strike~`, backticks) because that is what the
//! composer accepts back, so a yanked line can be pasted into a reply.
//!
//! Ids never survive rendering. A user or channel element becomes a name;
//! when the name is not known yet the element renders as the raw handle
//! Slack itself would show, never as `U024BE7LH`.

use serde_json::Value;

use crate::types::{Attachment, ChannelId, FileSummary, UserId};

/// Whatever the model knows about names right now. Rendering is a pure
/// function of the payload plus this, so a message re-renders correctly once
/// a late `users.info` fills a gap.
pub trait Names {
    fn user(&self, id: &UserId) -> Option<String>;
    fn channel(&self, id: &ChannelId) -> Option<String>;
}

/// No names known: every mention falls back to its placeholder. Used by the
/// renderer tests and before the first roster load.
pub struct NoNames;

impl Names for NoNames {
    fn user(&self, _id: &UserId) -> Option<String> {
        None
    }

    fn channel(&self, _id: &ChannelId) -> Option<String> {
        None
    }
}

/// Renders a whole message body: its blocks (or its plain `text` when it has
/// none), then attachment and file titles.
pub fn render_message(
    blocks: &[Value],
    text: &str,
    attachments: &[Attachment],
    files: &[FileSummary],
    names: &dyn Names,
) -> String {
    let mut rendered = if blocks.is_empty() {
        render_mrkdwn(text, names)
    } else {
        let parts = blocks
            .iter()
            .map(|block| render_block(block, names))
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>();
        parts.join("\n")
    };
    for attachment in attachments {
        for line in render_attachment(attachment, names) {
            push_line(&mut rendered, &line);
        }
    }
    for file in files {
        // A file is a thing the reader can open, named the way it would be
        // in a shell: no placeholder, no id.
        push_line(&mut rendered, &file.line());
    }
    // Shortcodes become glyphs last, so it happens once for blocks, plain
    // text, and attachment lines alike.
    crate::emoji::render(rendered.trim_end())
}

/// What the reader sees for a link, and where it points: the rendered text
/// shows the label alone, so the URL travels beside it and reaches the line
/// metadata `enter` reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub label: String,
    pub url: String,
}

/// Every link in a message, in the order the renderer prints them. Walked
/// from the source rather than from the rendered text, because the rendered
/// text no longer carries the URL.
pub fn links(blocks: &[Value], text: &str, attachments: &[Attachment]) -> Vec<Link> {
    let mut found = Vec::new();
    if blocks.is_empty() {
        mrkdwn_links(text, &mut found);
    } else {
        for block in blocks {
            block_links(block, &mut found);
        }
    }
    for attachment in attachments {
        if let Some(url) = attachment.url.clone() {
            let label = attachment
                .title
                .clone()
                .or_else(|| attachment.text.clone())
                .or_else(|| attachment.fallback.clone())
                .unwrap_or_else(|| url.clone());
            found.push(Link { label, url });
        }
    }
    found
}

fn block_links(block: &Value, found: &mut Vec<Link>) {
    match string(block, "type") {
        "rich_text"
        | "rich_text_section"
        | "rich_text_quote"
        | "rich_text_preformatted"
        | "rich_text_list"
        | "context"
        | "actions" => {
            for element in array(block, "elements") {
                block_links(element, found);
            }
        }
        "link" => {
            let url = string(block, "url").to_owned();
            let label = match string(block, "text") {
                "" => url.clone(),
                text => text.to_owned(),
            };
            if !url.is_empty() {
                found.push(Link { label, url });
            }
        }
        "section" | "header" => {
            if let Some(text) = block.get("text").map(|text| string(text, "text")) {
                mrkdwn_links(text, found);
            }
            for field in array(block, "fields") {
                mrkdwn_links(string(field, "text"), found);
            }
        }
        _ => {}
    }
}

/// The `<url|label>` escapes in a plain-text body, in the order they appear.
fn mrkdwn_links(text: &str, found: &mut Vec<Link>) {
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            return;
        };
        let body = &after[..end];
        rest = &after[end + 1..];
        let (target, label) = match body.split_once('|') {
            Some((target, label)) => (target, Some(label)),
            None => (body, None),
        };
        if matches!(target.chars().next(), Some('@' | '#' | '!') | None) {
            continue;
        }
        let url = unescape_entities(target);
        found.push(Link {
            label: label.map(unescape_entities).unwrap_or_else(|| url.clone()),
            url,
        });
    }
}

/// The bar down the left of an unfurl. Every line of the card carries it,
/// which is what makes the card one thing rather than several lines.
pub const UNFURL_BAR: &str = "\u{258e} ";

/// How much of someone else's page an unfurl is allowed to bring with it.
const UNFURL_LINES: usize = 2;

/// An attachment: a link preview, or an app's own card.
///
/// A preview collapses to its title. Slack paints the whole page under the
/// message, which buries the conversation the reader came for; the title is
/// the part they act on and the link is already in the message above it.
/// An app card keeps what it was given: its pretext, title, body, and the
/// labelled values it hung under them.
fn render_attachment(attachment: &Attachment, names: &dyn Names) -> Vec<String> {
    let headline = attachment
        .title
        .clone()
        .or_else(|| attachment.text.clone())
        .or_else(|| attachment.fallback.clone())
        .unwrap_or_default();
    if headline.trim().is_empty() {
        return Vec::new();
    }
    if attachment.is_unfurl {
        // A quote box, not loose lines: the reader sees one card hanging off
        // the message. The title names the page, the site says where it is,
        // and two lines of description are as much of someone else's web
        // page as a conversation should carry.
        let title = render_mrkdwn(&headline, names);
        let head = match attachment.service.as_deref() {
            Some(site) if !site.is_empty() => format!("{title} · {site}"),
            _ => title.clone(),
        };
        let mut lines = vec![format!("{UNFURL_BAR}{head}")];
        if let Some(text) = attachment.text.as_deref().filter(|text| *text != headline) {
            lines.extend(
                render_mrkdwn(text, names)
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .take(UNFURL_LINES)
                    .map(|line| format!("{UNFURL_BAR}{line}")),
            );
        }
        return lines;
    }
    let mut lines = Vec::new();
    if let Some(pretext) = &attachment.pretext {
        lines.push(format!("— {}", render_mrkdwn(pretext, names)));
    }
    lines.push(format!("— {}", render_mrkdwn(&headline, names)));
    if let Some(text) = &attachment.text {
        if Some(text) != attachment.title.as_ref() {
            lines.push(format!("  {}", render_mrkdwn(text, names)));
        }
    }
    if !attachment.fields.is_empty() {
        let fields = attachment
            .fields
            .iter()
            .map(|(title, value)| format!("{title}: {}", render_mrkdwn(value, names)))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(format!("  {fields}"));
    }
    lines
}

fn push_line(target: &mut String, line: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(line);
}

pub fn render_block(block: &Value, names: &dyn Names) -> String {
    match string(block, "type") {
        "rich_text" => array(block, "elements")
            .iter()
            .map(|element| render_rich_text_element(element, names))
            .collect::<String>(),
        "section" => {
            let mut parts = Vec::new();
            let text = render_text_object(block.get("text"), names);
            if !text.is_empty() {
                parts.push(text);
            }
            let fields = array(block, "fields")
                .iter()
                .map(|field| render_text_object(Some(field), names))
                .filter(|field| !field.is_empty())
                .collect::<Vec<_>>();
            if !fields.is_empty() {
                parts.push(fields.join("\n"));
            }
            parts.join("\n")
        }
        "header" => {
            let text = render_text_object(block.get("text"), names);
            match text.is_empty() {
                true => String::new(),
                false => format!("# {text}"),
            }
        }
        "divider" => "———".to_owned(),
        "image" => {
            let title = render_text_object(block.get("title"), names);
            let alt = string(block, "alt_text");
            match (title.is_empty(), alt.is_empty()) {
                (false, _) => format!("[image: {title}]"),
                (true, false) => format!("[image: {alt}]"),
                (true, true) => "[image]".to_owned(),
            }
        }
        "context" => array(block, "elements")
            .iter()
            .map(|element| match string(element, "type") {
                "image" => {
                    let alt = string(element, "alt_text");
                    match alt.is_empty() {
                        true => "[image]".to_owned(),
                        false => format!("[image: {alt}]"),
                    }
                }
                _ => render_text_object(Some(element), names),
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        // Nothing is interactive in this version, so an action row renders
        // as the labels it offers — enough to know the message wanted a
        // click, without pretending rho can deliver one.
        "actions" => array(block, "elements")
            .iter()
            .map(|element| {
                let label = render_text_object(element.get("text"), names);
                match label.is_empty() {
                    true => String::new(),
                    false => format!("[{label}]"),
                }
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        // An unknown block renders as nothing rather than as a debug dump:
        // Slack adds block types constantly and none of them are worth
        // showing a reader raw JSON over.
        _ => String::new(),
    }
}

fn render_rich_text_element(element: &Value, names: &dyn Names) -> String {
    match string(element, "type") {
        "rich_text_section" => inline(element, names),
        "rich_text_preformatted" => format!("```\n{}\n```\n", inline(element, names).trim_end()),
        "rich_text_quote" => {
            let text = inline(element, names);
            let quoted = text
                .trim_end_matches('\n')
                .split('\n')
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{quoted}\n")
        }
        "rich_text_list" => {
            let indent = " ".repeat(2 * usize::try_from(number(element, "indent")).unwrap_or(0));
            let ordered = string(element, "style") == "ordered";
            let items = array(element, "elements")
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let bullet = match ordered {
                        true => format!("{}.", index + 1),
                        false => "-".to_owned(),
                    };
                    let text = render_rich_text_element(item, names);
                    format!("{indent}{bullet} {}", text.trim_end())
                })
                .collect::<Vec<_>>();
            format!("{}\n", items.join("\n"))
        }
        _ => inline(element, names),
    }
}

fn inline(element: &Value, names: &dyn Names) -> String {
    array(element, "elements")
        .iter()
        .map(|element| render_inline(element, names))
        .collect()
}

fn render_inline(element: &Value, names: &dyn Names) -> String {
    let text = match string(element, "type") {
        "text" => string(element, "text").to_owned(),
        "user" => {
            let id = UserId(string(element, "user_id").to_owned());
            format!(
                "@{}",
                names.user(&id).unwrap_or_else(|| "someone".to_owned())
            )
        }
        "channel" => {
            let id = ChannelId(string(element, "channel_id").to_owned());
            format!(
                "#{}",
                names.channel(&id).unwrap_or_else(|| "a channel".to_owned())
            )
        }
        "usergroup" => {
            let handle = string(element, "handle");
            match handle.is_empty() {
                true => "@group".to_owned(),
                false => format!("@{handle}"),
            }
        }
        "emoji" => format!(":{}:", string(element, "name")),
        "broadcast" => format!("@{}", string(element, "range")),
        // The label is all the reader needs; printing the URL beside it
        // says the same thing twice. `links` carries the URL to the line,
        // which is what `enter` opens.
        "link" => {
            let url = string(element, "url");
            match string(element, "text") {
                "" => url.to_owned(),
                text => text.to_owned(),
            }
        }
        // A date element always ships the text Slack itself would show.
        "date" => string(element, "fallback").to_owned(),
        "team" => format!("@{}", string(element, "name")),
        "message_mention" => {
            let author = element
                .get("author_id")
                .and_then(Value::as_str)
                .and_then(|id| names.user(&UserId(id.to_owned())));
            let channel = element
                .get("channel_id")
                .and_then(Value::as_str)
                .and_then(|id| names.channel(&ChannelId(id.to_owned())));
            match (author, channel) {
                (Some(author), Some(channel)) => format!("@{author} in #{channel}"),
                (Some(author), None) => format!("@{author}"),
                (None, Some(channel)) => format!("#{channel}"),
                (None, None) => string(element, "url").to_owned(),
            }
        }
        "attachment_mention" | "canvas" | "citation" => {
            let text = string(element, "text");
            match text.is_empty() {
                true => string(element, "url").to_owned(),
                false => text.to_owned(),
            }
        }
        _ => String::new(),
    };
    apply_style(element.get("style"), &text)
}

/// Slack's own emphasis markers, which is also what the composer accepts, so
/// yanking a line out of a thread and pasting it into a reply round-trips.
fn apply_style(style: Option<&Value>, text: &str) -> String {
    let Some(style) = style else {
        return text.to_owned();
    };
    let flag = |name: &str| style.get(name).and_then(Value::as_bool).unwrap_or(false);
    if text.is_empty() {
        return text.to_owned();
    }
    if flag("code") {
        return format!("`{text}`");
    }
    if flag("bold") {
        return format!("*{text}*");
    }
    if flag("italic") {
        return format!("_{text}_");
    }
    if flag("strike") {
        return format!("~{text}~");
    }
    text.to_owned()
}

fn render_text_object(object: Option<&Value>, names: &dyn Names) -> String {
    let Some(object) = object else {
        return String::new();
    };
    render_mrkdwn(string(object, "text"), names)
}

/// Resolves the escapes Slack's older `mrkdwn` strings carry: `<@U…>`,
/// `<#C…|name>`, `<https://…|text>`, `<!here>`, and the three XML entities.
/// A message with blocks never needs this, but plain-text messages, older
/// posts, and attachment bodies all do.
pub fn render_mrkdwn(text: &str, names: &dyn Names) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        out.push_str(&render_escape(&after[..end], names));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    unescape_entities(&out)
}

fn render_escape(body: &str, names: &dyn Names) -> String {
    let (target, label) = match body.split_once('|') {
        Some((target, label)) => (target, Some(label)),
        None => (body, None),
    };
    match target.chars().next() {
        Some('@') => {
            let id = UserId(target[1..].to_owned());
            let name = names
                .user(&id)
                .or_else(|| label.map(str::to_owned))
                .unwrap_or_else(|| "someone".to_owned());
            format!("@{name}")
        }
        Some('#') => {
            let id = ChannelId(target[1..].to_owned());
            let name = names
                .channel(&id)
                .or_else(|| label.map(str::to_owned))
                .unwrap_or_else(|| "a channel".to_owned());
            format!("#{name}")
        }
        // `<!here>`, `<!channel>`, `<!subteam^S123|@team>`.
        Some('!') => match label {
            Some(label) => label.to_owned(),
            None => format!("@{}", target[1..].split('^').next().unwrap_or_default()),
        },
        _ => match label {
            Some(label) => label.to_owned(),
            None => target.to_owned(),
        },
    }
}

fn unescape_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn number(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Roster;

    impl Names for Roster {
        fn user(&self, id: &UserId) -> Option<String> {
            match id.0.as_str() {
                "U1" => Some("ada".to_owned()),
                "U2" => Some("grace".to_owned()),
                _ => None,
            }
        }

        fn channel(&self, id: &ChannelId) -> Option<String> {
            (id.0 == "C1").then(|| "design".to_owned())
        }
    }

    fn render(block: Value) -> String {
        render_block(&block, &Roster)
    }

    #[test]
    fn rich_text_resolves_mentions_and_keeps_emphasis() {
        let rendered = render(json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_section",
                "elements": [
                    {"type": "user", "user_id": "U1"},
                    {"type": "text", "text": " look at "},
                    {"type": "channel", "channel_id": "C1"},
                    {"type": "text", "text": " "},
                    {"type": "text", "text": "now", "style": {"bold": true}},
                    {"type": "text", "text": " "},
                    {"type": "emoji", "name": "wave"},
                    {"type": "broadcast", "range": "here"},
                ],
            }],
        }));
        assert_eq!(rendered, "@ada look at #design *now* :wave:@here");
    }

    #[test]
    fn unknown_ids_never_leak_into_the_text() {
        let rendered = render(json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_section",
                "elements": [
                    {"type": "user", "user_id": "U404"},
                    {"type": "text", "text": " in "},
                    {"type": "channel", "channel_id": "C404"},
                ],
            }],
        }));
        assert_eq!(rendered, "@someone in #a channel");
        assert!(!rendered.contains("U404"));
        assert!(!rendered.contains("C404"));
    }

    #[test]
    fn lists_quotes_and_code_carry_their_shape() {
        let rendered = render(json!({
            "type": "rich_text",
            "elements": [
                {
                    "type": "rich_text_list",
                    "style": "ordered",
                    "indent": 1,
                    "elements": [
                        {"type": "rich_text_section", "elements": [{"type": "text", "text": "first"}]},
                        {"type": "rich_text_section", "elements": [{"type": "text", "text": "second"}]},
                    ],
                },
                {
                    "type": "rich_text_quote",
                    "elements": [{"type": "text", "text": "they said\nthis"}],
                },
                {
                    "type": "rich_text_preformatted",
                    "elements": [{"type": "text", "text": "cargo test\n"}],
                },
            ],
        }));
        assert_eq!(
            rendered,
            "  1. first\n  2. second\n> they said\n> this\n```\ncargo test\n```\n"
        );
    }

    #[test]
    fn links_read_as_text_with_the_url() {
        let rendered = render(json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_section",
                "elements": [
                    {"type": "link", "url": "https://rho.example/x", "text": "the plan"},
                    {"type": "text", "text": " and "},
                    {"type": "link", "url": "https://bare.example"},
                ],
            }],
        }));
        assert_eq!(
            rendered, "the plan and https://bare.example",
            "a labelled link reads as its label; a bare one is its own label"
        );
        let links = links(
            &[json!({
                "type": "rich_text",
                "elements": [{
                    "type": "rich_text_section",
                    "elements": [
                        {"type": "link", "url": "https://rho.example/x", "text": "the plan"},
                        {"type": "link", "url": "https://bare.example"},
                    ],
                }],
            })],
            "",
            &[],
        );
        assert_eq!(
            links
                .iter()
                .map(|link| (link.label.as_str(), link.url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("the plan", "https://rho.example/x"),
                ("https://bare.example", "https://bare.example"),
            ],
            "the address the text no longer shows still reaches the line"
        );
    }

    #[test]
    fn layout_blocks_render_as_headings_fields_and_titles() {
        assert_eq!(
            render(json!({"type": "header", "text": {"type": "plain_text", "text": "Release"}})),
            "# Release"
        );
        assert_eq!(render(json!({"type": "divider"})), "———");
        assert_eq!(
            render(json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": "hi <@U2>"},
                "fields": [{"type": "mrkdwn", "text": "*owner*"}, {"type": "mrkdwn", "text": "ada"}],
            })),
            "hi @grace\n*owner*\nada"
        );
        assert_eq!(
            render(json!({"type": "image", "alt_text": "a graph", "image_url": "https://x/y.png"})),
            "[image: a graph]"
        );
        assert_eq!(
            render(json!({"type": "context", "elements": [
                {"type": "mrkdwn", "text": "posted by <@U1>"},
                {"type": "image", "alt_text": "avatar"},
            ]})),
            "posted by @ada [image: avatar]"
        );
        // A block rho does not know about is silence, not a debug dump.
        assert_eq!(
            render(json!({"type": "some_new_block", "elements": []})),
            ""
        );
    }

    #[test]
    fn mrkdwn_escapes_and_entities_resolve() {
        assert_eq!(
            render_mrkdwn(
                "<@U1> see <#C1|design> and <https://x.example|docs> &amp; <!here>",
                &Roster
            ),
            "@ada see #design and docs & @here"
        );
        let mut found = Vec::new();
        mrkdwn_links("see <https://x.example|docs> and <@U1>", &mut found);
        assert_eq!(
            found,
            vec![Link {
                label: "docs".to_owned(),
                url: "https://x.example".to_owned(),
            }],
            "a mention is not a link"
        );
        // An unterminated escape is text, not a panic.
        assert_eq!(render_mrkdwn("a < b", &Roster), "a < b");
    }

    #[test]
    fn an_app_card_keeps_its_fields_and_a_link_preview_collapses() {
        let card = Attachment {
            title: Some("build #412".to_owned()),
            text: Some("all checks passed".to_owned()),
            fallback: Some("build #412 passed".to_owned()),
            pretext: Some("pipeline".to_owned()),
            fields: vec![
                ("branch".to_owned(), "main".to_owned()),
                ("duration".to_owned(), "4m12s".to_owned()),
            ],
            is_unfurl: false,
            ..Attachment::default()
        };
        assert_eq!(
            render_message(&[], "deploy finished", &[card], &[], &Roster),
            "deploy finished\n— pipeline\n— build #412\n  all checks passed\n  branch: main · duration: 4m12s"
        );

        let preview = Attachment {
            title: Some("Worth a read".to_owned()),
            text: Some("A long preview body that never reaches the buffer.".to_owned()),
            fallback: None,
            pretext: None,
            fields: Vec::new(),
            is_unfurl: true,
            service: Some("example.com".to_owned()),
            url: Some("https://example.com/post".to_owned()),
            ..Attachment::default()
        };
        assert_eq!(
            render_message(&[], "worth a read", &[preview], &[], &Roster),
            "worth a read\n\u{258e} Worth a read · example.com\n\u{258e} A long preview body that never reaches the buffer.",
            "a preview is a quote box of a title and two lines, not a page"
        );
    }

    #[test]
    fn a_message_without_blocks_falls_back_to_text_and_lists_its_files() {
        let rendered = render_message(
            &[],
            "ping <@U2>",
            &[Attachment {
                title: Some("Build #12 failed".to_owned()),
                text: None,
                fallback: None,
                pretext: None,
                fields: Vec::new(),
                is_unfurl: false,
                ..Attachment::default()
            }],
            &[FileSummary {
                id: "F1".to_owned(),
                title: "trace.txt".to_owned(),
                filetype: "text".to_owned(),
                size: 2048,
                url: "https://files.example/trace.txt".to_owned(),
            }],
            &Roster,
        );
        assert_eq!(
            rendered,
            "ping @grace\n— Build #12 failed\ntrace.txt · text · 2 KB"
        );
    }
}
