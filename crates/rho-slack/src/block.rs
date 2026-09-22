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

use crate::markdown::{escape, remark};
use crate::types::{Attachment, ChannelId, FileSummary, UserId};

/// Whatever the model knows about names right now. Rendering is a pure
/// function of the payload plus this, so a message re-renders correctly once
/// a late `users.info` fills a gap.
pub trait Names {
    fn user(&self, id: &UserId) -> Option<String>;
    fn channel(&self, id: &ChannelId) -> Option<String>;
    /// Replaces a link label when workspace-specific semantics make it more
    /// useful than the sender's label. Ordinary renderers leave it alone.
    fn link_label(&self, _url: &str, _label: &str) -> Option<String> {
        None
    }
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

/// Which set of emphasis markers the rendering carries.
///
/// Slack's own `mrkdwn` is what the composer accepts back, so a line yanked
/// out of a thread pastes into a reply; markdown is what a document made of
/// these blocks is written in, and it is what the transcript's parse reads.
/// One renderer answers both, so ids, links and lists are resolved once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavour {
    Mrkdwn,
    Markdown,
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
    let (mut rendered, chrome) =
        render_parts_as(Flavour::Mrkdwn, blocks, text, attachments, files, names);
    for line in chrome {
        push_line(&mut rendered, &line);
    }
    rendered
}

/// What the sender wrote, and the lines the renderer hangs under it: an
/// attachment's card, a bot's fields, a file. The split is what lets the
/// time trail the message's own last line rather than the chrome's.
pub fn render_parts(
    blocks: &[Value],
    text: &str,
    attachments: &[Attachment],
    files: &[FileSummary],
    names: &dyn Names,
) -> (String, Vec<String>) {
    render_parts_as(Flavour::Mrkdwn, blocks, text, attachments, files, names)
}

/// The same, in the flavour asked for.
pub fn render_parts_as(
    flavour: Flavour,
    blocks: &[Value],
    text: &str,
    attachments: &[Attachment],
    files: &[FileSummary],
    names: &dyn Names,
) -> (String, Vec<String>) {
    let said = if blocks.is_empty() {
        render_mrkdwn_as(flavour, text, names)
    } else {
        let parts = blocks
            .iter()
            .map(|block| render_block_as(flavour, block, names))
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>();
        parts.join("\n")
    };
    let mut chrome = Vec::new();
    for attachment in attachments {
        chrome.extend(render_attachment(flavour, attachment, names));
    }
    for file in files {
        // A picture is just the picture: its name and size say nothing the
        // reader wanted, and the box under the message is the file. Anything
        // else is a thing to open, named the way it would be in a shell: no
        // placeholder, no id.
        if !file.is_image() {
            chrome.push(file.line());
        }
    }
    // Shortcodes become glyphs last, so it happens once for blocks, plain
    // text, and attachment lines alike.
    (
        crate::emoji::render(said.trim_end()),
        chrome
            .iter()
            .map(|line| crate::emoji::render(line.trim_end()))
            .collect(),
    )
}

/// A run of mrkdwn emphasis, markers included. Slack's markers stay in the
/// text so a yanked line pastes back into a reply; a surface styles the run
/// so it still reads as formatting rather than as punctuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emphasis {
    Bold,
    Italic,
    Struck,
}

/// Every emphasised run in rendered text. Conservative on purpose: a marker
/// opens only after whitespace and closes only before whitespace or
/// punctuation, so `snake_case` names and `:shortcodes:` are left alone, and
/// code keeps whatever was typed in it.
pub fn emphasis(text: &str) -> Vec<(Emphasis, std::ops::Range<usize>)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    let mut in_code = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'`' {
            in_code = !in_code;
            index += 1;
            continue;
        }
        let kind = match byte {
            b'*' => Emphasis::Bold,
            b'_' => Emphasis::Italic,
            b'~' => Emphasis::Struck,
            _ => {
                index += 1;
                continue;
            }
        };
        let opens = index == 0 || bytes[index - 1].is_ascii_whitespace();
        let has_content = bytes
            .get(index + 1)
            .is_some_and(|next| !next.is_ascii_whitespace() && *next != byte);
        if in_code || !opens || !has_content {
            index += 1;
            continue;
        }
        let end = bytes[index + 1..]
            .iter()
            .position(|candidate| *candidate == byte || *candidate == b'\n')
            .map(|offset| index + 1 + offset)
            .filter(|end| bytes[*end] == byte && !bytes[end - 1].is_ascii_whitespace());
        match end {
            Some(end) => {
                found.push((kind, index..end + 1));
                index = end + 1;
            }
            None => index += 1,
        }
    }
    found
}

/// What the reader sees for a link, and where it points: the rendered text
/// shows the label alone, so the URL travels beside it and reaches the line
/// metadata `enter` reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub label: String,
    pub url: String,
}

/// An interactive Block Kit control retained beside the rendered message text.
#[derive(Clone, Debug, PartialEq)]
pub struct Interaction {
    pub label: String,
    pub element_type: String,
    pub block_id: String,
    pub action_id: String,
    pub value: Option<String>,
    pub options: Vec<InteractionOption>,
    pub confirmation: Option<Confirmation>,
    /// Slack's original element payload, with the containing block id attached.
    pub payload: Value,
}

/// One choice in a `static_select` control.
#[derive(Clone, Debug, PartialEq)]
pub struct InteractionOption {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Confirmation {
    pub title: String,
    pub text: String,
    pub confirm: String,
    pub deny: String,
}

/// Interactive controls in display order. Buttons and static selects can be
/// dispatched; every other control is still returned so the UI can explain
/// that it is unsupported rather than silently eating `enter`.
pub fn interactions(blocks: &[Value], names: &dyn Names) -> Vec<Interaction> {
    let mut found = Vec::new();
    for block in blocks {
        let block_id = string(block, "block_id");
        match string(block, "type") {
            "actions" => {
                for element in array(block, "elements") {
                    push_interaction(element, block_id, names, &mut found);
                }
            }
            "section" => {
                if let Some(accessory) = block.get("accessory") {
                    push_interaction(accessory, block_id, names, &mut found);
                }
            }
            _ => {}
        }
    }
    found
}

fn push_interaction(
    element: &Value,
    block_id: &str,
    names: &dyn Names,
    found: &mut Vec<Interaction>,
) {
    let element_type = string(element, "type").to_owned();
    let action_id = string(element, "action_id").to_owned();
    if element_type.is_empty() || action_id.is_empty() {
        return;
    }
    let label = interaction_label(element, names);
    let options = array(element, "options")
        .iter()
        .filter_map(|option| {
            let value = string(option, "value").to_owned();
            if value.is_empty() {
                return None;
            }
            Some(InteractionOption {
                label: render_text_object(Flavour::Mrkdwn, option.get("text"), names),
                value,
            })
        })
        .collect();
    let confirmation = element.get("confirm").map(|confirm| Confirmation {
        title: render_text_object(Flavour::Mrkdwn, confirm.get("title"), names),
        text: render_text_object(Flavour::Mrkdwn, confirm.get("text"), names),
        confirm: render_text_object(Flavour::Mrkdwn, confirm.get("confirm"), names),
        deny: render_text_object(Flavour::Mrkdwn, confirm.get("deny"), names),
    });
    let mut payload = element.clone();
    if let Some(payload) = payload.as_object_mut() {
        payload.insert("block_id".to_owned(), Value::String(block_id.to_owned()));
        payload.remove("options");
        payload.remove("option_groups");
        payload.remove("initial_option");
        payload.remove("confirm");
    }
    found.push(Interaction {
        label,
        element_type,
        block_id: block_id.to_owned(),
        action_id,
        value: element
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned),
        options,
        confirmation,
        payload,
    });
}

fn interaction_label(element: &Value, names: &dyn Names) -> String {
    let direct = render_text_object(Flavour::Mrkdwn, element.get("text"), names);
    if !direct.is_empty() {
        return direct;
    }
    let initial = element
        .get("initial_option")
        .and_then(|option| option.get("text"));
    let initial = render_text_object(Flavour::Mrkdwn, initial, names);
    if !initial.is_empty() {
        return initial;
    }
    let placeholder = render_text_object(Flavour::Mrkdwn, element.get("placeholder"), names);
    if !placeholder.is_empty() {
        return placeholder;
    }
    match string(element, "type") {
        "overflow" => "more…".to_owned(),
        kind if !kind.is_empty() => kind.replace('_', " "),
        _ => "action".to_owned(),
    }
}

fn render_interaction(element: &Value, names: &dyn Names) -> String {
    let label = interaction_label(element, names);
    match string(element, "type") {
        "static_select"
        | "external_select"
        | "users_select"
        | "conversations_select"
        | "channels_select"
        | "overflow" => format!("[{label} ▾]"),
        _ => format!("[{label}]"),
    }
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

/// The bar down the left of an unfurl, for a surface with no gutter of its
/// own to draw one in. Every line of the card carries it, which is what
/// makes the card one thing rather than several lines.
pub const UNFURL_BAR: &str = "\u{258e} ";

/// How much of someone else's page an unfurl is allowed to bring with it.
const UNFURL_LINES: usize = 2;

/// An attachment: a link preview, or an app's own card.
///
/// Either way it is a quote box hanging off the message. Loose lines behind
/// a dash read as a stray dash in the middle of speech; a bar down the left
/// makes the card one thing the reader can skip past. In markdown the bar
/// is the gutter's, drawn beside the lines rather than typed into them --
/// the same bar the agent transcript puts beside a message -- so the card's
/// own words start at the margin like everything else.
///
/// A preview keeps its supplied title distinct from its body. Markdown keeps
/// the complete source so its surface can collapse and expand without another
/// rendering pass; the legacy mrkdwn surface retains its short preview.
/// An app card keeps what it was given: its pretext, title, body, and the
/// labelled values it hung under them.
pub(crate) fn render_attachment(
    flavour: Flavour,
    attachment: &Attachment,
    names: &dyn Names,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(pretext) = attachment
        .pretext
        .as_deref()
        .filter(|_| !attachment.is_unfurl)
    {
        lines.extend(
            render_mrkdwn_as(flavour, pretext, names)
                .lines()
                .map(str::to_owned),
        );
    }

    let author = attachment
        .author_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| attachment.author_id.as_ref().and_then(|id| names.user(id)));
    let channel = attachment
        .channel_id
        .as_ref()
        .and_then(|id| names.channel(id))
        .map(|name| format!("#{name}"));
    let header = [author, channel]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let has_header = !header.is_empty();
    if has_header {
        lines.push(match flavour {
            Flavour::Mrkdwn => header,
            Flavour::Markdown => escape(&header),
        });
    } else if flavour == Flavour::Markdown {
        if let Some(site) = attachment
            .service
            .as_deref()
            .filter(|site| !site.is_empty())
        {
            lines.push(escape(site));
        }
    }

    if let Some(title) = attachment
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        let title = render_mrkdwn_as(flavour, title, names);
        match flavour {
            Flavour::Markdown => {
                lines.extend(title.lines().map(|line| format!("**{line}**")));
            }
            Flavour::Mrkdwn => {
                let title = match attachment.service.as_deref() {
                    Some(site) if !site.is_empty() && !has_header => {
                        format!("{title} · {site}")
                    }
                    _ => title,
                };
                lines.extend(title.lines().map(str::to_owned));
            }
        }
    }

    let body = if attachment.blocks.is_empty() {
        attachment
            .text
            .as_deref()
            .or(attachment.fallback.as_deref())
            .map(|text| render_mrkdwn_as(flavour, text, names))
    } else {
        Some(
            attachment
                .blocks
                .iter()
                .map(|block| render_block_as(flavour, block, names))
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    if let Some(body) = body.filter(|body| !body.trim().is_empty()) {
        let body = body.trim_end_matches('\n').lines().map(str::to_owned);
        match (flavour, attachment.is_unfurl) {
            (Flavour::Mrkdwn, true) => lines.extend(body.take(UNFURL_LINES)),
            _ => lines.extend(body),
        }
    }

    if !attachment.fields.is_empty() {
        lines.push(
            attachment
                .fields
                .iter()
                .map(|(title, value)| {
                    format!("{title}: {}", render_mrkdwn_as(flavour, value, names))
                })
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    match flavour {
        Flavour::Mrkdwn => lines
            .into_iter()
            .map(|line| format!("{UNFURL_BAR}{line}"))
            .collect(),
        Flavour::Markdown => lines,
    }
}

fn push_line(target: &mut String, line: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(line);
}

pub fn render_block(block: &Value, names: &dyn Names) -> String {
    render_block_as(Flavour::Mrkdwn, block, names)
}

/// The same, in the flavour asked for.
pub fn render_block_as(flavour: Flavour, block: &Value, names: &dyn Names) -> String {
    match string(block, "type") {
        "rich_text" => {
            let mut rendered = String::new();
            let mut previous_was_list = false;
            for element in array(block, "elements") {
                let part = render_rich_text_element(flavour, element, names);
                let part = part.trim_end_matches('\n');
                if part.trim().is_empty() {
                    continue;
                }
                let current_is_list = string(element, "type") == "rich_text_list";
                if !rendered.is_empty() {
                    rendered.push_str(if previous_was_list && !current_is_list {
                        "\n\n"
                    } else {
                        "\n"
                    });
                }
                rendered.push_str(part);
                previous_was_list = current_is_list;
            }
            rendered
        }
        "section" => {
            let mut parts = Vec::new();
            let text = render_text_object(flavour, block.get("text"), names);
            if !text.is_empty() {
                parts.push(text);
            }
            let fields = array(block, "fields")
                .iter()
                .map(|field| render_text_object(flavour, Some(field), names))
                .filter(|field| !field.is_empty())
                .collect::<Vec<_>>();
            if !fields.is_empty() {
                parts.push(fields.join("\n"));
            }
            if let Some(accessory) = block.get("accessory") {
                let action = render_interaction(accessory, names);
                if !action.is_empty() {
                    parts.push(action);
                }
            }
            parts.join("\n")
        }
        "header" => {
            let text = render_text_object(flavour, block.get("text"), names);
            match text.is_empty() {
                true => String::new(),
                false => format!("# {text}"),
            }
        }
        "divider" => "———".to_owned(),
        "image" => {
            let title = render_text_object(flavour, block.get("title"), names);
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
                _ => render_text_object(flavour, Some(element), names),
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        "actions" => array(block, "elements")
            .iter()
            .map(|element| render_interaction(element, names))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        // An unknown block renders as nothing rather than as a debug dump:
        // Slack adds block types constantly and none of them are worth
        // showing a reader raw JSON over.
        _ => String::new(),
    }
}

fn render_rich_text_element(flavour: Flavour, element: &Value, names: &dyn Names) -> String {
    match string(element, "type") {
        "rich_text_section" => inline(flavour, element, names),
        "rich_text_preformatted" => format!(
            "```\n{}\n```\n",
            inline(Flavour::Mrkdwn, element, names).trim_end()
        ),
        "rich_text_quote" => {
            let text = inline(flavour, element, names);
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
                    let text = render_rich_text_element(flavour, item, names);
                    let prefix = format!("{indent}{bullet} ");
                    let continuation = " ".repeat(prefix.len());
                    let mut lines = text.trim_end().split('\n');
                    let mut rendered = format!("{prefix}{}", lines.next().unwrap_or_default());
                    for line in lines {
                        rendered.push_str(&format!("\n{continuation}{line}"));
                    }
                    rendered
                })
                .collect::<Vec<_>>();
            format!("{}\n", items.join("\n"))
        }
        _ => inline(flavour, element, names),
    }
}

fn inline(flavour: Flavour, element: &Value, names: &dyn Names) -> String {
    array(element, "elements")
        .iter()
        .map(|element| render_inline(flavour, element, names))
        .collect()
}

fn render_inline(flavour: Flavour, element: &Value, names: &dyn Names) -> String {
    let text = match string(element, "type") {
        // What the sender typed, kept as they typed it: in markdown the
        // characters that would otherwise open emphasis or a link are
        // escaped, because here they are text and not formatting.
        "text" => match flavour {
            Flavour::Mrkdwn => string(element, "text").to_owned(),
            Flavour::Markdown if element["style"]["code"].as_bool() == Some(true) => {
                string(element, "text").to_owned()
            }
            Flavour::Markdown => escape(string(element, "text")),
        },
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
            let original = string(element, "text");
            let special = names.link_label(url, original);
            let label = special.as_deref().unwrap_or(original);
            match (flavour, label) {
                (Flavour::Mrkdwn, "") => url.to_owned(),
                (Flavour::Mrkdwn, label) => label.to_owned(),
                // The address travels in the text now: a document of
                // markdown carries its own links, and the line no longer
                // has to be asked what it points at.
                (Flavour::Markdown, "") => format!("<{url}>"),
                (Flavour::Markdown, label) => format!("[{}]({url})", escape(label)),
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
    let text = if matches!(
        string(element, "type"),
        "user" | "usergroup" | "broadcast" | "team"
    ) {
        mention(flavour, &text)
    } else {
        text
    };
    apply_style(flavour, element.get("style"), &text)
}

fn mention(flavour: Flavour, text: &str) -> String {
    match flavour {
        Flavour::Markdown => format!("<mark>{}</mark>", escape(text)),
        Flavour::Mrkdwn => text.to_owned(),
    }
}

/// Slack's own emphasis markers, which is also what the composer accepts, so
/// yanking a line out of a thread and pasting it into a reply round-trips.
fn apply_style(flavour: Flavour, style: Option<&Value>, text: &str) -> String {
    let Some(style) = style else {
        return text.to_owned();
    };
    let flag = |name: &str| style.get(name).and_then(Value::as_bool).unwrap_or(false);
    if text.is_empty() {
        return text.to_owned();
    }
    let (bold, italic, struck) = match flavour {
        Flavour::Mrkdwn => ("*", "_", "~"),
        Flavour::Markdown => ("**", "*", "~~"),
    };
    if flag("code") {
        if flavour == Flavour::Mrkdwn {
            return format!("`{text}`");
        }
        let ticks = text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0) + 1;
        let marker = "`".repeat(ticks);
        let padding = if text.starts_with('`') || text.ends_with('`') {
            " "
        } else {
            ""
        };
        return format!("{marker}{padding}{text}{padding}{marker}");
    }
    let leading = &text[..text.len() - text.trim_start().len()];
    let trailing = &text[text.trim_end().len()..];
    let mut text = text.trim().to_owned();
    if text.is_empty() {
        return format!("{leading}");
    }
    for (enabled, marker) in [
        (flag("strike"), struck),
        (flag("italic"), italic),
        (flag("bold"), bold),
    ] {
        if enabled {
            text = format!("{marker}{text}{marker}");
        }
    }
    if flag("underline") && flavour == Flavour::Markdown {
        text = format!("<u>{text}</u>");
    }
    format!("{leading}{text}{trailing}")
}

fn render_text_object(flavour: Flavour, object: Option<&Value>, names: &dyn Names) -> String {
    let Some(object) = object else {
        return String::new();
    };
    render_mrkdwn_as(flavour, string(object, "text"), names)
}

/// Resolves the escapes Slack's older `mrkdwn` strings carry: `<@U…>`,
/// `<#C…|name>`, `<https://…|text>`, `<!here>`, and the three XML entities.
/// A message with blocks never needs this, but plain-text messages, older
/// posts, and attachment bodies all do.
pub fn render_mrkdwn(text: &str, names: &dyn Names) -> String {
    render_mrkdwn_as(Flavour::Mrkdwn, text, names)
}

/// The same, in the flavour asked for. In markdown the emphasis Slack's
/// plain bodies carry as characters is rewritten with markdown's own
/// markers, since here it is formatting and not punctuation; what is not a
/// marker is escaped, so a name with an underscore in it stays a name.
pub fn render_mrkdwn_as(flavour: Flavour, text: &str, names: &dyn Names) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let ticks = rest[start..]
            .bytes()
            .take_while(|byte| *byte == b'`')
            .count();
        let after = start + ticks;
        let mut search = after;
        let mut end = None;
        while let Some(at) = rest[search..].find('`') {
            let at = search + at;
            let count = rest[at..].bytes().take_while(|byte| *byte == b'`').count();
            if count == ticks {
                end = Some(at + count);
                break;
            }
            search = at + count;
        }
        let Some(end) = end else {
            break;
        };
        out.push_str(&render_mrkdwn_prose(flavour, &rest[..start], names));
        // Mentions and HTML-shaped text inside code are literal, not metadata.
        out.push_str(&unescape_entities(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(&render_mrkdwn_prose(flavour, rest, names));
    out
}

fn render_mrkdwn_prose(flavour: Flavour, text: &str, names: &dyn Names) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&plain(flavour, &rest[..start]));
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            out.push_str(&plain(flavour, &rest[start..]));
            rest = "";
            break;
        };
        out.push_str(&render_escape(
            flavour,
            &unescape_entities(&after[..end]),
            names,
        ));
        rest = &after[end + 1..];
    }
    out.push_str(&plain(flavour, rest));
    out
}

/// A run of a plain body between escapes, in the flavour asked for.
fn plain(flavour: Flavour, text: &str) -> String {
    match flavour {
        Flavour::Mrkdwn => unescape_entities(text),
        Flavour::Markdown => remark(&unescape_entities(text)),
    }
}

fn render_escape(flavour: Flavour, body: &str, names: &dyn Names) -> String {
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
            mention(flavour, &format!("@{name}"))
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
        Some('!') => mention(
            flavour,
            &match label {
                Some(label) => label.to_owned(),
                None => format!("@{}", target[1..].split('^').next().unwrap_or_default()),
            },
        ),
        _ => {
            let special = names.link_label(target, label.unwrap_or(target));
            let label = special.as_deref().or(label);
            match (flavour, label) {
                (Flavour::Mrkdwn, Some(label)) => label.to_owned(),
                (Flavour::Mrkdwn, None) => target.to_owned(),
                (Flavour::Markdown, Some(label)) => format!("[{}]({target})", escape(label)),
                (Flavour::Markdown, None) => format!("<{target}>"),
            }
        }
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

    #[test]
    fn emphasis_keeps_its_markers_and_skips_what_only_looks_like_it() {
        let text = "*bold*, _italic_, ~struck~, `*not bold*`";
        let found = emphasis(text)
            .into_iter()
            .map(|(kind, range)| (kind, text[range].to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            vec![
                (Emphasis::Bold, "*bold*".to_owned()),
                (Emphasis::Italic, "_italic_".to_owned()),
                (Emphasis::Struck, "~struck~".to_owned()),
            ],
            "code keeps what was typed in it"
        );
        assert!(
            emphasis(":forrest_gump_wave: and snake_case_name").is_empty(),
            "an underscore inside a word is part of the word"
        );
        assert!(
            emphasis("2 * 3 * 4").is_empty(),
            "a marker with nothing against it is arithmetic"
        );
    }
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
    fn markdown_retains_combined_rich_text_styles_and_semantic_mentions() {
        let block = json!({
            "type": "rich_text",
            "elements": [{"type": "rich_text_section", "elements": [
                {"type": "broadcast", "range": "channel"},
                {"type": "text", "text": " Trade-offs:", "style": {"bold": true, "underline": true}},
                {"type": "text", "text": "all", "style": {"bold": true, "italic": true, "strike": true}},
                {"type": "text", "text": "<u>literal</u>"},
            ]}]
        });
        assert_eq!(
            render_block_as(Flavour::Markdown, &block, &NoNames),
            "<mark>@channel</mark> <u>**Trade-offs:**</u>***~~all~~***\\<u\\>literal\\</u\\>"
        );
        assert_eq!(
            render_mrkdwn_as(Flavour::Markdown, "<!here> <@U404|Ada Lovelace>", &NoNames),
            "<mark>@here</mark> <mark>@Ada Lovelace</mark>"
        );
    }

    #[test]
    fn rich_inline_code_keeps_literal_characters_and_embedded_backticks() {
        for (text, expected) in [
            ("<!here> <u>code</u>", "`<!here> <u>code</u>`"),
            ("a ` b", "``a ` b``"),
            ("`edge`", "`` `edge` ``"),
        ] {
            let element = json!({"type":"text", "text":text, "style":{"code":true}});
            assert_eq!(
                render_inline(Flavour::Markdown, &element, &NoNames),
                expected
            );
        }
    }

    #[test]
    fn literal_entity_encoded_html_is_not_generated_formatting() {
        assert_eq!(
            render_mrkdwn_as(Flavour::Markdown, "&lt;u&gt;literal&lt;/u&gt;", &NoNames),
            r"\<u\>literal\</u\>"
        );
    }

    #[test]
    fn code_never_acquires_mention_markup() {
        for text in [
            "`<!here>` then <!here>",
            "``a ` <!here>`` then <!here>",
            "```\n<!here>\n``` then <!here>",
        ] {
            let rendered = render_mrkdwn_as(Flavour::Markdown, text, &NoNames);
            assert_eq!(rendered.matches("<mark>").count(), 1, "{rendered}");
            assert!(rendered.ends_with(" then <mark>@here</mark>"), "{rendered}");
            assert!(rendered.contains("<!here>"), "{rendered}");
        }
    }

    #[test]
    fn multiline_list_items_keep_continuations_inside_the_item() {
        let block = json!({"type":"rich_text","elements":[{
            "type":"rich_text_list", "style":"ordered", "indent":1,
            "elements":[{"type":"rich_text_section","elements":[
                {"type":"text","text":"first\ncontinued"}
            ]}]
        }]});
        assert_eq!(
            render_block_as(Flavour::Markdown, &block, &NoNames),
            "  1. first\n     continued"
        );
    }

    #[test]
    fn adjacent_nested_lists_share_one_list_boundary() {
        let block = json!({"type":"rich_text","elements":[
            {
                "type":"rich_text_list", "style":"bullet", "indent":0,
                "elements":[{"type":"rich_text_section","elements":[
                    {"type":"text","text":"parent"}
                ]}]
            },
            {
                "type":"rich_text_list", "style":"bullet", "indent":1,
                "elements":[{"type":"rich_text_section","elements":[
                    {"type":"text","text":"child"}
                ]}]
            },
            {
                "type":"rich_text_section",
                "elements":[{"type":"text","text":"After"}]
            }
        ]});
        assert_eq!(
            render_block_as(Flavour::Markdown, &block, &NoNames),
            "- parent\n  - child\n\nAfter",
            "adjacent list nodes form one list; only following prose starts a paragraph"
        );
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
            "  1. first\n  2. second\n\n> they said\n> this\n```\ncargo test\n```"
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
            "deploy finished\n\u{258e} pipeline\n\u{258e} build #412\n\u{258e} all checks passed\n\u{258e} branch: main · duration: 4m12s"
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
    fn markdown_attachment_body_is_plain_complete_and_keeps_paragraphs() {
        let attachment = Attachment {
            text: Some("first line\n\nsecond paragraph\nthird\nfourth".to_owned()),
            fallback: Some("notification fallback".to_owned()),
            is_unfurl: true,
            ..Attachment::default()
        };
        assert_eq!(
            render_attachment(Flavour::Markdown, &attachment, &Roster),
            vec!["first line", "", "second paragraph", "third", "fourth"],
            "body-only previews are not promoted to bold titles or truncated"
        );
        assert_eq!(
            render_attachment(Flavour::Mrkdwn, &attachment, &Roster),
            vec![format!("{UNFURL_BAR}first line"), UNFURL_BAR.to_owned(),],
            "the legacy surface retains its two-line preview limit"
        );
    }

    #[test]
    fn attachment_prefers_rich_blocks_and_keeps_list_boundaries_and_metadata() {
        let attachment = Attachment {
            title: Some("Linked discussion".to_owned()),
            text: Some("stale fallback".to_owned()),
            author_id: Some(UserId("U1".to_owned())),
            channel_id: Some(ChannelId("C1".to_owned())),
            blocks: vec![json!({
                "type": "rich_text",
                "elements": [
                    {"type": "rich_text_section", "elements": [
                        {"type": "text", "text": "Context"}
                    ]},
                    {"type": "rich_text_list", "style": "bullet", "elements": [
                        {"type": "rich_text_section", "elements": [
                            {"type": "text", "text": "one"}
                        ]},
                        {"type": "rich_text_section", "elements": [
                            {"type": "text", "text": "two"}
                        ]}
                    ]},
                    {"type": "rich_text_section", "elements": [
                        {"type": "text", "text": "Conclusion"}
                    ]}
                ]
            })],
            is_unfurl: true,
            ..Attachment::default()
        };
        assert_eq!(
            render_attachment(Flavour::Markdown, &attachment, &Roster),
            vec![
                "ada · #design",
                "**Linked discussion**",
                "Context",
                "- one",
                "- two",
                "",
                "Conclusion",
            ]
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
                original_w: 0,
                original_h: 0,
                thumb_url: String::new(),
            }],
            &Roster,
        );
        assert_eq!(
            rendered,
            "ping @grace\n\u{258e} Build #12 failed\ntrace.txt · text · 2 KB"
        );
    }
}

#[cfg(test)]
mod interaction_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn actions_render_one_per_line_and_keep_the_wire_identity() {
        let blocks = vec![json!({
            "type": "actions",
            "block_id": "deploy",
            "elements": [
                {"type": "button", "action_id": "approve", "value": "yes",
                 "text": {"type": "plain_text", "text": "Approve"}},
                {"type": "static_select", "action_id": "target",
                 "placeholder": {"type": "plain_text", "text": "Choose target"},
                 "options": [
                     {"text": {"type": "plain_text", "text": "Staging"}, "value": "staging"},
                     {"text": {"type": "plain_text", "text": "Production"}, "value": "production"}
                 ]}
            ]
        })];
        assert_eq!(
            render_message(&blocks, "", &[], &[], &NoNames),
            "[Approve]\n[Choose target ▾]"
        );
        let actions = interactions(&blocks, &NoNames);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].payload["block_id"], "deploy");
        assert_eq!(actions[0].payload["value"], "yes");
        assert_eq!(
            actions[1].options,
            vec![
                InteractionOption {
                    label: "Staging".to_owned(),
                    value: "staging".to_owned(),
                },
                InteractionOption {
                    label: "Production".to_owned(),
                    value: "production".to_owned(),
                },
            ]
        );
        assert!(
            actions[1].payload.get("options").is_none(),
            "the dispatched action carries the selection, not the entire menu"
        );
    }
}
