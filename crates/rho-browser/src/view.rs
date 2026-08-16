//! A GPUI portal onto a durable client-local web page.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    LinuxAxisRelativeDirection, LinuxAxisSource, LinuxDmaBufSurface, LinuxPinchEvent,
    LinuxPointerAxisEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, PhysicalKey, PhysicalKeyEvent, Render, Styled as _, Subscription, Task,
    Window, canvas, div, surface,
};
use rho_browser_wayland::{
    BrowserEvent, BrowserSession, PinchGesture, PointerAxisDirection, PointerAxisFrame,
    PointerAxisSource,
};

use crate::PageRecord;

struct RuntimePageState {
    session: Option<BrowserSession<crate::PageId>>,
    dma_buf: Option<LinuxDmaBufSurface>,
    status: Option<String>,
    sent_size: Rc<Cell<(u32, u32, u32)>>,
}

/// The persisted and live state associated with one durable page ID.
pub struct BrowserModel {
    persisted: PageRecord,
    runtime: RuntimePageState,
    _events_task: Task<()>,
}

impl BrowserModel {
    pub fn new_record(
        persisted: PageRecord,
        launch: anyhow::Result<BrowserSession<crate::PageId>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (session, status) = match launch {
            Ok(session) => (Some(session), Some("waiting for Chrome".into())),
            Err(error) => (None, Some(format!("Chrome failed: {error:#}"))),
        };
        let events = session.as_ref().map(BrowserSession::events);
        let events_task = cx.spawn(async move |this, cx| {
            let Some(events) = events else { return };
            while let Ok(event) = events.recv().await {
                let terminal = matches!(event, BrowserEvent::Closed | BrowserEvent::Failed(_));
                if this
                    .update(cx, |model, cx| {
                        match event {
                            BrowserEvent::DmaBuf(mut frame) => {
                                let Some(session) = &model.runtime.session else {
                                    return;
                                };
                                let Ok(fd) = frame.duplicate_fd() else {
                                    model.runtime.status =
                                        Some("duplicate Chrome DMA-BUF failed".into());
                                    cx.notify();
                                    return;
                                };
                                let Ok(acquire) = frame.duplicate_acquire_fence() else {
                                    model.runtime.status =
                                        Some("duplicate Chrome acquire fence failed".into());
                                    cx.notify();
                                    return;
                                };
                                model.runtime.dma_buf = Some(LinuxDmaBufSurface::new(
                                    frame.id,
                                    frame.width,
                                    frame.height,
                                    frame.fourcc,
                                    frame.modifier,
                                    frame.stride,
                                    frame.offset,
                                    frame.y_inverted,
                                    frame.source_origin,
                                    frame.source_size,
                                    fd,
                                    acquire,
                                    session.presentation_callback(frame.id),
                                    frame.take_release(),
                                ));
                                model.runtime.status = None;
                            }
                            BrowserEvent::FrameRetired(commit_id) => {
                                if model
                                    .runtime
                                    .dma_buf
                                    .as_ref()
                                    .is_some_and(|frame| frame.id() == commit_id)
                                {
                                    model.runtime.dma_buf = None;
                                    model.runtime.status =
                                        Some("Chrome frame import was retired".into());
                                }
                            }
                            BrowserEvent::ToplevelReady => {
                                model.runtime.status = Some("Chrome is starting".into());
                            }
                            BrowserEvent::Closed => {
                                model.runtime.status = Some("browser closed".into());
                                model.runtime.session = None;
                            }
                            BrowserEvent::Failed(error) => {
                                model.runtime.status = Some(format!("browser failed: {error}"));
                                model.runtime.session = None;
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                if terminal {
                    return;
                }
            }
        });
        Self {
            persisted,
            runtime: RuntimePageState {
                session,
                dma_buf: None,
                status,
                sent_size: Rc::new(Cell::new((0, 0, 0))),
            },
            _events_task: events_task,
        }
    }

    pub fn record(&self) -> &PageRecord {
        &self.persisted
    }

    fn resize(&self, width: u32, height: u32, scale: f32) {
        let scale = (scale * 120.0).round() / 120.0;
        let requested = (width, height, scale.to_bits());
        if self.runtime.sent_size.replace(requested) != requested
            && let Some(session) = &self.runtime.session
        {
            session.resize(width, height, scale);
        }
    }

    fn pointer_motion(&self, x: f64, y: f64) {
        if let (Some(session), Some(frame)) = (&self.runtime.session, &self.runtime.dma_buf) {
            session.pointer_motion(frame.id(), x, y);
        }
    }

    fn pointer_button(&self, button: u32, pressed: bool) {
        if let Some(session) = &self.runtime.session {
            session.pointer_button(button, pressed);
        }
    }

    fn pointer_axis(&self, event: &LinuxPointerAxisEvent) {
        if let Some(session) = &self.runtime.session {
            session.pointer_axis(PointerAxisFrame {
                source: match event.source {
                    LinuxAxisSource::Finger => PointerAxisSource::Finger,
                    LinuxAxisSource::Continuous => PointerAxisSource::Continuous,
                    LinuxAxisSource::Wheel => PointerAxisSource::Wheel,
                    LinuxAxisSource::WheelTilt => PointerAxisSource::WheelTilt,
                },
                value: event.value,
                v120: event.v120,
                stop: event.stop,
                relative_direction: (
                    axis_direction(event.relative_direction.0),
                    axis_direction(event.relative_direction.1),
                ),
            });
        }
    }

    fn pinch(&self, event: LinuxPinchEvent) {
        if let Some(session) = &self.runtime.session {
            session.pinch(match event {
                LinuxPinchEvent::Begin { fingers, .. } => PinchGesture::Begin { fingers },
                LinuxPinchEvent::Update {
                    delta,
                    scale,
                    rotation,
                    ..
                } => PinchGesture::Update {
                    delta,
                    scale,
                    rotation,
                },
                LinuxPinchEvent::End { cancelled } => PinchGesture::End { cancelled },
            });
        }
    }

    fn key(&self, keycode: u32, pressed: bool) {
        if let Some(session) = &self.runtime.session {
            session.key(keycode, pressed);
        }
    }
}

pub struct BrowserView {
    model: Entity<BrowserModel>,
    focus_handle: FocusHandle,
    origin: Rc<Cell<(f32, f32)>>,
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u32>,
    finger_axes: (bool, bool),
    pinch_active: bool,
    blur_subscription: Option<Subscription>,
    _model_changed: Subscription,
}

impl BrowserView {
    pub fn new(model: Entity<BrowserModel>, cx: &mut Context<Self>) -> Self {
        let model_changed = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus_handle: cx.focus_handle(),
            origin: Rc::new(Cell::new((0.0, 0.0))),
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            finger_axes: (false, false),
            pinch_active: false,
            blur_subscription: None,
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
        let button = linux_button(event.button);
        self.pressed_buttons.insert(button);
        model.pointer_button(button, true);
        cx.stop_propagation();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let button = linux_button(event.button);
        self.pressed_buttons.remove(&button);
        self.model.read(cx).pointer_button(button, false);
        cx.stop_propagation();
    }

    fn pointer_axis(
        &mut self,
        event: &LinuxPointerAxisEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.source == LinuxAxisSource::Finger {
            if event.value.0 != 0.0 {
                self.finger_axes.0 = true;
            }
            if event.value.1 != 0.0 {
                self.finger_axes.1 = true;
            }
            if event.stop.0 {
                self.finger_axes.0 = false;
            }
            if event.stop.1 {
                self.finger_axes.1 = false;
            }
        }
        self.model.read(cx).pointer_axis(event);
        cx.stop_propagation();
    }

    fn pinch(&mut self, event: &LinuxPinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.pinch_active = !matches!(event, LinuxPinchEvent::End { .. });
        self.model.read(cx).pinch(*event);
        cx.stop_propagation();
    }

    fn physical_key(&mut self, event: &PhysicalKeyEvent, _: &mut Window, cx: &mut Context<Self>) {
        let PhysicalKey::LinuxEvdev(keycode) = event.key;
        if event.pressed {
            if self.pressed_keys.insert(keycode) {
                self.model.read(cx).key(keycode, true);
            }
        } else if self.pressed_keys.remove(&keycode) {
            self.model.read(cx).key(keycode, false);
        }
        cx.stop_propagation();
    }

    fn release_input(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        for keycode in self.pressed_keys.drain() {
            model.key(keycode, false);
        }
        for button in self.pressed_buttons.drain() {
            model.pointer_button(button, false);
        }
        if std::mem::take(&mut self.finger_axes) != (false, false) {
            model.pointer_axis(&LinuxPointerAxisEvent {
                position: Default::default(),
                source: LinuxAxisSource::Finger,
                value: (0.0, 0.0),
                v120: (None, None),
                stop: (true, true),
                relative_direction: (
                    LinuxAxisRelativeDirection::Identical,
                    LinuxAxisRelativeDirection::Identical,
                ),
            });
        }
        if std::mem::take(&mut self.pinch_active) {
            model.pinch(LinuxPinchEvent::End { cancelled: true });
        }
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

fn axis_direction(direction: LinuxAxisRelativeDirection) -> PointerAxisDirection {
    match direction {
        LinuxAxisRelativeDirection::Identical => PointerAxisDirection::Identical,
        LinuxAxisRelativeDirection::Inverted => PointerAxisDirection::Inverted,
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.blur_subscription.is_none() {
            self.blur_subscription = Some(cx.on_blur(&self.focus_handle, window, |this, _, cx| {
                this.release_input(cx)
            }));
        }
        let model = self.model.read(cx);
        let dma_buf = model.runtime.dma_buf.clone();
        let status = model.runtime.status.clone();
        let scale = window.scale_factor();
        let browser = self.model.clone();
        let origin = self.origin.clone();
        let measure = canvas(
            move |bounds, _, cx| {
                origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
                let width = f32::from(bounds.size.width).round().max(1.0) as u32;
                let height = f32::from(bounds.size.height).round().max(1.0) as u32;
                browser.read(cx).resize(width, height, scale);
            },
            |_, _, _, _| {},
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
            .on_linux_pointer_axis(cx.listener(Self::pointer_axis))
            .on_linux_pinch(cx.listener(Self::pinch))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_pinch(|_, _, cx| cx.stop_propagation())
            .on_physical_key(cx.listener(Self::physical_key))
            .on_key_down(|_, _, cx| cx.stop_propagation())
            .on_key_up(|_, _, cx| cx.stop_propagation())
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(gpui::black())
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
