//! A GPUI portal onto a durable client-local web page.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, LinuxAxisRelativeDirection, LinuxAxisSource, LinuxDmaBufSurface, LinuxPinchEvent,
    LinuxPointerAxisEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, PhysicalKey, PhysicalKeyEvent, Render, RenderImage, Styled as _,
    StyledImage as _, Subscription, Task, Window, canvas, div, img, px, surface,
};
use image::{Frame, RgbaImage};
use rho_browser_wayland::{
    BrowserEvent, BrowserSession, BufferImport, PinchGesture, PointerAxisDirection,
    PointerAxisFrame, PointerAxisSource, SceneNode,
};
use theme::ActiveTheme as _;

use crate::PageId;
use crate::runtime::{BrowserRuntime, BrowserWindow};

struct RuntimePageState {
    session: Option<BrowserSession<BrowserWindow>>,
    buffers: HashMap<u64, BrowserBuffer>,
    scene: Vec<SceneNode>,
    scene_id: Option<u64>,
    painted_scene_id: Option<u64>,
    invalidated_through: u64,
    presented_barrier: u64,
    status: Option<String>,
    sent_size: Rc<Cell<(u32, u32, u32)>>,
}

#[derive(Clone)]
enum BrowserBuffer {
    DmaBuf(LinuxDmaBufSurface),
    Shm(Arc<RenderImage>),
}

/// The singleton live Chrome surface shared by every logical page view.
pub struct BrowserModel {
    browser: Arc<BrowserRuntime>,
    focused_page: Option<PageId>,
    desired_page: Option<PageId>,
    focus_in_flight: bool,
    next_frame_barrier: u64,
    awaiting_frame: Option<(u64, PageId)>,
    presentation_owner: Option<EntityId>,
    runtime: RuntimePageState,
    _events_task: Task<()>,
}

impl BrowserModel {
    pub(crate) fn new(
        browser: Arc<BrowserRuntime>,
        session: BrowserSession<BrowserWindow>,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = session.events();
        let events_task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let terminal = matches!(event, BrowserEvent::Closed | BrowserEvent::Failed(_));
                if this
                    .update(cx, |model, cx| {
                        match event {
                            BrowserEvent::Scene(mut scene) => {
                                if model.runtime.session.is_none() {
                                    return;
                                }
                                // Imports are leases independent of whether this scene is
                                // eligible for the current page handoff. A later coalesced
                                // scene may still reference them without re-importing.
                                let mut import_failed = false;
                                for import in scene.imports.drain(..) {
                                    let (id, buffer) = match import {
                                        BufferImport::DmaBuf(mut frame) => {
                                            let Ok(fd) = frame.duplicate_fd() else {
                                                model.runtime.status =
                                                    Some("duplicate Chrome DMA-BUF failed".into());
                                                import_failed = true;
                                                continue;
                                            };
                                            let Ok(acquire) = frame.duplicate_acquire_fence()
                                            else {
                                                model.runtime.status = Some(
                                                    "duplicate Chrome acquire fence failed".into(),
                                                );
                                                import_failed = true;
                                                continue;
                                            };
                                            let id = frame.id;
                                            let surface = LinuxDmaBufSurface::new(
                                                id,
                                                frame.width,
                                                frame.height,
                                                frame.fourcc,
                                                frame.modifier,
                                                frame.stride,
                                                frame.offset,
                                                frame.y_inverted,
                                                fd,
                                                acquire,
                                                || {},
                                                frame.take_release(),
                                            );
                                            (id, BrowserBuffer::DmaBuf(surface))
                                        }
                                        BufferImport::Shm(frame) => {
                                            let mut pixels = frame.pixels;
                                            bgra_to_rgba(&mut pixels);
                                            let Some(buffer) = RgbaImage::from_raw(
                                                frame.width,
                                                frame.height,
                                                pixels,
                                            ) else {
                                                model.runtime.status =
                                                    Some("invalid Chrome SHM surface".into());
                                                import_failed = true;
                                                continue;
                                            };
                                            (
                                                frame.id,
                                                BrowserBuffer::Shm(Arc::new(RenderImage::new(
                                                    smallvec::SmallVec::from_const([Frame::new(
                                                        buffer,
                                                    )]),
                                                ))),
                                            )
                                        }
                                    };
                                    if let Some(BrowserBuffer::Shm(image)) =
                                        model.runtime.buffers.insert(id, buffer)
                                    {
                                        cx.drop_image(image, None);
                                    }
                                }
                                let missing_buffer = scene.attached.iter().any(|buffer_id| {
                                    !model.runtime.buffers.contains_key(buffer_id)
                                });
                                let eligible = frame_is_eligible(
                                    model.desired_page,
                                    model.focused_page,
                                    model.awaiting_frame,
                                    scene.barrier,
                                );
                                if import_failed || missing_buffer || !eligible {
                                    if missing_buffer && !import_failed {
                                        model.runtime.status =
                                            Some("Chrome scene referenced a missing buffer".into());
                                    }
                                    let mut retained =
                                        scene.attached.iter().copied().collect::<HashSet<_>>();
                                    retained.extend(
                                        model.runtime.scene.iter().map(|node| node.buffer_id),
                                    );
                                    model.runtime.buffers.retain(|buffer_id, buffer| {
                                        let keep = retained.contains(buffer_id);
                                        if !keep && let BrowserBuffer::Shm(image) = buffer {
                                            cx.drop_image(image.clone(), None);
                                        }
                                        keep
                                    });
                                    return;
                                }
                                let referenced =
                                    scene.attached.iter().copied().collect::<HashSet<_>>();
                                model.runtime.buffers.retain(|buffer_id, buffer| {
                                    let keep = referenced.contains(buffer_id);
                                    if !keep && let BrowserBuffer::Shm(image) = buffer {
                                        cx.drop_image(image.clone(), None);
                                    }
                                    keep
                                });
                                complete_frame_handoff(
                                    model.desired_page,
                                    &mut model.focused_page,
                                    &mut model.awaiting_frame,
                                    scene.barrier,
                                );
                                model.runtime.presented_barrier = scene.barrier;
                                model.runtime.scene_id = Some(scene.id);
                                model.runtime.scene = scene.nodes;
                                model.runtime.status = None;
                            }
                            BrowserEvent::FrameRetired(buffer_id) => {
                                let was_visible = model
                                    .runtime
                                    .scene
                                    .iter()
                                    .any(|node| node.buffer_id == buffer_id);
                                if let Some(BrowserBuffer::Shm(image)) =
                                    model.runtime.buffers.remove(&buffer_id)
                                {
                                    cx.drop_image(image, None);
                                }
                                if was_visible {
                                    if let Some(scene_id) = model.runtime.scene_id {
                                        model.runtime.invalidated_through =
                                            model.runtime.invalidated_through.max(scene_id);
                                    }
                                    model.runtime.scene.clear();
                                    model.runtime.scene_id = None;
                                    model.runtime.painted_scene_id = None;
                                    model.runtime.status =
                                        Some("Chrome surface import was retired".into());
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
            browser,
            focused_page: None,
            desired_page: None,
            focus_in_flight: false,
            next_frame_barrier: 1,
            awaiting_frame: None,
            presentation_owner: None,
            runtime: RuntimePageState {
                session: Some(session),
                buffers: HashMap::new(),
                scene: Vec::new(),
                scene_id: None,
                painted_scene_id: None,
                invalidated_through: 0,
                presented_barrier: 0,
                status: Some("waiting for Chrome".into()),
                sent_size: Rc::new(Cell::new((1280, 720, 1.0_f32.to_bits()))),
            },
            _events_task: events_task,
        }
    }

    pub(crate) fn focus(&mut self, id: PageId, cx: &mut Context<Self>) {
        self.desired_page = Some(id);
        self.start_focus(cx);
    }

    fn start_focus(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.desired_page else {
            return;
        };
        if self.focus_in_flight
            || self.focused_page == Some(id)
            || self.awaiting_frame.is_some_and(|(_, page)| page == id)
        {
            return;
        }
        self.focus_in_flight = true;
        let browser = self.browser.clone();
        let request = cx.background_spawn(async move { browser.focus_page(id) });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |model, cx| {
                model.focus_in_flight = false;
                match result {
                    Ok(()) if model.desired_page == Some(id) => model.begin_frame_barrier(id),
                    Ok(()) => {}
                    Err(error) => {
                        model.runtime.status = Some(format!("browser: {error:#}"));
                        if model.desired_page == Some(id) {
                            model.desired_page = None;
                        }
                    }
                }
                if model.desired_page != model.focused_page && model.awaiting_frame.is_none() {
                    model.start_focus(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn claim_presentation(&mut self, owner: EntityId, page: PageId, cx: &mut Context<Self>) {
        // A fresh portal claim must refocus Chrome even if our last confirmed
        // request named this page: the user can switch tabs in Chrome itself.
        self.focused_page = None;
        self.awaiting_frame = None;
        if let Some(scene_id) = self.runtime.scene_id {
            self.runtime.invalidated_through = self.runtime.invalidated_through.max(scene_id);
        }
        self.runtime.scene.clear();
        self.runtime.scene_id = None;
        self.runtime.painted_scene_id = None;
        for (_, buffer) in self.runtime.buffers.drain() {
            if let BrowserBuffer::Shm(image) = buffer {
                cx.drop_image(image, None);
            }
        }
        self.runtime.status = Some("switching browser page".into());
        self.presentation_owner = Some(owner);
        self.focus(page, cx);
        cx.notify();
    }

    fn begin_frame_barrier(&mut self, page: PageId) {
        let barrier = self.next_frame_barrier;
        self.next_frame_barrier = self.next_frame_barrier.wrapping_add(1).max(1);
        let (width, height, scale) = self.runtime.sent_size.get();
        let probe_width = if width == u32::MAX {
            width - 1
        } else {
            width + 1
        };
        self.runtime.sent_size.set((probe_width, height, scale));
        if let Some(session) = &self.runtime.session {
            session.frame_barrier(barrier, probe_width, height, f32::from_bits(scale));
            self.awaiting_frame = Some((barrier, page));
        }
    }

    fn presents(&self, owner: EntityId, page: PageId) -> bool {
        self.presentation_owner == Some(owner)
            && self.desired_page == Some(page)
            && self.focused_page == Some(page)
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
        if let (Some(session), Some(scene_id)) =
            (&self.runtime.session, self.runtime.painted_scene_id)
        {
            session.pointer_motion(scene_id, x, y);
        }
    }

    fn pointer_button(&self, button: u32, pressed: bool) -> bool {
        if pressed && self.runtime.painted_scene_id.is_none() {
            return false;
        }
        if let Some(session) = &self.runtime.session {
            session.pointer_button(button, pressed);
            true
        } else {
            false
        }
    }

    fn pointer_axis(&self, event: &LinuxPointerAxisEvent) {
        if self.runtime.painted_scene_id.is_none() && event.stop == (false, false) {
            return;
        }
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
        if self.runtime.painted_scene_id.is_none() && !matches!(event, LinuxPinchEvent::End { .. })
        {
            return;
        }
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

    fn key(&self, keycode: u32, pressed: bool) -> bool {
        if pressed && self.runtime.painted_scene_id.is_none() {
            return false;
        }
        if let Some(session) = &self.runtime.session {
            session.key(keycode, pressed);
            true
        } else {
            false
        }
    }
}

fn frame_is_eligible(
    desired: Option<PageId>,
    focused: Option<PageId>,
    awaiting: Option<(u64, PageId)>,
    frame_barrier: u64,
) -> bool {
    if let Some((barrier, page)) = awaiting {
        return desired == Some(page) && frame_barrier >= barrier;
    }
    desired == focused
}

fn bgra_to_rgba(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
}

fn complete_frame_handoff(
    desired: Option<PageId>,
    focused: &mut Option<PageId>,
    awaiting: &mut Option<(u64, PageId)>,
    frame_barrier: u64,
) {
    if let Some((barrier, page)) = *awaiting
        && desired == Some(page)
        && frame_barrier >= barrier
    {
        *focused = Some(page);
        *awaiting = None;
    }
}

pub struct BrowserView {
    model: Entity<BrowserModel>,
    owner_id: EntityId,
    page_id: PageId,
    focus_handle: FocusHandle,
    origin: Rc<Cell<(f32, f32)>>,
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u32>,
    finger_axes: (bool, bool),
    pinch_active: bool,
    scheduled_scene: Option<u64>,
    blur_subscription: Option<Subscription>,
    focus_subscription: Option<Subscription>,
    _model_changed: Subscription,
}

impl BrowserView {
    pub fn new(model: Entity<BrowserModel>, page_id: PageId, cx: &mut Context<Self>) -> Self {
        let owner_id = cx.entity_id();
        let model_changed = cx.observe(&model, |_, _, cx| cx.notify());
        model.update(cx, |model, cx| {
            model.claim_presentation(owner_id, page_id, cx)
        });
        Self {
            model,
            owner_id,
            page_id,
            focus_handle: cx.focus_handle(),
            origin: Rc::new(Cell::new((0.0, 0.0))),
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            finger_axes: (false, false),
            pinch_active: false,
            scheduled_scene: None,
            blur_subscription: None,
            focus_subscription: None,
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
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            return;
        }
        let (x, y) = self.local_position(event.position);
        self.model.read(cx).pointer_motion(x, y);
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        let (x, y) = self.local_position(event.position);
        self.model.update(cx, |model, cx| {
            if !model.presents(self.owner_id, self.page_id) {
                model.claim_presentation(self.owner_id, self.page_id, cx);
            }
        });
        let model = self.model.read(cx);
        if !model.presents(self.owner_id, self.page_id) {
            cx.stop_propagation();
            return;
        }
        model.pointer_motion(x, y);
        let button = linux_button(event.button);
        if model.pointer_button(button, true) {
            self.pressed_buttons.insert(button);
        }
        cx.stop_propagation();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            self.pressed_buttons.remove(&linux_button(event.button));
            return;
        }
        let button = linux_button(event.button);
        if self.pressed_buttons.remove(&button) {
            self.model.read(cx).pointer_button(button, false);
        }
        cx.stop_propagation();
    }

    fn pointer_axis(
        &mut self,
        event: &LinuxPointerAxisEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            return;
        }
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
        let (x, y) = self.local_position(event.position);
        let model = self.model.read(cx);
        model.pointer_motion(x, y);
        model.pointer_axis(event);
        cx.stop_propagation();
    }

    fn pinch(&mut self, event: &LinuxPinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            return;
        }
        self.pinch_active = !matches!(event, LinuxPinchEvent::End { .. });
        let model = self.model.read(cx);
        if let LinuxPinchEvent::Begin { position, .. } | LinuxPinchEvent::Update { position, .. } =
            event
        {
            let (x, y) = self.local_position(*position);
            model.pointer_motion(x, y);
        }
        model.pinch(*event);
        cx.stop_propagation();
    }

    fn physical_key(&mut self, event: &PhysicalKeyEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            return;
        }
        let PhysicalKey::LinuxEvdev(keycode) = event.key;
        if event.pressed {
            if !self.pressed_keys.contains(&keycode) && self.model.read(cx).key(keycode, true) {
                self.pressed_keys.insert(keycode);
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
        if self.focus_subscription.is_none() {
            let page_id = self.page_id;
            let owner_id = self.owner_id;
            self.focus_subscription =
                Some(cx.on_focus(&self.focus_handle, window, move |this, _, cx| {
                    this.model.update(cx, |model, cx| {
                        model.claim_presentation(owner_id, page_id, cx)
                    })
                }));
        }
        // Rendering is synchronous with the Linux event loop: once this method
        // builds a scene, no input can interleave before GPUI presents it.
        // Advance hit testing here rather than in the next-frame callback,
        // which runs before that next frame's draw.
        let (owns_presentation, painted_scene_id) = {
            let model = self.model.read(cx);
            (
                model.presentation_owner == Some(self.owner_id),
                model
                    .presents(self.owner_id, self.page_id)
                    .then_some(model.runtime.scene_id)
                    .flatten(),
            )
        };
        if owns_presentation {
            self.model.update(cx, |model, _| {
                model.runtime.painted_scene_id = painted_scene_id;
            });
        }
        let model = self.model.read(cx);
        let colors = cx.theme().colors();
        let presents = model.presents(self.owner_id, self.page_id);
        if presents
            && model.runtime.scene_id != self.scheduled_scene
            && let (Some(scene_id), Some(session)) =
                (model.runtime.scene_id, model.runtime.session.as_ref())
        {
            self.scheduled_scene = Some(scene_id);
            let presented = session.presentation_callback(scene_id);
            let browser = self.model.clone();
            let owner_id = self.owner_id;
            let page_id = self.page_id;
            window.on_next_frame(move |_, cx| {
                let painted = browser.update(cx, |model, _| {
                    model.presents(owner_id, page_id)
                        && scene_id > model.runtime.invalidated_through
                });
                if painted {
                    presented();
                }
            });
        }
        let scene = if presents {
            model
                .runtime
                .scene
                .iter()
                .map(|node| {
                    let buffer = model
                        .runtime
                        .buffers
                        .get(&node.buffer_id)
                        .expect("accepted browser scene has every buffer")
                        .clone();
                    (node.clone(), buffer)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let status = if presents {
            model.runtime.status.clone()
        } else if model.presentation_owner == Some(self.owner_id) {
            Some("switching browser page".to_owned())
        } else {
            Some("browser is active in another pane".to_owned())
        };
        let scale = window.scale_factor();
        let owner_id = self.owner_id;
        let page_id = self.page_id;
        let browser = self.model.clone();
        let origin = self.origin.clone();
        let measure = canvas(
            move |bounds, _, cx| {
                origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
                let width = f32::from(bounds.size.width).round().max(1.0) as u32;
                let height = f32::from(bounds.size.height).round().max(1.0) as u32;
                let browser = browser.read(cx);
                if browser.presents(owner_id, page_id) {
                    browser.resize(width, height, scale);
                }
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
            .bg(colors.editor_background)
            .children(scene.into_iter().map(|(node, buffer)| {
                let content = match buffer {
                    BrowserBuffer::DmaBuf(frame) => surface(frame)
                        .source_rect(
                            (node.source.0.0 as f32, node.source.0.1 as f32),
                            (node.source.1.0 as f32, node.source.1.1 as f32),
                        )
                        .size_full()
                        .object_fit(ObjectFit::Fill)
                        .into_any_element(),
                    BrowserBuffer::Shm(image) => img(image)
                        .size_full()
                        .object_fit(ObjectFit::Fill)
                        .into_any_element(),
                };
                div()
                    .id(("rho-browser-surface", node.surface_id))
                    .absolute()
                    .left(px(node.origin.0 as f32))
                    .top(px(node.origin.1 as f32))
                    .w(px(node.destination.0 as f32))
                    .h(px(node.destination.1 as f32))
                    .child(content)
            }))
            .child(div().absolute().size_full().child(measure))
            .children(status.map(|status| {
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .bg(colors.surface_background)
                    .font(theme::theme_settings(cx).ui_font(cx).clone())
                    .text_size(theme::theme_settings(cx).ui_font_size(cx))
                    .text_color(colors.text_muted)
                    .child(status)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_response_frame_cannot_complete_focus_without_configure_barrier() {
        let page = PageId(uuid::Uuid::new_v4());
        let mut focused = None;
        let mut awaiting = None;

        // A target-looking frame may arrive before the activation RPC reply.
        assert!(!frame_is_eligible(Some(page), focused, awaiting, 0));
        assert_eq!(focused, None);

        // RPC completion installs a configure barrier. No ordinary later frame
        // can unblock presentation; only the post-ACK barrier commit can.
        awaiting = Some((7, page));
        assert!(!frame_is_eligible(Some(page), focused, awaiting, 0));
        assert!(frame_is_eligible(Some(page), focused, awaiting, 7));
        assert_eq!(focused, None, "eligibility alone must not enable input");
        complete_frame_handoff(Some(page), &mut focused, &mut awaiting, 7);
        assert_eq!(focused, Some(page));
    }

    #[test]
    fn popup_pixels_are_rgba_at_the_gpui_image_boundary() {
        let mut pixels = [3, 2, 1, 255, 30, 20, 10, 128];
        bgra_to_rgba(&mut pixels);
        assert_eq!(pixels, [1, 2, 3, 255, 10, 20, 30, 128]);
    }
}
