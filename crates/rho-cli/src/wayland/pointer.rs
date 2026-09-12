//! The pointer a headless seat does not have.
//!
//! Sway's `seat cursor` commands move a cursor that belongs to a pointer
//! device, and a headless session has no pointer: `swaymsg -t get_seats`
//! reports `capabilities: 0` with an empty device list, the ipc answers
//! `success: true`, and no client ever sees the click. Keys work because
//! `wtype` is a virtual keyboard for the length of a keystroke. This is the
//! same trick for the pointer, in process, over `zwlr_virtual_pointer_v1`:
//! the device exists for the length of one tap and the seat goes back to
//! having nothing on it.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

/// The compositor advertises a new pointer to clients before it can deliver
/// anything through it — the race `wtype` settles for the keyboard with its
/// own delay. A motion sent in the same breath as the device that carries it
/// arrives as `wl_pointer.enter` and nothing else, and a client that decides
/// hover from motion has been told where the cursor is without ever being
/// told it moved.
const POINTER_SETTLE: Duration = Duration::from_millis(50);
/// A press in the same frame as the motion that positioned it is a press with
/// no hover behind it. Screens that pick their target from the last motion —
/// which is every screen drawn with hit testing — need the motion to have
/// landed first.
const MOTION_TO_PRESS: Duration = Duration::from_millis(40);
/// A tap is a press and a release far enough apart to be a tap. Zero would
/// still be a click to most toolkits, but not to anything that measures a
/// hold, and a driver that can only produce instant clicks cannot drive a
/// long press later.
const PRESS_TO_RELEASE: Duration = Duration::from_millis(40);
/// Destroying the pointer takes the device off the seat. Do it too soon after
/// the release and the client is told the pointer left before it has drawn
/// what the release did.
const RELEASE_SETTLE: Duration = Duration::from_millis(80);

/// Button codes from `linux/input-event-codes.h`, which is what the protocol's
/// `button` argument is — not the `wl_pointer` enum, and not sway's names.
pub(super) const BTN_LEFT: u32 = 0x110;
pub(super) const BTN_RIGHT: u32 = 0x111;
pub(super) const BTN_MIDDLE: u32 = 0x112;

/// A virtual pointer on the session's seat, alive for as long as the value is.
pub(super) struct Pointer {
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    /// The absolute coordinate space motions are given in: the output's
    /// logical size, so a caller passes the coordinates it can read off a
    /// screenshot.
    extent: (u32, u32),
    /// Milliseconds, monotonic, made up. The protocol wants a timestamp with
    /// millisecond granularity and the compositor only ever compares them.
    clock: u32,
}

struct State;

impl Pointer {
    /// Connects to the session's compositor and creates the device.
    pub(super) fn open(socket: &Path, extent: (u32, u32)) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connect to the compositor at {}", socket.display()))?;
        let connection = Connection::from_socket(stream).context("speak Wayland")?;
        let (globals, queue) =
            registry_queue_init::<State>(&connection).context("read the compositor's globals")?;
        let handle = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&handle, 1..=9, ())
            .context("bind the session's seat")?;
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&handle, 1..=2, ()).context(
            "bind zwlr_virtual_pointer_manager_v1; the compositor does not offer virtual pointers",
        )?;
        let pointer = manager.create_virtual_pointer(Some(&seat), &handle, ());
        let mut this = Self {
            queue,
            state: State,
            pointer,
            extent,
            clock: 0,
        };
        this.flush()?;
        thread::sleep(POINTER_SETTLE);
        Ok(this)
    }

    /// Puts the cursor at a logical coordinate. Absolute, because a driver
    /// that moved by deltas would have to know where the cursor already is,
    /// and nothing here does.
    pub(super) fn move_to(&mut self, x: u32, y: u32) -> Result<()> {
        let time = self.tick();
        let (x_extent, y_extent) = self.extent;
        self.pointer.motion_absolute(time, x, y, x_extent, y_extent);
        self.pointer.frame();
        self.flush()
    }

    /// Presses and releases, with the motion already sent.
    pub(super) fn click(&mut self, button: u32) -> Result<()> {
        thread::sleep(MOTION_TO_PRESS);
        let time = self.tick();
        self.pointer
            .button(time, button, wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
        self.flush()?;
        thread::sleep(PRESS_TO_RELEASE);
        let time = self.tick();
        self.pointer
            .button(time, button, wl_pointer::ButtonState::Released);
        self.pointer.frame();
        self.flush()?;
        thread::sleep(RELEASE_SETTLE);
        Ok(())
    }

    fn tick(&mut self) -> u32 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn flush(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .context("send pointer events to the compositor")?;
        Ok(())
    }
}

impl Drop for Pointer {
    fn drop(&mut self) {
        self.pointer.destroy();
        let _ = self.queue.roundtrip(&mut self.state);
    }
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

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
