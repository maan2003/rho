use std::{
    collections::HashMap,
    os::fd::{AsFd, AsRawFd},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;
use gpui::{
    Bounds, LinuxWaylandPassthrough, LinuxWaylandPassthroughBuffer,
    LinuxWaylandPassthroughEvent, Pixels,
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
    linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
    },
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
    Present(u64, LinuxWaylandPassthroughBuffer, mpsc::SyncSender<Result<()>>),
    Hide,
    Stop,
}

struct Handle {
    commands: channel::Sender<Command>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
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
    dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    synchronization: zwp_linux_surface_synchronization_v1::ZwpLinuxSurfaceSynchronizationV1,
    presentation: wp_presentation::WpPresentation,
    qh: QueueHandle<Self>,
    events: EventSink,
    buffers: HashMap<u64, wl_buffer::WlBuffer>,
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
        let lease_id = buffer.surface.lease_id();
        let wl_buffer = if let Some(existing) = self.buffers.get(&lease_id) {
            existing.clone()
        } else {
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
                buffer.width.try_into().context("DMA-BUF width exceeds Wayland")?,
                buffer.height.try_into().context("DMA-BUF height exceeds Wayland")?,
                buffer.fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &self.qh,
                BufferData {
                    surface: buffer.surface.clone(),
                    lease_id,
                },
            );
            params.destroy();
            self.buffers.insert(lease_id, wl_buffer.clone());
            wl_buffer
        };

        self.surface.attach(Some(&wl_buffer), 0, 0);
        self.synchronization
            .set_acquire_fence(buffer.acquire_fence.as_fd());
        self.synchronization
            .get_release(&self.qh, BufferData {
                surface: buffer.surface.clone(),
                lease_id,
            });
        self.surface.frame(&self.qh, CommitData(scene_id));
        self.presentation
            .feedback(&self.surface, &self.qh, CommitData(scene_id));
        self.surface.commit();
        buffer.surface.submitted();
        Ok(())
    }

    fn hide(&self) {
        self.surface.attach(None, 0, 0);
        self.surface.commit();
    }
}

pub(super) fn create(
    connection: Connection,
    parent: wl_surface::WlSurface,
    events: EventSink,
) -> Result<Arc<dyn LinuxWaylandPassthrough>> {
    let (commands, receiver) = channel::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("gpui-wayland-passthrough".into())
        .spawn(move || {
            let result = run(connection, parent, events, receiver, ready_tx);
            if let Err(error) = result {
                log::error!("Wayland passthrough thread failed: {error:#}");
            }
        })
        .context("spawn Wayland passthrough thread")?;
    ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("Wayland passthrough setup thread stopped"))??;
    Ok(Arc::new(Handle { commands }))
}

fn run(
    connection: Connection,
    parent: wl_surface::WlSurface,
    events: EventSink,
    receiver: channel::Channel<Command>,
    ready: mpsc::SyncSender<Result<()>>,
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
        .bind(&qh, 3..=4, ())
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
    let mut state = State {
        surface,
        subsurface,
        viewport,
        dmabuf,
        synchronization,
        presentation,
        qh,
        events,
        buffers: HashMap::new(),
    };
    queue.roundtrip(&mut state).context("initialize passthrough globals")?;

    let mut event_loop = EventLoop::<State>::try_new().context("create passthrough event loop")?;
    let signal = event_loop.get_signal();
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
                Command::Stop => signal.stop(),
            }
        })
        .map_err(|_| anyhow::anyhow!("install passthrough command queue"))?;
    WaylandSource::new(connection, queue)
        .insert(event_loop.handle())
        .map_err(|_| anyhow::anyhow!("install passthrough Wayland queue"))?;
    let _ = ready.send(Ok(()));
    event_loop.run(None, &mut state, |_| {}).context("dispatch passthrough queue")
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
delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(State: ignore zwp_linux_explicit_synchronization_v1::ZwpLinuxExplicitSynchronizationV1);
delegate_noop!(State: ignore zwp_linux_surface_synchronization_v1::ZwpLinuxSurfaceSynchronizationV1);
delegate_noop!(State: ignore wp_presentation::WpPresentation);

impl Dispatch<wl_buffer::WlBuffer, BufferData> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            state.buffers.remove(&data.lease_id);
            proxy.destroy();
            data.surface.released();
        }
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
        if let Some(buffer) = state.buffers.remove(&data.lease_id) {
            buffer.destroy();
        }
        let surface = data.surface.clone();
        match event {
            zwp_linux_buffer_release_v1::Event::ImmediateRelease => surface.released(),
            zwp_linux_buffer_release_v1::Event::FencedRelease { fence } => {
                thread::spawn(move || {
                    let mut descriptor = libc::pollfd {
                        fd: fence.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // A sync_file becomes readable once all constituent fences signal.
                    loop {
                        let result = unsafe { libc::poll(&mut descriptor, 1, -1) };
                        if result >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                            break;
                        }
                    }
                    surface.released();
                });
            }
            _ => {}
        }
    }
}
