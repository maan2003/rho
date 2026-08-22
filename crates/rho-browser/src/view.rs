//! A GPUI portal onto a durable client-local web page.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd as _, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{
    AppContext as _, Context, CursorStyle, Entity, EntityId, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, LinuxAxisRelativeDirection,
    LinuxAxisSource, LinuxDmaBufSurface, LinuxPinchEvent, LinuxPointerAxisEvent,
    LinuxWaylandDmaBufPlane, LinuxWaylandPassthrough, LinuxWaylandPassthroughBuffer,
    LinuxWaylandPassthroughEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement as _, PhysicalKey, PhysicalKeyEvent, Render, RenderImage,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Task, Window,
    canvas, div, img, px, surface,
};
use image::{Frame, RgbaImage};
use rho_browser_wayland::{
    BrowserCursor, BrowserEvent, BrowserSession, BufferImport, HostPresentation, PinchGesture,
    PointerAxisDirection, PointerAxisFrame, PointerAxisSource, SceneNode,
};
use theme::ActiveTheme as _;

use crate::PageId;
use crate::runtime::{BrowserRuntime, BrowserWindow};

struct RuntimePageState {
    session: Option<BrowserSession<BrowserWindow>>,
    terminal: bool,
    buffers: HashMap<u64, BrowserBuffer>,
    scene: Vec<SceneNode>,
    scene_id: Option<u64>,
    logical_size: (u32, u32),
    painted_scene_id: Option<u64>,
    invalidated_through: u64,
    presented_barrier: u64,
    status: Option<String>,
    sent_size: Rc<Cell<(u32, u32, u32)>>,
    cursor: BrowserCursor,
}

#[derive(Clone)]
enum BrowserBuffer {
    DmaBuf(BrowserDmaBuf),
    Shm(Arc<RenderImage>),
}

#[derive(Clone)]
struct BrowserDmaBuf {
    surface: LinuxDmaBufSurface,
    planes: Arc<[(Arc<OwnedFd>, u32, u32)]>,
    acquire_fence: Arc<OwnedFd>,
}

impl BrowserDmaBuf {
    fn passthrough_buffer(&self) -> std::io::Result<LinuxWaylandPassthroughBuffer> {
        Ok(LinuxWaylandPassthroughBuffer {
            surface: self.surface.clone(),
            width: self.surface.width(),
            height: self.surface.height(),
            fourcc: self.surface.fourcc(),
            modifier: self.surface.modifier(),
            y_inverted: self.surface.y_inverted(),
            planes: self
                .planes
                .iter()
                .map(|(fd, offset, stride)| {
                    Ok(LinuxWaylandDmaBufPlane {
                        fd: fd.as_fd().try_clone_to_owned()?,
                        offset: *offset,
                        stride: *stride,
                    })
                })
                .collect::<std::io::Result<_>>()?,
            acquire_fence: self.acquire_fence.as_fd().try_clone_to_owned()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandoffPhase {
    Requested,
    Focusing,
    AwaitingFrame { barrier: u64 },
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageHandoff {
    generation: u64,
    target: PageId,
    phase: HandoffPhase,
    input_generation: Option<u64>,
}

/// The singleton live Chrome surface shared by every logical page view.
pub struct BrowserModel {
    browser: Arc<BrowserRuntime>,
    handoff: Option<PageHandoff>,
    active_focus: Option<PageId>,
    next_handoff_generation: u64,
    next_frame_barrier: u64,
    next_input_freeze: u64,
    input_freeze: Option<u64>,
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
                                rho_browser_wayland::record_browser_timing(
                                    rho_browser_wayland::BrowserTimingKind::SceneReceived,
                                    scene.id,
                                    scene.barrier,
                                    None,
                                    Some(scene.produced_at.elapsed()),
                                );
                                if model.runtime.session.is_none() {
                                    tracing::info!(
                                        scene_id = scene.id,
                                        scene_barrier = scene.barrier,
                                        "browser handoff ignored a scene after session shutdown"
                                    );
                                    return;
                                }
                                let awaiting_barrier = model.awaiting_barrier();
                                if awaiting_barrier.is_some() {
                                    tracing::info!(
                                        scene_id = scene.id,
                                        scene_barrier = scene.barrier,
                                        ?awaiting_barrier,
                                        imports = scene.imports.len(),
                                        attached = scene.attached.len(),
                                        nodes = scene.nodes.len(),
                                        "browser handoff received a compositor scene"
                                    );
                                }
                                // Imports are leases independent of whether this scene is
                                // eligible for the current page handoff. A later coalesced
                                // scene may still reference them without re-importing.
                                let mut import_failed = false;
                                for import in scene.imports.drain(..) {
                                    let (id, buffer) = match import {
                                        BufferImport::DmaBuf(mut frame) => {
                                            let Ok(passthrough_planes) = frame.duplicate_planes()
                                            else {
                                                model.runtime.status = Some(
                                                    "duplicate Chrome DMA-BUF planes failed".into(),
                                                );
                                                import_failed = true;
                                                continue;
                                            };
                                            let Ok(passthrough_acquire) =
                                                frame.duplicate_acquire_fence()
                                            else {
                                                model.runtime.status = Some(
                                                    "duplicate Chrome acquire fence failed".into(),
                                                );
                                                import_failed = true;
                                                continue;
                                            };
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
                                            let planes = passthrough_planes
                                                .into_iter()
                                                .map(|plane| {
                                                    (Arc::new(plane.fd), plane.offset, plane.stride)
                                                })
                                                .collect::<Vec<_>>()
                                                .into();
                                            (
                                                id,
                                                BrowserBuffer::DmaBuf(BrowserDmaBuf {
                                                    surface,
                                                    planes,
                                                    acquire_fence: Arc::new(passthrough_acquire),
                                                }),
                                            )
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
                                    model.handoff,
                                    scene.barrier,
                                );
                                if import_failed || missing_buffer {
                                    if awaiting_barrier.is_some() {
                                        tracing::info!(
                                            scene_id = scene.id,
                                            scene_barrier = scene.barrier,
                                            ?awaiting_barrier,
                                            import_failed,
                                            missing_buffer,
                                            "browser handoff rejected an invalid compositor scene"
                                        );
                                    }
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
                                if eligible {
                                    let was_awaiting = awaiting_barrier.is_some();
                                    let input_generation = model
                                        .handoff
                                        .and_then(|handoff| handoff.input_generation);
                                    complete_frame_handoff(&mut model.handoff, scene.barrier);
                                    if was_awaiting
                                        && model.handoff.is_some_and(|handoff| {
                                            handoff.phase == HandoffPhase::Ready
                                        })
                                        && let Some(generation) = input_generation
                                    {
                                        model.unfreeze_input(generation);
                                    }
                                    model.runtime.status = None;
                                } else if awaiting_barrier.is_some() {
                                    tracing::info!(
                                        scene_id = scene.id,
                                        scene_barrier = scene.barrier,
                                        "browser handoff displayed a scene while keeping input gated"
                                    );
                                }
                                model.runtime.presented_barrier = scene.barrier;
                                model.runtime.scene_id = Some(scene.id);
                                model.runtime.logical_size = scene.logical_size;
                                model.runtime.scene = scene.nodes;
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
                            BrowserEvent::Cursor(cursor) => {
                                model.runtime.cursor = cursor;
                            }
                            BrowserEvent::ToplevelReady => {
                                model.runtime.status = Some("Chrome is starting".into());
                            }
                            BrowserEvent::Closed => {
                                model.transition_terminal("browser closed".into(), cx);
                            }
                            BrowserEvent::Failed(error) => {
                                tracing::error!(%error, "browser compositor reported a terminal failure");
                                model.transition_terminal(format!("browser failed: {error}"), cx);
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
            handoff: None,
            active_focus: None,
            next_handoff_generation: 1,
            next_frame_barrier: 1,
            next_input_freeze: 1,
            input_freeze: None,
            presentation_owner: None,
            runtime: RuntimePageState {
                session: Some(session),
                terminal: false,
                buffers: HashMap::new(),
                scene: Vec::new(),
                scene_id: None,
                logical_size: (1280, 720),
                painted_scene_id: None,
                invalidated_through: 0,
                presented_barrier: 0,
                status: Some("waiting for Chrome".into()),
                sent_size: Rc::new(Cell::new((1280, 720, 1.0_f32.to_bits()))),
                cursor: BrowserCursor::Arrow,
            },
            _events_task: events_task,
        }
    }

    pub(crate) fn focus(&mut self, id: PageId, cx: &mut Context<Self>) {
        if self.runtime.terminal || self.runtime.session.is_none() {
            return;
        }
        if self.handoff.is_none_or(|handoff| handoff.target != id) {
            self.request_handoff(id);
        }
        self.start_focus(cx);
    }

    fn request_handoff(&mut self, target: PageId) {
        let generation = self.next_handoff_generation;
        self.next_handoff_generation = self.next_handoff_generation.wrapping_add(1).max(1);
        self.handoff = Some(PageHandoff {
            generation,
            target,
            phase: HandoffPhase::Requested,
            input_generation: None,
        });
    }

    fn awaiting_barrier(&self) -> Option<u64> {
        match self.handoff?.phase {
            HandoffPhase::AwaitingFrame { barrier } => Some(barrier),
            _ => None,
        }
    }

    fn start_focus(&mut self, cx: &mut Context<Self>) {
        if self.runtime.terminal || self.runtime.session.is_none() {
            return;
        }
        if self.active_focus.is_some() {
            return;
        }
        let Some(mut handoff) = self.handoff else {
            return;
        };
        if handoff.phase != HandoffPhase::Requested {
            return;
        }
        let (input_generation, input_barrier) = match self.input_barrier() {
            Ok(transition) => transition,
            Err(error) => {
                self.runtime.status = Some(format!("browser: {error:#}"));
                self.handoff = None;
                cx.notify();
                return;
            }
        };
        let id = handoff.target;
        let generation = handoff.generation;
        handoff.input_generation = Some(input_generation);
        handoff.phase = HandoffPhase::Focusing;
        self.handoff = Some(handoff);
        self.active_focus = Some(id);
        tracing::info!(
            generation = handoff.generation,
            "browser handoff requested extension tab focus"
        );
        let browser = self.browser.clone();
        let request = cx.background_spawn(async move {
            input_barrier.await?;
            browser.focus_page(id)
        });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |model, cx| {
                model.active_focus = None;
                if model.runtime.terminal || model.runtime.session.is_none() {
                    cx.notify();
                    return;
                }
                let current = model.handoff;
                tracing::info!(
                    succeeded = result.is_ok(),
                    still_desired = current.is_some_and(|handoff| handoff.generation == generation),
                    "browser handoff extension tab focus completed"
                );
                match result {
                    Ok(()) => {
                        if let Some(handoff) = matching_handoff(current, generation) {
                            model.begin_frame_barrier(handoff);
                        } else {
                            model.unfreeze_input(input_generation);
                        }
                    }
                    Err(error) => {
                        model.unfreeze_input(input_generation);
                        if current.is_some_and(|handoff| handoff.generation == generation) {
                            model.runtime.status = Some(format!("browser: {error:#}"));
                            model.handoff = None;
                        }
                    }
                }
                model.start_focus(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn input_barrier(
        &mut self,
    ) -> Result<(
        u64,
        impl std::future::Future<Output = Result<()>> + Send + 'static + use<>,
    )> {
        let generation = self.next_input_freeze;
        self.next_input_freeze = self.next_input_freeze.wrapping_add(1).max(1);
        let barrier = self
            .runtime
            .session
            .as_ref()
            .context("browser compositor session is unavailable")?
            .input_barrier(generation)?;
        self.input_freeze = Some(generation);
        Ok((generation, barrier))
    }

    fn unfreeze_input(&mut self, generation: u64) {
        if self.input_freeze != Some(generation) {
            return;
        }
        if let Some(session) = &self.runtime.session {
            session.unfreeze_input(generation);
        }
        self.input_freeze = None;
    }

    pub(crate) fn prepare_close(
        &mut self,
        id: PageId,
    ) -> Result<Option<impl std::future::Future<Output = Result<()>> + Send + 'static + use<>>>
    {
        match close_transition(self.handoff, id) {
            CloseTransition::Inactive => return Ok(None),
            CloseTransition::CancelRequested => {
                self.presentation_owner = None;
                self.runtime.painted_scene_id = None;
                self.handoff = None;
                return Ok(None);
            }
            CloseTransition::FreezeActive => {}
        }
        self.presentation_owner = None;
        self.runtime.painted_scene_id = None;
        self.handoff = None;
        let (_, barrier) = self.input_barrier()?;
        Ok(Some(barrier))
    }

    fn claim_presentation(&mut self, owner: EntityId, page: PageId, cx: &mut Context<Self>) {
        if self.runtime.terminal || self.runtime.session.is_none() {
            return;
        }
        // A fresh portal claim must refocus Chrome even if our last confirmed
        // request named this page: the user can switch tabs in Chrome itself.
        self.request_handoff(page);
        self.runtime.painted_scene_id = None;
        self.runtime.status = Some("switching browser page".into());
        self.presentation_owner = Some(owner);
        tracing::info!(
            previous_scene = ?self.runtime.scene_id,
            previous_barrier = self.runtime.presented_barrier,
            had_focus_request = self.active_focus.is_some(),
            "browser portal claimed presentation"
        );
        self.start_focus(cx);
        cx.notify();
    }

    fn owns_page(&self, owner: EntityId, page: PageId) -> bool {
        self.presentation_owner == Some(owner)
            && self.handoff.is_some_and(|handoff| handoff.target == page)
    }

    fn claim_or_join_presentation(
        &mut self,
        owner: EntityId,
        page: PageId,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        if !self.owns_page(owner, page) {
            self.claim_presentation(owner, page, cx);
        }
        self.handoff
            .filter(|handoff| self.presentation_owner == Some(owner) && handoff.target == page)
            .map(|handoff| handoff.generation)
    }

    fn transition_terminal(&mut self, status: String, cx: &mut Context<Self>) {
        if !transition_terminal_state(
            &mut self.runtime,
            &mut self.handoff,
            &mut self.active_focus,
            status,
            |image| cx.drop_image(image, None),
        ) {
            return;
        }
        self.browser.shutdown_background();
        crate::reset_runtime(&self.browser, cx);
    }

    fn begin_frame_barrier(&mut self, handoff: PageHandoff) {
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
            tracing::info!(
                barrier,
                width = probe_width,
                height,
                scale = f32::from_bits(scale),
                "browser handoff sent a compositor frame barrier"
            );
            session.frame_barrier(barrier, probe_width, height, f32::from_bits(scale));
            if self
                .handoff
                .is_some_and(|current| current.generation == handoff.generation)
            {
                self.handoff = Some(PageHandoff {
                    phase: HandoffPhase::AwaitingFrame { barrier },
                    ..handoff
                });
            }
        } else {
            tracing::info!(
                barrier,
                "browser handoff could not send barrier: no session"
            );
        }
    }

    fn presents(&self, owner: EntityId, page: PageId) -> bool {
        self.owns_page(owner, page)
            && self
                .handoff
                .is_some_and(|handoff| handoff.phase == HandoffPhase::Ready)
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

    fn pointer_leave(&self) {
        if let Some(session) = &self.runtime.session {
            session.pointer_leave();
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
                time: event.time,
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

    fn pinch(&self, gesture: PinchGesture) {
        if self.runtime.painted_scene_id.is_none() && !matches!(gesture, PinchGesture::End { .. }) {
            return;
        }
        if let Some(session) = &self.runtime.session {
            session.pinch(gesture);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloseTransition {
    Inactive,
    CancelRequested,
    FreezeActive,
}

fn close_transition(handoff: Option<PageHandoff>, id: PageId) -> CloseTransition {
    match handoff.filter(|handoff| handoff.target == id) {
        None => CloseTransition::Inactive,
        Some(PageHandoff {
            phase: HandoffPhase::Requested,
            ..
        }) => CloseTransition::CancelRequested,
        Some(_) => CloseTransition::FreezeActive,
    }
}

fn transition_terminal_state(
    runtime: &mut RuntimePageState,
    handoff: &mut Option<PageHandoff>,
    active_focus: &mut Option<PageId>,
    status: String,
    mut drop_image: impl FnMut(Arc<RenderImage>),
) -> bool {
    if runtime.terminal {
        return false;
    }
    runtime.terminal = true;
    runtime.session = None;
    *handoff = None;
    *active_focus = None;
    runtime.scene.clear();
    runtime.scene_id = None;
    runtime.painted_scene_id = None;
    runtime.presented_barrier = 0;
    for (_, buffer) in runtime.buffers.drain() {
        if let BrowserBuffer::Shm(image) = buffer {
            drop_image(image);
        }
    }
    runtime.status = Some(status);
    true
}

fn browser_status(
    runtime: &RuntimePageState,
    presents: bool,
    owns_presentation: bool,
) -> Option<String> {
    if runtime.terminal || runtime.session.is_none() || presents {
        runtime.status.clone()
    } else if owns_presentation {
        Some("switching browser page".to_owned())
    } else {
        Some("browser is active in another pane".to_owned())
    }
}

fn frame_is_eligible(handoff: Option<PageHandoff>, frame_barrier: u64) -> bool {
    match handoff.map(|handoff| handoff.phase) {
        Some(HandoffPhase::AwaitingFrame { barrier }) => frame_barrier >= barrier,
        Some(HandoffPhase::Ready) => true,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PassthroughPaintState {
    positioned: Option<u64>,
    submitted: Option<u64>,
    presented: Option<u64>,
    active: bool,
}

fn browser_passthrough_enabled() -> bool {
    // Keep the texture path as the default until compositor coverage has been
    // validated broadly. Flipping the default later is intentionally one line.
    std::env::var_os("RHO_BROWSER_PASSTHROUGH").is_some_and(|value| value == "1")
}

fn passthrough_scene<'a>(
    nodes: &'a [SceneNode],
    logical_size: (u32, u32),
    buffers: &'a HashMap<u64, BrowserBuffer>,
) -> Option<(u64, &'a BrowserDmaBuf)> {
    let [node] = nodes else { return None };
    let BrowserBuffer::DmaBuf(buffer) = buffers.get(&node.buffer_id)? else {
        return None;
    };
    passthrough_node_eligible(
        node,
        logical_size,
        (buffer.surface.width(), buffer.surface.height()),
    )
    .then_some((node.buffer_id, buffer))
}

fn passthrough_node_eligible(
    node: &SceneNode,
    logical_size: (u32, u32),
    buffer_size: (u32, u32),
) -> bool {
    node.source.0 == (0.0, 0.0)
        && node.source.1 == (f64::from(buffer_size.0), f64::from(buffer_size.1))
        && node.origin == (0.0, 0.0)
        && node.destination == (f64::from(logical_size.0), f64::from(logical_size.1))
}

fn bgra_to_rgba(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
}

fn gpui_cursor(cursor: BrowserCursor) -> CursorStyle {
    match cursor {
        BrowserCursor::Arrow => CursorStyle::Arrow,
        BrowserCursor::IBeam => CursorStyle::IBeam,
        BrowserCursor::Crosshair => CursorStyle::Crosshair,
        BrowserCursor::ClosedHand => CursorStyle::ClosedHand,
        BrowserCursor::OpenHand => CursorStyle::OpenHand,
        BrowserCursor::PointingHand => CursorStyle::PointingHand,
        BrowserCursor::ResizeLeft => CursorStyle::ResizeLeft,
        BrowserCursor::ResizeRight => CursorStyle::ResizeRight,
        BrowserCursor::ResizeLeftRight => CursorStyle::ResizeLeftRight,
        BrowserCursor::ResizeUp => CursorStyle::ResizeUp,
        BrowserCursor::ResizeDown => CursorStyle::ResizeDown,
        BrowserCursor::ResizeUpDown => CursorStyle::ResizeUpDown,
        BrowserCursor::ResizeUpLeftDownRight => CursorStyle::ResizeUpLeftDownRight,
        BrowserCursor::ResizeUpRightDownLeft => CursorStyle::ResizeUpRightDownLeft,
        BrowserCursor::ResizeColumn => CursorStyle::ResizeColumn,
        BrowserCursor::ResizeRow => CursorStyle::ResizeRow,
        BrowserCursor::VerticalText => CursorStyle::IBeamCursorForVerticalLayout,
        BrowserCursor::NotAllowed => CursorStyle::OperationNotAllowed,
        BrowserCursor::DragLink => CursorStyle::DragLink,
        BrowserCursor::DragCopy => CursorStyle::DragCopy,
        BrowserCursor::ContextMenu => CursorStyle::ContextualMenu,
    }
}

fn complete_frame_handoff(handoff: &mut Option<PageHandoff>, frame_barrier: u64) {
    if let Some(current) = handoff
        && let HandoffPhase::AwaitingFrame { barrier } = current.phase
        && frame_barrier >= barrier
    {
        current.phase = HandoffPhase::Ready;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputReplayState {
    Waiting,
    Ready,
    Cancel,
}

#[derive(Clone, Debug)]
struct QueuedInput {
    generation: u64,
    events: Vec<QueuedInputEvent>,
}

const MAX_QUEUED_INPUT_EVENTS: usize = 256;

#[derive(Clone, Copy, Debug)]
enum QueuedInputEvent {
    Motion {
        position: (f64, f64),
    },
    Button {
        position: (f64, f64),
        button: u32,
        pressed: bool,
    },
    Axis {
        position: (f64, f64),
        event: LinuxPointerAxisEvent,
    },
    Leave,
    Pinch {
        position: Option<(f64, f64)>,
        gesture: PinchGesture,
    },
    Key {
        keycode: u32,
        pressed: bool,
    },
}

impl QueuedInput {
    fn push(&mut self, event: QueuedInputEvent) -> Result<()> {
        if let QueuedInputEvent::Motion { position } = event
            && let Some(QueuedInputEvent::Motion { position: previous }) = self.events.last_mut()
        {
            *previous = position;
            return Ok(());
        }
        anyhow::ensure!(
            self.events.len() < MAX_QUEUED_INPUT_EVENTS,
            "queued browser pointer input overflowed"
        );
        self.events.push(event);
        Ok(())
    }

    fn record_release(&mut self, button: u32, position: (f64, f64)) -> Result<bool> {
        let pressed = self.events.iter().rev().find_map(|event| match event {
            QueuedInputEvent::Button {
                button: queued_button,
                pressed,
                ..
            } if *queued_button == button => Some(*pressed),
            _ => None,
        });
        if pressed != Some(true) {
            return Ok(false);
        }
        self.push(QueuedInputEvent::Button {
            position,
            button,
            pressed: false,
        })?;
        Ok(true)
    }

    fn record_key_release(&mut self, keycode: u32) -> Result<bool> {
        let pressed = self.events.iter().rev().find_map(|event| match event {
            QueuedInputEvent::Key {
                keycode: queued_keycode,
                pressed,
            } if *queued_keycode == keycode => Some(*pressed),
            _ => None,
        });
        if pressed != Some(true) {
            return Ok(false);
        }
        self.push(QueuedInputEvent::Key {
            keycode,
            pressed: false,
        })?;
        Ok(true)
    }
}

fn input_replay_state(
    generation: u64,
    owner: EntityId,
    page: PageId,
    presentation_owner: Option<EntityId>,
    handoff: Option<PageHandoff>,
    painted_scene_id: Option<u64>,
) -> InputReplayState {
    let Some(handoff) = handoff.filter(|handoff| {
        presentation_owner == Some(owner)
            && handoff.target == page
            && handoff.generation == generation
    }) else {
        return InputReplayState::Cancel;
    };
    if handoff.phase == HandoffPhase::Ready && painted_scene_id.is_some() {
        InputReplayState::Ready
    } else {
        InputReplayState::Waiting
    }
}

pub struct BrowserView {
    model: Entity<BrowserModel>,
    owner_id: EntityId,
    page_id: PageId,
    focus_handle: FocusHandle,
    origin: Rc<Cell<(f32, f32)>>,
    last_pointer_position: (f64, f64),
    pressed_keys: HashSet<u32>,
    pending_key: Option<(u32, Option<u64>)>,
    pressed_buttons: HashSet<u32>,
    queued_input: Option<QueuedInput>,
    claiming_pointer_focus: bool,
    finger_axes: (bool, bool),
    last_axis_time: u32,
    pinch_active: bool,
    scheduled_scene: Option<u64>,
    passthrough: Option<Arc<dyn LinuxWaylandPassthrough>>,
    passthrough_attempted: bool,
    passthrough_state: Rc<Cell<PassthroughPaintState>>,
    passthrough_events: Option<Task<()>>,
    passthrough_timing_enabled: Rc<Cell<bool>>,
    fallback_scene_id: Option<u64>,
    fallback_scene: Vec<(SceneNode, BrowserBuffer)>,
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
            last_pointer_position: (0.0, 0.0),
            pressed_keys: HashSet::new(),
            pending_key: None,
            pressed_buttons: HashSet::new(),
            queued_input: None,
            claiming_pointer_focus: false,
            finger_axes: (false, false),
            last_axis_time: 0,
            pinch_active: false,
            scheduled_scene: None,
            passthrough: None,
            passthrough_attempted: false,
            passthrough_state: Rc::new(Cell::new(PassthroughPaintState::default())),
            passthrough_events: None,
            passthrough_timing_enabled: Rc::new(Cell::new(false)),
            fallback_scene_id: None,
            fallback_scene: Vec::new(),
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

    fn queue_input_event(&mut self, generation: u64, event: QueuedInputEvent) {
        if self
            .queued_input
            .as_ref()
            .is_none_or(|queued| queued.generation != generation)
        {
            self.queued_input = Some(QueuedInput {
                generation,
                events: Vec::new(),
            });
        }
        if self.queued_input.as_mut().unwrap().push(event).is_err() {
            self.queued_input = None;
        }
    }

    fn queue_button_release(&mut self, button: u32, position: (f64, f64)) -> bool {
        let Some(queued) = &mut self.queued_input else {
            return false;
        };
        match queued.record_release(button, position) {
            Ok(queued) => queued,
            Err(_) => {
                self.queued_input = None;
                true
            }
        }
    }

    fn queue_key_release(&mut self, keycode: u32) -> bool {
        let Some(queued) = &mut self.queued_input else {
            return false;
        };
        match queued.record_key_release(keycode) {
            Ok(queued) => queued,
            Err(_) => {
                self.queued_input = None;
                true
            }
        }
    }

    fn claim_for_input(
        &mut self,
        focus_keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let was_focused = self.focus_handle.is_focused(window);
        let generation = self.model.update(cx, |model, cx| {
            if model.owns_page(self.owner_id, self.page_id) && (!focus_keyboard || was_focused) {
                model.handoff.map(|handoff| handoff.generation)
            } else {
                model.claim_or_join_presentation(self.owner_id, self.page_id, cx)
            }
        });
        if focus_keyboard && !was_focused {
            // GPUI dispatches on_focus during a later draw, so this is consumed
            // by that callback rather than reset immediately after focus().
            self.claiming_pointer_focus = true;
            self.focus_handle.focus(window, cx);
        }
        generation
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = self.local_position(event.position);
        self.last_pointer_position = (x, y);
        if let Some(generation) = self.queued_input.as_ref().map(|queued| queued.generation) {
            self.queue_input_event(generation, QueuedInputEvent::Motion { position: (x, y) });
            return;
        }
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            return;
        }
        self.model.read(cx).pointer_motion(x, y);
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = self.local_position(event.position);
        self.last_pointer_position = (x, y);
        let button = linux_button(event.button);
        let was_focused = self.focus_handle.is_focused(window);
        if self.queued_input.is_some()
            || !was_focused
            || !self.model.read(cx).presents(self.owner_id, self.page_id)
        {
            if let Some(generation) = self.claim_for_input(true, window, cx) {
                self.queue_input_event(
                    generation,
                    QueuedInputEvent::Button {
                        position: (x, y),
                        button,
                        pressed: true,
                    },
                );
            }
            cx.stop_propagation();
            return;
        }
        let model = self.model.read(cx);
        model.pointer_motion(x, y);
        if model.pointer_button(button, true) {
            self.pressed_buttons.insert(button);
        }
        cx.stop_propagation();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let button = linux_button(event.button);
        let position = self.local_position(event.position);
        if self.queue_button_release(button, position) {
            cx.stop_propagation();
            return;
        }
        if !self.model.read(cx).presents(self.owner_id, self.page_id) {
            self.pressed_buttons.remove(&button);
            return;
        }
        if self.pressed_buttons.remove(&button) {
            self.model.read(cx).pointer_button(button, false);
        }
        cx.stop_propagation();
    }

    fn pointer_axis(
        &mut self,
        event: &LinuxPointerAxisEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let finger_was_active = track_finger_axis(&mut self.finger_axes, event);
        if event.source == LinuxAxisSource::Finger {
            self.last_axis_time = event.time;
        }
        let cleanup_only = event.source == LinuxAxisSource::Finger
            && event.value == (0.0, 0.0)
            && event.v120 == (None, None)
            && event.stop != (false, false);
        let (x, y) = self.local_position(event.position);
        self.last_pointer_position = (x, y);
        let presents = self.model.read(cx).presents(self.owner_id, self.page_id);
        if event.source == LinuxAxisSource::Finger && finger_was_active && !presents {
            if let Some(generation) = self.queued_input.as_ref().map(|queued| queued.generation) {
                self.queue_input_event(
                    generation,
                    QueuedInputEvent::Axis {
                        position: (x, y),
                        event: *event,
                    },
                );
            }
            cx.stop_propagation();
            return;
        }
        if cleanup_only && !presents {
            cx.stop_propagation();
            return;
        }
        if self.queued_input.is_some() || !presents {
            if let Some(generation) = self.claim_for_input(false, window, cx) {
                self.queue_input_event(
                    generation,
                    QueuedInputEvent::Axis {
                        position: (x, y),
                        event: *event,
                    },
                );
            }
            cx.stop_propagation();
            return;
        }
        let model = self.model.read(cx);
        model.pointer_motion(x, y);
        model.pointer_axis(event);
        cx.stop_propagation();
    }

    fn pinch(&mut self, event: &LinuxPinchEvent, window: &mut Window, cx: &mut Context<Self>) {
        let pinch_was_active = self.pinch_active;
        let position = match event {
            LinuxPinchEvent::Begin { position, .. } | LinuxPinchEvent::Update { position, .. } => {
                Some(self.local_position(*position))
            }
            LinuxPinchEvent::End { .. } => None,
        };
        if let Some(position) = position {
            self.last_pointer_position = position;
        }
        let gesture = pinch_gesture(*event);
        let begins = matches!(gesture, PinchGesture::Begin { .. });
        let ends = matches!(gesture, PinchGesture::End { .. });
        let presents = self.model.read(cx).presents(self.owner_id, self.page_id);
        if !begins && !pinch_was_active {
            self.pinch_active = false;
            cx.stop_propagation();
            return;
        }
        self.pinch_active = !ends;
        if !begins && !presents {
            if let Some(generation) = self.queued_input.as_ref().map(|queued| queued.generation) {
                self.queue_input_event(generation, QueuedInputEvent::Pinch { position, gesture });
            } else {
                self.pinch_active = false;
            }
            cx.stop_propagation();
            return;
        }
        if self.queued_input.is_some() || !presents {
            if let Some(generation) = self.claim_for_input(false, window, cx) {
                self.queue_input_event(generation, QueuedInputEvent::Pinch { position, gesture });
            }
            cx.stop_propagation();
            return;
        }
        let model = self.model.read(cx);
        if let Some((x, y)) = position {
            model.pointer_motion(x, y);
        }
        model.pinch(gesture);
        cx.stop_propagation();
    }

    fn physical_key(
        &mut self,
        event: &PhysicalKeyEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PhysicalKey::LinuxEvdev(keycode) = event.key;
        let presents = self.model.read(cx).presents(self.owner_id, self.page_id);
        if event.pressed && !is_modifier_keycode(keycode) {
            let generation = if self.queued_input.is_some() || !presents {
                self.claim_for_input(false, window, cx)
            } else {
                None
            };
            if let Some(previous) = self.pending_key.replace((keycode, generation))
                && previous.0 != keycode
            {
                self.forward_key_press(previous.0, previous.1, cx);
            }
            cx.stop_propagation();
            return;
        }
        if event.pressed && (self.queued_input.is_some() || !presents) {
            if let Some(generation) = self.claim_for_input(false, window, cx) {
                self.queue_input_event(
                    generation,
                    QueuedInputEvent::Key {
                        keycode,
                        pressed: event.pressed,
                    },
                );
            }
            cx.stop_propagation();
            return;
        }
        if event.pressed {
            if !self.pressed_keys.contains(&keycode) && self.model.read(cx).key(keycode, true) {
                self.pressed_keys.insert(keycode);
            }
            cx.stop_propagation();
            return;
        }

        if self.pending_key.is_some_and(|pending| pending.0 == keycode) {
            let (_, generation) = self.pending_key.take().unwrap();
            self.forward_key_press(keycode, generation, cx);
            if !self.queue_key_release(keycode) && self.pressed_keys.remove(&keycode) {
                self.model.read(cx).key(keycode, false);
            }
        } else if self.queue_key_release(keycode) {
            // The matching press will be replayed after the handoff.
        } else if !presents {
            self.pressed_keys.remove(&keycode);
        } else if self.pressed_keys.remove(&keycode) {
            self.model.read(cx).key(keycode, false);
        }
        cx.stop_propagation();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.is_held {
            cx.stop_propagation();
            return;
        }
        let Some((keycode, generation)) = self.pending_key.take() else {
            cx.stop_propagation();
            return;
        };
        self.forward_key_press(keycode, generation, cx);
        cx.stop_propagation();
    }

    fn forward_key_press(&mut self, keycode: u32, generation: Option<u64>, cx: &mut Context<Self>) {
        if let Some(generation) = generation {
            self.queue_input_event(
                generation,
                QueuedInputEvent::Key {
                    keycode,
                    pressed: true,
                },
            );
        } else if !self.pressed_keys.contains(&keycode) && self.model.read(cx).key(keycode, true) {
            self.pressed_keys.insert(keycode);
        }
    }

    fn hover_changed(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if *hovered {
            return;
        }
        let finger_axes = std::mem::take(&mut self.finger_axes);
        let pinch_active = std::mem::take(&mut self.pinch_active);
        let axis_stop = (finger_axes != (false, false)).then(|| LinuxPointerAxisEvent {
            position: Default::default(),
            time: self.last_axis_time,
            source: LinuxAxisSource::Finger,
            value: (0.0, 0.0),
            v120: (None, None),
            stop: finger_axes,
            relative_direction: (
                LinuxAxisRelativeDirection::Identical,
                LinuxAxisRelativeDirection::Identical,
            ),
        });
        if let Some(generation) = self.queued_input.as_ref().map(|queued| queued.generation) {
            if let Some(event) = axis_stop {
                self.queue_input_event(
                    generation,
                    QueuedInputEvent::Axis {
                        position: self.last_pointer_position,
                        event,
                    },
                );
            }
            if pinch_active {
                self.queue_input_event(
                    generation,
                    QueuedInputEvent::Pinch {
                        position: None,
                        gesture: PinchGesture::End { cancelled: true },
                    },
                );
            }
            self.queue_input_event(generation, QueuedInputEvent::Leave);
        } else if self.model.read(cx).presents(self.owner_id, self.page_id) {
            let model = self.model.read(cx);
            if let Some(event) = axis_stop {
                model.pointer_axis(&event);
            }
            if pinch_active {
                model.pinch(PinchGesture::End { cancelled: true });
            }
            model.pointer_leave();
        }
    }

    fn release_input(&mut self, cx: &mut Context<Self>) {
        self.queued_input = None;
        self.pending_key = None;
        self.claiming_pointer_focus = false;
        let model = self.model.read(cx);
        let presents = model.presents(self.owner_id, self.page_id);
        for keycode in self.pressed_keys.drain() {
            if presents {
                model.key(keycode, false);
            }
        }
        for button in self.pressed_buttons.drain() {
            if presents {
                model.pointer_button(button, false);
            }
        }
        if std::mem::take(&mut self.finger_axes) != (false, false) && presents {
            model.pointer_axis(&LinuxPointerAxisEvent {
                position: Default::default(),
                time: self.last_axis_time,
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
        if std::mem::take(&mut self.pinch_active) && presents {
            model.pinch(PinchGesture::End { cancelled: true });
        }
    }
}

fn matching_handoff(handoff: Option<PageHandoff>, generation: u64) -> Option<PageHandoff> {
    handoff.filter(|handoff| handoff.generation == generation)
}

fn track_finger_axis(active: &mut (bool, bool), event: &LinuxPointerAxisEvent) -> bool {
    let was_active = *active != (false, false);
    if event.source != LinuxAxisSource::Finger {
        return was_active;
    }
    if event.value.0 != 0.0 {
        active.0 = true;
    }
    if event.value.1 != 0.0 {
        active.1 = true;
    }
    if event.stop.0 {
        active.0 = false;
    }
    if event.stop.1 {
        active.1 = false;
    }
    was_active
}

fn pinch_gesture(event: LinuxPinchEvent) -> PinchGesture {
    match event {
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

fn is_modifier_keycode(keycode: u32) -> bool {
    matches!(keycode, 29 | 42 | 54 | 56 | 58 | 69 | 97 | 100 | 125 | 126)
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
        if browser_passthrough_enabled() && !self.passthrough_attempted {
            self.passthrough_attempted = true;
            let (events_tx, events_rx) = async_channel::unbounded();
            match window.create_wayland_passthrough(move |event| {
                let _ = events_tx.try_send(event);
            }) {
                Some(Ok(passthrough)) => {
                    let paint_state = self.passthrough_state.clone();
                    let model = self.model.clone();
                    self.passthrough_events = Some(cx.spawn(async move |this, cx| {
                        while let Ok(event) = events_rx.recv().await {
                            let _ = this.update(cx, |_, cx| {
                                let model = model.read(cx);
                                let Some(session) = model.runtime.session.as_ref() else {
                                    return;
                                };
                                match event {
                                    LinuxWaylandPassthroughEvent::Frame {
                                        scene_id,
                                        callback_time,
                                    } => session.host_frame(scene_id, callback_time),
                                    LinuxWaylandPassthroughEvent::Presented {
                                        scene_id,
                                        timestamp,
                                        refresh,
                                        sequence,
                                        flags,
                                    } => {
                                        session.host_presentation(
                                            scene_id,
                                            HostPresentation::Presented {
                                                timestamp,
                                                refresh,
                                                sequence,
                                                flags,
                                            },
                                        );
                                        let mut state = paint_state.get();
                                        if state.submitted == Some(scene_id) {
                                            state.presented = Some(scene_id);
                                            paint_state.set(state);
                                            cx.notify();
                                        }
                                    }
                                    LinuxWaylandPassthroughEvent::Discarded { scene_id } => {
                                        session.host_presentation(
                                            scene_id,
                                            HostPresentation::Discarded,
                                        );
                                        let mut state = paint_state.get();
                                        if state.submitted == Some(scene_id) {
                                            state.presented = None;
                                            paint_state.set(state);
                                            cx.notify();
                                        }
                                    }
                                }
                            });
                        }
                    }));
                    self.passthrough = Some(passthrough);
                }
                Some(Err(error)) => {
                    tracing::warn!(?error, "Wayland browser passthrough unavailable");
                }
                None => {}
            }
        }
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
                    if std::mem::take(&mut this.claiming_pointer_focus) {
                        return;
                    }
                    this.model.update(cx, |model, cx| {
                        model.claim_or_join_presentation(owner_id, page_id, cx);
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
                if model.runtime.painted_scene_id != painted_scene_id
                    && (model.runtime.painted_scene_id.is_none() || painted_scene_id.is_none())
                {
                    tracing::info!(
                        previous_scene = ?model.runtime.painted_scene_id,
                        new_scene = ?painted_scene_id,
                        "browser portal changed its painted scene"
                    );
                }
                model.runtime.painted_scene_id = painted_scene_id;
            });
        }
        if let Some(queued) = self.queued_input.clone() {
            let replay_state = {
                let model = self.model.read(cx);
                input_replay_state(
                    queued.generation,
                    self.owner_id,
                    self.page_id,
                    model.presentation_owner,
                    model.handoff,
                    model.runtime.painted_scene_id,
                )
            };
            match replay_state {
                InputReplayState::Waiting => {}
                InputReplayState::Cancel => {
                    self.queued_input = None;
                    self.pinch_active = false;
                }
                InputReplayState::Ready => {
                    self.queued_input = None;
                    let model = self.model.read(cx);
                    for event in queued.events {
                        match event {
                            QueuedInputEvent::Motion { position } => {
                                model.pointer_motion(position.0, position.1);
                            }
                            QueuedInputEvent::Button {
                                position,
                                button,
                                pressed,
                            } => {
                                model.pointer_motion(position.0, position.1);
                                if pressed {
                                    if model.pointer_button(button, true) {
                                        self.pressed_buttons.insert(button);
                                    }
                                } else if self.pressed_buttons.remove(&button) {
                                    model.pointer_button(button, false);
                                }
                            }
                            QueuedInputEvent::Axis { position, event } => {
                                model.pointer_motion(position.0, position.1);
                                model.pointer_axis(&event);
                            }
                            QueuedInputEvent::Leave => model.pointer_leave(),
                            QueuedInputEvent::Pinch { position, gesture } => {
                                if let Some((x, y)) = position {
                                    model.pointer_motion(x, y);
                                }
                                self.pinch_active = !matches!(gesture, PinchGesture::End { .. });
                                model.pinch(gesture);
                            }
                            QueuedInputEvent::Key { keycode, pressed } => {
                                if pressed {
                                    if !self.pressed_keys.contains(&keycode)
                                        && model.key(keycode, true)
                                    {
                                        self.pressed_keys.insert(keycode);
                                    }
                                } else if self.pressed_keys.remove(&keycode) {
                                    model.key(keycode, false);
                                }
                            }
                        }
                    }
                }
            }
        }
        let model = self.model.read(cx);
        if let (Some(session), Some(vsync)) = (model.runtime.session.as_ref(), window.host_vsync())
        {
            session.host_vsync(vsync.timestamp, vsync.refresh_period);
        }
        let colors = cx.theme().colors();
        let presents = model.presents(self.owner_id, self.page_id);
        let presented = if owns_presentation
            && model.runtime.scene_id != self.scheduled_scene
            && let (Some(scene_id), Some(session)) =
                (model.runtime.scene_id, model.runtime.session.as_ref())
        {
            self.scheduled_scene = Some(scene_id);
            (scene_id > model.runtime.invalidated_through).then(|| {
                rho_browser_wayland::record_browser_timing(
                    rho_browser_wayland::BrowserTimingKind::SceneScheduled,
                    scene_id,
                    model.runtime.presented_barrier,
                    None,
                    None,
                );
                (
                    scene_id,
                    model.runtime.presented_barrier,
                    std::time::Instant::now(),
                    session.presentation_callback(scene_id, model.runtime.presented_barrier),
                )
            })
        } else {
            None
        };
        let current_scene = if owns_presentation {
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
        let candidate = owns_presentation
            .then(|| {
                passthrough_scene(
                    &model.runtime.scene,
                    model.runtime.logical_size,
                    &model.runtime.buffers,
                )
                .and_then(|(_, buffer)| Some((model.runtime.scene_id?, buffer.clone())))
            })
            .flatten();
        // A lease is claimed by exactly one presentation path. Keep the last
        // texture scene as the covering frame while a newer, never-rendered
        // lease is promoted to the child surface.
        let (scene, passthrough_scene) = if let Some((scene_id, buffer)) = candidate
            && self.passthrough.is_some()
            && self.fallback_scene_id.is_some()
            && self.fallback_scene_id != Some(scene_id)
            && !self.fallback_scene.iter().any(|(_, fallback)| {
                matches!(fallback, BrowserBuffer::DmaBuf(fallback) if fallback.surface.lease_id() == buffer.surface.lease_id())
            })
        {
            (self.fallback_scene.clone(), Some((scene_id, buffer)))
        } else {
            self.fallback_scene_id = model.runtime.scene_id;
            self.fallback_scene = current_scene.clone();
            (current_scene, None)
        };
        if passthrough_scene.is_some() && !self.passthrough_timing_enabled.get() {
            if let Some(session) = model.runtime.session.as_ref() {
                session.enable_presentation_passthrough();
                self.passthrough_timing_enabled.set(true);
            }
        }
        let status = browser_status(
            &model.runtime,
            presents,
            model.presentation_owner == Some(self.owner_id),
        );
        let cursor = if presents {
            gpui_cursor(model.runtime.cursor)
        } else {
            CursorStyle::Arrow
        };
        let scale = window.scale_factor();
        let owner_id = self.owner_id;
        let browser = self.model.clone();
        let origin = self.origin.clone();
        let passthrough = self.passthrough.clone();
        let passthrough_state = self.passthrough_state.clone();
        let passthrough_timing_enabled = self.passthrough_timing_enabled.clone();
        let timing_model = self.model.clone();
        let measure = canvas(
            move |bounds, _, cx| {
                origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
                let width = f32::from(bounds.size.width).round().max(1.0) as u32;
                let height = f32::from(bounds.size.height).round().max(1.0) as u32;
                let browser = browser.read(cx);
                if browser.presentation_owner == Some(owner_id) {
                    browser.resize(width, height, scale);
                }
            },
            move |bounds, _, window, cx| {
                let mut state = passthrough_state.get();
                let compositor_eligible = passthrough.is_some()
                    && passthrough_scene.is_some()
                    && window.compositor_child_hole_eligible(bounds);
                if compositor_eligible {
                    let passthrough = passthrough.as_ref().unwrap();
                    let (scene_id, buffer) = passthrough_scene.as_ref().unwrap();
                    if state.positioned != Some(*scene_id)
                        && passthrough.set_geometry(bounds).is_ok()
                    {
                        state.positioned = Some(*scene_id);
                        let scene_id = *scene_id;
                        let buffer = buffer.clone();
                        let passthrough = passthrough.clone();
                        let paint_state = passthrough_state.clone();
                        // place_below, position, and viewport destination are
                        // parent-latched. Commit the desynchronized child only
                        // after this GPUI frame has committed those requests.
                        window.on_next_frame(move |_, cx| {
                            let mut state = paint_state.get();
                            if state.positioned != Some(scene_id) {
                                return;
                            }
                            match buffer
                                .passthrough_buffer()
                                .map_err(anyhow::Error::from)
                                .and_then(|buffer| passthrough.present(scene_id, buffer))
                            {
                                Ok(()) => {
                                    state.submitted = Some(scene_id);
                                    state.presented = None;
                                }
                                Err(error) => {
                                    tracing::warn!(?error, "present browser DMA-BUF passthrough");
                                    passthrough.hide();
                                    state = PassthroughPaintState::default();
                                }
                            }
                            paint_state.set(state);
                            cx.notify(owner_id);
                        });
                    }
                    // Promotion keeps painting the texture until niri confirms
                    // the child commit was presented. Because hole punching is
                    // an after-content pass, ordinary GPUI overlays remain above
                    // the child once the hole becomes active.
                    if state.presented != Some(*scene_id) {
                        state.active = false;
                    } else {
                        window.paint_hole(bounds);
                        state.active = true;
                    }
                } else {
                    if passthrough_timing_enabled.replace(false)
                        && let Some(session) = timing_model.read(cx).runtime.session.as_ref()
                    {
                        session.disable_presentation_passthrough();
                    }
                    if state.submitted.is_some()
                        && let Some(passthrough) = passthrough.clone()
                    {
                        // The opaque texture is drawn before the asynchronous
                        // null attach, so demotion cannot expose stale child pixels.
                        window.on_next_frame(move |_, _| passthrough.hide());
                    }
                    state = PassthroughPaintState::default();
                }
                passthrough_state.set(state);
                if let Some((scene_id, barrier, scheduled_at, presented)) = presented {
                    // Match a nested compositor: unblock the client's next
                    // frame after this scene has been painted into GPUI's
                    // current frame, rather than one outer refresh later.
                    rho_browser_wayland::record_browser_timing(
                        rho_browser_wayland::BrowserTimingKind::ScenePainted,
                        scene_id,
                        barrier,
                        None,
                        Some(scheduled_at.elapsed()),
                    );
                    presented();
                }
            },
        )
        .size_full();

        div()
            .id("rho-browser")
            .track_focus(&self.focus_handle)
            .on_hover(cx.listener(Self::hover_changed))
            .cursor(cursor)
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
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(|_, _, cx| cx.stop_propagation())
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.editor_background)
            .children(scene.into_iter().map(|(node, buffer)| {
                let content = match buffer {
                    BrowserBuffer::DmaBuf(frame) => surface(frame.surface)
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
    fn queued_input_waits_for_its_painted_handoff_and_cancels_when_superseded() {
        let owner = EntityId::from(1);
        let other_owner = EntityId::from(2);
        let page = PageId(uuid::Uuid::new_v4());
        let handoff = PageHandoff {
            generation: 7,
            target: page,
            phase: HandoffPhase::Ready,
            input_generation: Some(3),
        };

        assert_eq!(
            input_replay_state(7, owner, page, Some(owner), Some(handoff), None),
            InputReplayState::Waiting
        );
        assert_eq!(
            input_replay_state(7, owner, page, Some(owner), Some(handoff), Some(9)),
            InputReplayState::Ready
        );
        assert_eq!(
            input_replay_state(7, owner, page, Some(other_owner), Some(handoff), Some(9)),
            InputReplayState::Cancel
        );
        assert_eq!(
            input_replay_state(
                7,
                owner,
                page,
                Some(owner),
                Some(PageHandoff {
                    generation: 8,
                    ..handoff
                }),
                Some(9)
            ),
            InputReplayState::Cancel
        );
    }

    #[test]
    fn queued_input_preserves_order_and_records_only_its_matching_release() {
        let mut queued = QueuedInput {
            generation: 1,
            events: vec![QueuedInputEvent::Button {
                position: (0.0, 0.0),
                button: 0x110,
                pressed: true,
            }],
        };
        assert!(!queued.record_release(0x111, (1.0, 1.0)).unwrap());
        assert_eq!(queued.events.len(), 1);
        queued
            .push(QueuedInputEvent::Motion {
                position: (1.0, 1.0),
            })
            .unwrap();
        queued
            .push(QueuedInputEvent::Motion {
                position: (2.0, 2.0),
            })
            .unwrap();
        assert!(queued.record_release(0x110, (2.0, 2.0)).unwrap());
        assert!(matches!(
            queued.events.as_slice(),
            [
                QueuedInputEvent::Button { pressed: true, .. },
                QueuedInputEvent::Motion {
                    position: (2.0, 2.0)
                },
                QueuedInputEvent::Button { pressed: false, .. }
            ]
        ));

        queued
            .push(QueuedInputEvent::Key {
                keycode: 30,
                pressed: true,
            })
            .unwrap();
        assert!(!queued.record_key_release(31).unwrap());
        assert!(queued.record_key_release(30).unwrap());
        assert!(matches!(
            queued.events.as_slice(),
            [
                ..,
                QueuedInputEvent::Key {
                    keycode: 30,
                    pressed: true
                },
                QueuedInputEvent::Key {
                    keycode: 30,
                    pressed: false
                }
            ]
        ));
    }

    #[test]
    fn focus_completion_matches_generation_not_a_repeated_page_target() {
        let page = PageId(uuid::Uuid::new_v4());
        let latest = PageHandoff {
            generation: 3,
            target: page,
            phase: HandoffPhase::Requested,
            input_generation: None,
        };

        assert_eq!(matching_handoff(Some(latest), 1), None);
        assert_eq!(matching_handoff(Some(latest), 3), Some(latest));
    }

    #[test]
    fn finger_sequence_remains_active_across_multiple_continuation_frames() {
        let mut active = (true, false);
        let mut event = LinuxPointerAxisEvent {
            position: Default::default(),
            time: 1,
            source: LinuxAxisSource::Finger,
            value: (1.0, 0.0),
            v120: (None, None),
            stop: (false, false),
            relative_direction: (
                LinuxAxisRelativeDirection::Identical,
                LinuxAxisRelativeDirection::Identical,
            ),
        };

        assert!(track_finger_axis(&mut active, &event));
        assert!(track_finger_axis(&mut active, &event));
        assert_eq!(active, (true, false));

        event.value = (0.0, 0.0);
        event.stop = (true, false);
        assert!(track_finger_axis(&mut active, &event));
        assert_eq!(active, (false, false));
    }

    #[test]
    fn pre_response_frame_cannot_complete_focus_without_configure_barrier() {
        let page = PageId(uuid::Uuid::new_v4());
        let mut handoff = Some(PageHandoff {
            generation: 1,
            target: page,
            phase: HandoffPhase::Focusing,
            input_generation: Some(1),
        });

        // A target-looking frame may arrive before the activation RPC reply.
        assert!(!frame_is_eligible(handoff, 0));

        // RPC completion installs a configure barrier. No ordinary later frame
        // can unblock presentation; only the post-ACK barrier commit can.
        handoff.as_mut().unwrap().phase = HandoffPhase::AwaitingFrame { barrier: 7 };
        assert!(!frame_is_eligible(handoff, 0));
        assert!(frame_is_eligible(handoff, 7));
        assert_ne!(handoff.unwrap().phase, HandoffPhase::Ready);
        complete_frame_handoff(&mut handoff, 7);
        assert_eq!(handoff.unwrap().phase, HandoffPhase::Ready);
        assert_eq!(handoff.unwrap().input_generation, Some(1));
    }

    #[test]
    fn queued_handoff_target_close_is_cancelled_not_treated_as_inactive() {
        let page = PageId(uuid::Uuid::new_v4());
        let other = PageId(uuid::Uuid::new_v4());
        let mut handoff = PageHandoff {
            generation: 1,
            target: page,
            phase: HandoffPhase::Requested,
            input_generation: None,
        };
        assert_eq!(
            close_transition(Some(handoff), page),
            CloseTransition::CancelRequested
        );
        handoff.phase = HandoffPhase::Focusing;
        handoff.input_generation = Some(3);
        assert_eq!(
            close_transition(Some(handoff), page),
            CloseTransition::FreezeActive
        );
        assert_eq!(
            close_transition(Some(handoff), other),
            CloseTransition::Inactive
        );
    }

    #[test]
    fn popup_pixels_are_rgba_at_the_gpui_image_boundary() {
        let mut pixels = [3, 2, 1, 255, 30, 20, 10, 128];
        bgra_to_rgba(&mut pixels);
        assert_eq!(pixels, [1, 2, 3, 255, 10, 20, 30, 128]);
    }

    #[test]
    fn passthrough_requires_one_full_untransformed_scene_node() {
        let mut node = SceneNode {
            surface_id: 1,
            buffer_id: 2,
            origin: (0.0, 0.0),
            destination: (640.0, 480.0),
            source: ((0.0, 0.0), (1280.0, 960.0)),
        };
        assert!(passthrough_node_eligible(&node, (640, 480), (1280, 960)));
        node.origin.0 = 1.0;
        assert!(!passthrough_node_eligible(&node, (640, 480), (1280, 960)));
        node.origin.0 = 0.0;
        node.source.1.0 = 1279.0;
        assert!(!passthrough_node_eligible(&node, (640, 480), (1280, 960)));
    }

    #[test]
    fn terminal_event_during_handoff_clears_frames_and_reports_failure_once() {
        let page = PageId(uuid::Uuid::new_v4());
        let image = Arc::new(RenderImage::new(smallvec::SmallVec::from_const([
            Frame::new(RgbaImage::new(1, 1)),
        ])));
        let mut runtime = RuntimePageState {
            session: None,
            terminal: false,
            buffers: HashMap::from([(7, BrowserBuffer::Shm(image))]),
            scene: vec![SceneNode {
                surface_id: 1,
                buffer_id: 7,
                origin: (0.0, 0.0),
                destination: (1.0, 1.0),
                source: ((0.0, 0.0), (1.0, 1.0)),
            }],
            scene_id: Some(9),
            logical_size: (1, 1),
            painted_scene_id: Some(9),
            invalidated_through: 0,
            presented_barrier: 4,
            status: Some("switching browser page".into()),
            sent_size: Rc::new(Cell::new((1, 1, 1.0_f32.to_bits()))),
            cursor: BrowserCursor::Arrow,
        };
        let mut handoff = Some(PageHandoff {
            generation: 1,
            target: page,
            phase: HandoffPhase::AwaitingFrame { barrier: 5 },
            input_generation: Some(1),
        });
        let mut active_focus = Some(page);
        let mut dropped = 0;

        assert!(transition_terminal_state(
            &mut runtime,
            &mut handoff,
            &mut active_focus,
            "browser failed: disconnected".into(),
            |_| dropped += 1,
        ));
        assert!(runtime.terminal);
        assert!(runtime.session.is_none());
        assert!(runtime.buffers.is_empty());
        assert!(runtime.scene.is_empty());
        assert_eq!(runtime.scene_id, None);
        assert_eq!(runtime.painted_scene_id, None);
        assert_eq!(runtime.presented_barrier, 0);
        assert_eq!(handoff, None);
        assert_eq!(active_focus, None);
        assert_eq!(dropped, 1);
        assert_eq!(
            browser_status(&runtime, false, true).as_deref(),
            Some("browser failed: disconnected")
        );

        assert!(!transition_terminal_state(
            &mut runtime,
            &mut handoff,
            &mut active_focus,
            "browser closed".into(),
            |_| dropped += 1,
        ));
        assert_eq!(
            runtime.status.as_deref(),
            Some("browser failed: disconnected")
        );
        assert_eq!(dropped, 1);
    }
}
