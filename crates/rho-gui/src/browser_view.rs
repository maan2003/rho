//! Client-local stock Chrome embedded through a private Wayland compositor.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, KeyUpEvent, LinuxDmaBufSurface, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, ParentElement as _, Render, RenderImage, Styled as _,
    StyledImage as _, Subscription, Task, Window, canvas, div, img, surface,
};
use image::{Frame, RgbaImage};
use rho_browser_wayland::{BrowserEvent, BrowserProgram, BrowserSession};
use smallvec::smallvec;

pub struct BrowserModel {
    session: Option<BrowserSession>,
    frame: Option<Arc<RenderImage>>,
    dma_buf: Option<LinuxDmaBufSurface>,
    status: Option<String>,
    sent_size: Rc<Cell<(u32, u32)>>,
    _task: Task<()>,
}

impl BrowserModel {
    pub fn new(url: String, cx: &mut Context<Self>) -> Self {
        Self::new_program(BrowserProgram::Chromium, url, cx)
    }

    pub fn new_program(program: BrowserProgram, url: String, cx: &mut Context<Self>) -> Self {
        let executable = PathBuf::from(match program {
            BrowserProgram::Chromium => rho_browser_wayland::chrome_wrapper(),
            BrowserProgram::Firefox => rho_browser_wayland::firefox_wrapper(),
        });
        let dma_buf = (program == BrowserProgram::Chromium)
            .then(gpui::linux_dmabuf_device)
            .flatten()
            .map(|device| rho_browser_wayland::DmaBufConfig {
                render_node: device.render_node.clone(),
                device_id: device.device_id,
                formats: device
                    .formats
                    .iter()
                    .map(|format| (format.fourcc, format.modifier))
                    .collect(),
            });
        let launch = cx.background_spawn(async move {
            BrowserSession::launch(program, executable, &url, (1280, 720), dma_buf)
        });
        let task = cx.spawn(async move |this, cx| {
            match launch.await {
                Ok(session) => {
                    if this
                        .update(cx, |model, cx| {
                            model.session = Some(session);
                            model.status = Some("waiting for Chrome".into());
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = this.update(cx, |model, cx| {
                        model.status = Some(format!("Chrome failed: {error:#}"));
                        cx.notify();
                    });
                    return;
                }
            }

            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(8))
                    .await;
                let keep_running = this
                    .update(cx, |model, cx| {
                        let mut changed = false;
                        let mut running = true;
                        while let Some(event) = model.session.as_ref().and_then(|s| s.try_recv()) {
                            changed = true;
                            match event {
                                BrowserEvent::Frame(frame) => {
                                    let Some(buffer) = RgbaImage::from_raw(
                                        frame.width,
                                        frame.height,
                                        frame.rgba.to_vec(),
                                    ) else {
                                        model.status = Some("Chrome sent an invalid frame".into());
                                        continue;
                                    };
                                    model.frame =
                                        Some(Arc::new(RenderImage::new(smallvec![Frame::new(
                                            buffer
                                        ),])));
                                    model.dma_buf = None;
                                    model.status = None;
                                }
                                BrowserEvent::DmaBuf(mut frame) => {
                                    let Some(session) = &model.session else {
                                        continue;
                                    };
                                    let fd = match frame.duplicate_fd() {
                                        Ok(fd) => fd,
                                        Err(error) => {
                                            model.status =
                                                Some(format!("duplicate Chrome DMA-BUF: {error}"));
                                            continue;
                                        }
                                    };
                                    let acquire = match frame.duplicate_acquire_fence() {
                                        Ok(fd) => fd,
                                        Err(error) => {
                                            model.status = Some(format!(
                                                "duplicate Chrome acquire fence: {error}"
                                            ));
                                            continue;
                                        }
                                    };
                                    model.dma_buf = Some(LinuxDmaBufSurface::new(
                                        frame.id,
                                        frame.width,
                                        frame.height,
                                        frame.fourcc,
                                        frame.modifier,
                                        frame.stride,
                                        frame.offset,
                                        frame.y_inverted,
                                        fd,
                                        acquire,
                                        session.presentation_callback(frame.id),
                                        frame.take_release(),
                                    ));
                                    model.frame = None;
                                    model.status = None;
                                }
                                BrowserEvent::Cleared => {
                                    model.dma_buf = None;
                                    model.frame = None;
                                    model.status = None;
                                }
                                BrowserEvent::ToplevelReady => {
                                    model.status = Some("Chrome is starting".into());
                                }
                                BrowserEvent::ChromeExited(code) => {
                                    model.status = Some(match code {
                                        Some(code) => format!("Chrome exited ({code})"),
                                        None => "Chrome exited".into(),
                                    });
                                    running = false;
                                }
                                BrowserEvent::Failed(error) => {
                                    model.status =
                                        Some(format!("browser compositor failed: {error}"));
                                    running = false;
                                }
                            }
                        }
                        if changed {
                            cx.notify();
                        }
                        running
                    })
                    .unwrap_or(false);
                if !keep_running {
                    return;
                }
            }
        });
        Self {
            session: None,
            frame: None,
            dma_buf: None,
            status: Some("launching stock Chrome".into()),
            sent_size: Rc::new(Cell::new((0, 0))),
            _task: task,
        }
    }

    fn resize(&self, width: u32, height: u32) {
        if self.sent_size.replace((width, height)) != (width, height) {
            if let Some(session) = &self.session {
                session.resize(width, height);
            }
        }
    }

    fn presented(&self) {
        if let Some(session) = &self.session {
            session.presented();
        }
    }

    fn pointer_motion(&self, x: f64, y: f64) {
        if let Some(session) = &self.session {
            session.pointer_motion(x, y);
        }
    }

    fn pointer_button(&self, button: u32, pressed: bool) {
        if let Some(session) = &self.session {
            session.pointer_button(button, pressed);
        }
    }

    fn key(&self, keycode: u32, pressed: bool) {
        if let Some(session) = &self.session {
            session.key(keycode, pressed);
        }
    }
}

pub struct BrowserView {
    model: Entity<BrowserModel>,
    focus_handle: FocusHandle,
    origin: Rc<Cell<(f32, f32)>>,
    _model_changed: Subscription,
}

impl BrowserView {
    pub fn new(model: Entity<BrowserModel>, cx: &mut Context<Self>) -> Self {
        let model_changed = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus_handle: cx.focus_handle(),
            origin: Rc::new(Cell::new((0.0, 0.0))),
            _model_changed: model_changed,
        }
    }

    pub fn model(&self) -> &Entity<BrowserModel> {
        &self.model
    }

    fn local_position(&self, position: gpui::Point<gpui::Pixels>) -> (f64, f64) {
        let (x, y) = self.origin.get();
        (
            (f32::from(position.x) - x).max(0.0) as f64,
            (f32::from(position.y) - y).max(0.0) as f64,
        )
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = self.local_position(event.position);
        self.model.read(cx).pointer_motion(x, y);
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        let (x, y) = self.local_position(event.position);
        let model = self.model.read(cx);
        model.pointer_motion(x, y);
        model.pointer_button(linux_button(event.button), true);
        cx.stop_propagation();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.model
            .read(cx)
            .pointer_button(linux_button(event.button), false);
        cx.stop_propagation();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.forward_key(&event.keystroke, true, cx);
    }

    fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.forward_key(&event.keystroke, false, cx);
    }

    fn forward_key(&self, keystroke: &gpui::Keystroke, pressed: bool, cx: &mut Context<Self>) {
        let Some(keycode) = evdev_keycode(&keystroke.key) else {
            return;
        };
        let model = self.model.read(cx);
        let modifiers = [
            (keystroke.modifiers.control, 29),
            (keystroke.modifiers.alt, 56),
            (keystroke.modifiers.shift, 42),
        ];
        if pressed {
            for (active, code) in modifiers {
                if active {
                    model.key(code, true);
                }
            }
            model.key(keycode, true);
        } else {
            model.key(keycode, false);
            for (active, code) in modifiers.into_iter().rev() {
                if active {
                    model.key(code, false);
                }
            }
        }
        cx.stop_propagation();
    }
}

fn linux_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Navigate(_) => 0x113,
    }
}

fn evdev_keycode(key: &str) -> Option<u32> {
    Some(match key {
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "escape" => 1,
        "backspace" => 14,
        "tab" => 15,
        "enter" => 28,
        "space" => 57,
        "left" => 105,
        "right" => 106,
        "up" => 103,
        "down" => 108,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "delete" => 111,
        "insert" => 110,
        "-" => 12,
        "=" => 13,
        "[" => 26,
        "]" => 27,
        ";" => 39,
        "'" => 40,
        "`" => 41,
        "\\" => 43,
        "," => 51,
        "." => 52,
        "/" => 53,
        _ => return None,
    })
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let frame = model.frame.clone();
        let dma_buf = model.dma_buf.clone();
        let status = model.status.clone();
        let browser = self.model.clone();
        let origin = self.origin.clone();
        let measure = canvas(
            move |bounds, _, cx| {
                origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
                let width = f32::from(bounds.size.width).round().max(1.0) as u32;
                let height = f32::from(bounds.size.height).round().max(1.0) as u32;
                browser.read(cx).resize(width, height);
            },
            {
                let browser = self.model.clone();
                move |_, _, window, _| {
                    window.on_next_frame(move |_, cx| browser.read(cx).presented());
                }
            },
        )
        .size_full();

        div()
            .id("rho-browser")
            .track_focus(&self.focus_handle)
            .key_context("RhoBrowser")
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(gpui::black())
            .children(frame.map(|frame| {
                div()
                    .absolute()
                    .size_full()
                    .child(img(frame).size_full().object_fit(ObjectFit::Fill))
            }))
            .children(dma_buf.map(|frame| {
                div()
                    .absolute()
                    .size_full()
                    .child(surface(frame).size_full().object_fit(ObjectFit::Fill))
            }))
            .child(div().absolute().size_full().child(measure))
            .children(status.map(|status| {
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .p_2()
                    .bg(gpui::black().opacity(0.8))
                    .text_color(gpui::white())
                    .child(status)
            }))
    }
}
