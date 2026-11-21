use rho_agent_types::ContentPart;

/// A message's text, with each picture named.
pub(crate) fn body(content: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: words } => text.push_str(words),
            ContentPart::Image { media_type, .. } => {
                text.push_str(&format!("[image · {media_type}]"))
            }
        }
        text.push('\n');
    }
    text.trim_end().to_owned()
}
