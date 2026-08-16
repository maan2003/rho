//! A private, protocol-only Wayland compositor for embedding stock Chrome.
//!
//! Chrome remains an unmodified Ozone/Wayland client. Rendering belongs to the
//! host GUI; this crate owns the shared Wayland protocol state and transfers
//! committed buffers to that host.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::input::{ButtonState, KeyState, Keycode};
use smithay::input::keyboard::{FilterResult, KeyboardHandle};
use smithay::input::pointer::{ButtonEvent, MotionEvent, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_callback, wl_seat, wl_shm};
use smithay::reexports::wayland_server::{Client, Display, ListeningSocket, Resource};
use smithay::utils::{Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SubsurfaceCachedState, SurfaceAttributes, TraversalAction, with_states,
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
use smithay::wayland::xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState as XdgSurfaceCachedState, ToplevelSurface,
    XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{
    delegate_compositor, delegate_dmabuf, delegate_drm_syncobj, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
};

#[derive(Clone, Debug)]
pub struct DmaBufConfig {
    pub render_node: PathBuf,
    pub device_id: u64,
    pub formats: Arc<[(u32, u64)]>,
}

pub struct DmaBufFrame {
    pub id: u64,
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

/// A copied Wayland SHM frame.
#[derive(Clone, Debug)]
pub struct ShmFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, straight-alpha RGBA8 pixels.
    pub rgba: Arc<[u8]>,
    surface_origin: (i32, i32),
}

#[derive(Debug)]
pub enum BrowserEvent {
    Frame(ShmFrame),
    DmaBuf(DmaBufFrame),
    Cleared,
    ToplevelReady,
    Failed(Arc<str>),
}

enum PageCommand {
    Resize(u32, u32),
    Presented(Option<u64>),
    Retired(u64),
    PointerMotion(f64, f64),
    PointerButton { button: u32, pressed: bool },
    Key { keycode: u32, pressed: bool },
    Close,
}
enum RuntimeCommand {
    Open {
        id: u64,
        size: (u32, u32),
        events: mpsc::Sender<BrowserEvent>,
        activation_reply: mpsc::SyncSender<String>,
    },
    Page(u64, PageCommand),
    Shutdown,
}
pub struct BrowserCompositor {
    commands: mpsc::Sender<RuntimeCommand>,
    socket: OsString,
    next_id: AtomicU64,
    thread: Option<thread::JoinHandle<()>>,
}
impl BrowserCompositor {
    pub fn launch(dma_buf: Option<DmaBufConfig>) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
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
            next_id: AtomicU64::new(1),
            thread: Some(thread),
        })
    }
    pub fn socket_name(&self) -> &std::ffi::OsStr {
        &self.socket
    }
    pub fn open(&self, size: (u32, u32)) -> Result<BrowserSession> {
        if size.0 == 0 || size.1 == 0 {
            bail!("browser dimensions must be nonzero")
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (events, rx) = mpsc::channel();
        let (activation_reply, activation_rx) = mpsc::sync_channel(1);
        self.commands
            .send(RuntimeCommand::Open {
                id,
                size,
                events,
                activation_reply,
            })
            .context("browser compositor stopped")?;
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
impl Drop for BrowserCompositor {
    fn drop(&mut self) {
        let _ = self.commands.send(RuntimeCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
pub struct BrowserSession {
    id: u64,
    commands: mpsc::Sender<RuntimeCommand>,
    events: mpsc::Receiver<BrowserEvent>,
    activation_token: String,
}
impl BrowserSession {
    fn send(&self, c: PageCommand) {
        let _ = self.commands.send(RuntimeCommand::Page(self.id, c));
    }
    pub fn activation_token(&self) -> &str {
        &self.activation_token
    }
    pub fn resize(&self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.send(PageCommand::Resize(w, h))
        }
    }
    pub fn presented(&self) {
        self.send(PageCommand::Presented(None))
    }
    pub fn pointer_motion(&self, x: f64, y: f64) {
        self.send(PageCommand::PointerMotion(x, y))
    }
    pub fn pointer_button(&self, button: u32, pressed: bool) {
        self.send(PageCommand::PointerButton { button, pressed })
    }
    pub fn key(&self, keycode: u32, pressed: bool) {
        self.send(PageCommand::Key { keycode, pressed })
    }
    pub fn try_recv(&self) -> Option<BrowserEvent> {
        self.events.try_recv().ok()
    }
    pub fn presentation_callback(&self, commit_id: u64) -> impl FnOnce() + Send + 'static {
        let tx = self.commands.clone();
        let id = self.id;
        move || {
            let _ = tx.send(RuntimeCommand::Page(
                id,
                PageCommand::Presented(Some(commit_id)),
            ));
        }
    }
}
impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.send(PageCommand::Close)
    }
}
struct WindowState {
    toplevel: Option<ToplevelSurface>,
    size: (u32, u32),
    events: mpsc::Sender<BrowserEvent>,
    dma_frame_callbacks: HashMap<u64, Vec<wl_callback::WlCallback>>,
    shm_surfaces: HashMap<ObjectId, ShmFrame>,
    pointer_frame: PointerFrame,
    pointer_location: (f64, f64),
    opened_at: Instant,
}

struct State {
    compositor: CompositorState,
    shell: XdgShellState,
    activation: XdgActivationState,
    _decoration: XdgDecorationState,
    shm: ShmState,
    dmabuf: DmabufState,
    _dmabuf_global: Option<DmabufGlobal>,
    syncobj: Option<DrmSyncobjState>,
    dma_formats: Arc<[(u32, u64)]>,
    _output: Output,
    seat_state: SeatState<Self>,
    _seat: Seat<Self>,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,
    serial: u32,
    started: Instant,
    windows: HashMap<u64, WindowState>,
    activation_windows: HashMap<String, u64>,
    unbound_toplevels: HashMap<ObjectId, (ToplevelSurface, Instant)>,
    pending_activations: HashMap<ObjectId, u64>,
    allow_initial_unambiguous_toplevel: bool,
    surface_windows: HashMap<ObjectId, u64>,
    next_buffer_id: u64,
    commands: mpsc::Sender<RuntimeCommand>,
}

#[derive(Clone, Copy)]
struct PointerFrame {
    origin: (i32, i32),
    size: (u32, u32),
}

impl State {
    fn next_serial(&mut self) -> Serial {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        serial.into()
    }

    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
    fn window_id_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        self.surface_windows.get(&root.id()).copied()
    }

    fn bind_toplevel(&mut self, id: u64, root: ObjectId) {
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
        let window = self.windows.get_mut(&id).expect("pending window exists");
        let size = window.size;
        surface.with_pending_state(|state| {
            state.size = Some((size.0 as i32, size.1 as i32).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        window.toplevel = Some(surface);
        self.allow_initial_unambiguous_toplevel = false;
        self.surface_windows.insert(root, id);
        self.activation_windows
            .retain(|_, window_id| *window_id != id);
        let _ = window.events.send(BrowserEvent::ToplevelReady);
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

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for State {
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
                let w = self.windows.get_mut(&wid).expect("known window");
                w.shm_surfaces.remove(&surface.id());
                if is_root {
                    let _ = w.events.send(BrowserEvent::Cleared);
                } else if let Some(t) = &w.toplevel {
                    let f = composite_shm_tree(t.wl_surface(), w.size, &w.shm_surfaces);
                    w.pointer_frame = PointerFrame {
                        origin: f.surface_origin,
                        size: (f.width, f.height),
                    };
                    let _ = w.events.send(BrowserEvent::Frame(f));
                }
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
        if let Ok(dmabuf) = get_dmabuf(&buffer) {
            if is_root && let (Some(a), Some(r)) = (acquire, release.take()) {
                let bid = self.next_buffer_id;
                self.next_buffer_id = self.next_buffer_id.wrapping_add(1).max(1);
                let source = dma_source_rect(surface, dmabuf.size().w, dmabuf.size().h);
                if let Ok(f) = dma_buf_frame(bid, dmabuf, source, a, r, wid, self.commands.clone())
                {
                    let w = self.windows.get_mut(&wid).expect("known window");
                    w.pointer_frame = PointerFrame {
                        origin: (f.source_origin.0 as i32, f.source_origin.1 as i32),
                        size: f.source_size,
                    };
                    if let Some(t) = &w.toplevel {
                        w.dma_frame_callbacks.insert(bid, drain_frame_callbacks(t));
                    }
                    let _ = w.events.send(BrowserEvent::DmaBuf(f));
                    return;
                }
                buffer.release();
                complete_surface_callbacks(surface, self.time());
                return;
            }
            if let Some(r) = release {
                let _ = r.signal();
            }
            buffer.release();
            complete_surface_callbacks(surface, self.time());
            return;
        }
        if let Some(r) = release {
            let _ = r.signal();
        }
        let time = self.time();
        if let Ok(f) = copy_shm_frame(&buffer) {
            let w = self.windows.get_mut(&wid).expect("known window");
            w.shm_surfaces.insert(surface.id(), f);
            if let Some(t) = &w.toplevel {
                let f = composite_shm_tree(t.wl_surface(), w.size, &w.shm_surfaces);
                w.pointer_frame = PointerFrame {
                    origin: f.surface_origin,
                    size: (f.width, f.height),
                };
                let _ = w.events.send(BrowserEvent::Frame(f));
            }
            // The copied producer buffer can be released below, but its frame
            // callbacks stay queued until GPUI reports an actual presentation.
        } else {
            complete_surface_callbacks(surface, time);
        }
        buffer.release();
    }
    fn destroyed(&mut self, surface: &WlSurface) {
        let Some(id) = self.window_id_for_surface(surface) else {
            self.unbound_toplevels.remove(&surface.id());
            self.pending_activations.remove(&surface.id());
            return;
        };
        if let Some(window) = self.windows.get_mut(&id) {
            window.shm_surfaces.remove(&surface.id());
        }
        // XdgShellHandler::toplevel_destroyed owns root teardown. Keeping the
        // route until then ensures either Smithay destruction callback order
        // removes the WindowState exactly once.
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| {
            s.size = Some((1280, 720).into());
            s.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        let root = surface.wl_surface().id();
        self.unbound_toplevels
            .insert(root.clone(), (surface, Instant::now()));
        if let Some(id) = self.pending_activations.remove(&root) {
            self.bind_toplevel(id, root);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let root = surface.wl_surface().id();
        self.unbound_toplevels.remove(&root);
        self.pending_activations.remove(&root);
        if let Some(id) = self.surface_windows.remove(&root)
            && let Some(w) = self.windows.remove(&id)
        {
            let _ = w.events.send(BrowserEvent::Cleared);
        }
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        let _ = surface.send_configure();
    }
    fn grab(&mut self, _: PopupSurface, _: wl_seat::WlSeat, _: Serial) {}
    fn reposition_request(&mut self, surface: PopupSurface, _: PositionerState, token: u32) {
        surface.send_repositioned(token);
    }
}

impl XdgActivationHandler for State {
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

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        set_server_side_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(toplevel);
    }
}

fn set_server_side_decoration(toplevel: ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
    toplevel.send_configure();
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl DmabufHandler for State {
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
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}
impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj.as_mut()
    }
}
impl OutputHandler for State {}

impl SeatHandler for State {
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

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_xdg_activation!(State);
delegate_xdg_decoration!(State);
delegate_shm!(State);
delegate_dmabuf!(State);
delegate_drm_syncobj!(State);
delegate_output!(State);
delegate_seat!(State);

fn copy_shm_frame(buffer: &wl_buffer::WlBuffer) -> Result<ShmFrame> {
    with_buffer_contents(buffer, |ptr, len, data| {
        if !matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        ) {
            bail!("unsupported Chrome SHM format {:?}", data.format);
        }
        let width = usize::try_from(data.width).context("negative SHM width")?;
        let height = usize::try_from(data.height).context("negative SHM height")?;
        let stride = usize::try_from(data.stride).context("negative SHM stride")?;
        let offset = usize::try_from(data.offset).context("negative SHM offset")?;
        let required = offset
            .checked_add(stride.checked_mul(height).context("SHM size overflow")?)
            .context("SHM range overflow")?;
        if stride < width * 4 || required > len {
            bail!("invalid Chrome SHM buffer bounds");
        }
        // The mapping may be mutated by Chrome, so copy it only inside Smithay's
        // guarded callback and never form a longer-lived reference.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let mut rgba = vec![0; width * height * 4];
        for y in 0..height {
            let src = &bytes[offset + y * stride..offset + y * stride + width * 4];
            let dst = &mut rgba[y * width * 4..(y + 1) * width * 4];
            let (source_pixels, source_remainder) = src.as_chunks::<4>();
            let (target_pixels, target_remainder) = dst.as_chunks_mut::<4>();
            debug_assert!(source_remainder.is_empty() && target_remainder.is_empty());
            for (source, target) in source_pixels.iter().zip(target_pixels) {
                let alpha = if data.format == wl_shm::Format::Argb8888 {
                    source[3]
                } else {
                    255
                };
                target.copy_from_slice(&[
                    unpremultiply(source[2], alpha),
                    unpremultiply(source[1], alpha),
                    unpremultiply(source[0], alpha),
                    alpha,
                ]);
            }
        }
        Ok(ShmFrame {
            width: width as u32,
            height: height as u32,
            rgba: rgba.into(),
            surface_origin: (0, 0),
        })
    })
    .context("read Chrome SHM buffer")?
}

fn dma_buf_frame(
    id: u64,
    dmabuf: &Dmabuf,
    source: ((u32, u32), (u32, u32)),
    acquire: DrmSyncPoint,
    release: DrmSyncPoint,
    window_id: u64,
    commands: mpsc::Sender<RuntimeCommand>,
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
) -> ((u32, u32), (u32, u32)) {
    let geometry = with_states(surface, |states| {
        let mut cached = states.cached_state.get::<XdgSurfaceCachedState>();
        cached.current().geometry
    });
    clamped_source_rect(
        geometry.map(|geometry| {
            (
                (geometry.loc.x, geometry.loc.y),
                (geometry.size.w, geometry.size.h),
            )
        }),
        buffer_width,
        buffer_height,
    )
}

fn clamped_source_rect(
    geometry: Option<((i32, i32), (i32, i32))>,
    buffer_width: i32,
    buffer_height: i32,
) -> ((u32, u32), (u32, u32)) {
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

fn composite_shm_tree(
    root: &WlSurface,
    size: (u32, u32),
    surfaces: &HashMap<smithay::reexports::wayland_server::backend::ObjectId, ShmFrame>,
) -> ShmFrame {
    let window_geometry = with_states(root, |states| {
        let mut cached = states.cached_state.get::<XdgSurfaceCachedState>();
        cached.current().geometry
    });
    let window_origin = window_geometry
        .map(|geometry| (geometry.loc.x, geometry.loc.y))
        .unwrap_or_default();
    let (width, height) = window_geometry
        .and_then(|geometry| {
            Some((
                u32::try_from(geometry.size.w).ok()?,
                u32::try_from(geometry.size.h).ok()?,
            ))
        })
        .filter(|&(width, height)| width > 0 && height > 0)
        .unwrap_or(size);
    let mut rgba = vec![0; width as usize * height as usize * 4];
    fn visit(
        surface: &WlSurface,
        parent: (i32, i32),
        root: &WlSurface,
        width: u32,
        height: u32,
        surfaces: &HashMap<smithay::reexports::wayland_server::backend::ObjectId, ShmFrame>,
        rgba: &mut [u8],
    ) {
        let location = if surface == root {
            (0, 0)
        } else {
            with_states(surface, |states| {
                let mut cached = states.cached_state.get::<SubsurfaceCachedState>();
                let point = cached.current().location;
                (point.x, point.y)
            })
        };
        let origin = (parent.0 + location.0, parent.1 + location.1);
        if let Some(frame) = surfaces.get(&surface.id()) {
            for sy in 0..frame.height as i32 {
                let dy = origin.1 + sy;
                if !(0..height as i32).contains(&dy) {
                    continue;
                }
                for sx in 0..frame.width as i32 {
                    let dx = origin.0 + sx;
                    if !(0..width as i32).contains(&dx) {
                        continue;
                    }
                    let source = ((sy as u32 * frame.width + sx as u32) * 4) as usize;
                    let target = ((dy as u32 * width + dx as u32) * 4) as usize;
                    let alpha = u32::from(frame.rgba[source + 3]);
                    let inverse = 255 - alpha;
                    for channel in 0..3 {
                        rgba[target + channel] = ((u32::from(frame.rgba[source + channel]) * alpha
                            + u32::from(rgba[target + channel]) * inverse
                            + 127)
                            / 255) as u8;
                    }
                    rgba[target + 3] =
                        (alpha + (u32::from(rgba[target + 3]) * inverse + 127) / 255) as u8;
                }
            }
        }
        for child in smithay::wayland::compositor::get_children(surface) {
            visit(&child, origin, root, width, height, surfaces, rgba);
        }
    }
    visit(
        root,
        (-window_origin.0, -window_origin.1),
        root,
        width,
        height,
        surfaces,
        &mut rgba,
    );
    let (width, height, rgba, trim_origin) = trim_translucent_margins(width, height, rgba);
    ShmFrame {
        width,
        height,
        rgba: rgba.into(),
        surface_origin: (
            window_origin.0 + trim_origin.0 as i32,
            window_origin.1 + trim_origin.1 as i32,
        ),
    }
}

fn trim_translucent_margins(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> (u32, u32, Vec<u8>, (u32, u32)) {
    let mut left = width;
    let mut top = height;
    let mut right = 0;
    let mut bottom = 0;
    for y in 0..height {
        for x in 0..width {
            let alpha = rgba[((y * width + x) * 4 + 3) as usize];
            if alpha >= 250 {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
    }
    if left == 0 && top == 0 && right == width && bottom == height || left >= right || top >= bottom
    {
        return (width, height, rgba, (0, 0));
    }
    let cropped_width = right - left;
    let cropped_height = bottom - top;
    let mut cropped = Vec::with_capacity((cropped_width * cropped_height * 4) as usize);
    for y in top..bottom {
        let start = ((y * width + left) * 4) as usize;
        let end = start + (cropped_width * 4) as usize;
        cropped.extend_from_slice(&rgba[start..end]);
    }
    (cropped_width, cropped_height, cropped, (left, top))
}

fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        0
    } else {
        ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
    }
}

fn run(
    commands: mpsc::Receiver<RuntimeCommand>,
    ready: mpsc::SyncSender<Result<OsString>>,
    config: Option<DmaBufConfig>,
    sender: mpsc::Sender<RuntimeCommand>,
) -> Result<()> {
    let mut display: Display<State> = Display::new().context("create private Wayland display")?;
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
    output.create_global::<State>(&dh);
    configure_output(&output, (1280, 720));
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "rho-browser");
    let keyboard = seat
        .add_keyboard(Default::default(), 600, 25)
        .context("create embedded Chrome keyboard")?;
    let pointer = seat.add_pointer();
    let mut dmabuf = DmabufState::new();
    let (mut global, mut syncobj, mut formats) = (None, None, Arc::from([]));
    if let Some(c) = &config {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&c.render_node)
            .with_context(|| format!("open DRM render node {}", c.render_node.display()))?;
        let drm = DrmDeviceFd::new(OwnedFd::from(file).into());
        if supports_syncobj_eventfd(&drm) {
            let fs = c
                .formats
                .iter()
                .filter_map(|&(fourcc, modifier)| {
                    Some(Format {
                        code: Fourcc::try_from(fourcc).ok()?,
                        modifier: Modifier::from(modifier),
                    })
                })
                .collect::<Vec<_>>();
            if !fs.is_empty() {
                let feedback = smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(
                    c.device_id as libc::dev_t,
                    fs,
                )
                .build()
                .context("build DMA-BUF feedback")?;
                global = Some(dmabuf.create_global_with_default_feedback::<State>(&dh, &feedback));
                syncobj = Some(DrmSyncobjState::new::<State>(&dh, drm));
                formats = Arc::clone(&c.formats);
            }
        }
    }
    let mut state = State {
        compositor: CompositorState::new::<State>(&dh),
        shell: XdgShellState::new::<State>(&dh),
        activation: XdgActivationState::new::<State>(&dh),
        _decoration: XdgDecorationState::new::<State>(&dh),
        shm: ShmState::new::<State>(&dh, vec![]),
        dmabuf,
        _dmabuf_global: global,
        syncobj,
        dma_formats: formats,
        _output: output,
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
    let result = service_loop(&mut display, &listener, &mut state, &commands);
    if let Err(error) = &result {
        let message: Arc<str> = format!("browser compositor failed: {error:#}").into();
        for window in state.windows.values() {
            let _ = window.events.send(BrowserEvent::Failed(message.clone()));
        }
    }
    result
}
fn service_loop(
    display: &mut Display<State>,
    listener: &ListeningSocket,
    state: &mut State,
    commands: &mpsc::Receiver<RuntimeCommand>,
) -> Result<()> {
    loop {
        while let Some(stream) = listener.accept().context("accept Chrome Wayland client")? {
            display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
                .context("register Chrome Wayland client")?;
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                RuntimeCommand::Open {
                    id,
                    size,
                    events,
                    activation_reply,
                } => {
                    let activation_token =
                        state.activation.create_external_token(None).0.to_string();
                    state
                        .activation_windows
                        .insert(activation_token.clone(), id);
                    let _ = activation_reply.send(activation_token);
                    state.windows.insert(
                        id,
                        WindowState {
                            toplevel: None,
                            size,
                            events,
                            dma_frame_callbacks: HashMap::new(),
                            shm_surfaces: HashMap::new(),
                            pointer_frame: PointerFrame {
                                origin: (0, 0),
                                size,
                            },
                            pointer_location: (0.0, 0.0),
                            opened_at: Instant::now(),
                        },
                    );
                }
                RuntimeCommand::Page(id, c) => handle_page_command(state, id, c),
                RuntimeCommand::Shutdown => return Ok(()),
            }
        }
        let expired = state
            .windows
            .iter()
            .filter(|(_, window)| {
                window.toplevel.is_none() && window.opened_at.elapsed() > Duration::from_secs(10)
            })
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        for id in expired {
            state
                .activation_windows
                .retain(|_, window_id| *window_id != id);
            state
                .pending_activations
                .retain(|_, window_id| *window_id != id);
            if let Some(window) = state.windows.remove(&id) {
                let _ = window.events.send(BrowserEvent::Failed(
                    "Chromium did not provide an unambiguous activated window".into(),
                ));
            }
        }
        state.unbound_toplevels.retain(|_, (surface, created)| {
            if created.elapsed() > Duration::from_secs(10) {
                surface.send_close();
                false
            } else {
                true
            }
        });
        display
            .dispatch_clients(state)
            .context("dispatch Chrome Wayland requests")?;
        // Some stock Chromium builds don't propagate activation tokens on the
        // first process launch. Dispatch queued token requests first, then
        // fall back only when one pending page and one top-level make the
        // match unambiguous.
        state.bind_unambiguous_toplevel();
        display
            .flush_clients()
            .context("flush Chrome Wayland events")?;
        thread::sleep(Duration::from_millis(2));
    }
}
fn handle_page_command(state: &mut State, id: u64, c: PageCommand) {
    match c {
        PageCommand::Resize(w, h) => resize(state, id, w, h),
        PageCommand::Presented(commit) => {
            let time = state.time();
            if let Some(w) = state.windows.get_mut(&id) {
                send_frame_callbacks(w, time, commit)
            }
        }
        PageCommand::Retired(commit) => {
            let time = state.time();
            if let Some(w) = state.windows.get_mut(&id) {
                send_frame_callbacks(w, time, Some(commit))
            }
        }
        PageCommand::PointerMotion(x, y) => pointer_motion(state, id, x, y),
        PageCommand::PointerButton { button, pressed } => {
            pointer_button(state, id, button, pressed)
        }
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

fn resize(state: &mut State, id: u64, width: u32, height: u32) {
    let Some(w) = state.windows.get_mut(&id) else {
        return;
    };
    if width == 0 || height == 0 || w.size == (width, height) {
        return;
    }
    w.size = (width, height);
    if let Some(t) = &w.toplevel {
        t.with_pending_state(|p| p.size = Some((width as i32, height as i32).into()));
        t.send_configure();
    }
}
fn pointer_motion(state: &mut State, id: u64, x: f64, y: f64) {
    let Some(w) = state.windows.get(&id) else {
        return;
    };
    let Some(surface) = w.toplevel.as_ref().map(|t| t.wl_surface().clone()) else {
        return;
    };
    let (frame, size) = (w.pointer_frame, w.size);
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
fn pointer_button(state: &mut State, id: u64, button: u32, pressed: bool) {
    let Some((surface, location)) = state.windows.get(&id).and_then(|window| {
        Some((
            window.toplevel.as_ref()?.wl_surface().clone(),
            window.pointer_location,
        ))
    }) else {
        return;
    };
    let serial = state.next_serial();
    let time = state.time();
    let pointer = state.pointer.clone();
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
fn keyboard_key(state: &mut State, id: u64, keycode: u32, pressed: bool) {
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

fn send_frame_callbacks(window: &mut WindowState, time: u32, commit_id: Option<u64>) {
    if let Some(id) = commit_id {
        if let Some(callbacks) = window.dma_frame_callbacks.remove(&id) {
            for callback in callbacks {
                callback.done(time);
            }
        }
        return;
    }
    let Some(toplevel) = &window.toplevel else {
        return;
    };
    with_surface_tree_downward(
        toplevel.wl_surface(),
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_, states, &()| {
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

/// Resolves the Chrome wrapper without ever selecting an underlying ELF.
pub fn chrome_wrapper() -> OsString {
    std::env::var_os("RHO_CHROME_BIN").unwrap_or_else(|| OsString::from("google-chrome-stable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_wrapper_has_safe_default() {
        if std::env::var_os("RHO_CHROME_BIN").is_none() {
            assert_eq!(chrome_wrapper(), "google-chrome-stable");
        }
    }

    #[test]
    fn trims_only_translucent_surface_margins() {
        let mut rgba = vec![0; 4 * 3 * 4];
        for y in 1..3 {
            for x in 1..4 {
                rgba[((y * 4 + x) * 4 + 3) as usize] = 255;
            }
        }
        let (width, height, cropped, origin) = trim_translucent_margins(4, 3, rgba);
        assert_eq!((width, height), (3, 2));
        assert_eq!(origin, (1, 1));
        assert!(
            cropped
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );
    }

    #[test]
    fn unpremultiplies_argb_channels_for_gpui() {
        assert_eq!(unpremultiply(0, 0), 0);
        assert_eq!(unpremultiply(64, 128), 128);
        assert_eq!(unpremultiply(255, 128), 255);
        assert_eq!(unpremultiply(37, 255), 37);
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
    fn pointer_coordinates_follow_cropped_source() {
        let frame = PointerFrame {
            origin: (10, 20),
            size: (80, 60),
        };
        assert_eq!(mapped_pointer(frame, (160, 120), 0.0, 0.0), (10.0, 20.0));
        assert_eq!(mapped_pointer(frame, (160, 120), 80.0, 60.0), (50.0, 50.0));
    }
}
