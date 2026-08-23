//! A private, protocol-only Wayland compositor for embedding stock Chrome.
//!
//! Chrome remains an unmodified Ozone/Wayland client. Rendering belongs to the
//! host GUI; this crate owns the shared Wayland protocol state and transfers
//! committed buffers to that host.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::cell::RefCell;
use std::ffi::OsString;
use std::hash::Hash;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use calloop::channel::{self, Event as ChannelEvent};
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode as PollMode, PostAction};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode};
use smithay::backend::renderer::{BufferType, buffer_type};
use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy,
    get_popup_toplevel_coords,
};
use smithay::input::keyboard::{FilterResult, KeyboardHandle};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, MotionEvent, PointerHandle,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::protocol::wl_subsurface::{self, WlSubsurface};
use smithay::reexports::wayland_server::protocol::{
    wl_buffer, wl_callback, wl_output, wl_seat, wl_shm,
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, ListeningSocket, Resource,
};
use smithay::utils::{Logical, Rectangle, Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SUBSURFACE_ROLE, SubsurfaceCachedState, SubsurfaceUserData, SurfaceAttributes,
    SurfaceData, TraversalAction, get_role, is_sync_subsurface, with_states,
    with_surface_tree_downward, with_surface_tree_upward,
};
use smithay::wayland::dmabuf::{
    DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::drm_syncobj::{
    DrmSyncPoint, DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState,
    supports_syncobj_eventfd,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::fractional_scale::{
    FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
};
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, PresentationState, Refresh,
};
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::viewporter::ViewportCachedState;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    Configure, PopupSurface, PositionerState, SurfaceCachedState as XdgSurfaceCachedState,
    ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::{
    delegate_cursor_shape, delegate_dmabuf, delegate_drm_syncobj, delegate_fractional_scale,
    delegate_output, delegate_pointer_gestures, delegate_presentation, delegate_seat, delegate_shm,
    delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
};

const MAX_POPUP_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCENE_SHM_BYTES: usize = 32 * 1024 * 1024;
const MAX_BROWSER_TIMINGS: usize = 4_096;

/// One stage in the Chromium-to-GPUI scene presentation pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BrowserTimingKind {
    SceneProduced = 1,
    SceneCoalesced = 2,
    SceneReceived = 3,
    SceneScheduled = 4,
    ScenePainted = 5,
    FrameAcknowledged = 6,
    FrameCallbackSent = 7,
    HostFrameCallbackSent = 8,
    FallbackFrameCallbackSent = 9,
}

/// Numeric-only timing marker retained in a fixed-size process-wide ring.
#[derive(Clone, Copy, Debug)]
pub struct BrowserTiming {
    pub kind: BrowserTimingKind,
    pub scene_id: u64,
    pub barrier: u64,
    pub related_scene_id: Option<u64>,
    pub at: Instant,
    pub duration: Option<Duration>,
}

static BROWSER_TIMINGS: Mutex<VecDeque<BrowserTiming>> = Mutex::new(VecDeque::new());

pub fn record_browser_timing(
    kind: BrowserTimingKind,
    scene_id: u64,
    barrier: u64,
    related_scene_id: Option<u64>,
    duration: Option<Duration>,
) {
    let mut timings = BROWSER_TIMINGS.lock().unwrap();
    if timings.len() >= MAX_BROWSER_TIMINGS {
        timings.pop_front();
    }
    timings.push_back(BrowserTiming {
        kind,
        scene_id,
        barrier,
        related_scene_id,
        at: Instant::now(),
        duration,
    });
}

/// Returns a non-destructive copy of the bounded browser-pipeline timing ring.
pub fn snapshot_browser_timings() -> Vec<BrowserTiming> {
    BROWSER_TIMINGS.lock().unwrap().iter().copied().collect()
}

/// Renderer-side requestAnimationFrame cadence stats reported by the
/// component extension; the ground truth for how often Chromium was
/// actually allowed to produce frames.
#[derive(Clone, Copy, Debug)]
pub struct ExtensionFrameStats {
    pub tab_id: i64,
    pub at: Instant,
    pub frames: u32,
    pub window_ms: u32,
    pub mean_interval_us: u32,
    pub p95_interval_us: u32,
    pub max_interval_us: u32,
    pub long_frames: u32,
}

const MAX_EXTENSION_FRAME_STATS: usize = 1024;
static EXTENSION_FRAME_STATS: Mutex<VecDeque<ExtensionFrameStats>> = Mutex::new(VecDeque::new());

pub fn record_extension_frame_stats(stats: ExtensionFrameStats) {
    let mut ring = EXTENSION_FRAME_STATS.lock().unwrap();
    if ring.len() >= MAX_EXTENSION_FRAME_STATS {
        ring.pop_front();
    }
    ring.push_back(stats);
}

/// Returns a non-destructive copy of the bounded extension frame-stats ring.
pub fn snapshot_extension_frame_stats() -> Vec<ExtensionFrameStats> {
    EXTENSION_FRAME_STATS.lock().unwrap().iter().copied().collect()
}

#[derive(Clone, Debug)]
pub struct DmaBufConfig {
    pub render_node: PathBuf,
    pub device_id: u64,
    pub formats: Arc<[(u32, u64)]>,
}

pub enum BrowserRenderConfig {
    DmaBuf(DmaBufConfig),
    /// Bounded owned SHM snapshots for software-only isolated QA sessions.
    SoftwareShmQa,
}

pub struct DmaBufFrame {
    pub id: u64,
    /// The newest host-requested frame barrier acknowledged before this commit.
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
    pub fd: OwnedFd,
    /// Additional DMA-BUF planes after the first plane above.
    pub additional_planes: Vec<DmaBufPlane>,
    pub acquire_fence: OwnedFd,
    release: Option<Box<dyn FnOnce() + Send>>,
}

#[derive(Debug)]
pub struct DmaBufPlane {
    pub fd: OwnedFd,
    pub stride: u32,
    pub offset: u32,
}

#[derive(Clone, Debug)]
pub struct ShmFrame {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    /// Straight-alpha BGRA pixels in Wayland SHM's native little-endian layout.
    /// The GPUI view converts them to RGBA at the `RgbaImage` boundary.
    pub pixels: Vec<u8>,
}

#[derive(Debug)]
pub enum BufferImport {
    DmaBuf(DmaBufFrame),
    Shm(ShmFrame),
}

impl BufferImport {
    fn id(&self) -> u64 {
        match self {
            Self::DmaBuf(frame) => frame.id,
            Self::Shm(frame) => frame.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneNode {
    pub surface_id: u64,
    pub buffer_id: u64,
    pub origin: (f64, f64),
    pub destination: (f64, f64),
    /// Source rectangle in buffer pixels.
    pub source: ((f64, f64), (f64, f64)),
}

#[derive(Debug)]
pub struct SceneUpdate {
    pub id: u64,
    pub barrier: u64,
    pub logical_size: (u32, u32),
    pub imports: Vec<BufferImport>,
    /// Every currently attached buffer, including buffers hidden by an unmapped
    /// ancestor.
    pub attached: Vec<u64>,
    /// Surface nodes in bottom-to-top paint order.
    pub nodes: Vec<SceneNode>,
    pub produced_at: Instant,
}

impl DmaBufFrame {
    pub fn duplicate_fd(&self) -> std::io::Result<OwnedFd> {
        self.fd.as_fd().try_clone_to_owned()
    }
    pub fn duplicate_acquire_fence(&self) -> std::io::Result<OwnedFd> {
        self.acquire_fence.as_fd().try_clone_to_owned()
    }
    pub fn duplicate_planes(&self) -> std::io::Result<Vec<DmaBufPlane>> {
        let mut planes = Vec::with_capacity(1 + self.additional_planes.len());
        planes.push(DmaBufPlane {
            fd: self.fd.as_fd().try_clone_to_owned()?,
            stride: self.stride,
            offset: self.offset,
        });
        for plane in &self.additional_planes {
            planes.push(DmaBufPlane {
                fd: plane.fd.as_fd().try_clone_to_owned()?,
                stride: plane.stride,
                offset: plane.offset,
            });
        }
        Ok(planes)
    }
    pub fn take_release(&mut self) -> Box<dyn FnOnce() + Send> {
        self.release
            .take()
            .expect("DMA-BUF release callback taken once")
    }
}

impl std::fmt::Debug for DmaBufFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmaBufFrame")
            .field("id", &self.id)
            .field("size", &(self.width, self.height))
            .field("fourcc", &self.fourcc)
            .field("modifier", &self.modifier)
            .finish()
    }
}

impl Drop for DmaBufFrame {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

#[derive(Debug)]
pub enum BrowserEvent {
    Scene(SceneUpdate),
    Cursor(BrowserCursor),
    FrameRetired(u64),
    ToplevelReady,
    Closed,
    Failed(Arc<str>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserCursor {
    #[default]
    Arrow,
    IBeam,
    Crosshair,
    ClosedHand,
    OpenHand,
    PointingHand,
    ResizeLeft,
    ResizeRight,
    ResizeLeftRight,
    ResizeUp,
    ResizeDown,
    ResizeUpDown,
    ResizeUpLeftDownRight,
    ResizeUpRightDownLeft,
    ResizeColumn,
    ResizeRow,
    VerticalText,
    NotAllowed,
    DragLink,
    DragCopy,
    ContextMenu,
}

fn browser_cursor(status: CursorImageStatus) -> BrowserCursor {
    let CursorImageStatus::Named(icon) = status else {
        return BrowserCursor::Arrow;
    };
    match icon {
        CursorIcon::Pointer => BrowserCursor::PointingHand,
        CursorIcon::Text => BrowserCursor::IBeam,
        CursorIcon::VerticalText => BrowserCursor::VerticalText,
        CursorIcon::Crosshair | CursorIcon::Cell => BrowserCursor::Crosshair,
        CursorIcon::Grab => BrowserCursor::OpenHand,
        CursorIcon::Grabbing => BrowserCursor::ClosedHand,
        CursorIcon::WResize => BrowserCursor::ResizeLeft,
        CursorIcon::EResize => BrowserCursor::ResizeRight,
        CursorIcon::EwResize => BrowserCursor::ResizeLeftRight,
        CursorIcon::NResize => BrowserCursor::ResizeUp,
        CursorIcon::SResize => BrowserCursor::ResizeDown,
        CursorIcon::NsResize => BrowserCursor::ResizeUpDown,
        CursorIcon::NwResize | CursorIcon::SeResize | CursorIcon::NwseResize => {
            BrowserCursor::ResizeUpLeftDownRight
        }
        CursorIcon::NeResize | CursorIcon::SwResize | CursorIcon::NeswResize => {
            BrowserCursor::ResizeUpRightDownLeft
        }
        CursorIcon::ColResize => BrowserCursor::ResizeColumn,
        CursorIcon::RowResize => BrowserCursor::ResizeRow,
        CursorIcon::NoDrop | CursorIcon::NotAllowed => BrowserCursor::NotAllowed,
        CursorIcon::Alias => BrowserCursor::DragLink,
        CursorIcon::Copy => BrowserCursor::DragCopy,
        CursorIcon::ContextMenu => BrowserCursor::ContextMenu,
        _ => BrowserCursor::Arrow,
    }
}

#[derive(Clone)]
struct BrowserEventSender {
    queue: Arc<Mutex<VecDeque<BrowserEvent>>>,
    wake: async_channel::Sender<()>,
}

impl BrowserEventSender {
    fn send(&self, event: BrowserEvent) {
        let mut queue = self.queue.lock().unwrap();
        if let BrowserEvent::Scene(incoming) = &event
            && let Some(position) = queue
                .iter()
                .position(|queued| matches!(queued, BrowserEvent::Scene(_)))
            && let BrowserEvent::Scene(mut previous) = queue.remove(position).unwrap()
        {
            record_browser_timing(
                BrowserTimingKind::SceneCoalesced,
                previous.id,
                previous.barrier,
                Some(incoming.id),
                Some(
                    incoming
                        .produced_at
                        .saturating_duration_since(previous.produced_at),
                ),
            );
            let referenced = incoming
                .attached
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            let already_incoming = incoming
                .imports
                .iter()
                .map(BufferImport::id)
                .collect::<std::collections::HashSet<_>>();
            previous.imports.retain(|import| {
                referenced.contains(&import.id()) && !already_incoming.contains(&import.id())
            });
            if let BrowserEvent::Scene(incoming) = event {
                previous.imports.extend(incoming.imports);
                queue.push_back(BrowserEvent::Scene(SceneUpdate {
                    imports: previous.imports,
                    ..incoming
                }));
                drop(queue);
                let _ = self.wake.try_send(());
                return;
            }
        }
        queue.push_back(event);
        drop(queue);
        let _ = self.wake.try_send(());
    }
}

#[derive(Clone)]
pub struct BrowserEventReceiver {
    queue: Arc<Mutex<VecDeque<BrowserEvent>>>,
    wake_sender: async_channel::Sender<()>,
    wake: async_channel::Receiver<()>,
}

impl BrowserEventReceiver {
    pub async fn recv(&self) -> Result<BrowserEvent, async_channel::RecvError> {
        loop {
            self.wake.recv().await?;
            let mut queue = self.queue.lock().unwrap();
            if let Some(event) = queue.pop_front() {
                if !queue.is_empty() {
                    let _ = self.wake_sender.try_send(());
                }
                return Ok(event);
            }
        }
    }
}

fn browser_event_channel() -> (BrowserEventSender, BrowserEventReceiver) {
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let (wake_sender, wake) = async_channel::bounded(1);
    (
        BrowserEventSender {
            queue: queue.clone(),
            wake: wake_sender.clone(),
        },
        BrowserEventReceiver {
            queue,
            wake_sender,
            wake,
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerAxisSource {
    Finger,
    Continuous,
    Wheel,
    WheelTilt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerAxisDirection {
    Identical,
    Inverted,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerAxisFrame {
    pub time: u32,
    pub source: PointerAxisSource,
    pub value: (f64, f64),
    pub v120: (Option<i32>, Option<i32>),
    pub stop: (bool, bool),
    pub relative_direction: (PointerAxisDirection, PointerAxisDirection),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PinchGesture {
    Begin {
        fingers: u32,
    },
    Update {
        delta: (f64, f64),
        scale: f64,
        rotation: f64,
    },
    End {
        cancelled: bool,
    },
}

/// The outer compositor's resolution of feedback requested for one scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPresentation {
    Presented {
        /// Presentation time in the host's `CLOCK_MONOTONIC` domain.
        timestamp: Duration,
        refresh: Duration,
        sequence: u64,
        /// Raw `wp_presentation_feedback.kind` bits from the outer compositor.
        flags: u32,
    },
    Discarded,
}

enum PageCommand {
    Resize(u32, u32, f64),
    FrameBarrier(u64, u32, u32, f64),
    Presented {
        scene_id: u64,
        barrier: u64,
    },
    EnablePresentationPassthrough {
        session_generation: u64,
    },
    DisablePresentationPassthrough {
        session_generation: u64,
    },
    HostPresentation {
        session_generation: u64,
        scene_id: u64,
        presentation: HostPresentation,
    },
    HostFrame {
        session_generation: u64,
        scene_id: u64,
        callback_time: u32,
    },
    Retired(u64),
    PointerMotion {
        commit_id: u64,
        x: f64,
        y: f64,
    },
    PointerLeave,
    PointerButton {
        button: u32,
        pressed: bool,
    },
    PointerAxis(PointerAxisFrame),
    Pinch(PinchGesture),
    Key {
        keycode: u32,
        pressed: bool,
    },
    InputBarrier(u64, async_channel::Sender<()>),
    UnfreezeInput(u64),
    Close,
}

#[derive(Debug)]
enum OrderedInput {
    PointerMotion(ResolvedPointerMotion),
    PointerButton { button: u32, pressed: bool },
    PointerAxis(PointerAxisFrame),
    Pinch(PinchGesture),
    Key { keycode: u32, pressed: bool },
    InputBarrier(async_channel::Sender<()>),
}

#[derive(Clone, Debug, PartialEq)]
struct ResolvedPointerMotion {
    location: (f64, f64),
    target: Option<(WlSurface, (f64, f64))>,
}
enum RuntimeCommand<K> {
    Open {
        id: K,
        session_generation: u64,
        size: (u32, u32),
        events: BrowserEventSender,
    },
    Page(K, PageCommand),
    Shutdown,
}
pub trait BrowserPageKey: Copy + Eq + Hash + Send + 'static {}
impl<T: Copy + Eq + Hash + Send + 'static> BrowserPageKey for T {}

pub struct BrowserCompositor<K: BrowserPageKey> {
    commands: channel::Sender<RuntimeCommand<K>>,
    socket: OsString,
    next_session_generation: AtomicU64,
    thread: Option<thread::JoinHandle<()>>,
}
impl<K: BrowserPageKey> BrowserCompositor<K> {
    pub fn launch(render: BrowserRenderConfig) -> Result<Self> {
        let (tx, rx) = channel::channel();
        let tx2 = tx.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let errors = ready_tx.clone();
        let thread = thread::Builder::new()
            .name("rho-browser-wayland".into())
            .spawn(move || {
                if let Err(e) = run(rx, ready_tx, render, tx2) {
                    let _ = errors.send(Err(anyhow::anyhow!("{e:#}")));
                }
            })
            .context("spawn browser compositor thread")?;
        let socket = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .context("browser compositor did not start")??;
        Ok(Self {
            commands: tx,
            socket,
            next_session_generation: AtomicU64::new(1),
            thread: Some(thread),
        })
    }
    pub fn socket_name(&self) -> &std::ffi::OsStr {
        &self.socket
    }
    pub fn open(&self, id: K, size: (u32, u32)) -> Result<BrowserSession<K>> {
        if size.0 == 0 || size.1 == 0 {
            bail!("browser dimensions must be nonzero")
        };
        let session_generation = self.next_session_generation.fetch_add(1, Ordering::Relaxed);
        let (events, rx) = browser_event_channel();
        self.commands
            .send(RuntimeCommand::Open {
                id,
                session_generation,
                size,
                events,
            })
            .map_err(|_| anyhow::anyhow!("browser compositor stopped"))?;
        Ok(BrowserSession {
            id,
            session_generation,
            commands: self.commands.clone(),
            events: rx,
        })
    }
}
impl<K: BrowserPageKey> Drop for BrowserCompositor<K> {
    fn drop(&mut self) {
        let _ = self.commands.send(RuntimeCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
pub struct BrowserSession<K: BrowserPageKey> {
    id: K,
    session_generation: u64,
    commands: channel::Sender<RuntimeCommand<K>>,
    events: BrowserEventReceiver,
}
impl<K: BrowserPageKey> BrowserSession<K> {
    fn send(&self, c: PageCommand) {
        let _ = self.commands.send(RuntimeCommand::Page(self.id, c));
    }
    pub fn resize(&self, w: u32, h: u32, scale: f32) {
        if w > 0 && h > 0 && scale.is_finite() && scale > 0.0 {
            self.send(PageCommand::Resize(w, h, f64::from(scale)))
        }
    }
    /// Requests a deliberately changed configure. A DMA frame carrying this
    /// barrier value was committed after Chrome acknowledged that configure.
    pub fn frame_barrier(&self, barrier: u64, w: u32, h: u32, scale: f32) {
        if w > 0 && h > 0 && scale.is_finite() && scale > 0.0 {
            self.send(PageCommand::FrameBarrier(barrier, w, h, f64::from(scale)))
        }
    }
    pub fn pointer_motion(&self, commit_id: u64, x: f64, y: f64) {
        self.send(PageCommand::PointerMotion { commit_id, x, y })
    }
    pub fn pointer_leave(&self) {
        self.send(PageCommand::PointerLeave)
    }
    pub fn pointer_button(&self, button: u32, pressed: bool) {
        self.send(PageCommand::PointerButton { button, pressed })
    }
    pub fn pointer_axis(&self, frame: PointerAxisFrame) {
        self.send(PageCommand::PointerAxis(frame))
    }
    pub fn pinch(&self, gesture: PinchGesture) {
        self.send(PageCommand::Pinch(gesture))
    }
    pub fn key(&self, keycode: u32, pressed: bool) {
        self.send(PageCommand::Key { keycode, pressed })
    }
    pub fn input_barrier(
        &self,
        generation: u64,
    ) -> Result<impl std::future::Future<Output = Result<()>> + Send + 'static + use<K>> {
        let (acknowledge, acknowledged) = async_channel::bounded(1);
        self.commands
            .send(RuntimeCommand::Page(
                self.id,
                PageCommand::InputBarrier(generation, acknowledge),
            ))
            .map_err(|_| anyhow::anyhow!("browser compositor stopped"))?;
        Ok(async move {
            acknowledged
                .recv()
                .await
                .context("browser input barrier was cancelled")
        })
    }
    pub fn unfreeze_input(&self, generation: u64) {
        self.send(PageCommand::UnfreezeInput(generation));
    }
    pub fn events(&self) -> BrowserEventReceiver {
        self.events.clone()
    }
    /// Switches this session from paint-acknowledgement frame delivery to
    /// relaying real outer frame and presentation callbacks. This also
    /// advertises the nested `wp_presentation` global to Chromium.
    pub fn enable_presentation_passthrough(&self) {
        self.send(PageCommand::EnablePresentationPassthrough {
            session_generation: self.session_generation,
        });
    }
    /// Returns this session to predicted host-vsync frame delivery and
    /// discards presentation feedback still pending from passthrough.
    pub fn disable_presentation_passthrough(&self) {
        self.send(PageCommand::DisablePresentationPassthrough {
            session_generation: self.session_generation,
        });
    }
    /// Resolves presentation feedback associated with `scene_id` from the
    /// corresponding outer compositor feedback object.
    pub fn host_presentation(&self, scene_id: u64, presentation: HostPresentation) {
        self.send(PageCommand::HostPresentation {
            session_generation: self.session_generation,
            scene_id,
            presentation,
        });
    }
    /// Completes nested frame callbacks through `scene_id` from the exact
    /// outer `wl_surface.frame` event associated with that scene.
    pub fn host_frame(&self, scene_id: u64, callback_time: u32) {
        self.send(PageCommand::HostFrame {
            session_generation: self.session_generation,
            scene_id,
            callback_time,
        });
    }
    pub fn presentation_callback(
        &self,
        scene_id: u64,
        barrier: u64,
    ) -> impl FnOnce() + Send + 'static {
        let tx = self.commands.clone();
        let id = self.id;
        move || {
            let _ = tx.send(RuntimeCommand::Page(
                id,
                PageCommand::Presented { scene_id, barrier },
            ));
        }
    }
}
impl<K: BrowserPageKey> Drop for BrowserSession<K> {
    fn drop(&mut self) {
        self.send(PageCommand::Close)
    }
}
struct WindowState {
    session_generation: u64,
    toplevel: Option<ToplevelSurface>,
    size: (u32, u32),
    scale: f64,
    events: BrowserEventSender,
    dma_frame_callbacks: HashMap<u64, Vec<wl_callback::WlCallback>>,
    presentation_feedback: HashMap<u64, Vec<PresentationFeedbackCallback>>,
    presentation_passthrough: bool,
    last_host_presentation_scene: Option<u64>,
    last_host_frame_scene: Option<u64>,
    pointer_frames: HashMap<u64, PointerFrame>,
    hit_scenes: HashMap<u64, Vec<HitNode>>,
    pointer_location: (f64, f64),
    pending_input: VecDeque<OrderedInput>,
    input_freeze: Option<u64>,
    active_finger_axes: (bool, bool),
    last_finger_axis_time: u32,
    active_buttons: HashSet<u32>,
    active_keys: HashSet<u32>,
    pinch_active: bool,
    surface_slots: HashMap<ObjectId, SurfaceSlot>,
    pending_imports: HashMap<u64, BufferImport>,
    next_scene_id: u64,
    unbound_barrier: Option<u64>,
    pending_barriers: Vec<(Serial, u64)>,
    acked_barrier: u64,
    committed_barrier: u64,
    terminal_failure: bool,
    opened_at: Instant,
}

impl Drop for WindowState {
    fn drop(&mut self) {
        for feedback in self
            .presentation_feedback
            .drain()
            .flat_map(|(_, value)| value)
        {
            feedback.discarded();
        }
    }
}

fn disable_window_presentation_passthrough(window: &mut WindowState) -> Option<u64> {
    if !window.presentation_passthrough {
        return None;
    }
    window.presentation_passthrough = false;
    for feedback in window
        .presentation_feedback
        .drain()
        .flat_map(|(_, value)| value)
    {
        feedback.discarded();
    }
    window.dma_frame_callbacks.keys().copied().max()
}

#[derive(Clone, Copy, Debug)]
struct SurfaceSlot {
    buffer_id: u64,
    width: u32,
    height: u32,
    shm_bytes: usize,
}

#[derive(Clone)]
struct HitNode {
    surface: WlSurface,
    origin: (f64, f64),
    destination: (f64, f64),
    input_region: Option<smithay::wayland::compositor::RegionAttributes>,
}

struct State<K: BrowserPageKey> {
    loop_handle: LoopHandle<'static, State<K>>,
    display_handle: DisplayHandle,
    compositor: CompositorState,
    shell: XdgShellState,
    _decoration: XdgDecorationState,
    shm: ShmState,
    dmabuf: DmabufState,
    _dmabuf_global: Option<DmabufGlobal>,
    syncobj: Option<DrmSyncobjState>,
    dma_formats: Arc<[(u32, u64)]>,
    allow_root_shm: bool,
    output: Output,
    _fractional_scale: FractionalScaleManagerState,
    _viewporter: ViewporterState,
    _pointer_gestures: PointerGesturesState,
    _cursor_shape: CursorShapeManagerState,
    presentation: Option<PresentationState>,
    seat_state: SeatState<Self>,
    _seat: Seat<Self>,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,
    serial: u32,
    windows: HashMap<K, WindowState>,
    unbound_toplevels: HashMap<ObjectId, (ToplevelSurface, Instant)>,
    surface_windows: HashMap<ObjectId, K>,
    next_buffer_id: u64,
    commands: channel::Sender<RuntimeCommand<K>>,
    popup_manager: PopupManager,
}

#[derive(Clone, Copy)]
struct PointerFrame;

fn committed_barrier_after_scene(
    committed: u64,
    acknowledged: u64,
    barrier_anchor: bool,
    visible_accepted_root: bool,
) -> u64 {
    if barrier_anchor && visible_accepted_root {
        acknowledged
    } else {
        committed
    }
}

fn shm_surface_limit(is_root: bool, allow_root_shm: bool, retained_bytes: usize) -> Result<usize> {
    let per_surface = if is_root && allow_root_shm {
        MAX_SCENE_SHM_BYTES
    } else {
        MAX_POPUP_BYTES
    };
    let remaining = MAX_SCENE_SHM_BYTES
        .checked_sub(retained_bytes)
        .context("existing Chromium SHM scene exceeds its byte budget")?;
    Ok(per_surface.min(remaining))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceMapping {
    offset: (f64, f64),
    scale: (f64, f64),
    logical_size: (u32, u32),
}

struct MappedSurface {
    offset: (i32, i32),
    mapping: SurfaceMapping,
    input_region: Option<smithay::wayland::compositor::RegionAttributes>,
}

struct SurfaceTreeSnapshot {
    nodes: Vec<SceneNode>,
    hits: Vec<HitNode>,
}

fn monotonic_time() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC is also the clock used by the outer Wayland compositor
    // for input event timestamps.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    assert_eq!(result, 0, "CLOCK_MONOTONIC must be available on Linux");
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

fn monotonic_time_ms() -> u32 {
    monotonic_time().as_millis() as u32
}

fn normalize_shm_pixels(format: wl_shm::Format, pixels: &mut [u8]) {
    if format == wl_shm::Format::Xrgb8888 {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 0xff;
        }
    } else {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            if alpha == 0 {
                pixel[..3].fill(0);
            } else if alpha < 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
    }
}

fn surface_mapping(
    buffer_size: (i32, i32),
    buffer_scale: i32,
    source: Option<((f64, f64), (f64, f64))>,
    destination: Option<(i32, i32)>,
) -> SurfaceMapping {
    let buffer_scale = f64::from(buffer_scale.max(1));
    let source = source.unwrap_or((
        (0.0, 0.0),
        (
            f64::from(buffer_size.0) / buffer_scale,
            f64::from(buffer_size.1) / buffer_scale,
        ),
    ));
    let logical_size = destination
        .map(|(width, height)| (width.max(1) as u32, height.max(1) as u32))
        .unwrap_or((source.1.0.max(1.0) as u32, source.1.1.max(1.0) as u32));
    SurfaceMapping {
        offset: (source.0.0 * buffer_scale, source.0.1 * buffer_scale),
        scale: (
            source.1.0 * buffer_scale / f64::from(logical_size.0),
            source.1.1 * buffer_scale / f64::from(logical_size.1),
        ),
        logical_size,
    }
}

fn map_surface_state(states: &SurfaceData, slot: SurfaceSlot) -> Result<MappedSurface> {
    // Copy everything needed from the cache guards before validation. Smithay's
    // validator locks ViewportCachedState itself, so retaining that guard here
    // would deadlock the compositor thread.
    let (viewport, buffer_scale, buffer_transform, input_region) = {
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        let mut attributes = states.cached_state.get::<SurfaceAttributes>();
        let attributes = attributes.current();
        (
            *viewport.current(),
            attributes.buffer_scale,
            attributes.buffer_transform,
            attributes.input_region.clone(),
        )
    };
    if buffer_transform != wl_output::Transform::Normal {
        bail!("unsupported buffer transform {buffer_transform:?}")
    }
    if buffer_scale <= 0 {
        bail!("non-positive buffer scale {buffer_scale}")
    }
    let buffer_scale_u32 = u32::try_from(buffer_scale)?;
    if !slot.width.is_multiple_of(buffer_scale_u32) || !slot.height.is_multiple_of(buffer_scale_u32)
    {
        bail!(
            "buffer dimensions {}x{} are not divisible by scale {buffer_scale}",
            slot.width,
            slot.height
        )
    }
    let logical_buffer_size = (
        i32::try_from(slot.width / buffer_scale_u32)?,
        i32::try_from(slot.height / buffer_scale_u32)?,
    );
    if !smithay::wayland::viewporter::ensure_viewport_valid(states, logical_buffer_size.into()) {
        bail!("invalid viewport state")
    }
    if slot.shm_bytes != 0 && viewport.src.is_some() {
        bail!("SHM viewport cropping is unsupported")
    }
    let mapping = surface_mapping(
        (slot.width as i32, slot.height as i32),
        buffer_scale,
        viewport
            .src
            .map(|source| ((source.loc.x, source.loc.y), (source.size.w, source.size.h))),
        viewport
            .dst
            .map(|destination| (destination.w, destination.h)),
    );
    let source_size = (
        mapping.scale.0 * f64::from(mapping.logical_size.0),
        mapping.scale.1 * f64::from(mapping.logical_size.1),
    );
    if mapping.offset.0 < 0.0
        || mapping.offset.1 < 0.0
        || mapping.offset.0 + source_size.0 > f64::from(slot.width)
        || mapping.offset.1 + source_size.1 > f64::from(slot.height)
    {
        bail!("viewport source is outside its buffer")
    }
    let offset = if states.role == Some(SUBSURFACE_ROLE) {
        let mut subsurface = states.cached_state.get::<SubsurfaceCachedState>();
        let location = subsurface.current().location;
        (location.x, location.y)
    } else {
        (0, 0)
    };
    Ok(MappedSurface {
        offset,
        mapping,
        input_region,
    })
}

fn discard_surface_buffer(surface: &WlSurface) {
    let (assignment, release) = with_states(surface, |states| {
        let mut attributes = states.cached_state.get::<SurfaceAttributes>();
        let assignment = attributes.current().buffer.take();
        let mut sync = states.cached_state.get::<DrmSyncobjCachedState>();
        let release = sync.current().release_point.take();
        (assignment, release)
    });
    if let Some(release) = release {
        let _ = release.signal();
    }
    if let Some(BufferAssignment::NewBuffer(buffer)) = assignment {
        buffer.release();
    }
}

fn snapshot_surface_tree(
    root: &WlSurface,
    root_origin: (i32, i32),
    slots: &HashMap<ObjectId, SurfaceSlot>,
) -> Result<SurfaceTreeSnapshot> {
    let mut nodes = Vec::new();
    let mut hits = Vec::new();
    let error = RefCell::new(None);
    with_surface_tree_upward(
        root,
        root_origin,
        |surface, states, parent_origin| {
            if !slots.contains_key(&surface.id()) {
                return TraversalAction::SkipChildren;
            }
            let offset = if states.role == Some(SUBSURFACE_ROLE) {
                let mut subsurface = states.cached_state.get::<SubsurfaceCachedState>();
                let location = subsurface.current().location;
                (location.x, location.y)
            } else {
                (0, 0)
            };
            TraversalAction::DoChildren((parent_origin.0 + offset.0, parent_origin.1 + offset.1))
        },
        |surface, states, parent_origin| {
            if error.borrow().is_some() {
                return;
            }
            let Some(slot) = slots.get(&surface.id()).copied() else {
                return;
            };
            match map_surface_state(states, slot) {
                Ok(mapped) => {
                    let origin = (
                        parent_origin.0 + mapped.offset.0,
                        parent_origin.1 + mapped.offset.1,
                    );
                    let origin = (f64::from(origin.0), f64::from(origin.1));
                    let destination = (
                        f64::from(mapped.mapping.logical_size.0),
                        f64::from(mapped.mapping.logical_size.1),
                    );
                    nodes.push(SceneNode {
                        surface_id: slot.buffer_id,
                        buffer_id: slot.buffer_id,
                        origin,
                        destination,
                        source: (
                            mapped.mapping.offset,
                            (
                                mapped.mapping.scale.0 * f64::from(mapped.mapping.logical_size.0),
                                mapped.mapping.scale.1 * f64::from(mapped.mapping.logical_size.1),
                            ),
                        ),
                    });
                    hits.push(HitNode {
                        surface: surface.clone(),
                        origin,
                        destination,
                        input_region: mapped.input_region,
                    });
                }
                Err(mapping_error) => *error.borrow_mut() = Some(mapping_error),
            }
        },
        |_, _, _| error.borrow().is_none(),
    );
    error
        .into_inner()
        .map_or(Ok(SurfaceTreeSnapshot { nodes, hits }), Err)
}

impl<K: BrowserPageKey> State<K> {
    fn next_serial(&mut self) -> Serial {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        serial.into()
    }

    fn time(&self) -> u32 {
        monotonic_time_ms()
    }
    fn window_id_for_surface(&self, surface: &WlSurface) -> Option<K> {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        self.surface_windows.get(&root.id()).copied()
    }

    fn constrain_popup(&self, id: K, surface: &PopupSurface, positioner: PositionerState) {
        let size = self.windows[&id].size;
        let mut target =
            Rectangle::<i32, Logical>::from_size((size.0 as i32, size.1 as i32).into());
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(surface.clone()));
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_unconstrained_geometry(target);
            state.positioner = positioner;
        });
    }

    fn snapshot_shm(
        &self,
        buffer_id: u64,
        buffer: &wl_buffer::WlBuffer,
        max_bytes: usize,
    ) -> Result<ShmFrame> {
        const MAX_SURFACE_DIMENSION: u32 = 8192;

        with_buffer_contents(buffer, |pointer, pool_len, data| -> Result<ShmFrame> {
            let width = u32::try_from(data.width).context("negative SHM width")?;
            let height = u32::try_from(data.height).context("negative SHM height")?;
            let stride = usize::try_from(data.stride).context("negative SHM stride")?;
            let offset = usize::try_from(data.offset).context("negative SHM offset")?;
            if width == 0
                || height == 0
                || width > MAX_SURFACE_DIMENSION
                || height > MAX_SURFACE_DIMENSION
            {
                bail!("SHM dimensions are outside the supported bounds: {width}x{height}")
            }
            if !matches!(
                data.format,
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
            ) {
                bail!("unsupported SHM format {:?}", data.format)
            }
            let row_bytes = usize::try_from(width)?
                .checked_mul(4)
                .context("SHM row overflow")?;
            if stride < row_bytes {
                bail!("SHM stride {stride} is shorter than row {row_bytes}")
            }
            let pixel_bytes = row_bytes
                .checked_mul(usize::try_from(height)?)
                .context("SHM size overflow")?;
            if pixel_bytes > max_bytes {
                bail!("SHM surface exceeds {max_bytes} bytes")
            }
            let last_row = stride
                .checked_mul(usize::try_from(height - 1)?)
                .and_then(|bytes| offset.checked_add(bytes))
                .and_then(|start| start.checked_add(row_bytes))
                .context("SHM pool range overflow")?;
            if last_row > pool_len {
                bail!("SHM buffer exceeds its pool")
            }
            let mut pixels = vec![0_u8; pixel_bytes];
            for row in 0..usize::try_from(height)? {
                // The pool can change concurrently, so copy directly from the
                // validated raw range without creating a Rust slice into it.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pointer.add(offset + row * stride),
                        pixels.as_mut_ptr().add(row * row_bytes),
                        row_bytes,
                    );
                }
            }
            normalize_shm_pixels(data.format, &mut pixels);
            Ok(ShmFrame {
                id: buffer_id,
                width,
                height,
                pixels,
            })
        })?
    }

    fn update_surface_buffer(&mut self, id: K, surface: &WlSurface) -> Result<Option<u64>> {
        let diagnosing_handoff = {
            let window = &self.windows[&id];
            !window.pending_barriers.is_empty()
                || window.unbound_barrier.is_some()
                || window.acked_barrier > window.committed_barrier
        };
        if diagnosing_handoff {
            tracing::info!(surface = ?surface.id(), "browser handoff reading committed surface state");
        }
        let (assignment, buffer_delta, (acquire, mut release)) = with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let assignment = current.buffer.take();
            let buffer_delta = current.buffer_delta.take();
            let mut sync = states.cached_state.get::<DrmSyncobjCachedState>();
            let sync = sync.current();
            (
                assignment,
                buffer_delta,
                (sync.acquire_point.take(), sync.release_point.take()),
            )
        });
        if diagnosing_handoff {
            tracing::info!(
                surface = ?surface.id(),
                assignment = match &assignment {
                    Some(BufferAssignment::NewBuffer(_)) => "new-buffer",
                    Some(BufferAssignment::Removed) => "removed",
                    None => "unchanged",
                },
                has_acquire = acquire.is_some(),
                has_release = release.is_some(),
                "browser handoff read committed surface state"
            );
        }
        if buffer_delta.is_some_and(|delta| delta.x != 0 || delta.y != 0) {
            if let Some(release) = release.take() {
                let _ = release.signal();
            }
            if let Some(BufferAssignment::NewBuffer(buffer)) = assignment {
                buffer.release();
            }
            bail!("nonzero Wayland buffer offset is unsupported")
        }
        let Some(assignment) = assignment else {
            if let Some(release) = release {
                let _ = release.signal();
            }
            return Ok(None);
        };
        if matches!(assignment, BufferAssignment::Removed) {
            self.windows
                .get_mut(&id)
                .unwrap()
                .surface_slots
                .remove(&surface.id());
            if let Some(release) = release {
                let _ = release.signal();
            }
            return Ok(None);
        }
        let BufferAssignment::NewBuffer(buffer) = assignment else {
            unreachable!()
        };
        let is_root = self.windows[&id]
            .toplevel
            .as_ref()
            .is_some_and(|toplevel| toplevel.wl_surface() == surface);
        let buffer_id = self.next_buffer_id;
        self.next_buffer_id = self.next_buffer_id.wrapping_add(1).max(1);

        let result = (|| -> Result<_> {
            if let Ok(dmabuf) = get_dmabuf(&buffer) {
                if diagnosing_handoff {
                    tracing::info!(
                        surface = ?surface.id(),
                        buffer_id,
                        planes = dmabuf.num_planes(),
                        size = ?dmabuf.size(),
                        "browser handoff identified a DMA-BUF surface"
                    );
                }
                let acquire = acquire
                    .context("DMA-BUF commit omitted required explicit-sync acquire point")?;
                let release_point = release
                    .take()
                    .context("DMA-BUF commit omitted required explicit-sync release point")?;
                let size = dmabuf.size();
                let width = u32::try_from(size.w).context("invalid DMA-BUF width")?;
                let height = u32::try_from(size.h).context("invalid DMA-BUF height")?;
                let frame = dma_buf_frame(
                    buffer_id,
                    dmabuf,
                    buffer.clone(),
                    acquire,
                    release_point,
                    (id, self.commands.clone()),
                    diagnosing_handoff,
                )?;
                if diagnosing_handoff {
                    tracing::info!(
                        surface = ?surface.id(),
                        buffer_id,
                        width,
                        height,
                        "browser handoff prepared a DMA-BUF import"
                    );
                }
                Ok((
                    SurfaceSlot {
                        buffer_id,
                        width,
                        height,
                        shm_bytes: 0,
                    },
                    BufferImport::DmaBuf(frame),
                    true,
                ))
            } else if matches!(buffer_type(&buffer), Some(BufferType::Shm)) {
                if diagnosing_handoff {
                    tracing::info!(
                        surface = ?surface.id(),
                        buffer_id,
                        "browser handoff identified an SHM surface"
                    );
                }
                if acquire.is_some() {
                    bail!("Chromium SHM surface unexpectedly used explicit synchronization")
                }
                if is_root && !self.allow_root_shm {
                    bail!("Chromium root surface did not provide the required zero-copy DMA-BUF")
                }
                let retained_bytes = self.windows[&id]
                    .surface_slots
                    .iter()
                    .filter(|(object_id, _)| **object_id != surface.id())
                    .try_fold(0_usize, |total, (_, slot)| {
                        total.checked_add(slot.shm_bytes)
                    })
                    .context("Chromium SHM scene size overflow")?;
                let max_bytes = shm_surface_limit(is_root, self.allow_root_shm, retained_bytes)?;
                let frame = self.snapshot_shm(buffer_id, &buffer, max_bytes)?;
                let slot = SurfaceSlot {
                    buffer_id,
                    width: frame.width,
                    height: frame.height,
                    shm_bytes: frame.pixels.len(),
                };
                if let Some(release) = release.take() {
                    let _ = release.signal();
                }
                buffer.release();
                Ok((slot, BufferImport::Shm(frame), false))
            } else {
                bail!("Chromium committed an unsupported surface buffer")
            }
        })();

        match result {
            Ok((slot, import, is_dma)) => {
                let window = self.windows.get_mut(&id).expect("known window");
                window.surface_slots.insert(surface.id(), slot);
                window.pending_imports.insert(buffer_id, import);
                Ok(is_dma.then_some(buffer_id))
            }
            Err(error) => {
                tracing::error!(
                    surface = ?surface.id(),
                    buffer_id,
                    ?error,
                    "browser handoff failed to import a committed surface buffer"
                );
                if let Some(release) = release {
                    let _ = release.signal();
                }
                buffer.release();
                Err(error)
            }
        }
    }

    fn fail_window(&mut self, id: K, error: Arc<str>) {
        let window = self.windows.get_mut(&id).expect("known window");
        if window.terminal_failure {
            return;
        }
        window.terminal_failure = true;
        window.pending_imports.clear();
        window.events.send(BrowserEvent::Failed(error));
    }

    fn publish_scene(&mut self, id: K, barrier_anchor: bool) -> Result<()> {
        if self.windows[&id].terminal_failure {
            bail!("browser window is already terminally failed")
        }
        let root = self.windows[&id]
            .toplevel
            .as_ref()
            .context("surface tree has no toplevel")?
            .wl_surface()
            .clone();
        let diagnosing_handoff = {
            let window = &self.windows[&id];
            !window.pending_barriers.is_empty()
                || window.unbound_barrier.is_some()
                || window.acked_barrier > window.committed_barrier
        };
        if diagnosing_handoff {
            tracing::info!(
                barrier_anchor,
                "browser handoff began surface-tree reconciliation"
            );
        }

        // Smithay applies synchronized child state at its effectively-unsynchronized
        // ancestor. Reconcile every current slot only at that transaction anchor.
        let mut surfaces = Vec::new();
        with_surface_tree_upward(
            &root,
            (),
            |_, _, &()| TraversalAction::DoChildren(()),
            |surface, _, &()| surfaces.push(surface.clone()),
            |_, _, &()| true,
        );
        let toplevel_surface_count = surfaces.len();
        for (popup, _) in PopupManager::popups_for_surface(&root) {
            with_surface_tree_upward(
                popup.wl_surface(),
                (),
                |_, _, &()| TraversalAction::DoChildren(()),
                |surface, _, &()| surfaces.push(surface.clone()),
                |_, _, &()| true,
            );
        }
        if diagnosing_handoff {
            tracing::info!(
                surfaces = surfaces.len(),
                toplevel_surfaces = toplevel_surface_count,
                "browser handoff collected the surface tree"
            );
        }
        let mut new_toplevel_dma = std::collections::HashSet::new();
        for (index, surface) in surfaces.iter().enumerate() {
            if diagnosing_handoff {
                tracing::info!(
                    surface = ?surface.id(),
                    index,
                    is_toplevel_tree = index < toplevel_surface_count,
                    "browser handoff reconciling a surface"
                );
            }
            let dma_buffer = self.update_surface_buffer(id, surface)?;
            if diagnosing_handoff {
                tracing::info!(
                    surface = ?surface.id(),
                    index,
                    ?dma_buffer,
                    "browser handoff reconciled a surface"
                );
            }
            if index < toplevel_surface_count
                && let Some(buffer_id) = dma_buffer
            {
                new_toplevel_dma.insert(buffer_id);
            }
        }

        let geometry = with_states(&root, |states| {
            *states.cached_state.get::<XdgSurfaceCachedState>().current()
        })
        .geometry;
        let root_origin = geometry
            .map(|geometry| (-geometry.loc.x, -geometry.loc.y))
            .unwrap_or((0, 0));
        let slots = &self.windows[&id].surface_slots;
        let SurfaceTreeSnapshot {
            mut nodes,
            mut hits,
        } = snapshot_surface_tree(&root, root_origin, slots)?;
        let new_visible_toplevel_dma = nodes
            .iter()
            .any(|node| new_toplevel_dma.contains(&node.buffer_id));
        // Chromium may acknowledge a configure with a damage/state commit that
        // keeps the current wl_buffer attached. That effective post-ACK commit
        // is sufficient for handoff readiness as long as the atomic scene still
        // contains a validated DMA-BUF; requiring a fresh attachment wedges on
        // ordinary swapchain reuse.
        let visible_accepted_root = nodes.iter().any(|node| {
            slots.values().any(|slot| {
                slot.buffer_id == node.buffer_id && (slot.shm_bytes == 0 || self.allow_root_shm)
            })
        });

        // PopupManager yields topmost first; GPUI paints bottom-to-top.
        let mut popups = PopupManager::popups_for_surface(&root).collect::<Vec<_>>();
        popups.reverse();
        for (popup, offset) in popups {
            let geometry = popup.geometry();
            let origin = offset - geometry.loc;
            let popup = snapshot_surface_tree(popup.wl_surface(), (origin.x, origin.y), slots)?;
            nodes.extend(popup.nodes);
            hits.extend(popup.hits);
        }

        let attached = slots
            .values()
            .map(|slot| slot.buffer_id)
            .collect::<std::collections::HashSet<_>>();
        let shm_bytes = slots
            .values()
            .try_fold(0_usize, |total, slot| total.checked_add(slot.shm_bytes))
            .context("Chromium SHM scene size overflow")?;
        if shm_bytes > MAX_SCENE_SHM_BYTES {
            bail!("Chromium SHM scene exceeds {MAX_SCENE_SHM_BYTES} bytes")
        }
        let window = self.windows.get_mut(&id).expect("known window");
        let previous_barrier = window.committed_barrier;
        if diagnosing_handoff {
            tracing::info!(
                scene_id = window.next_scene_id,
                committed_barrier = window.committed_barrier,
                acked_barrier = window.acked_barrier,
                pending_configures = window.pending_barriers.len(),
                barrier_anchor,
                new_toplevel_dma = new_toplevel_dma.len(),
                new_visible_toplevel_dma,
                visible_accepted_root,
                surfaces = surfaces.len(),
                toplevel_surfaces = toplevel_surface_count,
                nodes = nodes.len(),
                attached = attached.len(),
                "browser handoff compositor scene diagnostic"
            );
        }
        window.committed_barrier = committed_barrier_after_scene(
            window.committed_barrier,
            window.acked_barrier,
            barrier_anchor,
            visible_accepted_root,
        );
        if window.committed_barrier != previous_barrier {
            tracing::info!(
                barrier = window.committed_barrier,
                scene_id = window.next_scene_id,
                "Chromium frame barrier reached a visible accepted scene"
            );
        }
        window
            .pending_imports
            .retain(|buffer_id, _| attached.contains(buffer_id));
        let imports = window
            .pending_imports
            .drain()
            .map(|(_, import)| import)
            .collect();
        let scene_id = window.next_scene_id;
        window.next_scene_id = window.next_scene_id.wrapping_add(1).max(1);
        let presentation_feedback = with_states(&root, |states| {
            std::mem::take(
                &mut states
                    .cached_state
                    .get::<PresentationFeedbackCachedState>()
                    .current()
                    .callbacks,
            )
        });
        // Hit nodes and GPUI use the same window-local coordinate space. The
        // root's XDG geometry offset is already represented by its node origin.
        let pointer_frame = PointerFrame;
        window.pointer_frames.insert(scene_id, pointer_frame);
        window.hit_scenes.insert(scene_id, hits);
        window
            .dma_frame_callbacks
            .insert(scene_id, drain_frame_callbacks(&surfaces));
        if window.presentation_passthrough {
            window
                .presentation_feedback
                .insert(scene_id, presentation_feedback);
        } else {
            for feedback in presentation_feedback {
                feedback.discarded();
            }
        }
        let produced_at = Instant::now();
        record_browser_timing(
            BrowserTimingKind::SceneProduced,
            scene_id,
            window.committed_barrier,
            None,
            None,
        );
        window.events.send(BrowserEvent::Scene(SceneUpdate {
            id: scene_id,
            barrier: window.committed_barrier,
            logical_size: window.size,
            imports,
            attached: attached.into_iter().collect(),
            nodes,
            produced_at,
        }));
        Ok(())
    }

    fn bind_toplevel(&mut self, id: K, root: ObjectId) {
        if self
            .windows
            .get(&id)
            .is_none_or(|window| window.toplevel.is_some())
        {
            return;
        }
        let Some((surface, _)) = self.unbound_toplevels.remove(&root) else {
            return;
        };
        if let Some(client) = surface.wl_surface().client()
            && let Some(client) = client.get_data::<ClientState>()
        {
            client
                .events
                .lock()
                .unwrap()
                .push(self.windows[&id].events.clone());
        }
        let (size, scale) = {
            let window = &self.windows[&id];
            (window.size, window.scale)
        };
        self.surface_windows.insert(root.clone(), id);
        self.output.enter(surface.wl_surface());
        with_states(surface.wl_surface(), |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
        surface.with_pending_state(|state| {
            state.size = Some((size.0 as i32, size.1 as i32).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        let serial = surface.send_configure();
        self.windows
            .get_mut(&id)
            .expect("pending window exists")
            .toplevel = Some(surface);
        track_unbound_barrier(
            self.windows.get_mut(&id).expect("pending window exists"),
            serial,
        );
        self.windows[&id].events.send(BrowserEvent::ToplevelReady);
        let keyboard = self.keyboard.clone();
        let serial = self.next_serial();
        let focus = self.windows[&id]
            .toplevel
            .as_ref()
            .map(|top| top.wl_surface().clone());
        keyboard.set_focus(self, focus, serial);
    }

    fn bind_unambiguous_toplevel(&mut self) {
        let roots = self.unbound_toplevels.keys().cloned().collect::<Vec<_>>();
        let windows = self
            .windows
            .iter()
            .filter(|(_, window)| window.toplevel.is_none())
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        if roots.len() > 1 || windows.len() > 1 {
            return;
        }
        let ([root], [id]) = (roots.as_slice(), windows.as_slice()) else {
            return;
        };
        self.bind_toplevel(*id, root.clone());
    }
}

impl<K: BrowserPageKey> BufferHandler for State<K> {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl<K: BrowserPageKey> CompositorHandler for State<K> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("known browser client")
            .compositor
    }
    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let Some(id) = self.window_id_for_surface(parent) else {
            return;
        };
        let scale = self.windows[&id].scale;
        with_states(surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
        if let Err(error) = self.publish_scene(id, false) {
            self.fail_window(
                id,
                format!("invalid Chromium surface tree after subsurface creation: {error:#}")
                    .into(),
            );
        }
    }
    fn commit(&mut self, surface: &WlSurface) {
        self.popup_manager.commit(surface);

        if let Some(id) = self.window_id_for_surface(surface)
            && self.windows[&id].terminal_failure
        {
            discard_surface_buffer(surface);
            complete_surface_callbacks(surface, self.time());
            return;
        }

        // A synchronized child's current state is applied by Smithay when its
        // effectively-unsynchronized ancestor commits. Taking it here would
        // split one Wayland transaction into unrelated GPUI frames.
        if is_sync_subsurface(surface) {
            if let Some(id) = self.window_id_for_surface(surface) {
                let window = &self.windows[&id];
                if !window.pending_barriers.is_empty()
                    || window.acked_barrier > window.committed_barrier
                {
                    tracing::info!(
                        surface = ?surface.id(),
                        acked_barrier = window.acked_barrier,
                        committed_barrier = window.committed_barrier,
                        pending_configures = window.pending_barriers.len(),
                        "browser handoff deferred a synchronized child commit"
                    );
                }
            }
            return;
        }

        let Some(id) = self.window_id_for_surface(surface) else {
            let (assignment, release) = with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let assignment = attributes.current().buffer.take();
                let mut sync = states.cached_state.get::<DrmSyncobjCachedState>();
                let release = sync.current().release_point.take();
                (assignment, release)
            });
            if let Some(release) = release {
                let _ = release.signal();
            }
            if let Some(BufferAssignment::NewBuffer(buffer)) = assignment {
                buffer.release();
            }
            complete_surface_callbacks(surface, self.time());
            return;
        };

        let mut transaction_root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&transaction_root) {
            transaction_root = parent;
        }
        let barrier_anchor = self.windows[&id]
            .toplevel
            .as_ref()
            .is_some_and(|toplevel| toplevel.wl_surface() == &transaction_root);
        let window = &self.windows[&id];
        if !window.pending_barriers.is_empty() || window.acked_barrier > window.committed_barrier {
            tracing::info!(
                surface = ?surface.id(),
                transaction_root = ?transaction_root.id(),
                barrier_anchor,
                acked_barrier = window.acked_barrier,
                committed_barrier = window.committed_barrier,
                pending_configures = window.pending_barriers.len(),
                "browser handoff processing an effective surface commit"
            );
        }
        if let Err(error) = self.publish_scene(id, barrier_anchor) {
            tracing::error!(
                ?error,
                "browser handoff failed to publish a surface-tree commit"
            );
            self.fail_window(
                id,
                format!("invalid Chromium surface tree: {error:#}").into(),
            );
        }
    }
    fn destroyed(&mut self, surface: &WlSurface) {
        let Some(id) = self.window_id_for_surface(surface) else {
            self.unbound_toplevels.remove(&surface.id());
            return;
        };
        let is_root = self
            .windows
            .get(&id)
            .and_then(|window| window.toplevel.as_ref())
            .is_some_and(|toplevel| toplevel.wl_surface() == surface);
        if !is_root {
            self.surface_windows.remove(&surface.id());
            self.windows
                .get_mut(&id)
                .expect("surface window exists")
                .surface_slots
                .remove(&surface.id());
            if get_role(surface) == Some(SUBSURFACE_ROLE)
                && let Err(error) = self.publish_scene(id, false)
            {
                self.fail_window(
                    id,
                    format!("invalid Chromium surface tree after removal: {error:#}").into(),
                );
            }
        }
        // XdgShellHandler::toplevel_destroyed owns root teardown. Keeping the
        // route until then ensures either Smithay destruction callback order
        // removes the WindowState exactly once.
    }
}

impl<K: BrowserPageKey> XdgShellHandler for State<K> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let root = surface.wl_surface().id();
        let created = Instant::now();
        self.unbound_toplevels
            .insert(root.clone(), (surface, created));
        schedule_toplevel_deadlines(self, root.clone(), created);
        self.bind_unambiguous_toplevel();
    }

    fn ack_configure(&mut self, surface: WlSurface, configure: Configure) {
        let Configure::Toplevel(configure) = configure else {
            return;
        };
        let Some(id) = self.window_id_for_surface(&surface) else {
            return;
        };
        let window = self.windows.get_mut(&id).expect("configured window exists");
        let previous_barrier = window.acked_barrier;
        let previous_pending = window.pending_barriers.len();
        let mut acknowledged = window.acked_barrier;
        window.pending_barriers.retain(|(serial, barrier)| {
            if *serial <= configure.serial {
                acknowledged = acknowledged.max(*barrier);
                false
            } else {
                true
            }
        });
        window.acked_barrier = acknowledged;
        if previous_pending > 0 {
            tracing::info!(
                serial = ?configure.serial,
                previous_barrier,
                acked_barrier = window.acked_barrier,
                pending_before = previous_pending,
                pending_after = window.pending_barriers.len(),
                "browser handoff received Chromium configure ACK"
            );
        }
    }

    // Chromium serializes xdg state changes: after set_fullscreen it will not
    // ack any further configure (nor submit frames) until one arrives carrying
    // the requested state, so ignoring these requests wedges the whole window.
    // The page fills the host view element, which is our whole "output".
    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        self.set_fullscreen(surface, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.set_fullscreen(surface, false);
    }

    // Denied, but the reply configure is mandatory to keep Chromium's state
    // machine moving; an unanswered request stalls it just like fullscreen.
    // Unbound toplevels get their answer from the bind-time initial configure
    // instead: replying before the client's initial commit would make this
    // the initial configure, with no size and no Activated state.
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if self.surface_windows.contains_key(&surface.wl_surface().id()) {
            let _ = surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if self.surface_windows.contains_key(&surface.wl_surface().id()) {
            let _ = surface.send_configure();
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let root = surface.wl_surface().id();
        self.unbound_toplevels.remove(&root);
        if let Some(id) = self.surface_windows.remove(&root)
            && let Some(w) = self.windows.remove(&id)
        {
            w.events.send(BrowserEvent::Closed);
        }
    }
    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let window_id = if let Some(parent) = surface.get_parent_surface()
            && let Some(id) = self.window_id_for_surface(&parent)
        {
            let object_id = surface.wl_surface().id();
            self.surface_windows.insert(object_id, id);
            Some(id)
        } else {
            None
        };
        let _ = self
            .popup_manager
            .track_popup(PopupKind::Xdg(surface.clone()));
        if let Some(window_id) = window_id {
            let scale = self.windows[&window_id].scale;
            with_states(surface.wl_surface(), |states| {
                with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(scale);
                });
            });
            self.constrain_popup(window_id, &surface, positioner);
        }
        let _ = surface.send_configure();
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        let object_id = surface.wl_surface().id();
        let window_id = self.surface_windows.remove(&object_id);
        if let Some(window_id) = window_id {
            self.windows
                .get_mut(&window_id)
                .expect("popup window exists")
                .surface_slots
                .remove(&object_id);
        }
        self.popup_manager.cleanup();
        if let Some(window_id) = window_id
            && let Err(error) = self.publish_scene(window_id, false)
        {
            self.fail_window(
                window_id,
                format!("invalid Chromium popup tree after removal: {error:#}").into(),
            );
        }
    }
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(window_id) = self.window_id_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(root) = self.windows[&window_id]
            .toplevel
            .as_ref()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            return;
        };
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Ok(mut grab) =
            self.popup_manager
                .grab_popup::<Self>(root, PopupKind::Xdg(surface), &seat, serial)
        else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let Some(window_id) = self.window_id_for_surface(surface.wl_surface()) else {
            return;
        };
        self.constrain_popup(window_id, &surface, positioner);
        surface.send_repositioned(token);
    }
}

impl<K: BrowserPageKey> XdgDecorationHandler for State<K> {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.set_server_side_decoration(toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        self.set_server_side_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.set_server_side_decoration(toplevel);
    }
}

impl<K: BrowserPageKey> State<K> {
    fn set_fullscreen(&mut self, toplevel: ToplevelSurface, fullscreen: bool) {
        let size = self
            .window_id_for_surface(toplevel.wl_surface())
            .map(|id| self.windows[&id].size);
        toplevel.with_pending_state(|state| {
            if fullscreen {
                state.states.set(xdg_toplevel::State::Fullscreen);
            } else {
                state.states.unset(xdg_toplevel::State::Fullscreen);
            }
            if let Some((width, height)) = size {
                state.size = Some((width as i32, height as i32).into());
            }
        });
        // An unbound toplevel gets the state from its bind-time initial
        // configure; replying here would send a premature initial configure.
        if self
            .surface_windows
            .contains_key(&toplevel.wl_surface().id())
        {
            let serial = toplevel.send_configure();
            tracing::info!(fullscreen, serial = ?serial, "granted Chromium fullscreen change");
        } else {
            tracing::info!(fullscreen, "queued Chromium fullscreen change until bind");
        }
    }

    fn set_server_side_decoration(&self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if self
            .surface_windows
            .contains_key(&toplevel.wl_surface().id())
        {
            toplevel.send_configure();
        }
    }
}

impl<K: BrowserPageKey> ShmHandler for State<K> {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl<K: BrowserPageKey> DmabufHandler for State<K> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let format = dmabuf.format();
        let supported = dmabuf.num_planes() == 1
            && self.dma_formats.iter().any(|&(fourcc, modifier)| {
                fourcc == format.code as u32 && modifier == u64::from(format.modifier)
            });
        if supported {
            let _ = notifier.successful::<State<K>>();
        } else {
            notifier.failed();
        }
    }
}
impl<K: BrowserPageKey> DrmSyncobjHandler for State<K> {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj.as_mut()
    }
}
impl<K: BrowserPageKey> OutputHandler for State<K> {}

impl<K: BrowserPageKey> FractionalScaleHandler for State<K> {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self
            .window_id_for_surface(&surface)
            .and_then(|id| self.windows.get(&id))
            .map_or(1.0, |window| window.scale);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl<K: BrowserPageKey> SeatHandler for State<K> {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let cursor = browser_cursor(image);
        for window in self.windows.values() {
            window.events.send(BrowserEvent::Cursor(cursor));
        }
    }
}

impl<K: BrowserPageKey> TabletSeatHandler for State<K> {}

struct ClientState {
    compositor: CompositorClientState,
    events: Arc<Mutex<Vec<BrowserEventSender>>>,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        let events = self.events.lock().unwrap().clone();
        for events in events {
            events.send(BrowserEvent::Failed(
                "Chromium disconnected from the embedded compositor".into(),
            ));
        }
    }
}

impl<K: BrowserPageKey> Dispatch<WlSubsurface, SubsurfaceUserData> for State<K> {
    fn request(
        state: &mut Self,
        client: &Client,
        subsurface: &WlSubsurface,
        request: wl_subsurface::Request,
        data: &SubsurfaceUserData,
        display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        <CompositorState as Dispatch<WlSubsurface, SubsurfaceUserData, Self>>::request(
            state, client, subsurface, request, data, display, data_init,
        );
    }

    fn destroyed(
        state: &mut Self,
        client_id: ClientId,
        subsurface: &WlSubsurface,
        data: &SubsurfaceUserData,
    ) {
        let surface = data.surface().clone();
        let window_id = state.window_id_for_surface(&surface);
        <CompositorState as Dispatch<WlSubsurface, SubsurfaceUserData, Self>>::destroyed(
            state, client_id, subsurface, data,
        );
        let Some(window_id) = window_id else {
            return;
        };
        // The wl_surface and its attached buffer outlive the role. Keep the
        // lease as hidden attached state so recreating the role can remap it,
        // but republish after Smithay removes it from the surface tree.
        if let Err(error) = state.publish_scene(window_id, false) {
            state.fail_window(
                window_id,
                format!("invalid Chromium surface tree after subsurface removal: {error:#}").into(),
            );
        }
    }
}

smithay::reexports::wayland_server::delegate_global_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_compositor::WlCompositor: ()
] => CompositorState);
smithay::reexports::wayland_server::delegate_global_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_subcompositor::WlSubcompositor: ()
] => CompositorState);
smithay::reexports::wayland_server::delegate_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_compositor::WlCompositor: ()
] => CompositorState);
delegate_presentation!(@<K: BrowserPageKey> State<K>);
smithay::reexports::wayland_server::delegate_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface:
        smithay::wayland::compositor::SurfaceUserData
] => CompositorState);
smithay::reexports::wayland_server::delegate_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_region::WlRegion:
        smithay::wayland::compositor::RegionUserData
] => CompositorState);
smithay::reexports::wayland_server::delegate_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_callback::WlCallback: ()
] => CompositorState);
smithay::reexports::wayland_server::delegate_dispatch!(@<K: BrowserPageKey> State<K>: [
    smithay::reexports::wayland_server::protocol::wl_subcompositor::WlSubcompositor: ()
] => CompositorState);
delegate_xdg_shell!(@<K: BrowserPageKey> State<K>);
delegate_xdg_decoration!(@<K: BrowserPageKey> State<K>);
delegate_shm!(@<K: BrowserPageKey> State<K>);
delegate_dmabuf!(@<K: BrowserPageKey> State<K>);
delegate_drm_syncobj!(@<K: BrowserPageKey> State<K>);
delegate_output!(@<K: BrowserPageKey> State<K>);
delegate_seat!(@<K: BrowserPageKey> State<K>);
delegate_fractional_scale!(@<K: BrowserPageKey> State<K>);
delegate_viewporter!(@<K: BrowserPageKey> State<K>);
delegate_pointer_gestures!(@<K: BrowserPageKey> State<K>);
delegate_cursor_shape!(@<K: BrowserPageKey> State<K>);

fn dma_buf_frame<K: BrowserPageKey>(
    id: u64,
    dmabuf: &Dmabuf,
    buffer: wl_buffer::WlBuffer,
    acquire: DrmSyncPoint,
    release: DrmSyncPoint,
    retirement: (K, channel::Sender<RuntimeCommand<K>>),
    diagnosing_handoff: bool,
) -> Result<DmaBufFrame> {
    let mut release = ReleasePointGuard(Some(release));
    if diagnosing_handoff {
        tracing::info!(
            buffer_id = id,
            "browser handoff began DMA-BUF frame preparation"
        );
    }
    let size = dmabuf.size();
    let format = dmabuf.format();
    if diagnosing_handoff {
        tracing::info!(
            buffer_id = id,
            size = ?size,
            fourcc = format.code as u32,
            modifier = u64::from(format.modifier),
            "browser handoff read DMA-BUF metadata"
        );
    }
    let mut planes = dmabuf
        .handles()
        .zip(dmabuf.strides())
        .zip(dmabuf.offsets())
        .map(|((fd, stride), offset)| {
            Ok(DmaBufPlane {
                fd: fd.try_clone_to_owned().context("duplicate DMA-BUF plane")?,
                stride,
                offset,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if planes.len() != dmabuf.num_planes() {
        bail!("DMA-BUF plane metadata is incomplete");
    }
    let first = (!planes.is_empty())
        .then(|| planes.remove(0))
        .context("DMA-BUF has no plane")?;
    if diagnosing_handoff {
        tracing::info!(
            buffer_id = id,
            "browser handoff duplicated the DMA-BUF plane"
        );
        tracing::info!(
            buffer_id = id,
            "browser handoff exporting the acquire sync file"
        );
    }
    let acquire_fence = acquire
        .export_sync_file()
        .context("export DMA-BUF acquire fence")?;
    if diagnosing_handoff {
        tracing::info!(
            buffer_id = id,
            "browser handoff exported the acquire sync file"
        );
    }
    let keep_alive = dmabuf.clone();
    let release = release.0.take().expect("release point available");
    let (window_id, commands) = retirement;
    Ok(DmaBufFrame {
        id,
        width: u32::try_from(size.w).context("invalid DMA-BUF width")?,
        height: u32::try_from(size.h).context("invalid DMA-BUF height")?,
        fourcc: format.code as u32,
        modifier: u64::from(format.modifier),
        stride: first.stride,
        offset: first.offset,
        y_inverted: dmabuf.y_inverted(),
        fd: first.fd,
        additional_planes: planes,
        acquire_fence,
        release: Some(Box::new(move || {
            let _keep_alive = keep_alive;
            if let Err(error) = release.signal() {
                tracing::error!(?error, "signal Chrome DMA-BUF release point");
            }
            buffer.release();
            let _ = commands.send(RuntimeCommand::Page(window_id, PageCommand::Retired(id)));
        })),
    })
}

struct ReleasePointGuard(Option<DrmSyncPoint>);

impl Drop for ReleasePointGuard {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.signal();
        }
    }
}

fn run<K: BrowserPageKey>(
    commands: channel::Channel<RuntimeCommand<K>>,
    ready: mpsc::SyncSender<Result<OsString>>,
    render: BrowserRenderConfig,
    sender: channel::Sender<RuntimeCommand<K>>,
) -> Result<()> {
    let event_loop: EventLoop<'static, State<K>> =
        EventLoop::try_new().context("create browser compositor event loop")?;
    let loop_handle = event_loop.handle();
    let display: Display<State<K>> = Display::new().context("create private Wayland display")?;
    let dh = display.handle();
    let listener =
        ListeningSocket::bind_auto("rho-chrome", 1..1000).context("bind private Wayland socket")?;
    let socket = listener
        .socket_name()
        .context("private Wayland socket has no name")?
        .to_owned();
    let output = Output::new(
        "rho-browser".into(),
        PhysicalProperties {
            size: (300, 200).into(),
            subpixel: Subpixel::Unknown,
            make: "Rho".into(),
            model: "Embedded browser".into(),
        },
    );
    output.create_global::<State<K>>(&dh);
    configure_output(&output, (1280, 720));
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "rho-browser");
    let keyboard = seat
        .add_keyboard(Default::default(), 600, 25)
        .context("create embedded Chrome keyboard")?;
    let pointer = seat.add_pointer();
    let mut dmabuf = DmabufState::new();
    let (global, syncobj, formats, allow_root_shm) = match render {
        BrowserRenderConfig::DmaBuf(config) => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&config.render_node)
                .with_context(|| {
                    format!("open DRM render node {}", config.render_node.display())
                })?;
            let drm = DrmDeviceFd::new(OwnedFd::from(file).into());
            if !supports_syncobj_eventfd(&drm) {
                bail!(
                    "GPU does not support explicit-sync eventfd required for zero-copy browser frames"
                );
            }
            let fs = config
                .formats
                .iter()
                .filter_map(|&(fourcc, modifier)| {
                    Some(Format {
                        code: Fourcc::try_from(fourcc).ok()?,
                        modifier: Modifier::from(modifier),
                    })
                })
                .collect::<Vec<_>>();
            if fs.is_empty() {
                bail!("GPUI did not provide any importable DMA-BUF formats");
            }
            let feedback = smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(
                config.device_id as libc::dev_t,
                fs,
            )
            .build()
            .context("build DMA-BUF feedback")?;
            let global = dmabuf.create_global_with_default_feedback::<State<K>>(&dh, &feedback);
            (
                Some(global),
                Some(DrmSyncobjState::new::<State<K>>(&dh, drm)),
                Arc::clone(&config.formats),
                false,
            )
        }
        BrowserRenderConfig::SoftwareShmQa => (None, None, Arc::from([]), true),
    };
    let mut state = State {
        loop_handle,
        display_handle: dh.clone(),
        compositor: CompositorState::new::<State<K>>(&dh),
        shell: XdgShellState::new::<State<K>>(&dh),
        _decoration: XdgDecorationState::new::<State<K>>(&dh),
        shm: ShmState::new::<State<K>>(&dh, vec![]),
        dmabuf,
        _dmabuf_global: global,
        syncobj,
        dma_formats: formats,
        allow_root_shm,
        output,
        _fractional_scale: FractionalScaleManagerState::new::<State<K>>(&dh),
        _viewporter: ViewporterState::new::<State<K>>(&dh),
        _pointer_gestures: PointerGesturesState::new::<State<K>>(&dh),
        _cursor_shape: CursorShapeManagerState::new::<State<K>>(&dh),
        presentation: None,
        seat_state,
        _seat: seat,
        keyboard,
        pointer,
        serial: 1,
        windows: HashMap::new(),
        unbound_toplevels: HashMap::new(),
        surface_windows: HashMap::new(),
        next_buffer_id: 1,
        commands: sender,
        popup_manager: PopupManager::default(),
    };
    let _ = ready.send(Ok(socket));
    let result = service_loop(event_loop, display, listener, &mut state, commands);
    if let Err(error) = &result {
        let message: Arc<str> = format!("browser compositor failed: {error:#}").into();
        for window in state.windows.values() {
            window.events.send(BrowserEvent::Failed(message.clone()));
        }
    }
    result
}
fn service_loop<K: BrowserPageKey>(
    mut event_loop: EventLoop<'static, State<K>>,
    display: Display<State<K>>,
    listener: ListeningSocket,
    state: &mut State<K>,
    commands: channel::Channel<RuntimeCommand<K>>,
) -> Result<()> {
    let handle = event_loop.handle();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, PollMode::Level),
            |_, listener, state| {
                while let Some(stream) = unsafe { listener.get_mut() }.accept()? {
                    state.display_handle.insert_client(
                        stream,
                        Arc::new(ClientState {
                            compositor: CompositorClientState::default(),
                            events: Arc::new(Mutex::new(Vec::new())),
                        }),
                    )?;
                }
                Ok(PostAction::Continue)
            },
        )
        .context("register Chrome Wayland listener")?;
    handle
        .insert_source(
            Generic::new(display, Interest::READ, PollMode::Level),
            |_, display, state| {
                unsafe { display.get_mut() }.dispatch_clients(state)?;
                state.popup_manager.cleanup();
                state.bind_unambiguous_toplevel();
                unsafe { display.get_mut() }.flush_clients()?;
                Ok(PostAction::Continue)
            },
        )
        .context("register Chrome Wayland display")?;
    let signal = event_loop.get_signal();
    handle
        .insert_source(commands, move |event, _, state| {
            match event {
                ChannelEvent::Msg(command) => {
                    if handle_runtime_command(state, command) {
                        signal.stop();
                    }
                }
                ChannelEvent::Closed => signal.stop(),
            }
            let _ = state.display_handle.flush_clients();
        })
        .map_err(|_| anyhow::anyhow!("register browser command channel"))?;
    handle
        .insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            |_, _, state: &mut State<K>| {
                // Liveness floor, mirroring niri's fallback frame-callback
                // timer: consumption acknowledgements normally complete
                // callbacks within milliseconds, so anything still pending a
                // tick later has lost its consumer and would wedge Chromium's
                // frame clock.
                for window in state.windows.values_mut() {
                    if let Some(pending) = window.dma_frame_callbacks.keys().copied().max() {
                        send_frame_callbacks(
                            window,
                            monotonic_time_ms(),
                            pending,
                            BrowserTimingKind::FallbackFrameCallbackSent,
                        );
                    }
                }
                let _ = state.display_handle.flush_clients();
                TimeoutAction::ToDuration(Duration::from_secs(1))
            },
        )
        .map_err(|_| anyhow::anyhow!("register frame-callback fallback timer"))?;
    event_loop
        .run(None, state, |_| {})
        .context("run browser compositor event loop")
}

fn handle_runtime_command<K: BrowserPageKey>(
    state: &mut State<K>,
    command: RuntimeCommand<K>,
) -> bool {
    match command {
        RuntimeCommand::Open {
            id,
            session_generation,
            size,
            events,
        } => {
            state.windows.insert(
                id,
                WindowState {
                    session_generation,
                    toplevel: None,
                    size,
                    scale: 1.0,
                    events,
                    dma_frame_callbacks: HashMap::new(),
                    presentation_feedback: HashMap::new(),
                    presentation_passthrough: false,
                    last_host_presentation_scene: None,
                    last_host_frame_scene: None,
                    pointer_frames: HashMap::new(),
                    hit_scenes: HashMap::new(),
                    pointer_location: (0.0, 0.0),
                    pending_input: VecDeque::new(),
                    input_freeze: None,
                    active_finger_axes: (false, false),
                    last_finger_axis_time: 0,
                    active_buttons: HashSet::new(),
                    active_keys: HashSet::new(),
                    pinch_active: false,
                    surface_slots: HashMap::new(),
                    pending_imports: HashMap::new(),
                    next_scene_id: 1,
                    unbound_barrier: None,
                    pending_barriers: Vec::new(),
                    acked_barrier: 0,
                    committed_barrier: 0,
                    terminal_failure: false,
                    opened_at: Instant::now(),
                },
            );
            schedule_window_expiry(state, id);
            false
        }
        RuntimeCommand::Page(id, command) => {
            handle_page_command(state, id, command);
            false
        }
        RuntimeCommand::Shutdown => true,
    }
}

fn schedule_window_expiry<K: BrowserPageKey>(state: &State<K>, id: K) {
    let opened_at = state.windows[&id].opened_at;
    let _ = state.loop_handle.insert_source(
        Timer::from_duration(Duration::from_secs(10)),
        move |_, _, state| {
            let still_pending = state
                .windows
                .get(&id)
                .is_some_and(|window| window.opened_at == opened_at && window.toplevel.is_none());
            if !still_pending {
                return TimeoutAction::Drop;
            }
            if let Some(window) = state.windows.remove(&id) {
                window.events.send(BrowserEvent::Failed(
                    "Chromium did not provide an unambiguous top-level window".into(),
                ));
            }
            let _ = state.display_handle.flush_clients();
            TimeoutAction::Drop
        },
    );
}

fn schedule_toplevel_deadlines<K: BrowserPageKey>(
    state: &State<K>,
    root: ObjectId,
    created: Instant,
) {
    let _ = state.loop_handle.insert_source(
        Timer::from_duration(Duration::from_secs(10)),
        move |_, _, state| {
            let should_close = state
                .unbound_toplevels
                .get(&root)
                .is_some_and(|(_, current)| *current == created);
            if should_close && let Some((surface, _)) = state.unbound_toplevels.remove(&root) {
                surface.send_close();
                let _ = state.display_handle.flush_clients();
            }
            TimeoutAction::Drop
        },
    );
}

fn handle_page_command<K: BrowserPageKey>(state: &mut State<K>, id: K, c: PageCommand) {
    match c {
        PageCommand::Resize(w, h, scale) => resize(state, id, w, h, scale),
        PageCommand::FrameBarrier(barrier, w, h, scale) => {
            frame_barrier(state, id, barrier, w, h, scale)
        }
        PageCommand::Presented { scene_id, barrier } => {
            if let Some(w) = state.windows.get_mut(&id) {
                record_browser_timing(
                    BrowserTimingKind::FrameAcknowledged,
                    scene_id,
                    barrier,
                    None,
                    None,
                );
                prune_pointer_scenes(&mut w.pointer_frames, &mut w.hit_scenes, scene_id);
                // The host consumed this scene, so Chromium may render the
                // next one. Passthrough submissions also complete callbacks
                // through the relayed outer frame event; whichever arrives
                // first drains the entry.
                send_frame_callbacks(
                    w,
                    monotonic_time_ms(),
                    scene_id,
                    BrowserTimingKind::FrameCallbackSent,
                );
            }
        }
        PageCommand::EnablePresentationPassthrough { session_generation } => {
            let Some(window) = state
                .windows
                .get_mut(&id)
                .filter(|window| window.session_generation == session_generation)
            else {
                return;
            };
            window.presentation_passthrough = true;
            if state.presentation.is_none() {
                state.presentation = Some(PresentationState::new::<State<K>>(
                    &state.display_handle,
                    libc::CLOCK_MONOTONIC as u32,
                ));
            }
        }
        PageCommand::DisablePresentationPassthrough { session_generation } => {
            let scene = {
                let Some(window) = state
                    .windows
                    .get_mut(&id)
                    .filter(|window| window.session_generation == session_generation)
                else {
                    return;
                };
                disable_window_presentation_passthrough(window)
            };
            if !state
                .windows
                .values()
                .any(|window| window.presentation_passthrough)
                && let Some(presentation) = state.presentation.take()
            {
                state
                    .display_handle
                    .disable_global::<State<K>>(presentation.global());
            }
            // Complete anything the passthrough path left pending; the paint
            // acknowledgements resume completing callbacks from here.
            if let Some(scene_id) = scene
                && let Some(window) = state.windows.get_mut(&id)
            {
                send_frame_callbacks(
                    window,
                    monotonic_time_ms(),
                    scene_id,
                    BrowserTimingKind::FallbackFrameCallbackSent,
                );
            }
        }
        PageCommand::HostPresentation {
            session_generation,
            scene_id,
            presentation,
        } => {
            let current_generation = state
                .windows
                .get(&id)
                .map(|window| window.session_generation);
            if current_generation == Some(session_generation) {
                relay_host_presentation(state, id, scene_id, presentation);
            }
        }
        PageCommand::HostFrame {
            session_generation,
            scene_id,
            callback_time,
        } => {
            let Some(window) = state
                .windows
                .get_mut(&id)
                .filter(|window| window.session_generation == session_generation)
            else {
                return;
            };
            if accept_host_scene(
                window.presentation_passthrough,
                &mut window.last_host_frame_scene,
                window.next_scene_id,
                scene_id,
            ) {
                send_frame_callbacks(
                    window,
                    callback_time,
                    scene_id,
                    BrowserTimingKind::HostFrameCallbackSent,
                );
            }
        }
        PageCommand::Retired(commit) => {
            let removed_attached = state.windows.get_mut(&id).is_some_and(|window| {
                let before = window.surface_slots.len();
                window
                    .surface_slots
                    .retain(|_, slot| slot.buffer_id != commit);
                before != window.surface_slots.len()
            });
            if removed_attached && let Err(error) = state.publish_scene(id, false) {
                state.fail_window(
                    id,
                    format!("invalid Chromium surface tree after buffer retirement: {error:#}")
                        .into(),
                );
            }
            if let Some(window) = state.windows.get(&id) {
                window.events.send(BrowserEvent::FrameRetired(commit));
            }
        }
        PageCommand::PointerMotion { commit_id, x, y } => {
            queue_pointer_motion(state, id, commit_id, x, y)
        }
        PageCommand::PointerLeave => queue_pointer_leave(state, id),
        PageCommand::PointerButton { button, pressed } => {
            queue_input(state, id, OrderedInput::PointerButton { button, pressed })
        }
        PageCommand::PointerAxis(frame) => queue_input(state, id, OrderedInput::PointerAxis(frame)),
        PageCommand::Pinch(gesture) => queue_input(state, id, OrderedInput::Pinch(gesture)),
        PageCommand::Key { keycode, pressed } => {
            queue_input(state, id, OrderedInput::Key { keycode, pressed })
        }
        PageCommand::InputBarrier(generation, acknowledge) => {
            let Some(window) = state.windows.get_mut(&id) else {
                return;
            };
            window.input_freeze = Some(generation);
            window
                .pending_input
                .push_back(OrderedInput::InputBarrier(acknowledge));
            schedule_input_delivery(state, id);
        }
        PageCommand::UnfreezeInput(generation) => {
            if let Some(window) = state.windows.get_mut(&id) {
                clear_input_freeze(&mut window.input_freeze, generation);
            }
        }
        PageCommand::Close => {
            if let Some(window) = state.windows.get_mut(&id) {
                window.pending_input.clear();
            }
            if let Some(t) = state.windows.get(&id).and_then(|w| w.toplevel.as_ref()) {
                t.send_close()
            } else {
                state.windows.remove(&id);
            }
        }
    }
}

fn configure_output(output: &Output, size: (u32, u32)) {
    let mode = Mode {
        size: (size.0 as i32, size.1 as i32).into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
}

fn resize<K: BrowserPageKey>(state: &mut State<K>, id: K, width: u32, height: u32, scale: f64) {
    let Some(w) = state.windows.get_mut(&id) else {
        return;
    };
    if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let changed = w.size != (width, height) || w.scale != scale;
    if !changed {
        return;
    }
    w.size = (width, height);
    w.scale = scale;
    if let Some(t) = &w.toplevel {
        with_states(t.wl_surface(), |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
        t.with_pending_state(|p| p.size = Some((width as i32, height as i32).into()));
        t.send_configure();
    }
}

fn frame_barrier<K: BrowserPageKey>(
    state: &mut State<K>,
    id: K,
    barrier: u64,
    width: u32,
    height: u32,
    scale: f64,
) {
    let Some(window) = state.windows.get_mut(&id) else {
        tracing::info!(
            barrier,
            "browser handoff dropped barrier for missing window"
        );
        return;
    };
    if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
        tracing::info!(
            barrier,
            width,
            height,
            scale,
            "browser handoff rejected invalid barrier"
        );
        return;
    }
    window.size = (width, height);
    window.scale = scale;
    let Some(toplevel) = &window.toplevel else {
        window.unbound_barrier = Some(barrier);
        tracing::info!(
            barrier,
            width,
            height,
            scale,
            "browser handoff queued barrier before toplevel binding"
        );
        return;
    };
    with_states(toplevel.wl_surface(), |states| {
        with_fractional_scale(states, |fractional| {
            fractional.set_preferred_scale(scale);
        });
    });
    toplevel
        .with_pending_state(|pending| pending.size = Some((width as i32, height as i32).into()));
    let serial = toplevel.send_configure();
    window.pending_barriers.push((serial, barrier));
    tracing::info!(
        barrier,
        serial = ?serial,
        width,
        height,
        scale,
        pending_configures = window.pending_barriers.len(),
        "browser handoff sent Chromium configure"
    );
}

fn track_unbound_barrier(window: &mut WindowState, serial: Serial) {
    if let Some(barrier) = window.unbound_barrier.take() {
        window.pending_barriers.push((serial, barrier));
    }
}

fn pointer_target(
    hit_scenes: &HashMap<u64, Vec<HitNode>>,
    scene_id: u64,
    location: (f64, f64),
) -> Option<(WlSurface, (f64, f64))> {
    for hit in hit_scenes.get(&scene_id)?.iter().rev() {
        let local = (location.0 - hit.origin.0, location.1 - hit.origin.1);
        if local.0 < 0.0
            || local.1 < 0.0
            || local.0 >= hit.destination.0
            || local.1 >= hit.destination.1
        {
            continue;
        }
        if hit.input_region.as_ref().is_some_and(|region| {
            !region.contains((local.0.floor() as i32, local.1.floor() as i32))
        }) {
            continue;
        }
        return Some((hit.surface.clone(), hit.origin));
    }
    None
}

fn prune_pointer_scenes(
    pointer_frames: &mut HashMap<u64, PointerFrame>,
    hit_scenes: &mut HashMap<u64, Vec<HitNode>>,
    presented_scene_id: u64,
) {
    pointer_frames.retain(|scene, _| *scene >= presented_scene_id);
    hit_scenes.retain(|scene, _| *scene >= presented_scene_id);
}

fn resolve_pointer_motion(
    pointer_frames: &HashMap<u64, PointerFrame>,
    hit_scenes: &HashMap<u64, Vec<HitNode>>,
    commit_id: u64,
    x: f64,
    y: f64,
) -> Option<ResolvedPointerMotion> {
    let frame = pointer_frames.get(&commit_id).copied()?;
    let (x, y) = mapped_pointer(frame, x, y);
    Some(ResolvedPointerMotion {
        location: (x, y),
        target: pointer_target(hit_scenes, commit_id, (x, y)),
    })
}

fn deliver_pointer_motion<K: BrowserPageKey>(
    state: &mut State<K>,
    id: K,
    motion: ResolvedPointerMotion,
) {
    let (x, y) = motion.location;
    if let Some(window) = state.windows.get_mut(&id) {
        window.pointer_location = (x, y);
    } else {
        return;
    }
    let event = MotionEvent {
        location: (x, y).into(),
        serial: state.next_serial(),
        time: state.time(),
    };
    let pointer = state.pointer.clone();
    pointer.motion(
        state,
        motion
            .target
            .map(|(surface, origin)| (surface, origin.into())),
        &event,
    );
    pointer.frame(state);
}

fn mapped_pointer(_frame: PointerFrame, x: f64, y: f64) -> (f64, f64) {
    (x.max(0.0), y.max(0.0))
}
fn pointer_button<K: BrowserPageKey>(state: &mut State<K>, id: K, button: u32, pressed: bool) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    if !track_binary_input(&mut window.active_buttons, button, pressed) {
        return;
    }
    let pointer = state.pointer.clone();
    let serial = state.next_serial();
    let time = state.time();
    pointer.button(
        state,
        &ButtonEvent {
            serial,
            time,
            button,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        },
    );
    pointer.frame(state);
}

fn track_binary_input(active: &mut HashSet<u32>, code: u32, pressed: bool) -> bool {
    if pressed {
        active.insert(code);
        true
    } else {
        active.remove(&code)
    }
}

fn active_release_codes(active: &HashSet<u32>) -> Vec<u32> {
    active.iter().copied().collect()
}

fn queue_input<K: BrowserPageKey>(state: &mut State<K>, id: K, input: OrderedInput) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    if !admit_input(
        window.input_freeze.is_some(),
        &mut window.pending_input,
        input,
    ) {
        return;
    }
    schedule_input_delivery(state, id);
}

fn admit_input(frozen: bool, pending: &mut VecDeque<OrderedInput>, input: OrderedInput) -> bool {
    if frozen {
        return false;
    }
    pending.push_back(input);
    true
}

fn clear_input_freeze(active: &mut Option<u64>, generation: u64) -> bool {
    if *active != Some(generation) {
        return false;
    }
    *active = None;
    true
}

fn queue_pointer_motion<K: BrowserPageKey>(
    state: &mut State<K>,
    id: K,
    commit_id: u64,
    x: f64,
    y: f64,
) {
    let Some(window) = state.windows.get(&id) else {
        return;
    };
    let Some(motion) =
        resolve_pointer_motion(&window.pointer_frames, &window.hit_scenes, commit_id, x, y)
    else {
        return;
    };
    queue_input(state, id, OrderedInput::PointerMotion(motion));
}

fn queue_pointer_leave<K: BrowserPageKey>(state: &mut State<K>, id: K) {
    let Some(window) = state.windows.get(&id) else {
        return;
    };
    let motion = pointer_leave_motion(window.pointer_location);
    queue_input(state, id, OrderedInput::PointerMotion(motion));
}

fn pointer_leave_motion(location: (f64, f64)) -> ResolvedPointerMotion {
    ResolvedPointerMotion {
        location,
        target: None,
    }
}

fn dequeue_ready_input(pending: &mut VecDeque<OrderedInput>) -> Option<OrderedInput> {
    pending.pop_front()
}

fn schedule_input_delivery<K: BrowserPageKey>(state: &mut State<K>, id: K) {
    loop {
        let input = state
            .windows
            .get_mut(&id)
            .and_then(|window| dequeue_ready_input(&mut window.pending_input));
        let Some(input) = input else { return };
        deliver_input(state, id, input);
    }
}

fn deliver_input<K: BrowserPageKey>(state: &mut State<K>, id: K, input: OrderedInput) {
    match input {
        OrderedInput::PointerMotion(motion) => deliver_pointer_motion(state, id, motion),
        OrderedInput::PointerButton { button, pressed } => {
            pointer_button(state, id, button, pressed)
        }
        OrderedInput::PointerAxis(frame) => deliver_pointer_axis(state, id, frame),
        OrderedInput::Pinch(gesture) => pointer_pinch(state, id, gesture),
        OrderedInput::Key { keycode, pressed } => keyboard_key(state, id, keycode, pressed),
        OrderedInput::InputBarrier(acknowledge) => {
            release_active_input(state, id);
            let _ = state.display_handle.flush_clients();
            let _ = acknowledge.try_send(());
        }
    }
}

fn release_active_input<K: BrowserPageKey>(state: &mut State<K>, id: K) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    let buttons = active_release_codes(&window.active_buttons);
    let keys = active_release_codes(&window.active_keys);
    let finger_axes = window.active_finger_axes;
    let finger_time = window.last_finger_axis_time;
    let pinch_active = window.pinch_active;

    for keycode in keys {
        keyboard_key(state, id, keycode, false);
    }
    for button in buttons {
        pointer_button(state, id, button, false);
    }
    if finger_axes != (false, false) {
        deliver_pointer_axis(
            state,
            id,
            PointerAxisFrame {
                time: finger_time,
                source: PointerAxisSource::Finger,
                value: (0.0, 0.0),
                v120: (None, None),
                stop: finger_axes,
                relative_direction: (
                    PointerAxisDirection::Identical,
                    PointerAxisDirection::Identical,
                ),
            },
        );
    }
    if pinch_active {
        pointer_pinch(state, id, PinchGesture::End { cancelled: true });
    }
}

fn deliver_pointer_axis<K: BrowserPageKey>(
    state: &mut State<K>,
    id: K,
    mut event: PointerAxisFrame,
) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    if event.source == PointerAxisSource::Finger {
        event.stop = filter_finger_stops(window.active_finger_axes, event.value, event.stop);
        if event.value == (0.0, 0.0) && event.v120 == (None, None) && event.stop == (false, false) {
            return;
        }
        window.last_finger_axis_time = event.time;
        if event.value.0 != 0.0 {
            window.active_finger_axes.0 = true;
        }
        if event.value.1 != 0.0 {
            window.active_finger_axes.1 = true;
        }
        if event.stop.0 {
            window.active_finger_axes.0 = false;
        }
        if event.stop.1 {
            window.active_finger_axes.1 = false;
        }
        if window.active_finger_axes == (false, false) {}
    }
    let pointer = state.pointer.clone();
    let mut frame = AxisFrame::new(event.time).source(match event.source {
        PointerAxisSource::Finger => AxisSource::Finger,
        PointerAxisSource::Continuous => AxisSource::Continuous,
        PointerAxisSource::Wheel => AxisSource::Wheel,
        PointerAxisSource::WheelTilt => AxisSource::WheelTilt,
    });
    let direction = |direction| match direction {
        PointerAxisDirection::Identical => {
            smithay::backend::input::AxisRelativeDirection::Identical
        }
        PointerAxisDirection::Inverted => smithay::backend::input::AxisRelativeDirection::Inverted,
    };
    if event.value.0 != 0.0 {
        frame = frame
            .relative_direction(Axis::Horizontal, direction(event.relative_direction.0))
            .value(Axis::Horizontal, event.value.0);
    }
    if event.value.1 != 0.0 {
        frame = frame
            .relative_direction(Axis::Vertical, direction(event.relative_direction.1))
            .value(Axis::Vertical, event.value.1);
    }
    if let Some(v120) = event.v120.0 {
        frame = frame.v120(Axis::Horizontal, v120);
    }
    if let Some(v120) = event.v120.1 {
        frame = frame.v120(Axis::Vertical, v120);
    }
    if event.stop.0 {
        frame = frame.stop(Axis::Horizontal);
    }
    if event.stop.1 {
        frame = frame.stop(Axis::Vertical);
    }
    pointer.axis(state, frame);
    pointer.frame(state);
}

fn filter_finger_stops(
    active: (bool, bool),
    value: (f64, f64),
    requested: (bool, bool),
) -> (bool, bool) {
    (
        requested.0 && (active.0 || value.0 != 0.0),
        requested.1 && (active.1 || value.1 != 0.0),
    )
}

fn pointer_pinch<K: BrowserPageKey>(state: &mut State<K>, id: K, event: PinchGesture) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    if !track_pinch(&mut window.pinch_active, event) {
        return;
    }
    let pointer = state.pointer.clone();
    match event {
        PinchGesture::Begin { fingers } => {
            let serial = state.next_serial();
            pointer.gesture_pinch_begin(
                state,
                &GesturePinchBeginEvent {
                    serial,
                    time: state.time(),
                    fingers,
                },
            );
        }
        PinchGesture::Update {
            delta,
            scale,
            rotation,
        } => {
            pointer.gesture_pinch_update(
                state,
                &GesturePinchUpdateEvent {
                    time: state.time(),
                    delta: delta.into(),
                    scale,
                    rotation,
                },
            );
        }
        PinchGesture::End { cancelled } => {
            let serial = state.next_serial();
            pointer.gesture_pinch_end(
                state,
                &GesturePinchEndEvent {
                    serial,
                    time: state.time(),
                    cancelled,
                },
            );
        }
    }
}

fn track_pinch(active: &mut bool, event: PinchGesture) -> bool {
    match event {
        PinchGesture::Begin { .. } => {
            *active = true;
            true
        }
        PinchGesture::Update { .. } => *active,
        PinchGesture::End { .. } => std::mem::replace(active, false),
    }
}

fn keyboard_key<K: BrowserPageKey>(state: &mut State<K>, id: K, keycode: u32, pressed: bool) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    if window.toplevel.is_none() {
        return;
    }
    if !track_binary_input(&mut window.active_keys, keycode, pressed) {
        return;
    }
    let keyboard = state.keyboard.clone();
    let serial = state.next_serial();
    let time = state.time();
    keyboard.input::<(), _>(
        state,
        Keycode::from(xkb_keycode(keycode)),
        if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        },
        serial,
        time,
        |_, _, _| FilterResult::Forward,
    );
}

fn xkb_keycode(evdev_keycode: u32) -> u32 {
    evdev_keycode + 8
}

fn complete_surface_callbacks(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surface, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

fn drain_frame_callbacks(surfaces: &[WlSurface]) -> Vec<wl_callback::WlCallback> {
    let mut callbacks = Vec::new();
    for surface in surfaces {
        with_states(surface, |states| {
            callbacks.append(
                &mut states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks,
            );
        });
    }
    callbacks
}

fn presentation_kind(flags: u32) -> wp_presentation_feedback::Kind {
    let mut kind = wp_presentation_feedback::Kind::empty();
    for (bit, value) in [
        (1, wp_presentation_feedback::Kind::Vsync),
        (2, wp_presentation_feedback::Kind::HwClock),
        (4, wp_presentation_feedback::Kind::HwCompletion),
        (8, wp_presentation_feedback::Kind::ZeroCopy),
    ] {
        if flags & bit != 0 {
            kind |= value;
        }
    }
    kind
}

fn accept_host_scene(
    passthrough: bool,
    last_scene: &mut Option<u64>,
    next_scene: u64,
    scene: u64,
) -> bool {
    if !passthrough
        || scene == 0
        || scene >= next_scene
        || last_scene.is_some_and(|last| scene <= last)
    {
        return false;
    }
    *last_scene = Some(scene);
    true
}

fn relay_host_presentation<K: BrowserPageKey>(
    state: &mut State<K>,
    id: K,
    scene_id: u64,
    presentation: HostPresentation,
) {
    let Some(window) = state.windows.get_mut(&id) else {
        return;
    };
    if !accept_host_scene(
        window.presentation_passthrough,
        &mut window.last_host_presentation_scene,
        window.next_scene_id,
        scene_id,
    ) {
        return;
    }

    let feedback_scenes = window
        .presentation_feedback
        .keys()
        .copied()
        .filter(|pending| *pending <= scene_id)
        .collect::<Vec<_>>();
    for pending_scene in feedback_scenes {
        let feedback = window
            .presentation_feedback
            .remove(&pending_scene)
            .expect("known presentation feedback scene");
        for callback in feedback {
            match presentation {
                HostPresentation::Presented {
                    timestamp,
                    refresh,
                    sequence,
                    flags,
                } if pending_scene == scene_id => callback.presented(
                    &state.output,
                    timestamp,
                    Refresh::fixed(refresh),
                    sequence,
                    presentation_kind(flags),
                ),
                HostPresentation::Presented { .. } | HostPresentation::Discarded => {
                    callback.discarded()
                }
            }
        }
    }
}

fn send_frame_callbacks(
    window: &mut WindowState,
    time: u32,
    commit_id: u64,
    kind: BrowserTimingKind,
) {
    let completed = window
        .dma_frame_callbacks
        .keys()
        .copied()
        .filter(|scene_id| *scene_id <= commit_id)
        .collect::<Vec<_>>();
    for scene_id in completed {
        let callbacks = window.dma_frame_callbacks.remove(&scene_id).unwrap();
        record_browser_timing(
            kind,
            scene_id,
            0,
            Some(commit_id),
            None,
        );
        for callback in callbacks {
            callback.done(time);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn axis_frame(time: u32, source: PointerAxisSource) -> PointerAxisFrame {
        PointerAxisFrame {
            time,
            source,
            value: (0.0, 0.0),
            v120: (None, None),
            stop: (false, false),
            relative_direction: (
                PointerAxisDirection::Identical,
                PointerAxisDirection::Identical,
            ),
        }
    }

    #[test]
    fn popup_pixels_are_normalized_to_straight_alpha_bgra() {
        let mut xrgb = [3, 2, 1, 0];
        normalize_shm_pixels(wl_shm::Format::Xrgb8888, &mut xrgb);
        assert_eq!(xrgb, [3, 2, 1, 255]);

        let mut argb = [32, 64, 96, 128, 10, 20, 30, 0, 3, 2, 1, 255];
        normalize_shm_pixels(wl_shm::Format::Argb8888, &mut argb);
        assert_eq!(argb, [64, 128, 191, 128, 0, 0, 0, 0, 3, 2, 1, 255]);
    }

    #[test]
    fn compositor_time_uses_the_host_monotonic_clock() {
        let before = monotonic_time_ms();
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) },
            0
        );
        let after = monotonic_time_ms();
        let clock = (time.tv_sec as u64 * 1_000 + time.tv_nsec as u64 / 1_000_000) as u32;
        assert!(clock.wrapping_sub(before) <= after.wrapping_sub(before));
    }

    #[test]
    fn presentation_passthrough_accepts_only_new_published_scene_generations() {
        let mut last_presentation = None;
        assert!(!accept_host_scene(false, &mut last_presentation, 4, 1));
        assert_eq!(last_presentation, None);

        assert!(!accept_host_scene(true, &mut last_presentation, 4, 0));
        assert!(!accept_host_scene(true, &mut last_presentation, 4, 4));
        assert!(accept_host_scene(true, &mut last_presentation, 4, 2));
        assert_eq!(last_presentation, Some(2));
        assert!(!accept_host_scene(true, &mut last_presentation, 5, 1));
        assert!(!accept_host_scene(true, &mut last_presentation, 5, 2));
        assert!(accept_host_scene(true, &mut last_presentation, 5, 4));
        assert_eq!(last_presentation, Some(4));

        // wl_surface.frame and wp_presentation resolve independently for the
        // same scene, but each rejects a duplicate or older generation.
        let mut last_frame = None;
        assert!(accept_host_scene(true, &mut last_frame, 5, 4));
        assert!(!accept_host_scene(true, &mut last_frame, 5, 4));
    }

    #[test]
    fn presentation_flags_relay_only_protocol_defined_bits() {
        let all = presentation_kind(1 | 2 | 4 | 8 | 0x8000_0000);
        assert!(all.contains(wp_presentation_feedback::Kind::Vsync));
        assert!(all.contains(wp_presentation_feedback::Kind::HwClock));
        assert!(all.contains(wp_presentation_feedback::Kind::HwCompletion));
        assert!(all.contains(wp_presentation_feedback::Kind::ZeroCopy));
        assert_eq!(all.bits(), 1 | 2 | 4 | 8);
    }

    #[test]
    fn ordered_input_is_dequeued_immediately_without_overtaking() {
        let mut queued = VecDeque::from([
            OrderedInput::PointerAxis(axis_frame(18, PointerAxisSource::Finger)),
            OrderedInput::PointerAxis(axis_frame(1_000, PointerAxisSource::Wheel)),
            OrderedInput::PointerMotion(ResolvedPointerMotion {
                location: (10.0, 20.0),
                target: None,
            }),
            OrderedInput::PointerButton {
                button: 0x110,
                pressed: true,
            },
        ]);

        assert!(matches!(
            dequeue_ready_input(&mut queued),
            Some(OrderedInput::PointerAxis(PointerAxisFrame {
                source: PointerAxisSource::Finger,
                ..
            }))
        ));
        assert!(matches!(
            dequeue_ready_input(&mut queued),
            Some(OrderedInput::PointerAxis(PointerAxisFrame {
                source: PointerAxisSource::Wheel,
                ..
            }))
        ));
        assert!(matches!(
            dequeue_ready_input(&mut queued),
            Some(OrderedInput::PointerMotion(_))
        ));
        assert!(matches!(
            dequeue_ready_input(&mut queued),
            Some(OrderedInput::PointerButton { .. })
        ));
        assert!(dequeue_ready_input(&mut queued).is_none());
    }

    #[test]
    fn frozen_input_cutoff_rejects_events_after_its_ordered_barrier() {
        let (acknowledge, acknowledged) = async_channel::bounded(1);
        let mut queued = VecDeque::new();
        assert!(admit_input(
            false,
            &mut queued,
            OrderedInput::PointerAxis(axis_frame(18, PointerAxisSource::Finger))
        ));
        queued.push_back(OrderedInput::InputBarrier(acknowledge.clone()));
        assert!(!admit_input(
            true,
            &mut queued,
            OrderedInput::PointerButton {
                button: 0x110,
                pressed: false,
            }
        ));
        assert!(!admit_input(
            true,
            &mut queued,
            OrderedInput::Key {
                keycode: 30,
                pressed: true,
            }
        ));
        assert_eq!(queued.len(), 2);

        assert!(acknowledged.try_recv().is_err());
        assert!(matches!(
            dequeue_ready_input(&mut queued),
            Some(OrderedInput::PointerAxis(_))
        ));
        assert!(matches!(
            dequeue_ready_input(&mut queued),
            Some(OrderedInput::InputBarrier(_))
        ));
        acknowledge.try_send(()).unwrap();
        assert_eq!(acknowledged.try_recv(), Ok(()));

        let mut active_freeze = Some(2);
        assert!(!clear_input_freeze(&mut active_freeze, 1));
        assert_eq!(active_freeze, Some(2));
        assert!(clear_input_freeze(&mut active_freeze, 2));
        assert_eq!(active_freeze, None);

        let mut buttons = HashSet::new();
        assert!(track_binary_input(&mut buttons, 0x110, true));
        let synthetic_releases = active_release_codes(&buttons);
        assert_eq!(synthetic_releases, vec![0x110]);
        assert!(buttons.contains(&0x110));
        for button in synthetic_releases {
            assert!(track_binary_input(&mut buttons, button, false));
        }
        assert!(!track_binary_input(&mut buttons, 0x110, false));

        let mut pinch_active = false;
        assert!(track_pinch(
            &mut pinch_active,
            PinchGesture::Begin { fingers: 2 }
        ));
        assert!(track_pinch(
            &mut pinch_active,
            PinchGesture::End { cancelled: true }
        ));
        assert!(!track_pinch(
            &mut pinch_active,
            PinchGesture::End { cancelled: false }
        ));
        assert_eq!(
            filter_finger_stops((true, false), (0.0, 0.0), (true, true)),
            (true, false)
        );
        assert_eq!(
            filter_finger_stops((false, false), (0.0, 0.0), (true, true)),
            (false, false)
        );
    }

    #[test]
    fn scene_mailbox_preserves_imports_attached_but_hidden_in_latest_scene() {
        fn frame(id: u64, releases: Arc<AtomicUsize>) -> DmaBufFrame {
            let fd = std::fs::File::open("/dev/null").unwrap();
            let fence = std::fs::File::open("/dev/null").unwrap();
            DmaBufFrame {
                id,
                width: 1,
                height: 1,
                fourcc: 0,
                modifier: 0,
                stride: 4,
                offset: 0,
                y_inverted: false,
                fd: fd.into(),
                additional_planes: Vec::new(),
                acquire_fence: fence.into(),
                release: Some(Box::new(move || {
                    releases.fetch_add(1, Ordering::SeqCst);
                })),
            }
        }
        fn node(buffer_id: u64) -> SceneNode {
            SceneNode {
                surface_id: buffer_id,
                buffer_id,
                origin: (0.0, 0.0),
                destination: (1.0, 1.0),
                source: ((0.0, 0.0), (1.0, 1.0)),
            }
        }

        let releases = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = browser_event_channel();
        sender.send(BrowserEvent::Scene(SceneUpdate {
            id: 1,
            barrier: 1,
            logical_size: (1, 1),
            imports: vec![BufferImport::DmaBuf(frame(7, releases.clone()))],
            attached: vec![7],
            nodes: vec![node(7)],
            produced_at: Instant::now(),
        }));
        sender.send(BrowserEvent::Scene(SceneUpdate {
            id: 2,
            barrier: 2,
            logical_size: (1, 1),
            imports: Vec::new(),
            attached: vec![7],
            nodes: Vec::new(),
            produced_at: Instant::now(),
        }));
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        let event = futures_lite::future::block_on(receiver.recv()).unwrap();
        assert!(matches!(
            event,
            BrowserEvent::Scene(SceneUpdate { id: 2, imports, .. })
                if matches!(imports.as_slice(), [BufferImport::DmaBuf(frame)] if frame.id == 7)
        ));
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert!(snapshot_browser_timings().iter().any(|timing| {
            timing.kind == BrowserTimingKind::SceneCoalesced
                && timing.scene_id == 1
                && timing.related_scene_id == Some(2)
        }));
    }

    #[test]
    fn browser_timing_history_is_bounded() {
        for scene_id in 0..MAX_BROWSER_TIMINGS as u64 + 17 {
            record_browser_timing(BrowserTimingKind::SceneProduced, scene_id, 0, None, None);
        }
        let timings = snapshot_browser_timings();
        assert_eq!(timings.len(), MAX_BROWSER_TIMINGS);
        assert!(timings.iter().any(|timing| {
            timing.kind == BrowserTimingKind::SceneProduced
                && timing.scene_id == MAX_BROWSER_TIMINGS as u64 + 16
        }));
    }

    fn window_for_test() -> WindowState {
        let (events, _receiver) = browser_event_channel();
        WindowState {
            session_generation: 1,
            toplevel: None,
            size: (1281, 720),
            scale: 1.0,
            events,
            dma_frame_callbacks: HashMap::new(),
            presentation_feedback: HashMap::new(),
            presentation_passthrough: false,
            last_host_presentation_scene: None,
            last_host_frame_scene: None,
            pointer_frames: HashMap::new(),
            hit_scenes: HashMap::new(),
            pointer_location: (0.0, 0.0),
            pending_input: VecDeque::new(),
            input_freeze: None,
            active_finger_axes: (false, false),
            last_finger_axis_time: 0,
            active_buttons: HashSet::new(),
            active_keys: HashSet::new(),
            pinch_active: false,
            surface_slots: HashMap::new(),
            pending_imports: HashMap::new(),
            next_scene_id: 1,
            unbound_barrier: None,
            pending_barriers: Vec::new(),
            acked_barrier: 0,
            committed_barrier: 0,
            terminal_failure: false,
            opened_at: Instant::now(),
        }
    }

    #[test]
    fn barrier_requested_before_toplevel_bind_tracks_initial_configure() {
        let mut window = window_for_test();
        window.unbound_barrier = Some(9);

        track_unbound_barrier(&mut window, 42_u32.into());

        assert_eq!(window.unbound_barrier, None);
        assert_eq!(window.pending_barriers, vec![(42_u32.into(), 9)]);
    }

    #[test]
    fn disabling_presentation_passthrough_reports_the_newest_pending_scene() {
        let mut window = window_for_test();
        window.presentation_passthrough = true;
        window.dma_frame_callbacks.insert(2, Vec::new());
        window.dma_frame_callbacks.insert(4, Vec::new());

        assert_eq!(
            disable_window_presentation_passthrough(&mut window),
            Some(4)
        );
        assert!(!window.presentation_passthrough);
        assert_eq!(disable_window_presentation_passthrough(&mut window), None);
    }

    #[test]
    fn visible_accepted_scene_advances_barrier_at_transaction_anchor() {
        assert_eq!(committed_barrier_after_scene(3, 7, true, true), 7);
        assert_eq!(committed_barrier_after_scene(3, 7, true, false), 3);
        assert_eq!(committed_barrier_after_scene(3, 7, false, true), 3);
    }

    #[test]
    fn qa_root_shm_uses_scene_limit_without_expanding_popup_limit() {
        assert_eq!(
            shm_surface_limit(true, true, 0).unwrap(),
            MAX_SCENE_SHM_BYTES
        );
        assert_eq!(shm_surface_limit(false, true, 0).unwrap(), MAX_POPUP_BYTES);
        assert_eq!(shm_surface_limit(true, false, 0).unwrap(), MAX_POPUP_BYTES);
        assert_eq!(
            shm_surface_limit(true, true, MAX_POPUP_BYTES).unwrap(),
            MAX_POPUP_BYTES
        );
    }

    #[test]
    fn named_browser_cursors_map_to_host_cursor_shapes() {
        assert_eq!(
            browser_cursor(CursorImageStatus::Named(CursorIcon::Pointer)),
            BrowserCursor::PointingHand
        );
        assert_eq!(
            browser_cursor(CursorImageStatus::Named(CursorIcon::Text)),
            BrowserCursor::IBeam
        );
        assert_eq!(
            browser_cursor(CursorImageStatus::Named(CursorIcon::NwseResize)),
            BrowserCursor::ResizeUpLeftDownRight
        );
        assert_eq!(
            browser_cursor(CursorImageStatus::Hidden),
            BrowserCursor::Arrow
        );
    }

    #[test]
    fn converts_evdev_codes_to_xkb_codes() {
        assert_eq!(xkb_keycode(14), 22); // Backspace
        assert_eq!(xkb_keycode(28), 36); // Enter
    }

    #[test]
    fn maps_viewport_surface_coordinates_to_raw_buffer_pixels() {
        assert_eq!(
            surface_mapping((1600, 1200), 2, None, Some((800, 600))),
            SurfaceMapping {
                offset: (0.0, 0.0),
                scale: (2.0, 2.0),
                logical_size: (800, 600),
            }
        );
        assert_eq!(
            surface_mapping(
                (1600, 1200),
                2,
                Some(((100.0, 50.0), (400.0, 300.0))),
                Some((800, 600)),
            ),
            SurfaceMapping {
                offset: (200.0, 100.0),
                scale: (1.0, 1.0),
                logical_size: (800, 600),
            }
        );
    }

    #[test]
    fn pointer_coordinates_follow_the_painted_scene() {
        let frame = PointerFrame;
        assert_eq!(mapped_pointer(frame, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(mapped_pointer(frame, 80.0, 60.0), (80.0, 60.0));
    }

    #[test]
    fn pointer_leave_clears_the_nested_surface_without_moving_the_cursor() {
        assert_eq!(
            pointer_leave_motion((80.0, 60.0)),
            ResolvedPointerMotion {
                location: (80.0, 60.0),
                target: None,
            }
        );
    }

    #[test]
    fn resolved_motion_survives_presentation_pruning_its_scene() {
        let mut pointer_frames = HashMap::from([(4, PointerFrame)]);
        let mut hit_scenes: HashMap<u64, Vec<HitNode>> = HashMap::from([(4, Vec::new())]);
        let motion = resolve_pointer_motion(&pointer_frames, &hit_scenes, 4, 80.0, 60.0).unwrap();

        prune_pointer_scenes(&mut pointer_frames, &mut hit_scenes, 5);

        assert_eq!(motion.location, (80.0, 60.0));
        assert_eq!(motion.target, None);
        assert!(!pointer_frames.contains_key(&4));
        assert!(!hit_scenes.contains_key(&4));
    }
}
