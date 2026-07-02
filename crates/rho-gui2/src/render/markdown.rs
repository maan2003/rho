//! Tree-sitter markdown highlighting for assistant messages.
//!
//! Produces spans carrying [`StyleClass::Syntax`] highlight ids; the theme
//! color for each id is resolved at highlight-application time, so rendered
//! spans stay theme-independent.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use gpui::App;
use language::{Language, LanguageConfig, LanguageMatcher, LanguageQueries, Rope};
use theme::ActiveTheme as _;

use super::Span;
use crate::style::StyleClass;

static MARKDOWN_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();
static MARKDOWN_INLINE_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();

/// Highlights `text` as markdown, appending a trailing newline if missing.
pub fn markdown_spans_with_newline(text: &str, cx: &App) -> Vec<Span> {
    let mut text = text.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    markdown_spans(&text, cx)
}

pub fn markdown_spans(text: &str, cx: &App) -> Vec<Span> {
    let Some(markdown_language) = markdown_language(cx) else {
        return vec![Span::new(text, StyleClass::Default)];
    };
    markdown_language.set_theme(cx.theme().syntax());
    let rope = Rope::from(text);
    let mut highlights = markdown_language.highlight_text(&rope, 0..text.len());
    if let Some(markdown_inline_language) = markdown_inline_language(cx) {
        markdown_inline_language.set_theme(cx.theme().syntax());
        highlights.extend(markdown_inline_language.highlight_text(&rope, 0..text.len()));
    }
    highlights.sort_by_key(|(range, _)| range.start);

    let mut spans = Vec::new();
    let mut cursor = 0;
    for (range, highlight_id) in highlights {
        if range.start > cursor {
            spans.push(Span::new(&text[cursor..range.start], StyleClass::Default));
        }
        let start = range.start.max(cursor);
        if range.end > start {
            spans.push(Span::new(
                &text[start..range.end],
                StyleClass::Syntax(usize::from(highlight_id) as u32),
            ));
        }
        cursor = cursor.max(range.end);
    }
    if cursor < text.len() {
        spans.push(Span::new(&text[cursor..], StyleClass::Default));
    }
    spans
}

fn markdown_language(cx: &App) -> Option<&'static Arc<Language>> {
    MARKDOWN_LANGUAGE
        .get_or_init(|| {
            let language = Language::new(
                LanguageConfig {
                    name: "Markdown".into(),
                    matcher: LanguageMatcher {
                        path_suffixes: vec!["md".into()],
                        ..Default::default()
                    },
                    ..LanguageConfig::default()
                },
                Some(tree_sitter_md::LANGUAGE.into()),
            )
            .with_queries(LanguageQueries {
                highlights: Some(Cow::from(include_str!(
                    "../grammars/markdown/highlights.scm"
                ))),
                ..LanguageQueries::default()
            })
            .ok()?;
            let language = Arc::new(language);
            language.set_theme(cx.theme().syntax());
            Some(language)
        })
        .as_ref()
}

fn markdown_inline_language(cx: &App) -> Option<&'static Arc<Language>> {
    MARKDOWN_INLINE_LANGUAGE
        .get_or_init(|| {
            let language = Language::new(
                LanguageConfig {
                    name: "Markdown-Inline".into(),
                    hidden: true,
                    ..LanguageConfig::default()
                },
                Some(tree_sitter_md::INLINE_LANGUAGE.into()),
            )
            .with_queries(LanguageQueries {
                highlights: Some(Cow::from(include_str!(
                    "../grammars/markdown-inline/highlights.scm"
                ))),
                ..LanguageQueries::default()
            })
            .ok()?;
            let language = Arc::new(language);
            language.set_theme(cx.theme().syntax());
            Some(language)
        })
        .as_ref()
}
