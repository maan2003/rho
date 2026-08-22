use std::{
    collections::{HashMap, HashSet},
    fs::File,
    os::fd::{AsFd, AsRawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use calloop::{EventLoop, LoopHandle, LoopSignal, PostAction, channel, generic::Generic};
use calloop_wayland_source::WaylandSource;
use gpui::{
    Bounds, LinuxWaylandPassthrough, LinuxWaylandPassthroughBuffer, LinuxWaylandPassthroughEvent,
    Pixels,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_registry, wl_subcompositor, wl_subsurface,
        wl_surface,
    },
};
use wayland_protocols::wp::{
    linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
    linux_explicit_synchronization::zv1::client::{
        zwp_linux_buffer_release_v1, zwp_linux_explicit_synchronization_v1,
        zwp_linux_surface_synchronization_v1,
    },
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
    viewporter::client::{wp_viewport, wp_viewporter},
};

type EventSink = Arc<dyn Fn(LinuxWaylandPassthroughEvent) + Send + Sync>;

enum Command {
    Geometry(Bounds<Pixels>, mpsc::SyncSender<Result<()>>),
    Present(
        u64,
        LinuxWaylandPassthroughBuffer,
        mpsc::SyncSender<Result<()>>,
    ),
    Hide,
    Stop,
}

struct Handle {
    commands: channel::Sender<Command>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.get_mut().unwrap().take() {
            let _ = thread.join();
        }
    }
}

impl LinuxWaylandPassthrough for Handle {
    fn set_geometry(&self, bounds: Bounds<Pixels>) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Geometry(bounds, tx))
            .map_err(|_| anyhow::anyhow!("Wayland passthrough thread stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("Wayland passthrough thread stopped"))?
    }

    fn present(&self, scene_id: u64, buffer: LinuxWaylandPassthroughBuffer) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Present(scene_id, buffer, tx))
            .map_err(|_| anyhow::anyhow!("Wayland passthrough thread stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("Wayland passthrough thread stopped"))?
    }

    fn hide(&self) {
        let _ = self.commands.send(Command::Hide);
    }
}

struct BufferData {
    surface: gpui::LinuxDmaBufSurface,
    lease_id: u64,
}

#[derive(Clone, Copy)]
struct CommitData(u64);

struct State {
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    viewporter: wp_viewporter::WpViewporter,
    dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    synchronization: zwp_linux_surface_synchronization_v1::ZwpLinuxSurfaceSynchronizationV1,
    presentation: wp_presentation::WpPresentation,
    explicit: zwp_linux_explicit_synchronization_v1::ZwpLinuxExplicitSynchronizationV1,
    qh: QueueHandle<Self>,
    events: EventSink,
    buffers: HashMap<u64, wl_buffer::WlBuffer>,
    formats: HashSet<(u32, u64)>,
    presentation_clock_monotonic: bool,
    loop_handle: LoopHandle<'static, Self>,
    owner_id: u64,
    signal: LoopSignal,
    stopping: bool,
    pending_releases: usize,
    connection: Connection,
    finalized: bool,
}

impl State {
    fn geometry(&self, bounds: Bounds<Pixels>) -> Result<()> {
        let x = f32::from(bounds.origin.x).round() as i32;
        let y = f32::from(bounds.origin.y).round() as i32;
        let width = f32::from(bounds.size.width).round() as i32;
        let height = f32::from(bounds.size.height).round() as i32;
        if width <= 0 || height <= 0 {
            bail!("Wayland passthrough has empty geometry");
        }
        self.subsurface.set_position(x, y);
        self.viewport.set_destination(width, height);
        Ok(())
    }

    fn present(&mut self, scene_id: u64, buffer: LinuxWaylandPassthroughBuffer) -> Result<()> {
        if !self.presentation_clock_monotonic {
            bail!("host wp_presentation clock is not CLOCK_MONOTONIC");
        }
        if !self.formats.contains(&(buffer.fourcc, buffer.modifier)) {
            bail!(
                "host compositor does not advertise DMA-BUF format {:#x} modifier {:#x}",
                buffer.fourcc,
                buffer.modifier
            );
        }
        if !buffer.surface.claim_wayland_passthrough(self.owner_id) {
            bail!("DMA-BUF lease is already claimed by another presentation path");
        }
        let lease_id = buffer.surface.lease_id();
        if buffer.surface.is_released() {
            bail!("DMA-BUF lease was already released by this passthrough surface");
        }
        let new_buffer = !self.buffers.contains_key(&lease_id);
        if new_buffer {
            let params = self.dmabuf.create_params(&self.qh, ());
            let modifier_hi = (buffer.modifier >> 32) as u32;
            let modifier_lo = buffer.modifier as u32;
            for (index, plane) in buffer.planes.into_iter().enumerate() {
                params.add(
                    plane.fd.as_fd(),
                    index.try_into().context("too many DMA-BUF planes")?,
                    plane.offset,
                    plane.stride,
                    modifier_hi,
                    modifier_lo,
                );
            }
            let wl_buffer = params.create_immed(
                buffer
                    .width
                    .try_into()
                    .context("DMA-BUF width exceeds Wayland")?,
                buffer
                    .height
                    .try_into()
                    .context("DMA-BUF height exceeds Wayland")?,
                buffer.fourcc,
                if buffer.y_inverted {
                    zwp_linux_buffer_params_v1::Flags::YInvert
                } else {
                    zwp_linux_buffer_params_v1::Flags::empty()
                },
                &self.qh,
                BufferData {
                    surface: buffer.surface.clone(),
                    lease_id,
                },
            );
            params.destroy();
            self.buffers.insert(lease_id, wl_buffer.clone());
            self.surface.attach(Some(&wl_buffer), 0, 0);
            self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            self.synchronization
                .set_acquire_fence(buffer.acquire_fence.as_fd());
            self.synchronization.get_release(
                &self.qh,
                BufferData {
                    surface: buffer.surface.clone(),
                    lease_id,
                },
            );
            self.pending_releases += 1;
            buffer.surface.submitted();
        }

        self.surface.frame(&self.qh, CommitData(scene_id));
        self.presentation
            .feedback(&self.surface, &self.qh, CommitData(scene_id));
        self.surface.commit();
        Ok(())
    }

    fn hide(&self) {
        self.surface.attach(None, 0, 0);
        self.surface.commit();
    }

    fn release_finished(&mut self) {
        self.pending_releases = self.pending_releases.saturating_sub(1);
        if self.stopping && self.pending_releases == 0 {
            self.finalize_stop();
        }
    }

    fn finalize_stop(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.hide();
        self.synchronization.destroy();
        self.viewport.destroy();
        self.subsurface.destroy();
        self.surface.destroy();
        self.dmabuf.destroy();
        self.presentation.destroy();
        self.explicit.destroy();
        self.viewporter.destroy();
        if let Err(error) = self.connection.flush() {
            log::warn!("flush Wayland passthrough teardown: {error}");
        }
        self.signal.stop();
    }
}

pub(super) fn create(
    connection: Connection,
    parent: wl_surface::WlSurface,
    events: EventSink,
) -> Result<Arc<dyn LinuxWaylandPassthrough>> {
    static NEXT_OWNER_ID: AtomicU64 = AtomicU64::new(1);
    let owner_id = NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed);
    let (commands, receiver) = channel::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("gpui-wayland-passthrough".into())
        .spawn(move || {
            let result = run(connection, parent, events, receiver, ready_tx, owner_id);
            if let Err(error) = result {
                log::error!("Wayland passthrough thread failed: {error:#}");
            }
        })
        .context("spawn Wayland passthrough thread")?;
    ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("Wayland passthrough setup thread stopped"))??;
    Ok(Arc::new(Handle {
        commands,
        thread: Mutex::new(Some(thread)),
    }))
}

fn run(
    connection: Connection,
    parent: wl_surface::WlSurface,
    events: EventSink,
    receiver: channel::Channel<Command>,
    ready: mpsc::SyncSender<Result<()>>,
    owner_id: u64,
) -> Result<()> {
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).context("create passthrough event queue")?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .context("host lacks wl_compositor")?;
    let subcompositor: wl_subcompositor::WlSubcompositor = globals
        .bind(&qh, 1..=1, ())
        .context("host lacks wl_subcompositor")?;
    let viewporter: wp_viewporter::WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .context("host lacks wp_viewporter")?;
    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals
        // Bind v3 deliberately: its format/modifier events provide the safe
        // import intersection without parsing v4's feedback-table mmap.
        .bind(&qh, 3..=3, ())
        .context("host lacks linux-dmabuf v3")?;
    let explicit: zwp_linux_explicit_synchronization_v1::ZwpLinuxExplicitSynchronizationV1 =
        globals
            .bind(&qh, 1..=2, ())
            .context("host lacks linux explicit synchronization")?;
    let presentation: wp_presentation::WpPresentation = globals
        .bind(&qh, 1..=2, ())
        .context("host lacks wp_presentation")?;

    let surface = compositor.create_surface(&qh, ());
    let subsurface = subcompositor.get_subsurface(&surface, &parent, &qh, ());
    subsurface.place_below(&parent);
    subsurface.set_desync();
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    let synchronization = explicit.get_synchronization(&surface, &qh, ());
    let mut event_loop = EventLoop::<State>::try_new().context("create passthrough event loop")?;
    let mut state = State {
        surface,
        subsurface,
        viewport,
        viewporter,
        dmabuf,
        synchronization,
        presentation,
        explicit,
        qh,
        events,
        buffers: HashMap::new(),
        formats: HashSet::new(),
        presentation_clock_monotonic: false,
        loop_handle: event_loop.handle(),
        owner_id,
        signal: event_loop.get_signal(),
        stopping: false,
        pending_releases: 0,
        connection: connection.clone(),
        finalized: false,
    };
    queue
        .roundtrip(&mut state)
        .context("initialize passthrough globals")?;

    event_loop
        .handle()
        .insert_source(receiver, move |event, _, state| {
            let channel::Event::Msg(command) = event else {
                return;
            };
            match command {
                Command::Geometry(bounds, reply) => {
                    let _ = reply.send(state.geometry(bounds));
                }
                Command::Present(scene_id, buffer, reply) => {
                    let _ = reply.send(state.present(scene_id, buffer));
                }
                Command::Hide => state.hide(),
                Command::Stop => {
                    state.hide();
                    state.stopping = true;
                    if state.pending_releases == 0 {
                        state.finalize_stop();
                    }
                }
            }
        })
        .map_err(|_| anyhow::anyhow!("install passthrough command queue"))?;
    WaylandSource::new(connection, queue)
        .insert(event_loop.handle())
        .map_err(|_| anyhow::anyhow!("install passthrough Wayland queue"))?;
    let _ = ready.send(Ok(()));
    event_loop
        .run(None, &mut state, |_| {})
        .context("dispatch passthrough queue")
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(State: ignore zwp_linux_explicit_synchronization_v1::ZwpLinuxExplicitSynchronizationV1);
delegate_noop!(State: ignore zwp_linux_surface_synchronization_v1::ZwpLinuxSurfaceSynchronizationV1);

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation_clock_monotonic = clk_id == libc::CLOCK_MONOTONIC as u32;
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_dmabuf_v1::Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                state.formats.insert((
                    format,
                    (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                ));
            }
            zwp_linux_dmabuf_v1::Event::Format { format } => {
                // DRM_FORMAT_MOD_INVALID is the implicit-modifier sentinel.
                state.formats.insert((format, u64::MAX));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Explicit synchronization owns the release. wl_buffer.release is not
        // sufficient: it may precede the fenced-release fence being signaled.
        let _ = (event, data);
    }
}

impl Dispatch<wl_callback::WlCallback, CommitData> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        data: &CommitData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_callback::Event::Done { callback_data } = event else {
            return;
        };
        (state.events)(LinuxWaylandPassthroughEvent::Frame {
            scene_id: data.0,
            callback_time: callback_data,
        });
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, CommitData> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &CommitData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                seq_hi,
                seq_lo,
                flags,
            } => {
                let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                (state.events)(LinuxWaylandPassthroughEvent::Presented {
                    scene_id: data.0,
                    timestamp: Duration::new(seconds, tv_nsec),
                    refresh: Duration::from_nanos(u64::from(refresh)),
                    sequence: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
                    flags: flags.into_result().map(|flags| flags.bits()).unwrap_or(0),
                });
            }
            wp_presentation_feedback::Event::Discarded => {
                (state.events)(LinuxWaylandPassthroughEvent::Discarded { scene_id: data.0 });
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_buffer_release_v1::ZwpLinuxBufferReleaseV1, BufferData> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_buffer_release_v1::ZwpLinuxBufferReleaseV1,
        event: zwp_linux_buffer_release_v1::Event,
        data: &BufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let surface = data.surface.clone();
        match event {
            zwp_linux_buffer_release_v1::Event::ImmediateRelease => {
                if let Some(buffer) = state.buffers.remove(&data.lease_id) {
                    buffer.destroy();
                }
                surface.released();
                state.release_finished();
            }
            zwp_linux_buffer_release_v1::Event::FencedRelease { fence } => {
                // Keep fence waiting on the dedicated calloop thread so
                // presentation/frame dispatch remains nonblocking without
                // creating a waiter thread for every frame.
                let lease_id = data.lease_id;
                let fallback_surface = surface.clone();
                if let Err(mut error) = state.loop_handle.insert_source(
                    Generic::new(
                        File::from(fence),
                        calloop::Interest::READ,
                        calloop::Mode::Level,
                    ),
                    move |_, _, state| {
                        if let Some(buffer) = state.buffers.remove(&lease_id) {
                            buffer.destroy();
                        }
                        surface.released();
                        state.release_finished();
                        Ok(PostAction::Remove)
                    },
                ) {
                    log::error!("register DMA-BUF release fence: {}", error.error);
                    if let Some(buffer) = state.buffers.remove(&lease_id) {
                        buffer.destroy();
                    }
                    state.release_finished();
                    let file = unsafe { error.inserted.get_mut() }
                        .try_clone()
                        .expect("clone release fence after calloop insertion failure");
                    thread::spawn(move || {
                        let mut descriptor = libc::pollfd {
                            fd: file.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        while unsafe { libc::poll(&mut descriptor, 1, -1) } < 0
                            && std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::Interrupted
                        {}
                        fallback_surface.released();
                    });
                }
            }
            _ => {}
        }
    }
}
