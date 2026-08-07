//! Tree-sitter presentation for Desk documents.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use language::{Buffer, Language, LanguageConfig, LanguageMatcher, LanguageQueries};

static DESK_LANGUAGE: OnceLock<Option<Arc<Language>>> = OnceLock::new();

fn language() -> Option<&'static Arc<Language>> {
    DESK_LANGUAGE
        .get_or_init(|| {
            Language::new(
                LanguageConfig {
                    name: "Rho Desk".into(),
                    matcher: LanguageMatcher {
                        path_suffixes: vec!["rho-desk".into()],
                        ..Default::default()
                    }
                    .into(),
                    ..LanguageConfig::default()
                },
                Some(org_element::language()),
            )
            .with_queries(LanguageQueries {
                highlights: Some(Cow::Borrowed(include_str!("grammars/desk/highlights.scm"))),
                outline: Some(Cow::Borrowed(include_str!("grammars/desk/outline.scm"))),
                conceals: Some(Cow::Borrowed(include_str!("grammars/desk/conceals.scm"))),
                ..LanguageQueries::default()
            })
            .ok()
            .map(Arc::new)
        })
        .as_ref()
}

pub fn configure(buffer: &mut Buffer, cx: &mut gpui::Context<Buffer>) {
    let Some(language) = language() else {
        return;
    };
    let registry = crate::zed_remote::language_registry(cx);
    registry.add(language.clone());
    buffer.set_language_registry(registry);
    buffer.set_sync_parse_timeout(None);
    buffer.set_language_deferred(Some(language.clone()), cx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn grammar_parses_nested_sections() {
        assert!(super::language().is_some(), "Desk queries must compile");
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&org_element::language()).unwrap();
        let tree = parser
            .parse("* TODO root\nbody\n** DONE child\n:id: h-a\n", None)
            .unwrap();
        // The upstream org grammar reports our compact direct `:id:` property
        // as a recovery node. Desk queries decorate that node; headings remain
        // stable syntax nodes and the daemon parser remains authoritative.
        let sexp = tree.root_node().to_sexp();
        assert_eq!(sexp.matches("(headline ").count(), 2, "{sexp}");
        assert!(sexp.contains("(ERROR (day))"), "{sexp}");
    }
}
