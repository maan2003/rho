//! Markdown languages used by the transcript buffer's persistent syntax map.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use gpui::{App, Global};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher, LanguageQueries};
use theme::ActiveTheme as _;

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

struct MarkdownLanguagesRegistered;
impl Global for MarkdownLanguagesRegistered {}

/// Gives a transcript buffer Zed's persistent, background Markdown syntax
/// pipeline. The transcript limits query-backed concealment to assistant
/// message ranges at the editor layer.
pub fn configure_buffer(buffer: &mut Buffer, cx: &mut gpui::Context<Buffer>) {
    let markdown = Markdown::new(cx);
    let (Some(block), Some(inline)) = (markdown.block, markdown.inline) else {
        return;
    };
    let registry = crate::zed_remote::language_registry(cx);
    if !cx.has_global::<MarkdownLanguagesRegistered>() {
        registry.add(block.clone());
        registry.add(inline.clone());
        cx.set_global(MarkdownLanguagesRegistered);
    }
    buffer.set_language_registry(registry);
    // Transcript composition activates this after excerpts and editor
    // attachments are in place. Keeping assignment separate from activation
    // avoids exposing a half-composed buffer to syntax consumers.
    buffer.set_sync_parse_timeout(None);
    buffer.set_language_deferred(Some(block.clone()), cx);
}

fn markdown_language(cx: &App) -> Option<&'static Arc<Language>> {
    MARKDOWN_LANGUAGE
        .get_or_init(|| {
            let language = Language::new(
                LanguageConfig {
                    name: "Rho Markdown".into(),
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
                injections: Some(Cow::from(include_str!(
                    "../grammars/markdown/injections.scm"
                ))),
                conceals: Some(Cow::from(include_str!("../grammars/markdown/conceals.scm"))),
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
                    name: "Rho Markdown Inline".into(),
                    hidden: true,
                    ..LanguageConfig::default()
                },
                Some(tree_sitter_md::INLINE_LANGUAGE.into()),
            )
            .with_queries(LanguageQueries {
                highlights: Some(Cow::from(include_str!(
                    "../grammars/markdown-inline/highlights.scm"
                ))),
                conceals: Some(Cow::from(include_str!(
                    "../grammars/markdown-inline/conceals.scm"
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
