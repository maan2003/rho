//! A picture filling the window.
//!
//! A shared image is worth looking at properly, and handing it to the
//! desktop's viewer takes the reader out of rho for something rho can draw
//! itself. The surface holds nothing but the picture: `q` or `escape`
//! closes it and the conversation is underneath again. A pinch zooms it
//! and, zoomed, a drag moves it, which is how a phone looks at a picture.

use camino::Utf8PathBuf;
use gpui::{
    App, Context, Focusable, InteractiveElement as _, IntoElement, ParentElement, Pixels, Point,
    Render, Styled, Window, div, img, relative,
};
use theme::ActiveTheme as _;

const MAX_ZOOM: f32 = 8.;

pub struct ImageView {
    path: Utf8PathBuf,
    focus: gpui::FocusHandle,
    zoom: f32,
    /// How far the zoomed picture is moved from centered.
    pan: Point<Pixels>,
}

impl ImageView {
    pub fn new(path: Utf8PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            path,
            focus: cx.focus_handle(),
            zoom: 1.,
            pan: Point::default(),
        }
    }

    fn pinch(&mut self, delta: f32, cx: &mut Context<Self>) {
        let zoom = (self.zoom * (1. + delta)).clamp(1., MAX_ZOOM);
        // Zooming about the middle keeps what is there in the middle.
        self.pan = self.pan * (zoom / self.zoom);
        self.zoom = zoom;
        cx.notify();
    }

    pub fn zoom_and_pan_for_test(&self) -> (f32, Point<Pixels>) {
        (self.zoom, self.pan)
    }

    /// A drag over a zoomed picture moves it; unzoomed there is nothing to
    /// move and the drag belongs to whatever is around it.
    fn drag(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        if self.zoom <= 1. {
            return false;
        }
        self.pan += delta;
        cx.notify();
        true
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
            .relative()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _, cx| {
                this.pinch(event.delta, cx);
                cx.stop_propagation();
            }))
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
                    let delta = event.delta.pixel_delta(window.line_height());
                    if this.drag(delta, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                // Zoomed, the picture is a frame `zoom` times the surface,
                // centered and then moved by the pan.
                div()
                    .absolute()
                    .left(relative((1. - self.zoom) / 2.))
                    .top(relative((1. - self.zoom) / 2.))
                    .w(relative(self.zoom))
                    .h(relative(self.zoom))
                    .child(
                        div()
                            .relative()
                            .left(self.pan.x)
                            .top(self.pan.y)
                            .size_full()
                            .child(
                                // Filling the frame with the image's own aspect
                                // kept is gpui's default fit, which is the one a
                                // viewer wants.
                                img(self.path.as_std_path().to_path_buf()).size_full(),
                            ),
                    ),
            )
    }
}
