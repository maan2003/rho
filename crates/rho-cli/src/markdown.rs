use rho_cli_term_raw::{Color, Span, Style, StyledBlock, StyledText};

pub(crate) fn markdown_block(text: &str) -> StyledBlock {
    StyledBlock::new(markdown_text(text))
}

pub(crate) fn markdown_text(text: &str) -> StyledText {
    let mut spans = Vec::new();
    let mut in_code_block = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            spans.push(Span::new(
                line.to_owned(),
                Style::default().fg(Color::DarkYellow),
            ));
            spans.push(Span::plain("\n"));
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("# ") {
            spans.push(Span::new(
                heading.to_owned(),
                Style::default().fg(Color::Green).bold(),
            ));
        } else if let Some(heading) = trimmed.strip_prefix("## ") {
            spans.push(Span::new(
                heading.to_owned(),
                Style::default().fg(Color::Green).bold(),
            ));
        } else if trimmed.starts_with('>') {
            spans.push(Span::new(
                line.to_owned(),
                Style::default().fg(Color::DarkGrey),
            ));
        } else {
            push_inline_markdown(&mut spans, line);
        }
        spans.push(Span::plain("\n"));
    }
    if spans.last().is_some_and(|span| span.text == "\n") {
        spans.pop();
    }
    StyledText::from(spans)
}

fn push_inline_markdown(spans: &mut Vec<Span>, mut input: &str) {
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("**")
            && let Some(end) = rest.find("**")
        {
            spans.push(Span::new(rest[..end].to_owned(), Style::default().bold()));
            input = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = input.strip_prefix('`')
            && let Some(end) = rest.find('`')
        {
            spans.push(Span::new(
                rest[..end].to_owned(),
                Style::default().fg(Color::DarkYellow),
            ));
            input = &rest[end + 1..];
            continue;
        }
        let next = input
            .find("**")
            .into_iter()
            .chain(input.find('`'))
            .min()
            .unwrap_or(input.len());
        if next == 0 {
            let next_char = input
                .char_indices()
                .nth(1)
                .map_or(input.len(), |(index, _)| index);
            spans.push(Span::plain(input[..next_char].to_owned()));
            input = &input[next_char..];
            continue;
        }
        spans.push(Span::plain(input[..next].to_owned()));
        input = &input[next..];
    }
}
