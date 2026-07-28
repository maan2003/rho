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
use super::conceal::Trees;
use crate::style::StyleClass;

static MARKDOWN_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();
static MARKDOWN_INLINE_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();

/// The markdown grammars, carrying the theme they were last styled with.
///
/// Resolving them once per sync keeps the theme lookup out of the per-block
/// path, and leaves rendering with no handle on the app - a block can then
/// be rendered away from the main thread.
#[derive(Clone, Copy)]
pub struct Markdown {
    block: Option<&'static Arc<Language>>,
    inline: Option<&'static Arc<Language>>,
}

impl Markdown {
    pub fn new(cx: &App) -> Self {
        let markdown = Self {
            block: markdown_language(cx),
            inline: markdown_inline_language(cx),
        };
        for language in [markdown.block, markdown.inline].into_iter().flatten() {
            language.set_theme(cx.theme().syntax());
        }
        markdown
    }
}

/// Highlights `text`, parsing it for that alone; rendering shares its parse
/// with concealment through [`markdown_spans_of`].
#[cfg(test)]
pub fn markdown_spans(text: &str, markdown: &Markdown) -> Vec<Span> {
    markdown_spans_of(text, &super::conceal::parse(text), markdown)
}

/// Highlights `text` as markdown from trees the caller already parsed.
pub fn markdown_spans_of(text: &str, trees: &Trees, markdown: &Markdown) -> Vec<Span> {
    let Some(markdown_language) = markdown.block else {
        return vec![Span::new(text, StyleClass::Default)];
    };
    let Some(block_tree) = trees.block() else {
        return vec![Span::new(text, StyleClass::Default)];
    };
    let rope = Rope::from(text);
    let mut highlights = markdown_language.highlight_tree(&rope, block_tree, 0..text.len());
    if let Some(markdown_inline_language) = markdown.inline {
        for (span, tree) in trees.inline() {
            let span = span.start_byte..span.end_byte;
            // Highlights come back relative to the range asked for.
            let offset = span.start;
            highlights.extend(
                markdown_inline_language
                    .highlight_tree(&rope, tree, span)
                    .into_iter()
                    .map(|(range, id)| (range.start + offset..range.end + offset, id)),
            );
        }
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
                    }
                    .into(),
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
