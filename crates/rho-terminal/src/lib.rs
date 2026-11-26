//! A host-owned terminal shown as a surface.
//!
//! [`TerminalModel`] is the buffer role: it owns the wire display state
//! ([`protocol::WireScreen`]), the input stream, and the read task — shared by
//! every surface showing the terminal. [`TerminalView`] is the viewport role,
//! with its own focus, scrollback offset, and mode.
//!
//! Input is deliberately mode-free on the wire — keystrokes go to the
//! agent host as structured [`protocol::TermKeystroke`]s and are encoded
//! against the terminal's live modes there, so this side never tracks
//! application cursor keys, bracketed paste, or anything else stateful.
//!
//! A terminal surface has the two modes of every raw place
//! (`rho_frame::keys`): **live** (the default) forwards every keystroke to
//! the pty but `escape`, which releases the keyboard back to rho in a
//! read-only copy of the screen and its scrollback, where the window's
//! keys work and j/k/ctrl-d/ctrl-u/gg/G browse; `f9` sends the pty a
//! literal escape. `i`/`a` return to live.
//!
//! Only the focused view's size is sent to the pty (tmux `window-size
//! latest`): only the visible, focused terminal surface sizes the pty.
//!
//! [`protocol`] is what a client and a host say about terminals; the host
//! and the workset runtime use it alone, without the `client` feature.
//! With `ui`, [`ui`] is the terminals as the frame shows them: the surface
//! and its keys.

pub mod protocol;

#[cfg(feature = "client")]
pub mod channel;
#[cfg(feature = "client")]
mod surface;
#[cfg(feature = "ui")]
pub mod ui;

#[cfg(feature = "client")]
pub use surface::*;
