//! A private, protocol-only Wayland compositor for embedding stock Chrome.
//!
//! Chrome remains an unmodified Ozone/Wayland client. Rendering belongs to the
//! host GUI; this crate owns the shared Wayland protocol state and transfers
//! committed buffers to that host.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, VecDeque};
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
use smithay::input::keyboard::{FilterResult, KeyboardHandle};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
    GesturePinchUpdateEvent, MotionEvent, PointerHandle,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_callback, wl_output, wl_seat};
use smithay::reexports::wayland_server::{
    Client, Display, DisplayHandle, ListeningSocket, Resource,
};
use smithay::utils::{Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SUBSURFACE_ROLE, SurfaceAttributes, TraversalAction, get_role, with_states,
    with_surface_tree_downward,
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
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_dmabuf, delegate_drm_syncobj, delegate_fractional_scale,
    delegate_output, delegate_pointer_gestures, delegate_seat, delegate_shm,
    delegate_viewporter, delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
};

#[derive(Clone, Debug)]
pub struct DmaBufConfig {
    pub render_node: PathBuf,
    pub device_id: u64,
    pub formats: Arc<[(u32, u64)]>,
}

pub struct DmaBufFrame {
    pub id: u64,
    /// The newest host-requested frame barrier acknowledged before this commit.
    pub barrier: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
    pub source_origin: (u32, u32),
    pub source_size: (u32, u32),
    pub fd: OwnedFd,
    pub acquire_fence: OwnedFd,
    release: Option<Box<dyn FnOnce() + Send>>,
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
    DmaBuf(DmaBufFrame),
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
        if matches!(event, BrowserEvent::DmaBuf(_))
            && let Some(position) = queue
                .iter()
                .position(|queued| matches!(queued, BrowserEvent::DmaBuf(_)))
        {
            queue.remove(position);
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
    pointer_location: (f64, f64),
    unbound_barrier: Option<u64>,
    pending_barriers: Vec<(Serial, u64)>,
    acked_barrier: u64,
    opened_at: Instant,
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
}

#[derive(Clone, Copy)]
struct PointerFrame {
    origin: (i32, i32),
    size: (u32, u32),
}

type BufferRect = ((u32, u32), (u32, u32));

#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceMapping {
    offset: (f64, f64),
    scale: (f64, f64),
    logical_size: (u32, u32),
}

fn permits_uncomposited_shm_subsurface(
    is_root: bool,
    role: Option<&str>,
    buffer_type: Option<&BufferType>,
) -> bool {
    !is_root && role == Some(SUBSURFACE_ROLE) && matches!(buffer_type, Some(BufferType::Shm))
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
    fn commit(&mut self, surface: &WlSurface) {
        let wid = self.window_id_for_surface(surface);
        let is_root = wid.is_some_and(|id| {
            self.windows
                .get(&id)
                .and_then(|w| w.toplevel.as_ref())
                .is_some_and(|t| t.wl_surface() == surface)
        });
        let role = get_role(surface);
        let is_subsurface = role == Some(SUBSURFACE_ROLE);
        let (buffer, (acquire, mut release)) = with_states(surface, |states| {
            let mut c = states.cached_state.get::<SurfaceAttributes>();
            let b = c.current().buffer.take();
            let mut y = states.cached_state.get::<DrmSyncobjCachedState>();
            let y = y.current();
            (b, (y.acquire_point.take(), y.release_point.take()))
        });
        let Some(wid) = wid else {
            if let Some(r) = release {
                let _ = r.signal();
            }
            if let Some(BufferAssignment::NewBuffer(b)) = buffer {
                b.release();
            }
            complete_surface_callbacks(surface, self.time());
            return;
        };
        let buffer = match buffer {
            Some(BufferAssignment::NewBuffer(b)) => b,
            Some(BufferAssignment::Removed) => {
                if let Some(r) = release {
                    let _ = r.signal();
                }
                let time = self.time();
                complete_surface_callbacks(surface, time);
                return;
            }
            None => {
                if let Some(r) = release {
                    let _ = r.signal();
                }
                complete_surface_callbacks(surface, self.time());
                return;
            }
        };
        let buffer_type = buffer_type(&buffer);
        if permits_uncomposited_shm_subsurface(is_root, role, buffer_type.as_ref()) {
            if let Some(r) = release {
                let _ = r.signal();
            }
            buffer.release();
            complete_surface_callbacks(surface, self.time());
            return;
        }
        if let Ok(dmabuf) = get_dmabuf(&buffer) {
            if is_root {
                let (Some(a), Some(r)) = (acquire, release.take()) else {
                    self.windows[&wid].events.send(BrowserEvent::Failed(
                        "Chromium DMA-BUF commit omitted required explicit-sync points".into(),
                    ));
                    buffer.release();
                    complete_surface_callbacks(surface, self.time());
                    return;
                };
                let bid = self.next_buffer_id;
                self.next_buffer_id = self.next_buffer_id.wrapping_add(1).max(1);
                let (source, pointer_frame) =
                    match dma_source_rect(surface, dmabuf.size().w, dmabuf.size().h) {
                        Ok(frame) => frame,
                        Err(error) => {
                            let message: Arc<str> =
                                format!("unsupported Chromium surface mapping: {error:#}").into();
                            self.windows[&wid]
                                .events
                                .send(BrowserEvent::Failed(message));
                            buffer.release();
                            complete_surface_callbacks(surface, self.time());
                            return;
                        }
                    };
                let barrier = self.windows[&wid].acked_barrier;
                match dma_buf_frame(bid, dmabuf, source, a, r, wid, self.commands.clone()) {
                    Ok(mut frame) => {
                        frame.barrier = barrier;
                        let window = self.windows.get_mut(&wid).expect("known window");
                        window.pointer_frames.insert(bid, pointer_frame);
                        if let Some(toplevel) = &window.toplevel {
                            window
                                .dma_frame_callbacks
                                .insert(bid, drain_frame_callbacks(toplevel));
                        }
                        window.events.send(BrowserEvent::DmaBuf(frame));
                        return;
                    }
                    Err(error) => {
                        let message: Arc<str> =
                            format!("invalid Chromium DMA-BUF: {error:#}").into();
                        self.windows[&wid]
                            .events
                            .send(BrowserEvent::Failed(message));
                    }
                }
                buffer.release();
                complete_surface_callbacks(surface, self.time());
                return;
            }
            if let Some(r) = release {
                let _ = r.signal();
            }
            self.windows[&wid].events.send(BrowserEvent::Failed(
                if is_subsurface {
                    "Chromium committed a content-bearing DMA-BUF subsurface; zero-copy tree composition is not supported"
                } else {
                    "Chromium committed a DMA-BUF auxiliary surface; zero-copy tree composition is not supported"
                }
                .into(),
            ));
            buffer.release();
            complete_surface_callbacks(surface, self.time());
            return;
        }
        if let Some(r) = release {
            let _ = r.signal();
        }
        complete_surface_callbacks(surface, self.time());
        self.windows[&wid].events.send(BrowserEvent::Failed(
            if is_root {
                "Chromium did not provide the required zero-copy DMA-BUF with explicit sync"
            } else if is_subsurface {
                "Chromium committed an unsupported non-SHM subsurface buffer"
            } else {
                "Chromium committed an unsupported non-DMA-BUF auxiliary surface buffer"
            }
            .into(),
        ));
        buffer.release();
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
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        if let Some(parent) = surface.get_parent_surface()
            && let Some(id) = self.window_id_for_surface(&parent)
        {
            self.surface_windows.insert(surface.wl_surface().id(), id);
        }
        let _ = surface.send_configure();
    }
    fn grab(&mut self, _: PopupSurface, _: wl_seat::WlSeat, _: Serial) {}
    fn reposition_request(&mut self, surface: PopupSurface, _: PositionerState, token: u32) {
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

delegate_compositor!(@<K: BrowserPageKey> State<K>);
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
    source: BufferRect,
    acquire: DrmSyncPoint,
    release: DrmSyncPoint,
    window_id: K,
    commands: channel::Sender<RuntimeCommand<K>>,
) -> Result<DmaBufFrame> {
    let mut release = ReleasePointGuard(Some(release));
    if dmabuf.num_planes() != 1 {
        bail!("only single-plane DMA-BUFs are supported");
    }
    let size = dmabuf.size();
    let format = dmabuf.format();
    let fd = dmabuf
        .handles()
        .next()
        .context("DMA-BUF has no plane")?
        .try_clone_to_owned()
        .context("duplicate DMA-BUF plane")?;
    let acquire_fence = acquire
        .export_sync_file()
        .context("export DMA-BUF acquire fence")?;
    let stride = dmabuf.strides().next().context("DMA-BUF has no stride")?;
    let offset = dmabuf.offsets().next().context("DMA-BUF has no offset")?;
    let keep_alive = dmabuf.clone();
    let release = release.0.take().expect("release point available");
    Ok(DmaBufFrame {
        id,
        barrier: 0,
        width: u32::try_from(size.w).context("invalid DMA-BUF width")?,
        height: u32::try_from(size.h).context("invalid DMA-BUF height")?,
        fourcc: format.code as u32,
        modifier: u64::from(format.modifier),
        stride,
        offset,
        y_inverted: dmabuf.y_inverted(),
        source_origin: source.0,
        source_size: source.1,
        fd,
        acquire_fence,
        release: Some(Box::new(move || {
            let _keep_alive = keep_alive;
            if let Err(error) = release.signal() {
                tracing::error!(?error, "signal Chrome DMA-BUF release point");
            }
            let _ = commands.send(RuntimeCommand::Page(window_id, PageCommand::Retired(id)));
        })),
    })
}

fn dma_source_rect(
    surface: &WlSurface,
    buffer_width: i32,
    buffer_height: i32,
) -> Result<(BufferRect, PointerFrame)> {
    let (geometry, viewport, buffer_scale, buffer_transform) = with_states(surface, |states| {
        let mut cached = states.cached_state.get::<XdgSurfaceCachedState>();
        let geometry = cached.current().geometry;
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        let mut surface = states.cached_state.get::<SurfaceAttributes>();
        (
            geometry,
            *viewport.current(),
            surface.current().buffer_scale,
            surface.current().buffer_transform,
        )
    });
    if buffer_transform != wl_output::Transform::Normal {
        bail!("buffer transform {buffer_transform:?}");
    }
    let mapping = surface_mapping(
        (buffer_width, buffer_height),
        buffer_scale,
        viewport
            .src
            .map(|source| ((source.loc.x, source.loc.y), (source.size.w, source.size.h))),
        viewport
            .dst
            .map(|destination| (destination.w, destination.h)),
    );
    let (scale_x, scale_y) = mapping.scale;
    let (offset_x, offset_y) = mapping.offset;
    let pointer_frame = geometry
        .map(|geometry| PointerFrame {
            origin: (geometry.loc.x, geometry.loc.y),
            size: (geometry.size.w.max(1) as u32, geometry.size.h.max(1) as u32),
        })
        .unwrap_or(PointerFrame {
            origin: (0, 0),
            size: mapping.logical_size,
        });
    let source = clamped_source_rect(
        geometry.map(|geometry| {
            let left = offset_x + f64::from(geometry.loc.x) * scale_x;
            let top = offset_y + f64::from(geometry.loc.y) * scale_y;
            let right =
                offset_x + f64::from(geometry.loc.x.saturating_add(geometry.size.w)) * scale_x;
            let bottom =
                offset_y + f64::from(geometry.loc.y.saturating_add(geometry.size.h)) * scale_y;
            (
                (left.floor() as i32, top.floor() as i32),
                (
                    (right.ceil() - left.floor()) as i32,
                    (bottom.ceil() - top.floor()) as i32,
                ),
            )
        }),
        buffer_width,
        buffer_height,
    );
    Ok((source, pointer_frame))
}

fn clamped_source_rect(
    geometry: Option<((i32, i32), (i32, i32))>,
    buffer_width: i32,
    buffer_height: i32,
) -> BufferRect {
    let Some(((x, y), (width, height))) = geometry else {
        return (
            (0, 0),
            (buffer_width.max(1) as u32, buffer_height.max(1) as u32),
        );
    };
    let left = x.clamp(0, buffer_width);
    let top = y.clamp(0, buffer_height);
    let right = x.saturating_add(width).clamp(left, buffer_width);
    let bottom = y.saturating_add(height).clamp(top, buffer_height);
    if right == left || bottom == top {
        (
            (0, 0),
            (buffer_width.max(1) as u32, buffer_height.max(1) as u32),
        )
    } else {
        (
            (left as u32, top as u32),
            ((right - left) as u32, (bottom - top) as u32),
        )
    }
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
                    pointer_location: (0.0, 0.0),
                    unbound_barrier: None,
                    pending_barriers: Vec::new(),
                    acked_barrier: 0,
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
                send_frame_callbacks(w, time, commit)
            }
        }
        PageCommand::Retired(commit) => {
            let time = state.time();
            if let Some(w) = state.windows.get_mut(&id) {
                w.pointer_frames.remove(&commit);
                send_frame_callbacks(w, time, commit);
                w.events.send(BrowserEvent::FrameRetired(commit));
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
        return;
    };
    if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
        return;
    }
    window.size = (width, height);
    window.scale = scale;
    let Some(toplevel) = &window.toplevel else {
        window.unbound_barrier = Some(barrier);
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
}

fn track_unbound_barrier(window: &mut WindowState, serial: Serial) {
    if let Some(barrier) = window.unbound_barrier.take() {
        window.pending_barriers.push((serial, barrier));
    }
}
fn pointer_motion<K: BrowserPageKey>(state: &mut State<K>, id: K, commit_id: u64, x: f64, y: f64) {
    let Some(w) = state.windows.get(&id) else {
        return;
    };
    let Some(surface) = w.toplevel.as_ref().map(|t| t.wl_surface().clone()) else {
        return;
    };
    let Some(frame) = w.pointer_frames.get(&commit_id).copied() else {
        return;
    };
    let size = w.size;
    let (x, y) = mapped_pointer(frame, size, x, y);
    if let Some(window) = state.windows.get_mut(&id) {
        window.pointer_location = (x, y);
    }
    let event = MotionEvent {
        location: (x, y).into(),
        serial: state.next_serial(),
        time: state.time(),
    };
    let pointer = state.pointer.clone();
    pointer.motion(state, Some((surface, (0.0, 0.0).into())), &event);
    pointer.frame(state);
}

fn mapped_pointer(frame: PointerFrame, window_size: (u32, u32), x: f64, y: f64) -> (f64, f64) {
    (
        f64::from(frame.origin.0) + x.max(0.0) * f64::from(frame.size.0) / f64::from(window_size.0),
        f64::from(frame.origin.1) + y.max(0.0) * f64::from(frame.size.1) / f64::from(window_size.1),
    )
}
fn pointer_button<K: BrowserPageKey>(state: &mut State<K>, id: K, button: u32, pressed: bool) {
    let Some((surface, location)) = state.windows.get(&id).and_then(|window| {
        Some((
            window.toplevel.as_ref()?.wl_surface().clone(),
            window.pointer_location,
        ))
    }) else {
        return;
    };
    let pointer = state.pointer.clone();
    let serial = state.next_serial();
    let time = state.time();
    pointer.motion(
        state,
        Some((surface, (0.0, 0.0).into())),
        &MotionEvent {
            location: location.into(),
            serial,
            time,
        },
    );
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
    let Some((surface, location)) = state.windows.get(&id).and_then(|window| {
        Some((
            window.toplevel.as_ref()?.wl_surface().clone(),
            window.pointer_location,
        ))
    }) else {
        return;
    };
    let pointer = state.pointer.clone();
    let serial = state.next_serial();
    let time = state.time();
    pointer.motion(
        state,
        Some((surface, (0.0, 0.0).into())),
        &MotionEvent {
            location: location.into(),
            serial,
            time,
        },
    );
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
    let Some(surface) = state
        .windows
        .get(&id)
        .and_then(|w| w.toplevel.as_ref())
        .map(|t| t.wl_surface().clone())
    else {
        return;
    };
    let keyboard = state.keyboard.clone();
    let serial = state.next_serial();
    keyboard.set_focus(state, Some(surface), serial);
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

fn drain_frame_callbacks(toplevel: &ToplevelSurface) -> Vec<wl_callback::WlCallback> {
    let mut callbacks = Vec::new();
    with_surface_tree_downward(
        toplevel.wl_surface(),
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surface, states, &()| {
            callbacks.append(
                &mut states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks,
            );
        },
        |_, _, &()| true,
    );
    callbacks
}

fn send_frame_callbacks(window: &mut WindowState, time: u32, commit_id: u64) {
    if let Some(callbacks) = window.dma_frame_callbacks.remove(&commit_id) {
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
    fn only_shm_subsurfaces_may_be_discarded_without_composition() {
        use smithay::wayland::shell::xdg::{XDG_POPUP_ROLE, XDG_TOPLEVEL_ROLE};

        let shm = BufferType::Shm;
        let dma = BufferType::Dma;
        let cases = [
            (true, Some(XDG_TOPLEVEL_ROLE), Some(&shm), false),
            (true, Some(XDG_TOPLEVEL_ROLE), Some(&dma), false),
            (false, Some(SUBSURFACE_ROLE), Some(&shm), true),
            (false, Some(SUBSURFACE_ROLE), Some(&dma), false),
            (false, Some(SUBSURFACE_ROLE), None, false),
            (false, Some(XDG_POPUP_ROLE), Some(&shm), false),
        ];
        for (is_root, role, buffer_type, expected) in cases {
            assert_eq!(
                permits_uncomposited_shm_subsurface(is_root, role, buffer_type),
                expected,
                "is_root={is_root}, role={role:?}, buffer_type={buffer_type:?}"
            );
        }
    }

    #[test]
    fn chrome_wrapper_has_safe_default() {
        if std::env::var_os("RHO_CHROME_BIN").is_none() {
            assert_eq!(chrome_wrapper(), "google-chrome-stable");
        }
    }

    #[test]
    fn frame_mailbox_keeps_only_the_latest_dma_commit() {
        fn frame(id: u64, releases: Arc<AtomicUsize>) -> DmaBufFrame {
            let fd = std::fs::File::open("/dev/null").unwrap();
            let fence = std::fs::File::open("/dev/null").unwrap();
            DmaBufFrame {
                id,
                barrier: 0,
                width: 1,
                height: 1,
                fourcc: 0,
                modifier: 0,
                stride: 4,
                offset: 0,
                y_inverted: false,
                source_origin: (0, 0),
                source_size: (1, 1),
                fd: fd.into(),
                acquire_fence: fence.into(),
                release: Some(Box::new(move || {
                    releases.fetch_add(1, Ordering::SeqCst);
                })),
            }
        }

        let releases = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = browser_event_channel();
        sender.send(BrowserEvent::DmaBuf(frame(1, releases.clone())));
        sender.send(BrowserEvent::DmaBuf(frame(2, releases.clone())));
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        let event = futures_lite::future::block_on(receiver.recv()).unwrap();
        assert!(matches!(event, BrowserEvent::DmaBuf(frame) if frame.id == 2));
        assert_eq!(releases.load(Ordering::SeqCst), 2);
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
            pointer_location: (0.0, 0.0),
            unbound_barrier: Some(9),
            pending_barriers: Vec::new(),
            acked_barrier: 0,
            opened_at: Instant::now(),
        };

        track_unbound_barrier(&mut window, 42_u32.into());

        assert_eq!(window.unbound_barrier, None);
        assert_eq!(window.pending_barriers, vec![(42_u32.into(), 9)]);
    }

    #[test]
    fn converts_evdev_codes_to_xkb_codes() {
        assert_eq!(xkb_keycode(14), 22); // Backspace
        assert_eq!(xkb_keycode(28), 36); // Enter
    }

    #[test]
    fn clamps_window_geometry_to_dma_buf_source() {
        assert_eq!(
            clamped_source_rect(Some(((10, 20), (80, 60))), 120, 100),
            ((10, 20), (80, 60))
        );
        assert_eq!(
            clamped_source_rect(Some(((-5, 90), (20, 20))), 120, 100),
            ((0, 90), (15, 10))
        );
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
    fn pointer_coordinates_follow_cropped_source() {
        let frame = PointerFrame {
            origin: (10, 20),
            size: (80, 60),
        };
        assert_eq!(mapped_pointer(frame, (160, 120), 0.0, 0.0), (10.0, 20.0));
        assert_eq!(mapped_pointer(frame, (160, 120), 80.0, 60.0), (50.0, 50.0));
    }
}
