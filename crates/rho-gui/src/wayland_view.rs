//! Native remote application window. Closing it drops the video subscription.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::*;
use rho_desktop_proto::Input;

pub struct WaylandView {
    viewer: rho_hosts::wayland::Viewer,
    image: Option<Arc<rho_hosts::wayland::Image>>,
    frozen: Option<Arc<rho_hosts::wayland::Image>>,
    strokes: Vec<Vec<(u32, u32)>>,
    drawing: bool,
    target: Option<Entity<rho_agents::AgentModel>>,
    status: Option<String>,
    size: (usize, usize),
    bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    error: Option<String>,
    _updates: Task<()>,
    _activation: Subscription,
}
impl WaylandView {
    pub fn new(
        viewer: rho_hosts::wayland::Viewer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut images = viewer.images.clone();
        let mut errors = viewer.errors.clone();
        let updates = cx.spawn(async move |this, cx| {
            loop {
                let changed = {
                    let image = Box::pin(images.changed());
                    let error = Box::pin(errors.changed());
                    match futures::future::select(image, error).await {
                        futures::future::Either::Left((result, _)) => result.is_ok(),
                        futures::future::Either::Right((result, _)) => result.is_ok(),
                    }
                };
                if !changed {
                    break;
                }
                let image = images.borrow_and_update().clone();
                let error = errors.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        if let Some(image) = image {
                            if this.frozen.is_none() {
                                this.size = (image.width, image.height);
                            }
                            this.image = Some(image);
                        }
                        this.error = error;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.viewer.motion.send_replace(None);
                this.send(Input::ReleaseAll, cx);
                this.drawing = false;
            }
        });
        Self {
            _activation: activation,
            viewer,
            image: None,
            frozen: None,
            strokes: Vec::new(),
            drawing: false,
            target: None,
            status: None,
            size: (1, 1),
            bounds: Rc::new(Cell::new(Bounds::default())),
            focus,
            error: None,
            _updates: updates,
        }
    }
    pub fn with_target(mut self, target: Option<Entity<rho_agents::AgentModel>>) -> Self {
        self.target = target;
        self
    }
    fn annotate(&mut self, cx: &mut Context<Self>) {
        if self.frozen.is_some() {
            self.frozen = None;
            self.strokes.clear();
            self.drawing = false;
            if let Some(image) = self.viewer.images.borrow().as_ref() {
                self.size = (image.width, image.height);
            }
        } else {
            self.send(Input::ReleaseAll, cx);
            self.viewer.motion.send_replace(None);
            self.frozen = self.image.clone();
            self.status = None;
        }
        cx.notify();
    }
    fn export(&mut self, attach: bool, cx: &mut Context<Self>) {
        let Some(image) = self.frozen.as_ref() else {
            return;
        };
        let mut pixels = match image.export_bgra() {
            Ok(pixels) => pixels,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        for stroke in &self.strokes {
            if let Some(&point) = stroke.first() {
                draw_line(&mut pixels, self.size, point, point);
            }
            for pair in stroke.windows(2) {
                draw_line(&mut pixels, self.size, pair[0], pair[1]);
            }
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        let image =
            image::RgbaImage::from_raw(self.size.0 as u32, self.size.1 as u32, pixels).unwrap();
        if let Err(error) = image.write_to(&mut png, image::ImageFormat::Png) {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let bytes = png.into_inner();
        if attach {
            if let Some(target) = &self.target {
                target.update(cx, |model, cx| {
                    model.add_image("image/png".into(), bytes, cx)
                });
                self.status = Some("Added to agent prompt".into());
            }
        } else {
            let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes);
            cx.write_to_clipboard(ClipboardItem::new_image(&image));
            self.status = Some("Annotation copied".into());
        }
        cx.notify();
    }
    fn send(&mut self, input: Input, cx: &mut Context<Self>) {
        if self.viewer.input.try_send(input).is_err() {
            self.viewer.disconnect();
            self.error = Some("Viewer disconnected because input could not be delivered".into());
            cx.notify();
        }
    }
    fn position(&self, p: Point<Pixels>) -> Option<(u32, u32)> {
        let bounds = self.bounds.get();
        if !bounds.contains(&p) || bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        Some((
            (((p.x - bounds.origin.x) / bounds.size.width) * self.size.0 as f32) as u32,
            (((p.y - bounds.origin.y) / bounds.size.height) * self.size.1 as f32) as u32,
        ))
        .map(|(x, y)| (x.min(self.size.0 as u32 - 1), y.min(self.size.1 as u32 - 1)))
    }
}
impl Render for WaylandView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.frozen.clone().or_else(|| self.image.clone());
        let strokes = self.strokes.clone();
        let size = self.size;
        let bounds = self.bounds.clone();
        let canvas = canvas(
            move |available, _, _| {
                let ratio = (f32::from(available.size.width) / size.0 as f32)
                    .min(f32::from(available.size.height) / size.1 as f32);
                let size = gpui::size(px(size.0 as f32 * ratio), px(size.1 as f32 * ratio));
                let fitted = Bounds::new(
                    available.origin
                        + point(
                            (available.size.width - size.width) / 2.,
                            (available.size.height - size.height) / 2.,
                        ),
                    size,
                );
                bounds.set(fitted);
                fitted
            },
            move |_, bounds, window, _| {
                if let Some(image) = image {
                    #[cfg(target_os = "linux")]
                    window.paint_video(bounds, image.render.clone());
                }
                let map = |(x, y): (u32, u32)| {
                    bounds.origin
                        + point(
                            bounds.size.width * (x as f32 / size.0 as f32),
                            bounds.size.height * (y as f32 / size.1 as f32),
                        )
                };
                for stroke in strokes {
                    let Some(&first) = stroke.first() else {
                        continue;
                    };
                    let mut path =
                        PathBuilder::stroke(px(4.) * f32::from(bounds.size.width) / size.0 as f32);
                    path.move_to(map(first));
                    if stroke.len() == 1 {
                        let radius = bounds.size.width * (2. / size.0 as f32);
                        let dot = Bounds::new(
                            map(first) - point(radius, radius),
                            gpui::size(radius * 2., radius * 2.),
                        );
                        window.paint_quad(fill(dot, rgb(0xff4050)));
                    }
                    for p in stroke.into_iter().skip(1) {
                        path.line_to(map(p));
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(0xff4050));
                    }
                }
            },
        )
        .size_full();
        let mut surface = div()
            .id("remote-wayland")
            .track_focus(&self.focus)
            .flex_1()
            .min_h_0()
            .relative()
            .bg(rgb(0x161616))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some(position) = this.position(event.position) {
                    if this.frozen.is_some() {
                        if this.drawing {
                            if let Some(stroke) = this.strokes.last_mut() {
                                if stroke.last() != Some(&position) {
                                    stroke.push(position);
                                    cx.notify();
                                }
                            }
                        }
                    } else {
                        this.viewer.motion.send_replace(Some(position));
                    }
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if this.frozen.is_some() {
                    return;
                }
                let (horizontal, vertical) = match event.delta {
                    ScrollDelta::Pixels(p) => (-f32::from(p.x) as f64, -f32::from(p.y) as f64),
                    ScrollDelta::Lines(p) => (-p.x as f64 * 15., -p.y as f64 * 15.),
                };
                this.send(
                    Input::Scroll {
                        horizontal,
                        vertical,
                    },
                    cx,
                );
                cx.stop_propagation();
            }))
            .on_physical_key(cx.listener(|this, event: &PhysicalKeyEvent, _, cx| {
                if this.frozen.is_some() {
                    return;
                }
                let PhysicalKey::LinuxEvdev(code) = event.key;
                this.send(
                    Input::Physical {
                        code,
                        pressed: event.pressed,
                    },
                    cx,
                );
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.frozen.is_some() {
                    return;
                }
                if cfg!(target_os = "linux") {
                    cx.stop_propagation();
                    return;
                }
                let key = &event.keystroke;
                if !key.modifiers.control && !key.modifiers.platform && !key.modifiers.alt {
                    if let Some(text) = &key.key_char {
                        this.send(Input::Text(text.clone()), cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                let mut chord = String::new();
                if key.modifiers.control {
                    chord.push_str("ctrl+");
                }
                if key.modifiers.alt {
                    chord.push_str("alt+");
                }
                if key.modifiers.shift {
                    chord.push_str("shift+");
                }
                if key.modifiers.platform {
                    chord.push_str("super+");
                }
                chord.push_str(&key.key);
                this.send(Input::Key(chord), cx);
                cx.stop_propagation();
            }))
            .child(canvas);
        for (button, code) in [
            (MouseButton::Left, 0x110),
            (MouseButton::Right, 0x111),
            (MouseButton::Middle, 0x112),
        ] {
            surface = surface
                .on_mouse_down(
                    button,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        if let Some((x, y)) = this.position(event.position) {
                            if this.frozen.is_some() {
                                if button == MouseButton::Left {
                                    this.strokes.push(vec![(x, y)]);
                                    this.drawing = true;
                                    cx.notify();
                                }
                                return;
                            }
                            this.viewer.motion.send_replace(None);
                            this.send(Input::Move { x, y }, cx);
                            this.send(
                                Input::Button {
                                    button: code,
                                    pressed: true,
                                },
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                        this.drawing = false;
                        if this.frozen.is_none() {
                            this.send(
                                Input::Button {
                                    button: code,
                                    pressed: false,
                                },
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    button,
                    cx.listener(move |this, _, _, cx| {
                        this.drawing = false;
                        if this.frozen.is_none() {
                            this.send(
                                Input::Button {
                                    button: code,
                                    pressed: false,
                                },
                                cx,
                            );
                        }
                    }),
                );
        }
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .cursor_pointer()
                .bg(rgb(0x303840))
                .text_color(rgb(0xffffff))
                .child(label)
        };
        let mut toolbar = div().flex().gap_2().p_2().bg(rgb(0x20242a)).child(
            button(
                "annotate",
                if self.frozen.is_some() {
                    "Resume live"
                } else {
                    "Annotate"
                },
            )
            .on_click(cx.listener(|this, _, _, cx| this.annotate(cx))),
        );
        if self.frozen.is_some() {
            toolbar = toolbar
                .child(
                    button("undo", "Undo").on_click(cx.listener(|this, _, _, cx| {
                        this.strokes.pop();
                        cx.notify();
                    })),
                )
                .child(
                    button("copy", "Copy")
                        .on_click(cx.listener(|this, _, _, cx| this.export(false, cx))),
                );
            if self.target.is_some() {
                toolbar = toolbar.child(
                    button("attach", "Add to prompt")
                        .on_click(cx.listener(|this, _, _, cx| this.export(true, cx))),
                );
            }
        }
        if let Some(status) = &self.status {
            toolbar = toolbar.child(div().text_color(rgb(0xa8d8b0)).child(status.clone()));
        }
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .child(toolbar)
            .child(surface);
        if let Some(error) = &self.error {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .p_2()
                    .bg(rgb(0x401818))
                    .text_color(rgb(0xffffff))
                    .child(error.clone()),
            );
        }
        root
    }
}

/// Burn the same output-pixel strokes into the frozen frame, not a later frame.
fn draw_line(pixels: &mut [u8], size: (usize, usize), from: (u32, u32), to: (u32, u32)) {
    let (mut x, mut y) = (from.0 as i32, from.1 as i32);
    let (end_x, end_y) = (to.0 as i32, to.1 as i32);
    let dx = (end_x - x).abs();
    let dy = -(end_y - y).abs();
    let sx = if x < end_x { 1 } else { -1 };
    let sy = if y < end_y { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        for oy in -2..=2 {
            for ox in -2..=2 {
                if ox * ox + oy * oy > 4 {
                    continue;
                }
                let (px, py) = (x + ox, y + oy);
                if px >= 0 && py >= 0 && (px as usize) < size.0 && (py as usize) < size.1 {
                    let i = (py as usize * size.0 + px as usize) * 4;
                    pixels[i..i + 4].copy_from_slice(&[0x50, 0x40, 0xff, 255]);
                }
            }
        }
        if x == end_x && y == end_y {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x += sx;
        }
        if twice <= dx {
            error += dx;
            y += sy;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::draw_line;
    #[cfg(target_os = "linux")]
    #[test]
    fn planar_video_gpu_rendering() -> anyhow::Result<()> {
        gpui_wgpu::video_tests::planar_video_renders_color_stride_and_clipping()
    }
    #[test]
    fn stroke_keeps_channels_and_clips_at_image_edges() {
        let mut pixels = vec![7; 13 * 9 * 4];
        draw_line(&mut pixels, (13, 9), (0, 1), (11, 7));
        assert_eq!(&pixels[4 * 13..4 * 13 + 4], &[0x50, 0x40, 0xff, 255]);
        assert_eq!(
            &pixels[(7 * 13 + 11) * 4..(7 * 13 + 12) * 4],
            &[0x50, 0x40, 0xff, 255]
        );
        assert_eq!(&pixels[(8 * 13) * 4..(8 * 13 + 1) * 4], &[7; 4]);
    }
}
