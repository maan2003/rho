//! A private, protocol-only Wayland compositor for embedding stock Chrome.
//!
//! Chrome remains an unmodified Ozone/Wayland client. Rendering belongs to the
//! host GUI; this crate owns the shared Wayland protocol state and transfers
//! committed buffers to that host.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, VecDeque};
use std::cell::RefCell;
use std::ffi::OsString;
use std::hash::Hash;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
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
    AxisFrame, ButtonEvent, Focus, GesturePinchBeginEvent, GesturePinchEndEvent,
    GesturePinchUpdateEvent, MotionEvent, PointerHandle,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
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
use smithay::wayland::drm_syncobj::{
    DrmSyncPoint, DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState,
    supports_syncobj_eventfd,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::fractional_scale::{
    FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
};
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::viewporter::ViewportCachedState;
use smithay::wayland::xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    Configure, PopupSurface, PositionerState, SurfaceCachedState as XdgSurfaceCachedState,
    ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{
    delegate_dmabuf, delegate_drm_syncobj, delegate_fractional_scale,
    delegate_output, delegate_pointer_gestures, delegate_seat, delegate_shm,
    delegate_viewporter, delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
};

const MAX_POPUP_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCENE_SHM_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DmaBufConfig {
    pub render_node: PathBuf,
    pub device_id: u64,
    pub formats: Arc<[(u32, u64)]>,
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
    pub acquire_fence: OwnedFd,
    release: Option<Box<dyn FnOnce() + Send>>,
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
}

impl DmaBufFrame {
    pub fn duplicate_fd(&self) -> std::io::Result<OwnedFd> {
        self.fd.as_fd().try_clone_to_owned()
    }
    pub fn duplicate_acquire_fence(&self) -> std::io::Result<OwnedFd> {
        self.acquire_fence.as_fd().try_clone_to_owned()
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
    FrameRetired(u64),
    ToplevelReady,
    Closed,
    Failed(Arc<str>),
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

enum PageCommand {
    Resize(u32, u32, f64),
    FrameBarrier(u64, u32, u32, f64),
    Presented(u64),
    Retired(u64),
    PointerMotion { commit_id: u64, x: f64, y: f64 },
    PointerButton { button: u32, pressed: bool },
    PointerAxis(PointerAxisFrame),
    Pinch(PinchGesture),
    Key { keycode: u32, pressed: bool },
    Close,
}
enum RuntimeCommand<K> {
    Open {
        id: K,
        size: (u32, u32),
        events: BrowserEventSender,
        activation_reply: mpsc::SyncSender<String>,
    },
    Page(K, PageCommand),
    Shutdown,
}
pub trait BrowserPageKey: Copy + Eq + Hash + Send + 'static {}
impl<T: Copy + Eq + Hash + Send + 'static> BrowserPageKey for T {}

pub struct BrowserCompositor<K: BrowserPageKey> {
    commands: channel::Sender<RuntimeCommand<K>>,
    socket: OsString,
    thread: Option<thread::JoinHandle<()>>,
}
impl<K: BrowserPageKey> BrowserCompositor<K> {
    pub fn launch(dma_buf: DmaBufConfig) -> Result<Self> {
        let (tx, rx) = channel::channel();
        let tx2 = tx.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let errors = ready_tx.clone();
        let thread = thread::Builder::new()
            .name("rho-browser-wayland".into())
            .spawn(move || {
                if let Err(e) = run(rx, ready_tx, dma_buf, tx2) {
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
        let (events, rx) = browser_event_channel();
        let (activation_reply, activation_rx) = mpsc::sync_channel(1);
        self.commands
            .send(RuntimeCommand::Open {
                id,
                size,
                events,
                activation_reply,
            })
            .map_err(|_| anyhow::anyhow!("browser compositor stopped"))?;
        let activation_token = activation_rx
            .recv_timeout(Duration::from_secs(2))
            .context("browser compositor did not issue an activation token")?;
        Ok(BrowserSession {
            id,
            commands: self.commands.clone(),
            events: rx,
            activation_token,
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
    commands: channel::Sender<RuntimeCommand<K>>,
    events: BrowserEventReceiver,
    activation_token: String,
}
impl<K: BrowserPageKey> BrowserSession<K> {
    fn send(&self, c: PageCommand) {
        let _ = self.commands.send(RuntimeCommand::Page(self.id, c));
    }
    pub fn activation_token(&self) -> &str {
        &self.activation_token
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
    pub fn events(&self) -> BrowserEventReceiver {
        self.events.clone()
    }
    pub fn presentation_callback(&self, commit_id: u64) -> impl FnOnce() + Send + 'static {
        let tx = self.commands.clone();
        let id = self.id;
        move || {
            let _ = tx.send(RuntimeCommand::Page(id, PageCommand::Presented(commit_id)));
        }
    }
}
impl<K: BrowserPageKey> Drop for BrowserSession<K> {
    fn drop(&mut self) {
        self.send(PageCommand::Close)
    }
}
struct WindowState {
    toplevel: Option<ToplevelSurface>,
    size: (u32, u32),
    scale: f64,
    events: BrowserEventSender,
    dma_frame_callbacks: HashMap<u64, Vec<wl_callback::WlCallback>>,
    pointer_frames: HashMap<u64, PointerFrame>,
    hit_scenes: HashMap<u64, Vec<HitNode>>,
    pointer_location: (f64, f64),
    surface_slots: HashMap<ObjectId, SurfaceSlot>,
    pending_imports: HashMap<u64, BufferImport>,
    next_scene_id: u64,
    unbound_barrier: Option<u64>,
    pending_barriers: Vec<(Serial, u64)>,
    acked_barrier: u64,
    committed_barrier: u64,
    opened_at: Instant,
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
    activation: XdgActivationState,
    _decoration: XdgDecorationState,
    shm: ShmState,
    dmabuf: DmabufState,
    _dmabuf_global: DmabufGlobal,
    syncobj: DrmSyncobjState,
    dma_formats: Arc<[(u32, u64)]>,
    _output: Output,
    _fractional_scale: FractionalScaleManagerState,
    _viewporter: ViewporterState,
    _pointer_gestures: PointerGesturesState,
    seat_state: SeatState<Self>,
    _seat: Seat<Self>,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,
    serial: u32,
    started: Instant,
    windows: HashMap<K, WindowState>,
    activation_windows: HashMap<String, K>,
    unbound_toplevels: HashMap<ObjectId, (ToplevelSurface, Instant)>,
    pending_activations: HashMap<ObjectId, K>,
    allow_initial_unambiguous_toplevel: bool,
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
    new_visible_toplevel_dma: bool,
) -> u64 {
    if barrier_anchor && new_visible_toplevel_dma {
        acknowledged
    } else {
        committed
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceMapping {
    offset: (f64, f64),
    scale: (f64, f64),
    logical_size: (u32, u32),
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

fn surface_node_mapping(states: &SurfaceData, slot: SurfaceSlot) -> Result<SurfaceMapping> {
    // Copy everything needed from the cache guards before validation. Smithay's
    // validator locks ViewportCachedState itself, so retaining that guard here
    // would deadlock the compositor thread.
    let (viewport, buffer_scale, buffer_transform) = {
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        let mut attributes = states.cached_state.get::<SurfaceAttributes>();
        (
            *viewport.current(),
            attributes.current().buffer_scale,
            attributes.current().buffer_transform,
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
    Ok(mapping)
}

fn subsurface_offset(states: &SurfaceData) -> (i32, i32) {
    if states.role != Some(SUBSURFACE_ROLE) {
        return (0, 0);
    }
    let mut subsurface = states.cached_state.get::<SubsurfaceCachedState>();
    let location = subsurface.current().location;
    (location.x, location.y)
}

fn append_surface_tree(
    root: &WlSurface,
    root_origin: (i32, i32),
    slots: &HashMap<ObjectId, SurfaceSlot>,
    nodes: &mut Vec<SceneNode>,
    hits: &mut Vec<HitNode>,
) -> Result<()> {
    let error = RefCell::new(None);
    with_surface_tree_upward(
        root,
        root_origin,
        |surface, states, parent_origin| {
            if !slots.contains_key(&surface.id()) {
                return TraversalAction::SkipChildren;
            }
            // Smithay holds this surface's tree mutex during traversal. Use the
            // supplied state rather than re-entering `with_states`/`get_role`.
            let offset = subsurface_offset(states);
            TraversalAction::DoChildren((parent_origin.0 + offset.0, parent_origin.1 + offset.1))
        },
        |surface, states, parent_origin| {
            if error.borrow().is_some() {
                return;
            }
            let Some(slot) = slots.get(&surface.id()).copied() else {
                return;
            };
            let offset = subsurface_offset(states);
            let origin = (parent_origin.0 + offset.0, parent_origin.1 + offset.1);
            match surface_node_mapping(states, slot) {
                Ok(mapping) => {
                    let origin = (f64::from(origin.0), f64::from(origin.1));
                    let destination = (
                        f64::from(mapping.logical_size.0),
                        f64::from(mapping.logical_size.1),
                    );
                    nodes.push(SceneNode {
                        surface_id: slot.buffer_id,
                        buffer_id: slot.buffer_id,
                        origin,
                        destination,
                        source: (
                            mapping.offset,
                            (
                                mapping.scale.0 * f64::from(mapping.logical_size.0),
                                mapping.scale.1 * f64::from(mapping.logical_size.1),
                            ),
                        ),
                    });
                    let input_region = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .input_region
                        .clone();
                    hits.push(HitNode {
                        surface: surface.clone(),
                        origin,
                        destination,
                        input_region,
                    });
                }
                Err(mapping_error) => *error.borrow_mut() = Some(mapping_error),
            }
        },
        |_, _, _| error.borrow().is_none(),
    );
    error.into_inner().map_or(Ok(()), Err)
}

impl<K: BrowserPageKey> State<K> {
    fn next_serial(&mut self) -> Serial {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        serial.into()
    }

    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
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

    fn snapshot_shm(&self, buffer_id: u64, buffer: &wl_buffer::WlBuffer) -> Result<ShmFrame> {
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
            if pixel_bytes > MAX_POPUP_BYTES {
                bail!("SHM surface exceeds {MAX_POPUP_BYTES} bytes")
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
                if is_root {
                    bail!("Chromium root surface did not provide the required zero-copy DMA-BUF")
                }
                let frame = self.snapshot_shm(buffer_id, &buffer)?;
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

    fn publish_scene(&mut self, id: K, barrier_anchor: bool) -> Result<()> {
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
        let mut nodes = Vec::new();
        let mut hits = Vec::new();
        append_surface_tree(&root, root_origin, slots, &mut nodes, &mut hits)?;
        let new_visible_toplevel_dma = nodes
            .iter()
            .any(|node| new_toplevel_dma.contains(&node.buffer_id));

        // PopupManager yields topmost first; GPUI paints bottom-to-top.
        let mut popups = PopupManager::popups_for_surface(&root).collect::<Vec<_>>();
        popups.reverse();
        for (popup, offset) in popups {
            let geometry = popup.geometry();
            let origin = offset - geometry.loc;
            append_surface_tree(
                popup.wl_surface(),
                (origin.x, origin.y),
                slots,
                &mut nodes,
                &mut hits,
            )?;
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
            new_visible_toplevel_dma,
        );
        if window.committed_barrier != previous_barrier {
            tracing::info!(
                barrier = window.committed_barrier,
                scene_id = window.next_scene_id,
                "Chromium frame barrier reached a visible DMA scene"
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
        // Hit nodes and GPUI use the same window-local coordinate space. The
        // root's XDG geometry offset is already represented by its node origin.
        let pointer_frame = PointerFrame;
        window.pointer_frames.insert(scene_id, pointer_frame);
        window.hit_scenes.insert(scene_id, hits);
        window
            .dma_frame_callbacks
            .insert(scene_id, drain_frame_callbacks(&surfaces));
        window.events.send(BrowserEvent::Scene(SceneUpdate {
            id: scene_id,
            barrier: window.committed_barrier,
            logical_size: window.size,
            imports,
            attached: attached.into_iter().collect(),
            nodes,
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
        let (size, scale) = {
            let window = &self.windows[&id];
            (window.size, window.scale)
        };
        self.surface_windows.insert(root.clone(), id);
        self._output.enter(surface.wl_surface());
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
        self.allow_initial_unambiguous_toplevel = false;
        self.activation_windows
            .retain(|_, window_id| *window_id != id);
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
        if !self.allow_initial_unambiguous_toplevel {
            return;
        }
        let roots = self.unbound_toplevels.keys().cloned().collect::<Vec<_>>();
        let windows = self
            .windows
            .iter()
            .filter(|(_, window)| window.toplevel.is_none())
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        if roots.len() > 1 || windows.len() > 1 {
            self.allow_initial_unambiguous_toplevel = false;
            return;
        }
        let ([root], [id]) = (roots.as_slice(), windows.as_slice()) else {
            return;
        };
        if self.pending_activations.contains_key(root) {
            return;
        }
        if self.unbound_toplevels[root].1.elapsed() < Duration::from_millis(250) {
            return;
        }
        if self
            .pending_activations
            .values()
            .any(|pending| pending == id)
        {
            return;
        }
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
    fn new_subsurface(&mut self, _surface: &WlSurface, parent: &WlSurface) {
        let Some(id) = self.window_id_for_surface(parent) else {
            return;
        };
        if let Err(error) = self.publish_scene(id, false) {
            self.windows[&id].events.send(BrowserEvent::Failed(
                format!("invalid Chromium surface tree after subsurface creation: {error:#}")
                    .into(),
            ));
        }
    }
    fn commit(&mut self, surface: &WlSurface) {
        self.popup_manager.commit(surface);

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
            self.windows[&id].events.send(BrowserEvent::Failed(
                format!("invalid Chromium surface tree: {error:#}").into(),
            ));
        }
    }
    fn destroyed(&mut self, surface: &WlSurface) {
        let Some(id) = self.window_id_for_surface(surface) else {
            self.unbound_toplevels.remove(&surface.id());
            self.pending_activations.remove(&surface.id());
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
                self.windows[&id].events.send(BrowserEvent::Failed(
                    format!("invalid Chromium surface tree after removal: {error:#}").into(),
                ));
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
        if let Some(id) = self.pending_activations.remove(&root) {
            self.bind_toplevel(id, root);
        }
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

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let root = surface.wl_surface().id();
        self.unbound_toplevels.remove(&root);
        self.pending_activations.remove(&root);
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
            self.windows[&window_id].events.send(BrowserEvent::Failed(
                format!("invalid Chromium popup tree after removal: {error:#}").into(),
            ));
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

impl<K: BrowserPageKey> XdgActivationHandler for State<K> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation
    }
    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        _data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let Some(id) = self.activation_windows.get(&token.to_string()).copied() else {
            return;
        };
        self.allow_initial_unambiguous_toplevel = false;
        self.activation.remove_token(&token);
        let root = surface.id();
        if self.unbound_toplevels.contains_key(&root) {
            self.bind_toplevel(id, root);
        } else {
            self.pending_activations.insert(root, id);
        }
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
        Some(&mut self.syncobj)
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
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
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
            state.windows[&window_id].events.send(BrowserEvent::Failed(
                format!("invalid Chromium surface tree after subsurface removal: {error:#}").into(),
            ));
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
delegate_xdg_activation!(@<K: BrowserPageKey> State<K>);
delegate_xdg_decoration!(@<K: BrowserPageKey> State<K>);
delegate_shm!(@<K: BrowserPageKey> State<K>);
delegate_dmabuf!(@<K: BrowserPageKey> State<K>);
delegate_drm_syncobj!(@<K: BrowserPageKey> State<K>);
delegate_output!(@<K: BrowserPageKey> State<K>);
delegate_seat!(@<K: BrowserPageKey> State<K>);
delegate_fractional_scale!(@<K: BrowserPageKey> State<K>);
delegate_viewporter!(@<K: BrowserPageKey> State<K>);
delegate_pointer_gestures!(@<K: BrowserPageKey> State<K>);

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
    if dmabuf.num_planes() != 1 {
        bail!("only single-plane DMA-BUFs are supported");
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
    let fd = dmabuf
        .handles()
        .next()
        .context("DMA-BUF has no plane")?
        .try_clone_to_owned()
        .context("duplicate DMA-BUF plane")?;
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
    let stride = dmabuf.strides().next().context("DMA-BUF has no stride")?;
    let offset = dmabuf.offsets().next().context("DMA-BUF has no offset")?;
    let keep_alive = dmabuf.clone();
    let release = release.0.take().expect("release point available");
    let (window_id, commands) = retirement;
    Ok(DmaBufFrame {
        id,
        width: u32::try_from(size.w).context("invalid DMA-BUF width")?,
        height: u32::try_from(size.h).context("invalid DMA-BUF height")?,
        fourcc: format.code as u32,
        modifier: u64::from(format.modifier),
        stride,
        offset,
        y_inverted: dmabuf.y_inverted(),
        fd,
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
    config: DmaBufConfig,
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
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&config.render_node)
        .with_context(|| format!("open DRM render node {}", config.render_node.display()))?;
    let drm = DrmDeviceFd::new(OwnedFd::from(file).into());
    if !supports_syncobj_eventfd(&drm) {
        bail!("GPU does not support explicit-sync eventfd required for zero-copy browser frames");
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
    let feedback =
        smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(config.device_id as libc::dev_t, fs)
            .build()
            .context("build DMA-BUF feedback")?;
    let global = dmabuf.create_global_with_default_feedback::<State<K>>(&dh, &feedback);
    let syncobj = DrmSyncobjState::new::<State<K>>(&dh, drm);
    let formats = Arc::clone(&config.formats);
    let mut state = State {
        loop_handle,
        display_handle: dh.clone(),
        compositor: CompositorState::new::<State<K>>(&dh),
        shell: XdgShellState::new::<State<K>>(&dh),
        activation: XdgActivationState::new::<State<K>>(&dh),
        _decoration: XdgDecorationState::new::<State<K>>(&dh),
        shm: ShmState::new::<State<K>>(&dh, vec![]),
        dmabuf,
        _dmabuf_global: global,
        syncobj,
        dma_formats: formats,
        _output: output,
        _fractional_scale: FractionalScaleManagerState::new::<State<K>>(&dh),
        _viewporter: ViewporterState::new::<State<K>>(&dh),
        _pointer_gestures: PointerGesturesState::new::<State<K>>(&dh),
        seat_state,
        _seat: seat,
        keyboard,
        pointer,
        serial: 1,
        started: Instant::now(),
        windows: HashMap::new(),
        activation_windows: HashMap::new(),
        unbound_toplevels: HashMap::new(),
        pending_activations: HashMap::new(),
        allow_initial_unambiguous_toplevel: true,
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
                    state
                        .display_handle
                        .insert_client(stream, Arc::new(ClientState::default()))?;
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
            size,
            events,
            activation_reply,
        } => {
            let activation_token = state.activation.create_external_token(None).0.to_string();
            state
                .activation_windows
                .insert(activation_token.clone(), id);
            let _ = activation_reply.send(activation_token);
            state.windows.insert(
                id,
                WindowState {
                    toplevel: None,
                    size,
                    scale: 1.0,
                    events,
                    dma_frame_callbacks: HashMap::new(),
                    pointer_frames: HashMap::new(),
                    hit_scenes: HashMap::new(),
                    pointer_location: (0.0, 0.0),
                    surface_slots: HashMap::new(),
                    pending_imports: HashMap::new(),
                    next_scene_id: 1,
                    unbound_barrier: None,
                    pending_barriers: Vec::new(),
                    acked_barrier: 0,
                    committed_barrier: 0,
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
            state
                .activation_windows
                .retain(|_, window_id| *window_id != id);
            state
                .pending_activations
                .retain(|_, window_id| *window_id != id);
            if let Some(window) = state.windows.remove(&id) {
                window.events.send(BrowserEvent::Failed(
                    "Chromium did not provide an unambiguous activated window".into(),
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
    let fallback_root = root.clone();
    let _ = state.loop_handle.insert_source(
        Timer::from_duration(Duration::from_millis(250)),
        move |_, _, state| {
            if state
                .unbound_toplevels
                .get(&fallback_root)
                .is_some_and(|(_, current)| *current == created)
            {
                state.bind_unambiguous_toplevel();
                let _ = state.display_handle.flush_clients();
            }
            TimeoutAction::Drop
        },
    );
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
        PageCommand::Presented(commit) => {
            let time = state.time();
            if let Some(w) = state.windows.get_mut(&id) {
                send_frame_callbacks(w, time, commit);
                w.pointer_frames.retain(|scene, _| *scene >= commit);
                w.hit_scenes.retain(|scene, _| *scene >= commit);
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
                state.windows[&id].events.send(BrowserEvent::Failed(
                    format!("invalid Chromium surface tree after buffer retirement: {error:#}")
                        .into(),
                ));
            }
            if let Some(window) = state.windows.get(&id) {
                window.events.send(BrowserEvent::FrameRetired(commit));
            }
        }
        PageCommand::PointerMotion { commit_id, x, y } => {
            pointer_motion(state, id, commit_id, x, y)
        }
        PageCommand::PointerButton { button, pressed } => {
            pointer_button(state, id, button, pressed)
        }
        PageCommand::PointerAxis(frame) => pointer_axis(state, id, frame),
        PageCommand::Pinch(gesture) => pointer_pinch(state, id, gesture),
        PageCommand::Key { keycode, pressed } => keyboard_key(state, id, keycode, pressed),
        PageCommand::Close => {
            state.activation_windows.retain(|_, v| *v != id);
            state.pending_activations.retain(|_, v| *v != id);
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
    window: &WindowState,
    scene_id: u64,
    location: (f64, f64),
) -> Option<(WlSurface, (f64, f64))> {
    for hit in window.hit_scenes.get(&scene_id)?.iter().rev() {
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

fn pointer_motion<K: BrowserPageKey>(state: &mut State<K>, id: K, commit_id: u64, x: f64, y: f64) {
    let Some(w) = state.windows.get(&id) else {
        return;
    };
    let Some(frame) = w.pointer_frames.get(&commit_id).copied() else {
        return;
    };
    let (x, y) = mapped_pointer(frame, x, y);
    let target = pointer_target(w, commit_id, (x, y));
    if let Some(window) = state.windows.get_mut(&id) {
        window.pointer_location = (x, y);
    }
    let event = MotionEvent {
        location: (x, y).into(),
        serial: state.next_serial(),
        time: state.time(),
    };
    let pointer = state.pointer.clone();
    pointer.motion(
        state,
        target.map(|(surface, origin)| (surface, origin.into())),
        &event,
    );
    pointer.frame(state);
}

fn mapped_pointer(_frame: PointerFrame, x: f64, y: f64) -> (f64, f64) {
    (x.max(0.0), y.max(0.0))
}
fn pointer_button<K: BrowserPageKey>(state: &mut State<K>, id: K, button: u32, pressed: bool) {
    if !state.windows.contains_key(&id) {
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

fn pointer_axis<K: BrowserPageKey>(state: &mut State<K>, id: K, event: PointerAxisFrame) {
    if !state.windows.contains_key(&id) {
        return;
    }
    let pointer = state.pointer.clone();
    let time = state.time();
    let mut frame = AxisFrame::new(time).source(match event.source {
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

fn pointer_pinch<K: BrowserPageKey>(state: &mut State<K>, id: K, event: PinchGesture) {
    if !state.windows.contains_key(&id) {
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

fn keyboard_key<K: BrowserPageKey>(state: &mut State<K>, id: K, keycode: u32, pressed: bool) {
    if state
        .windows
        .get(&id)
        .and_then(|w| w.toplevel.as_ref())
        .is_none()
    {
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

fn send_frame_callbacks(window: &mut WindowState, time: u32, commit_id: u64) {
    let completed = window
        .dma_frame_callbacks
        .keys()
        .copied()
        .filter(|scene_id| *scene_id <= commit_id)
        .collect::<Vec<_>>();
    for scene_id in completed {
        let callbacks = window.dma_frame_callbacks.remove(&scene_id).unwrap();
        for callback in callbacks {
            callback.done(time);
        }
    }
}

/// Resolves the Chrome wrapper without ever selecting an underlying ELF.
pub fn chrome_wrapper() -> OsString {
    std::env::var_os("RHO_CHROME_BIN").unwrap_or_else(|| OsString::from("google-chrome-stable"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

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
    fn chrome_wrapper_has_safe_default() {
        if std::env::var_os("RHO_CHROME_BIN").is_none() {
            assert_eq!(chrome_wrapper(), "google-chrome-stable");
        }
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
        }));
        sender.send(BrowserEvent::Scene(SceneUpdate {
            id: 2,
            barrier: 2,
            logical_size: (1, 1),
            imports: Vec::new(),
            attached: vec![7],
            nodes: Vec::new(),
        }));
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        let event = futures_lite::future::block_on(receiver.recv()).unwrap();
        assert!(matches!(
            event,
            BrowserEvent::Scene(SceneUpdate { id: 2, imports, .. })
                if matches!(imports.as_slice(), [BufferImport::DmaBuf(frame)] if frame.id == 7)
        ));
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn barrier_requested_before_toplevel_bind_tracks_initial_configure() {
        let (events, _receiver) = browser_event_channel();
        let mut window = WindowState {
            toplevel: None,
            size: (1281, 720),
            scale: 1.0,
            events,
            dma_frame_callbacks: HashMap::new(),
            pointer_frames: HashMap::new(),
            hit_scenes: HashMap::new(),
            pointer_location: (0.0, 0.0),
            surface_slots: HashMap::new(),
            pending_imports: HashMap::new(),
            next_scene_id: 1,
            unbound_barrier: Some(9),
            pending_barriers: Vec::new(),
            acked_barrier: 0,
            committed_barrier: 0,
            opened_at: Instant::now(),
        };

        track_unbound_barrier(&mut window, 42_u32.into());

        assert_eq!(window.unbound_barrier, None);
        assert_eq!(window.pending_barriers, vec![(42_u32.into(), 9)]);
    }

    #[test]
    fn synchronized_child_dma_advances_barrier_at_root_transaction() {
        assert_eq!(committed_barrier_after_scene(3, 7, true, true), 7);
        assert_eq!(committed_barrier_after_scene(3, 7, true, false), 3);
        assert_eq!(committed_barrier_after_scene(3, 7, false, true), 3);
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
}
