//! The languages a buffer can be highlighted with.
//!
//! One registry for the whole app, built the first time a screen asks for
//! it and re-themed with the window: the transcript's Markdown, the file
//! view's sources and the diff view all read from this one, so a language
//! added for one screen is a language every screen has.

use std::sync::Arc;

use gpui::App;
use theme::{ActiveTheme as _, GlobalTheme};

struct Registry(Arc<language::LanguageRegistry>);
impl gpui::Global for Registry {}

pub fn registry(cx: &mut App) -> Arc<language::LanguageRegistry> {
    if !cx.has_global::<Registry>() {
        let languages = Arc::new(language::LanguageRegistry::new(
            cx.background_executor().clone(),
        ));
        languages.set_theme(cx.theme().clone());
        {
            let fs: Arc<dyn fs::Fs> =
                Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
            languages::init(
                languages.clone(),
                fs,
                node_runtime::NodeRuntime::unavailable(),
                cx,
            );
        }
        cx.observe_global::<GlobalTheme>({
            let languages = languages.clone();
            move |cx| languages.set_theme(cx.theme().clone())
        })
        .detach();
        cx.set_global(Registry(languages));
    }
    cx.global::<Registry>().0.clone()
}
