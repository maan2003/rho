//! A picture filling the window.
//!
//! A shared image is worth looking at properly, and handing it to the
//! desktop's viewer takes the reader out of rho for something rho can draw
//! itself. The surface holds nothing but the picture: `q` or `escape`
//! closes it and the conversation is underneath again.

use camino::Utf8PathBuf;
use gpui::{
    App, Context, Focusable, InteractiveElement as _, IntoElement, ParentElement, Render, Styled,
    Window, div, img,
};
use theme::ActiveTheme as _;

pub struct ImageView {
    path: Utf8PathBuf,
    focus: gpui::FocusHandle,
}

impl ImageView {
    pub fn new(path: Utf8PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            path,
            focus: cx.focus_handle(),
        }
    }
}

impl Focusable for ImageView {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus.clone()
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rho-image")
            .key_context("RhoImage")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(
                // Filling the surface with the image's own aspect kept is
                // gpui's default fit, which is the one a viewer wants.
                img(self.path.as_std_path().to_path_buf()).size_full(),
            )
    }
}
