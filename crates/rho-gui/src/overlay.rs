//! Where focus goes back to when a modal overlay closes.
//!
//! There is one modal overlay at a time — a transient, a minibuffer, a
//! menu, the Git approval prompt — and while one is up it holds keyboard
//! focus. Something has to remember what had focus before, or closing an
//! overlay leaves the reader nowhere.
//!
//! The rule is not "each overlay remembers its own", and that is the whole
//! reason this is a type. Overlays replace each other: a transient opens a
//! minibuffer, which opens another. The target belongs to the *chain*, so
//! the first overlay captures it and every replacement inherits it
//! untouched. An overlay that captured on open would hand focus back to the
//! overlay that opened it, which by then is gone.

use gpui::{App, FocusHandle, Window};

/// The focus a chain of overlays will return to.
#[derive(Default)]
pub(crate) struct OverlayFocus(Option<FocusHandle>);

impl OverlayFocus {
    /// Remembers what had focus, if nothing is remembered yet. Called by
    /// every overlay as it opens: the first one in a chain records the
    /// target and the rest inherit it.
    pub(crate) fn capture(&mut self, window: &Window, cx: &App) {
        if self.0.is_none() {
            self.0 = window.focused(cx);
        }
    }

    /// Sets the target directly, for focus that moves underneath an
    /// overlay that is already up: the reader is on a different surface
    /// when they come back, not the one they left.
    pub(crate) fn set(&mut self, handle: FocusHandle) {
        self.0 = Some(handle);
    }

    /// What the chain will return to, if anything.
    pub(crate) fn target(&self) -> Option<&FocusHandle> {
        self.0.as_ref()
    }

    /// Gives focus back and ends the chain. Answers with the handle to
    /// focus, or `None` when there is nothing remembered and the caller
    /// should fall back to the active surface.
    pub(crate) fn finish(&mut self) -> Option<FocusHandle> {
        self.0.take()
    }
}
