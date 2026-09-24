//! A daemon-owned terminal shown as a surface.
//!
//! [`TerminalModel`] is the buffer role: it owns the wire display state
//! ([`protocol::WireScreen`]), the input stream, and the read task — shared by
//! every surface showing the terminal. [`TerminalView`] is the viewport role,
//! with its own focus, scrollback offset, and mode.
//!
//! Input is deliberately mode-free on the wire — keystrokes go to the
//! daemon as structured [`protocol::TermKeystroke`]s and are encoded against
//! the terminal's live modes there, so this side never tracks application
//! cursor keys, bracketed paste, or anything else stateful.
//!
//! Views have two modes, vim-style: **raw** (the default) forwards every
//! keystroke to the pty; **normal** (`ctrl-\ ctrl-n`, or `ctrl-shift-n`)
//! releases the keyboard back to rho — `:` opens the command line, the
//! space leader works, and j/k/ctrl-d/ctrl-u/gg/G browse scrollback.
//! `i`/`a`/`enter` return to raw.
//!
//! Only the focused view's size is sent to the pty (tmux `window-size
//! latest`): only the visible, focused terminal surface sizes the pty.
//!
//! [`protocol`] is what a client and a host say about terminals; the host
//! and the workset runtime use it alone, without the `client` feature.

pub mod protocol;

#[cfg(feature = "client")]
pub mod channel;
#[cfg(feature = "client")]
mod surface;

#[cfg(feature = "client")]
pub use surface::*;
