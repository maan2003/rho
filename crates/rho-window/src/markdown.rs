//! Markdown languages used by the transcript buffer's persistent syntax map.

use std::borrow::Cow;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui::{App, Global};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher, LanguageQueries};
use theme::ActiveTheme as _;

/// How long a chunk's parse may hold the frame it belongs to.
const SYNC_PARSE_BUDGET: Duration = Duration::from_millis(1);

/// Longer than any parse a harness composes: the absence of a bound rather
/// than a wait, so the parse always lands in the frame that made the text.
const SYNC_PARSE_UNBOUNDED: Duration = Duration::from_secs(60);

/// Whether the parse of a chunk is bounded by the clock.
///
/// A harness that hashes a scene has to own every bound on the work behind
/// it: with the budget in, whether a parse lands in its own frame or a
/// later one depends on how busy the machine is, and the same seed then
/// draws the markup one run and the concealed text the next. Off the
/// harness this stays on, which is what keeps a long parse out of a frame.
static SYNC_PARSE_BUDGET_ENABLED: AtomicBool = AtomicBool::new(true);

/// Lets a chunk's parse run to the end, for a harness whose scenes must be
/// the same on every run. The same switch the wrap map's batch clock has.
pub fn set_sync_parse_budget_enabled(enabled: bool) {
    SYNC_PARSE_BUDGET_ENABLED.store(enabled, atomic::Ordering::Relaxed);
}

fn sync_parse_timeout() -> Duration {
    if SYNC_PARSE_BUDGET_ENABLED.load(atomic::Ordering::Relaxed) {
        SYNC_PARSE_BUDGET
    } else {
        SYNC_PARSE_UNBOUNDED
    }
}

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

/// Gives an assistant-message buffer Zed's persistent, background Markdown
/// syntax pipeline. Concealment is part of the resulting syntax generation;
/// non-Markdown transcript records live in separate source buffers.
pub fn configure_buffer(buffer: &mut Buffer, cx: &mut gpui::Context<Buffer>) {
    let markdown = Markdown::new(cx);
    let (Some(block), Some(inline)) = (markdown.block, markdown.inline) else {
        return;
    };
    let registry = crate::languages::registry(cx);
    if !cx.has_global::<MarkdownLanguagesRegistered>() {
        registry.add(block.clone());
        registry.add(inline.clone());
        cx.set_global(MarkdownLanguagesRegistered);
    }
    buffer.set_language_registry(registry);
    // Transcript composition activates this after excerpts and editor
    // attachments are in place. Keeping assignment separate from activation
    // avoids exposing a half-composed buffer to syntax consumers.
    //
    // A chunk parses inside the frame that edits it, up to the timeout: the
    // concealed text and the text on screen are then the same text, and a
    // streamed edit never flashes the markup it is about to conceal.
    buffer.set_sync_parse_timeout(Some(sync_parse_timeout()));
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
                highlights: Some(Cow::from(include_str!("grammars/markdown/highlights.scm"))),
                injections: Some(Cow::from(include_str!("grammars/markdown/injections.scm"))),
                conceals: Some(Cow::from(include_str!("grammars/markdown/conceals.scm"))),
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
                    "grammars/markdown-inline/highlights.scm"
                ))),
                conceals: Some(Cow::from(include_str!(
                    "grammars/markdown-inline/conceals.scm"
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
