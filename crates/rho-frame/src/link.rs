//! How a system or a surface reaches the frame from inside its own
//! update.
//!
//! A surface's key handler runs with the surface leased, and the frame
//! may be leased above it; nothing can borrow the frame right then. So
//! every call here is deferred to the end of the current effect cycle and
//! then made on the frame through its window. The frame is the window's
//! root view, which is what makes one handle enough.

use gpui::{App, Window, WindowHandle};
use rho_dealer::Address;
use rho_window::style::StyleClass;

use crate::frame::Frame;
use crate::menu::Rows;

#[derive(Clone)]
pub struct FrameLink {
    window: WindowHandle<Frame>,
}

impl FrameLink {
    pub fn new(window: WindowHandle<Frame>) -> Self {
        Self { window }
    }

    /// Runs `f` on the frame once the current update has finished.
    pub fn with(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut Frame, &mut Window, &mut gpui::Context<Frame>) + 'static,
    ) {
        let window = self.window;
        cx.defer(move |cx| {
            let _ = window.update(cx, f);
        });
    }

    /// Runs `f` on the frame now; `None` while the frame is leased. For
    /// what cannot wait for the end of the update: the app quitting.
    pub fn update<R>(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut Frame, &mut Window, &mut gpui::Context<Frame>) -> R,
    ) -> Option<R> {
        self.window.update(cx, f).ok()
    }

    /// Reads from the frame now; `None` while the frame is leased.
    pub fn read<R>(&self, cx: &App, f: impl FnOnce(&Frame, &App) -> R) -> Option<R> {
        self.window.read(cx).ok().map(|frame| f(frame, cx))
    }

    /// What the reader has seen of the card at `address` while it waits,
    /// after the frame has finished dispatching the action that needs it.
    pub fn with_seen(
        &self,
        address: Address,
        cx: &mut App,
        f: impl FnOnce(Option<rho_dealer::facts::Seen>, &mut App) + 'static,
    ) {
        self.with(cx, move |frame, _, cx| f(frame.card_seen(&address, cx), cx));
    }

    pub fn open(&self, address: Address, cx: &mut App) {
        self.with(cx, move |frame, window, cx| frame.open(address, window, cx));
    }

    /// Opens a web link the user chose in a message.
    pub fn open_link(&self, url: url::Url, cx: &mut App) {
        self.with(cx, move |frame, _, cx| frame.open_link(url, cx));
    }

    pub fn echo(&self, text: impl Into<String>, class: StyleClass, cx: &mut App) {
        let text = text.into();
        self.with(cx, move |frame, _, cx| frame.echo(&text, class, cx));
    }

    pub fn menu(&self, rows: Rows, cx: &mut App) {
        self.with(cx, move |frame, window, cx| {
            frame.open_menu(rows, window, cx)
        });
    }

    /// What the frame draws from a system changed: its claims, its Desk
    /// sections, its channels, and what it holds for a screen that waits
    /// on its hosts.
    pub fn changed(&self, cx: &mut App) {
        self.with(cx, |frame, window, cx| {
            frame.reopen_waiting(window, cx);
            frame.systems_changed(cx);
        });
    }

    /// Closes every open surface whose address `which` picks.
    pub fn close_all(&self, which: impl Fn(&Address) -> bool + 'static, cx: &mut App) {
        self.with(cx, move |frame, window, cx| {
            frame.close_all(which, window, cx)
        });
    }

    pub fn close(&self, address: Address, cx: &mut App) {
        self.with(cx, move |frame, window, cx| {
            frame.close(&address, window, cx)
        });
    }
}
