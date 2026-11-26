//! The live view of a host desktop: its frames painted as they arrive, the
//! user's mouse and keys sent back, and a frozen frame to draw on and copy.
//! Dropping the view drops the video subscription.
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::*;
use rho_desktop_proto::Input;
use theme::ActiveTheme as _;

/// A decoded image and the one frame the renderer draws it from: the
/// renderer knows a frame by its identity, so an image gets exactly one.
struct Shown {
    image: Arc<crate::viewer::Image>,
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    render: VideoFrame,
}

impl Shown {
    fn new(image: Arc<crate::viewer::Image>) -> anyhow::Result<Self> {
        let render = VideoFrame::new(Arc::new(Planes(image.clone())))?;
        Ok(Self { image, render })
    }
}

struct Planes(Arc<crate::viewer::Image>);

impl Yuv444Data for Planes {
    fn size(&self) -> (u32, u32) {
        (self.0.planes.width() as u32, self.0.planes.height() as u32)
    }
    fn plane(&self, index: usize) -> &[u8] {
        self.0.planes.plane(index)
    }
    fn stride(&self, index: usize) -> u32 {
        self.0.planes.stride(index) as u32
    }
}

/// Where the desktop's keys go: the two modes of every raw place
/// (`rho_frame::keys`), and a frozen frame to draw on.
enum Keys {
    /// The remote app takes keys, clicks and the wheel, all but the
    /// window's reserved chords.
    Live,
    /// The window's keys; the app goes on live but hears nothing.
    Normal,
    /// A frame held still to draw on, in the window's keys.
    Draw(Drawing),
}

struct Drawing {
    frozen: Rc<Shown>,
    strokes: Vec<Vec<(u32, u32)>>,
    /// The pointer is down, drawing the last stroke.
    pen_down: bool,
    copied: bool,
}

pub struct DesktopView {
    viewer: crate::viewer::Viewer,
    image: Option<Rc<Shown>>,
    keys: Keys,
    physical: PhysicalKeys,
    size: (usize, usize),
    bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    error: Option<String>,
    first_paint: Rc<Cell<bool>>,
    painted: Rc<Cell<Option<rho_desktop_proto::FrameId>>>,
    _updates: Task<()>,
    _activation: Subscription,
}
impl DesktopView {
    pub fn new(viewer: crate::viewer::Viewer, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let started = viewer.started_at;
        let desktop_id = viewer.desktop_id;
        tracing::info!(
            desktop_id,
            elapsed_ms = started.elapsed().as_millis(),
            "desktop viewer attached to GUI"
        );
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
                        this.error = error;
                        if let Some(image) = image {
                            if this.image.is_none() {
                                tracing::info!(
                                    desktop_id,
                                    elapsed_ms = started.elapsed().as_millis(),
                                    "desktop first frame delivered to GUI"
                                );
                            }
                            match Shown::new(image) {
                                Ok(shown) => {
                                    if !matches!(this.keys, Keys::Draw(_)) {
                                        this.size = (shown.image.width, shown.image.height);
                                    }
                                    this.image = Some(Rc::new(shown));
                                }
                                Err(error) => this.error = Some(format!("{error:#}")),
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let focus = cx.focus_handle();
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.release_all(cx);
                if let Keys::Draw(drawing) = &mut this.keys {
                    drawing.pen_down = false;
                }
            }
        });
        Self {
            first_paint: Rc::new(Cell::new(true)),
            painted: Rc::new(Cell::new(None)),
            _activation: activation,
            viewer,
            image: None,
            keys: Keys::Live,
            physical: PhysicalKeys::default(),
            size: (1, 1),
            bounds: Rc::new(Cell::new(Bounds::default())),
            focus,
            error: None,
            _updates: updates,
        }
    }

    /// Whether a frozen frame is up to draw on, rather than the live one.
    pub fn annotating(&self) -> bool {
        matches!(self.keys, Keys::Draw(_))
    }

    /// The status line's mode word: `INS` only while the app takes keys.
    pub fn live(&self) -> bool {
        matches!(self.keys, Keys::Live)
    }

    pub fn toggle_annotation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        match self.keys {
            Keys::Draw(_) => self.go_live(cx),
            Keys::Live | Keys::Normal => self.annotate(cx),
        }
    }

    /// Back to the app, live; a drawing is dropped.
    pub(crate) fn go_live(&mut self, cx: &mut Context<Self>) {
        if let Keys::Draw(_) = self.keys
            && let Some(image) = self.viewer.images.borrow().as_ref()
        {
            self.size = (image.width, image.height);
        }
        self.keys = Keys::Live;
        cx.notify();
    }

    fn go_normal(&mut self, _: &rho_frame::keys::Normal, _: &mut Window, cx: &mut Context<Self>) {
        self.release_all(cx);
        self.keys = Keys::Normal;
        cx.notify();
    }

    /// Holds the frame in view still to draw on.
    fn annotate(&mut self, cx: &mut Context<Self>) {
        let Some(frozen) = self.image.clone() else {
            return;
        };
        self.release_all(cx);
        self.keys = Keys::Draw(Drawing {
            frozen,
            strokes: Vec::new(),
            pen_down: false,
            copied: false,
        });
        cx.notify();
    }

    /// Lets go of every key and button the app holds from this view.
    fn release_all(&mut self, cx: &mut Context<Self>) {
        self.physical = PhysicalKeys::default();
        self.viewer.motion.send_replace(None);
        self.send(Input::ReleaseAll, cx);
    }

    /// `f9` in live input: the escape that `escape` itself never sends.
    fn guest_escape(
        &mut self,
        _: &rho_frame::keys::GuestEscape,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cfg!(target_os = "linux") {
            for pressed in [true, false] {
                self.send(
                    Input::Physical {
                        code: KEY_ESC,
                        pressed,
                    },
                    cx,
                );
            }
        } else {
            self.send(Input::Key("escape".into()), cx);
        }
    }

    /// Linux live input: the app hears a key only once gpui has matched
    /// it and left it alone ([`PhysicalKeys`]).
    fn physical_key(
        &mut self,
        event: &PhysicalKeyEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PhysicalKey::LinuxEvdev(code) = event.key;
        let pending = window.has_pending_keystrokes();
        for input in self.physical.event(code, event.pressed, pending) {
            self.send(input, cx);
        }
        cx.stop_propagation();
    }

    fn live_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if cfg!(target_os = "linux") {
            if !event.is_held
                && let Some(input) = self.physical.typed()
            {
                self.send(input, cx);
            }
            return;
        }
        let key = &event.keystroke;
        if !key.modifiers.control
            && !key.modifiers.platform
            && !key.modifiers.alt
            && let Some(text) = &key.key_char
        {
            self.send(Input::Text(text.clone()), cx);
            return;
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
        self.send(Input::Key(chord), cx);
    }

    /// The drawing's own keys, beside the window's.
    fn draw_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let plain = !key.modifiers.control && !key.modifiers.alt;
        match key.key.as_str() {
            "escape" => self.go_live(cx),
            "g" if key.modifiers.control => self.go_live(cx),
            "u" if plain => self.undo_stroke(cx),
            "y" if plain => self.export(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn undo_stroke(&mut self, cx: &mut Context<Self>) {
        if let Keys::Draw(drawing) = &mut self.keys {
            drawing.strokes.pop();
            cx.notify();
        }
    }

    /// Copies the frozen frame with what is drawn on it.
    fn export(&mut self, cx: &mut Context<Self>) {
        let Keys::Draw(drawing) = &self.keys else {
            return;
        };
        let (frozen, strokes) = (drawing.frozen.clone(), drawing.strokes.clone());
        let mut pixels = match frozen.image.export_bgra() {
            Ok(pixels) => pixels,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        for stroke in &strokes {
            if let Some(&point) = stroke.first() {
                draw_line(&mut pixels, self.size, point, point);
            }
            for pair in stroke.windows(2) {
                draw_line(&mut pixels, self.size, pair[0], pair[1]);
            }
        }
        for pixel in pixels.as_chunks_mut::<4>().0 {
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
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes);
        cx.write_to_clipboard(ClipboardItem::new_image(&image));
        if let Keys::Draw(drawing) = &mut self.keys {
            drawing.copied = true;
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
        // Before the first frame, the placeholder dimensions are not
        // coordinates.
        self.image.as_ref()?;
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

/// evdev's escape.
const KEY_ESC: u32 = 1;

/// evdev's modifier keys, which go to the app as they are pressed: they
/// make no `key_down` to wait for.
fn is_modifier(code: u32) -> bool {
    matches!(code, 29 | 42 | 54 | 56 | 58 | 69 | 97 | 100 | 125 | 126)
}

/// The app's side of live physical keys (Linux). gpui matches a key
/// against the window's keymap after the physical press and before the
/// view's `key_down`, so a press is held from the app until then: one that
/// reaches `key_down` ([`Self::typed`]) is the app's. One released, or
/// followed by another press, without reaching it was taken by a window
/// binding and never reaches the app, unless gpui still holds it as the
/// first key of a longer binding (`pending`), which it then gives back.
#[derive(Default)]
struct PhysicalKeys {
    held: Option<u32>,
    /// Keys whose press the app has had, to release.
    sent: HashSet<u32>,
}

impl PhysicalKeys {
    fn event(&mut self, code: u32, pressed: bool, pending: bool) -> Vec<Input> {
        let mut out = Vec::new();
        if pressed {
            if is_modifier(code) {
                out.extend(self.press(code));
            } else if let Some(previous) = self.held.replace(code)
                && previous != code
                && pending
            {
                out.extend(self.press(previous));
            }
            return out;
        }
        if self.held == Some(code) {
            self.held = None;
            if pending {
                out.extend(self.press(code));
            }
        }
        if self.sent.remove(&code) {
            out.push(Input::Physical {
                code,
                pressed: false,
            });
        }
        out
    }

    /// gpui left the held key alone: its press is the app's.
    fn typed(&mut self) -> Option<Input> {
        let code = self.held.take()?;
        self.press(code)
    }

    fn press(&mut self, code: u32) -> Option<Input> {
        self.sent.insert(code).then_some(Input::Physical {
            code,
            pressed: true,
        })
    }
}

impl Focusable for DesktopView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DesktopView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (image, strokes) = match &self.keys {
            Keys::Draw(drawing) => (Some(drawing.frozen.clone()), drawing.strokes.clone()),
            Keys::Live | Keys::Normal => (self.image.clone(), Vec::new()),
        };
        let first_paint = self.first_paint.clone();
        let painted = self.painted.clone();
        let started = self.viewer.started_at;
        let desktop_id = self.viewer.desktop_id;
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
                    let presented = image.image.clone();
                    on_presented(image.image.id, &painted, window, move || {
                        presented.presented()
                    });
                    if first_paint.replace(false) {
                        tracing::info!(
                            desktop_id,
                            elapsed_ms = started.elapsed().as_millis(),
                            "desktop first frame paint queued"
                        );
                        // A paint callback only queues primitives. This
                        // next-frame marker also
                        // exposes stalls in rendering/presentation.
                        window.on_next_frame(move |_, _| {
                            tracing::info!(
                                desktop_id,
                                elapsed_ms = started.elapsed().as_millis(),
                                "desktop frame callback after first paint"
                            );
                        });
                    }
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
        // Live, the app takes every key the window does not keep; escaped
        // or drawing, the window's keys work as on any screen.
        let context = match self.keys {
            Keys::Live => format!("RhoDesktop {}", rho_frame::keys::LIVE),
            Keys::Normal | Keys::Draw(_) => format!("RhoDesktop {}", rho_frame::keys::PLACE),
        };
        let mut surface = div()
            .id("remote-wayland")
            .key_context(context.as_str())
            .track_focus(&self.focus)
            .flex_1()
            .min_h_0()
            .relative()
            .bg(rgb(0x161616))
            .on_action(cx.listener(|this, _: &rho_frame::keys::Live, _, cx| this.go_live(cx)))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(position) = this.position(event.position) else {
                    return;
                };
                match &mut this.keys {
                    Keys::Live | Keys::Normal => {
                        this.viewer.motion.send_replace(Some(position));
                    }
                    Keys::Draw(drawing) => {
                        if drawing.pen_down
                            && let Some(stroke) = drawing.strokes.last_mut()
                            && stroke.last() != Some(&position)
                        {
                            stroke.push(position);
                            cx.notify();
                        }
                    }
                }
            }))
            .child(canvas);
        surface = match self.keys {
            Keys::Live => surface
                .on_action(cx.listener(Self::go_normal))
                .on_action(cx.listener(Self::guest_escape))
                .on_physical_key(cx.listener(Self::physical_key))
                .on_key_down(cx.listener(Self::live_key_down)),
            Keys::Draw(_) => surface.on_key_down(cx.listener(Self::draw_key_down)),
            Keys::Normal => surface,
        };
        // Only the keys wait for `i`: the pointer and the wheel work on a
        // live app either way, as a click is how live input comes back.
        if !matches!(self.keys, Keys::Draw(_)) {
            surface =
                surface.on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
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
                }));
        }
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
                            if let Keys::Draw(drawing) = &mut this.keys {
                                if button == MouseButton::Left {
                                    drawing.strokes.push(vec![(x, y)]);
                                    drawing.pen_down = true;
                                    cx.notify();
                                }
                                return;
                            }
                            // A click on the app is live input again.
                            this.go_live(cx);
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
                        this.button_up(code, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    button,
                    cx.listener(move |this, _, _, cx| this.button_up(code, cx)),
                );
        }
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .child(surface);
        if let Keys::Draw(drawing) = &self.keys {
            let action =
                |id: &'static str, label: &'static str| div().id(id).cursor_pointer().child(label);
            let mut mode_line = div()
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py(px(3.))
                .text_size(px(12.))
                .bg(cx.theme().colors().editor_background)
                .text_color(cx.theme().colors().text_muted)
                .child(
                    div()
                        .text_color(cx.theme().colors().text_accent)
                        .child("DRAW"),
                )
                .child(
                    action("undo", "u undo")
                        .on_click(cx.listener(|this, _, _, cx| this.undo_stroke(cx))),
                )
                .child(
                    action("copy", "y copy")
                        .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
                )
                .child(
                    action("resume", "esc live")
                        .on_click(cx.listener(|this, _, _, cx| this.go_live(cx))),
                );
            if drawing.copied {
                mode_line = mode_line.child(div().child("Annotation copied"));
            }
            root = root.child(mode_line);
        }
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

impl DesktopView {
    fn button_up(&mut self, code: u32, cx: &mut Context<Self>) {
        match &mut self.keys {
            Keys::Draw(drawing) => drawing.pen_down = false,
            Keys::Live => self.send(
                Input::Button {
                    button: code,
                    pressed: false,
                },
                cx,
            ),
            Keys::Normal => {}
        }
    }
}

/// A repeated paint (including annotation on a frozen frame) must not refresh
/// the source's progress clock. Report only after the following GUI frame.
fn on_presented(
    id: rho_desktop_proto::FrameId,
    painted: &Cell<Option<rho_desktop_proto::FrameId>>,
    window: &Window,
    report: impl FnOnce() + 'static,
) {
    if painted.get().is_none_or(|previous| id > previous) {
        painted.set(Some(id));
        // Renderer progress, not a hardware scanout fence.
        window.on_next_frame(move |_, _| report());
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
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{EmptyView, TestAppContext};
    use rho_desktop_proto::Input;

    use super::{PhysicalKeys, draw_line, on_presented};

    fn press(code: u32) -> Input {
        Input::Physical {
            code,
            pressed: true,
        }
    }

    fn release(code: u32) -> Input {
        Input::Physical {
            code,
            pressed: false,
        }
    }

    const CTRL: u32 = 29;
    const SEMICOLON: u32 = 39;
    const K: u32 = 37;
    const A: u32 = 30;
    const O: u32 = 24;

    /// `ctrl-shift-;` opens the palette: the app gets the modifier, which
    /// it cannot act on alone, and never the `;`.
    #[test]
    fn a_key_a_window_binding_takes_never_reaches_the_app() {
        let mut keys = PhysicalKeys::default();
        assert_eq!(keys.event(CTRL, true, false), [press(CTRL)]);
        assert_eq!(keys.event(SEMICOLON, true, false), []);
        // No `key_down`: the binding took it.
        assert_eq!(keys.event(SEMICOLON, false, false), []);
        assert_eq!(keys.event(CTRL, false, false), [release(CTRL)]);
        assert_eq!(keys.typed(), None);
    }

    #[test]
    fn a_key_the_window_leaves_reaches_the_app_once_matched() {
        let mut keys = PhysicalKeys::default();
        assert_eq!(keys.event(A, true, false), []);
        assert_eq!(keys.typed(), Some(press(A)));
        // A second press before the first is released is held in turn.
        assert_eq!(keys.event(K, true, false), []);
        assert_eq!(keys.event(A, false, false), [release(A)]);
        assert_eq!(keys.typed(), Some(press(K)));
        assert_eq!(keys.event(K, false, false), [release(K)]);
    }

    /// gpui holds `ctrl-k` while a longer binding could follow; a key it
    /// still holds is the app's, not the window's.
    #[test]
    fn a_key_gpui_still_holds_goes_to_the_app() {
        let mut keys = PhysicalKeys::default();
        assert_eq!(keys.event(K, true, false), []);
        assert_eq!(keys.event(K, false, true), [press(K), release(K)]);
        assert_eq!(keys.event(K, true, false), []);
        assert_eq!(keys.event(O, true, true), [press(K)]);
        assert_eq!(keys.event(K, false, false), [release(K)]);
    }

    #[gpui::test]
    fn presentation_feedback_waits_for_frame_and_ignores_repaints(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let painted = Cell::new(None);
        let reported = Rc::new(Cell::new(0));
        window
            .update(cx, |_, window, _| {
                for (epoch, timestamp_us) in [(2, 50), (2, 50), (1, 90), (2, 60)] {
                    let reported = reported.clone();
                    on_presented(
                        rho_desktop_proto::FrameId {
                            epoch,
                            timestamp_us,
                        },
                        &painted,
                        window,
                        move || reported.set(reported.get() + 1),
                    );
                }
                assert_eq!(reported.get(), 0, "paint is not presentation");
            })
            .unwrap();
        window
            .update(cx, |_, window, cx| {
                assert_eq!(window.simulate_next_frame(cx), 2);
            })
            .unwrap();
        assert_eq!(reported.get(), 2, "one callback per advancing frame");
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
