//! A private, protocol-only Wayland compositor for embedding stock Chrome.
//!
//! Chrome remains an unmodified Ozone/Wayland client. Rendering belongs to the
//! host GUI; this crate only owns Wayland protocol state, Chrome's process and
//! the transfer of committed buffers to that host.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
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
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
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
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState as XdgSurfaceCachedState, ToplevelSurface,
    XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{
    delegate_compositor, delegate_dmabuf, delegate_drm_syncobj, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
};

#[derive(Clone, Debug)]
pub struct DmaBufConfig {
    pub render_node: PathBuf,
    pub device_id: u64,
    pub formats: Arc<[(u32, u64)]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserProgram {
    Chromium,
    Firefox,
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
    ChromeExited(Option<i32>),
    Failed(Arc<str>),
}

enum CommandMessage {
    Resize(u32, u32),
    Presented(Option<u64>),
    Retired(u64),
    PointerMotion(f64, f64),
    PointerButton { button: u32, pressed: bool },
    Key { keycode: u32, pressed: bool },
    Shutdown,
}

/// A running stock-Chrome instance and its private Wayland display.
pub struct BrowserSession {
    commands: mpsc::Sender<CommandMessage>,
    events: mpsc::Receiver<BrowserEvent>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BrowserSession {
    pub fn launch(
        program: BrowserProgram,
        executable: impl AsRef<Path>,
        url: &str,
        initial_size: (u32, u32),
        dma_buf: Option<DmaBufConfig>,
    ) -> Result<Self> {
        let executable = executable.as_ref().to_owned();
        let url = url.to_owned();
        if initial_size.0 == 0 || initial_size.1 == 0 {
            bail!("browser dimensions must be nonzero");
        }
        let (command_tx, command_rx) = mpsc::channel();
        let compositor_commands = command_tx.clone();
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let ready_errors = ready_tx.clone();
        let thread = thread::Builder::new()
            .name("rho-browser-wayland".into())
            .spawn(move || {
                let result = run(
                    program,
                    executable,
                    url,
                    initial_size,
                    command_rx,
                    event_tx.clone(),
                    ready_tx,
                    dma_buf,
                    compositor_commands,
                );
                if let Err(error) = result {
                    let _ = ready_errors.send(Err(anyhow::anyhow!("{error:#}")));
                    let _ = event_tx.send(BrowserEvent::Failed(format!("{error:#}").into()));
                }
            })
            .context("spawn browser compositor thread")?;
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .context("browser compositor did not start")??;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        })
    }

    pub fn resize(&self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            let _ = self.commands.send(CommandMessage::Resize(width, height));
        }
    }

    /// Completes Chrome's pending frame callbacks after the host painted.
    pub fn presented(&self) {
        let _ = self.commands.send(CommandMessage::Presented(None));
    }

    pub fn pointer_motion(&self, x: f64, y: f64) {
        let _ = self.commands.send(CommandMessage::PointerMotion(x, y));
    }

    pub fn pointer_button(&self, button: u32, pressed: bool) {
        let _ = self
            .commands
            .send(CommandMessage::PointerButton { button, pressed });
    }

    pub fn key(&self, keycode: u32, pressed: bool) {
        let _ = self.commands.send(CommandMessage::Key { keycode, pressed });
    }

    pub fn try_recv(&self) -> Option<BrowserEvent> {
        self.events.try_recv().ok()
    }

    pub fn presentation_callback(&self, commit_id: u64) -> impl FnOnce() + Send + 'static {
        let commands = self.commands.clone();
        move || {
            let _ = commands.send(CommandMessage::Presented(Some(commit_id)));
        }
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct State {
    compositor: CompositorState,
    shell: XdgShellState,
    _decoration: XdgDecorationState,
    shm: ShmState,
    dmabuf: DmabufState,
    _dmabuf_global: Option<DmabufGlobal>,
    syncobj: Option<DrmSyncobjState>,
    dma_formats: Arc<[(u32, u64)]>,
    output: Output,
    seat_state: SeatState<Self>,
    _seat: Seat<Self>,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,
    serial: u32,
    started: Instant,
    toplevel: Option<ToplevelSurface>,
    size: (u32, u32),
    events: mpsc::Sender<BrowserEvent>,
    next_buffer_id: u64,
    dma_frame_callbacks: HashMap<u64, Vec<wl_callback::WlCallback>>,
    shm_surfaces: HashMap<smithay::reexports::wayland_server::backend::ObjectId, ShmFrame>,
    commands: mpsc::Sender<CommandMessage>,
    pointer_frame: PointerFrame,
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
        let is_root = self
            .toplevel
            .as_ref()
            .is_some_and(|top| top.wl_surface() == surface);
        let belongs_to_toplevel = self.toplevel.as_ref().is_some_and(|top| {
            let mut current = surface.clone();
            while let Some(parent) = smithay::wayland::compositor::get_parent(&current) {
                current = parent;
            }
            current == *top.wl_surface()
        });
        let (buffer, sync_points) = with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceAttributes>();
            let attrs = cached.current();
            let buffer = attrs.buffer.take();
            let mut sync = states.cached_state.get::<DrmSyncobjCachedState>();
            let sync = sync.current();
            (
                buffer,
                (sync.acquire_point.take(), sync.release_point.take()),
            )
        });
        let (acquire, mut release) = sync_points;
        let buffer = match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => buffer,
            Some(BufferAssignment::Removed) => {
                if let Some(release) = release {
                    let _ = release.signal();
                }
                self.shm_surfaces.remove(&surface.id());
                if is_root {
                    let _ = self.events.send(BrowserEvent::Cleared);
                } else if belongs_to_toplevel && let Some(toplevel) = &self.toplevel {
                    let frame =
                        composite_shm_tree(toplevel.wl_surface(), self.size, &self.shm_surfaces);
                    self.pointer_frame = PointerFrame {
                        origin: frame.surface_origin,
                        size: (frame.width, frame.height),
                    };
                    let _ = self.events.send(BrowserEvent::Frame(frame));
                }
                complete_surface_callbacks(surface, self.time());
                return;
            }
            None => {
                if let Some(release) = release {
                    let _ = release.signal();
                }
                complete_surface_callbacks(surface, self.time());
                return;
            }
        };
        if !belongs_to_toplevel {
            if let Some(release) = release {
                let _ = release.signal();
            }
            buffer.release();
            complete_surface_callbacks(surface, self.time());
            return;
        }
        if let Ok(dmabuf) = get_dmabuf(&buffer) {
            if is_root && let (Some(acquire), Some(release)) = (acquire, release.take()) {
                if let Ok(frame) = dma_buf_frame(
                    self.next_buffer_id,
                    dmabuf,
                    acquire,
                    release,
                    self.commands.clone(),
                ) {
                    self.pointer_frame = PointerFrame {
                        origin: (0, 0),
                        size: (frame.width, frame.height),
                    };
                    if let Some(toplevel) = &self.toplevel {
                        self.dma_frame_callbacks
                            .insert(self.next_buffer_id, drain_frame_callbacks(toplevel));
                    }
                    self.next_buffer_id = self.next_buffer_id.wrapping_add(1).max(1);
                    let _ = self.events.send(BrowserEvent::DmaBuf(frame));
                    return;
                }
                buffer.release();
                complete_surface_callbacks(surface, self.time());
                return;
            }
            if let Some(release) = release {
                let _ = release.signal();
            }
            buffer.release();
            complete_surface_callbacks(surface, self.time());
            return;
        }
        if let Some(release) = release {
            let _ = release.signal();
        }
        if let Ok(frame) = copy_shm_frame(&buffer) {
            self.shm_surfaces.insert(surface.id(), frame);
            if let Some(toplevel) = &self.toplevel {
                let frame =
                    composite_shm_tree(toplevel.wl_surface(), self.size, &self.shm_surfaces);
                self.pointer_frame = PointerFrame {
                    origin: frame.surface_origin,
                    size: (frame.width, frame.height),
                };
                let _ = self.events.send(BrowserEvent::Frame(frame));
            }
            // The producer buffer is no longer needed once its pixels have
            // been copied, so SHM frame callbacks need not wait for GPUI.
            send_frame_callbacks(self, None);
        } else {
            complete_surface_callbacks(surface, self.time());
        }
        buffer.release();
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.shm_surfaces.remove(&surface.id());
        if self
            .toplevel
            .as_ref()
            .is_some_and(|toplevel| toplevel.wl_surface() == surface)
        {
            self.toplevel = None;
            let _ = self.events.send(BrowserEvent::Cleared);
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let size = self.size;
        surface.with_pending_state(|state| {
            state.size = Some((size.0 as i32, size.1 as i32).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        self.toplevel = Some(surface);
        let keyboard = self.keyboard.clone();
        let serial = self.next_serial();
        let focus = self.toplevel.as_ref().map(|top| top.wl_surface().clone());
        keyboard.set_focus(self, focus, serial);
        let _ = self.events.send(BrowserEvent::ToplevelReady);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        _positioner: PositionerState,
        token: u32,
    ) {
        surface.send_repositioned(token);
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
            for (source, target) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
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
    acquire: DrmSyncPoint,
    release: DrmSyncPoint,
    commands: mpsc::Sender<CommandMessage>,
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
        fd,
        acquire_fence,
        release: Some(Box::new(move || {
            let _keep_alive = keep_alive;
            if let Err(error) = release.signal() {
                tracing::error!(?error, "signal Chrome DMA-BUF release point");
            }
            let _ = commands.send(CommandMessage::Retired(id));
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
    program: BrowserProgram,
    executable: PathBuf,
    url: String,
    initial_size: (u32, u32),
    commands: mpsc::Receiver<CommandMessage>,
    events: mpsc::Sender<BrowserEvent>,
    ready: mpsc::SyncSender<Result<()>>,
    dma_buf_config: Option<DmaBufConfig>,
    command_sender: mpsc::Sender<CommandMessage>,
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
    configure_output(&output, initial_size);
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "rho-browser");
    let keyboard = seat
        .add_keyboard(Default::default(), 600, 25)
        .context("create embedded Chrome keyboard")?;
    let pointer = seat.add_pointer();
    let started = Instant::now();
    let mut dmabuf = DmabufState::new();
    let mut dmabuf_global = None;
    let mut syncobj = None;
    let mut dma_formats: Arc<[(u32, u64)]> = Arc::from([]);
    if let Some(config) = &dma_buf_config {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&config.render_node)
            .with_context(|| format!("open DRM render node {}", config.render_node.display()))?;
        let drm = DrmDeviceFd::new(OwnedFd::from(file).into());
        if supports_syncobj_eventfd(&drm) {
            let formats = config
                .formats
                .iter()
                .filter_map(|&(fourcc, modifier)| {
                    Some(Format {
                        code: Fourcc::try_from(fourcc).ok()?,
                        modifier: Modifier::from(modifier),
                    })
                })
                .collect::<Vec<_>>();
            if !formats.is_empty() {
                let feedback = smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(
                    config.device_id as libc::dev_t,
                    formats,
                )
                .build()
                .context("build DMA-BUF feedback")?;
                dmabuf_global =
                    Some(dmabuf.create_global_with_default_feedback::<State>(&dh, &feedback));
                syncobj = Some(DrmSyncobjState::new::<State>(&dh, drm));
                dma_formats = Arc::clone(&config.formats);
            }
        }
    }
    let mut state = State {
        compositor: CompositorState::new::<State>(&dh),
        shell: XdgShellState::new::<State>(&dh),
        _decoration: XdgDecorationState::new::<State>(&dh),
        shm: ShmState::new::<State>(&dh, vec![]),
        dmabuf,
        _dmabuf_global: dmabuf_global,
        syncobj,
        dma_formats,
        output,
        seat_state,
        _seat: seat,
        keyboard,
        pointer,
        serial: 1,
        started,
        toplevel: None,
        size: initial_size,
        events,
        next_buffer_id: 1,
        dma_frame_callbacks: HashMap::new(),
        shm_surfaces: HashMap::new(),
        commands: command_sender,
        pointer_frame: PointerFrame {
            origin: (0, 0),
            size: initial_size,
        },
    };
    let profile = tempfile::Builder::new()
        .prefix("rho-chrome-profile-")
        .tempdir()
        .context("create private Chrome profile")?;
    let mut child = spawn_browser(
        program,
        &executable,
        &url,
        &socket,
        profile.path(),
        state.syncobj.is_some(),
    )?;
    let _ = ready.send(Ok(()));

    let result = service_loop(&mut display, &listener, &mut state, &commands, &mut child);
    stop_child(&mut child);
    result
}

fn service_loop(
    display: &mut Display<State>,
    listener: &ListeningSocket,
    state: &mut State,
    commands: &mpsc::Receiver<CommandMessage>,
    child: &mut Child,
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
                CommandMessage::Resize(width, height) => resize(state, width, height),
                CommandMessage::Presented(commit_id) => send_frame_callbacks(state, commit_id),
                CommandMessage::Retired(commit_id) => send_frame_callbacks(state, Some(commit_id)),
                CommandMessage::PointerMotion(x, y) => pointer_motion(state, x, y),
                CommandMessage::PointerButton { button, pressed } => {
                    pointer_button(state, button, pressed)
                }
                CommandMessage::Key { keycode, pressed } => keyboard_key(state, keycode, pressed),
                CommandMessage::Shutdown => {
                    return Ok(());
                }
            }
        }
        display
            .dispatch_clients(state)
            .context("dispatch Chrome Wayland requests")?;
        display
            .flush_clients()
            .context("flush Chrome Wayland events")?;
        if let Some(status) = child.try_wait().context("poll Chrome")? {
            let _ = state.events.send(BrowserEvent::ChromeExited(status.code()));
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
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

fn resize(state: &mut State, width: u32, height: u32) {
    if width == 0 || height == 0 || state.size == (width, height) {
        return;
    }
    state.size = (width, height);
    configure_output(&state.output, state.size);
    if let Some(surface) = &state.toplevel {
        surface.with_pending_state(|pending| {
            pending.size = Some((width as i32, height as i32).into());
        });
        surface.send_configure();
    }
}

fn pointer_motion(state: &mut State, x: f64, y: f64) {
    let Some(surface) = state.toplevel.as_ref().map(|top| top.wl_surface().clone()) else {
        return;
    };
    let frame = state.pointer_frame;
    let x =
        f64::from(frame.origin.0) + x.max(0.0) * f64::from(frame.size.0) / f64::from(state.size.0);
    let y =
        f64::from(frame.origin.1) + y.max(0.0) * f64::from(frame.size.1) / f64::from(state.size.1);
    let event = MotionEvent {
        location: (x, y).into(),
        serial: state.next_serial(),
        time: state.time(),
    };
    let pointer = state.pointer.clone();
    pointer.motion(state, Some((surface, (0.0, 0.0).into())), &event);
    pointer.frame(state);
}

fn pointer_button(state: &mut State, button: u32, pressed: bool) {
    let event = ButtonEvent {
        serial: state.next_serial(),
        time: state.time(),
        button,
        state: if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
    };
    let pointer = state.pointer.clone();
    pointer.button(state, &event);
    pointer.frame(state);
}

fn keyboard_key(state: &mut State, keycode: u32, pressed: bool) {
    let keyboard = state.keyboard.clone();
    let serial = state.next_serial();
    let time = state.time();
    keyboard.input::<(), _>(
        state,
        Keycode::from(keycode),
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
            callbacks.extend(
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks
                    .drain(..),
            );
        },
        |_, _, &()| true,
    );
    callbacks
}

fn send_frame_callbacks(state: &mut State, commit_id: Option<u64>) {
    let time = state.started.elapsed().as_millis() as u32;
    if let Some(commit_id) = commit_id {
        if let Some(callbacks) = state.dma_frame_callbacks.remove(&commit_id) {
            for callback in callbacks {
                callback.done(time);
            }
        }
        return;
    }
    let Some(toplevel) = &state.toplevel else {
        return;
    };
    let mut sent = 0usize;
    with_surface_tree_downward(
        toplevel.wl_surface(),
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
                sent += 1;
            }
        },
        |_, _, &()| true,
    );
    tracing::debug!(sent, "completed embedded browser frame callbacks");
}

fn spawn_browser(
    program: BrowserProgram,
    executable: &Path,
    url: &str,
    socket: &std::ffi::OsStr,
    profile: &Path,
    explicit_sync: bool,
) -> Result<Child> {
    let mut command = Command::new(executable);
    command
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    match program {
        BrowserProgram::Chromium => {
            command
                .arg("--ozone-platform=wayland")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg(format!("--user-data-dir={}", profile.display()))
                .arg(format!("--app={url}"));
            if !explicit_sync {
                command.arg("--disable-features=WaylandLinuxDrmSyncobj");
            }
        }
        BrowserProgram::Firefox => {
            command
                .env("MOZ_ENABLE_WAYLAND", "1")
                .arg("--no-remote")
                .arg("--new-instance")
                .arg("--profile")
                .arg(profile)
                .arg("--new-window")
                .arg(url);
        }
    }
    // Put Chrome and all descendants in their own process group for teardown.
    command.process_group(0);
    command
        .spawn()
        .with_context(|| format!("launch stock {program:?} wrapper {}", executable.display()))
}

fn stop_child(child: &mut Child) {
    let process_group = -(child.id() as i32);
    // Chrome is a process tree. Signal the dedicated group so renderer/GPU
    // children cannot survive a closed pane.
    unsafe { libc::kill(process_group, libc::SIGTERM) };
    for _ in 0..20 {
        if !process_group_exists(process_group) {
            let _ = child.wait();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    unsafe { libc::kill(process_group, libc::SIGKILL) };
    let _ = child.wait();
}

fn process_group_exists(process_group: i32) -> bool {
    let result = unsafe { libc::kill(process_group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Resolves the Chrome wrapper without ever selecting an underlying ELF.
pub fn chrome_wrapper() -> OsString {
    std::env::var_os("RHO_CHROME_BIN").unwrap_or_else(|| OsString::from("google-chrome-stable"))
}

pub fn firefox_wrapper() -> OsString {
    std::env::var_os("RHO_FIREFOX_BIN").unwrap_or_else(|| OsString::from("firefox"))
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
        assert!(cropped.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn unpremultiplies_argb_channels_for_gpui() {
        assert_eq!(unpremultiply(0, 0), 0);
        assert_eq!(unpremultiply(64, 128), 128);
        assert_eq!(unpremultiply(255, 128), 255);
        assert_eq!(unpremultiply(37, 255), 37);
    }
}
