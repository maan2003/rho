//! The frame: rho's one window and everything every screen shares.
//!
//! rho is an editor the way Emacs is one. Every screen is a surface in a
//! buffer, keys do everything, and there are no modals: the minibuffer
//! asks, the echo line answers, and a menu under the point offers keys.
//! The frame is what holds that together and nothing else: the history
//! of surfaces the reader has been on ([`rho_window::history`]), the
//! minibuffer ([`minibuffer`]), the menus ([`menu`]), the keymap
//! ([`keys`]), the attached agent hosts ([`hosts`]), the user's own facts
//! in the ledger and the dealer that ranks what wants them
//! ([`attention`], [`deal`]), and Home ([`home`]). A window narrow
//! enough to be a phone's is laid out for touch by `phone`, over the
//! same history and the same surfaces.
//!
//! Everything the user looks at belongs to a [`System`]: chat, Slack,
//! agents, notes, files. A system plugs in under address
//! kinds ([`rho_dealer::Address`]): it opens a surface for an address,
//! offers menu rows for what is in view, claims the user's attention for
//! the things of its own that want it, lists what it has for Home and for
//! `find`, and runs its own streams to each host. The frame knows no
//! system by name, so the list of systems is the binary's, and a system
//! is written once for the desk and the phone alike.
//!
//! One surface is in view at a time, on every device. Back and forward
//! walk the one history; down from the newest entry deals the next card.

pub mod attention;
pub mod deal;
pub mod frame;
pub mod home;
pub mod hosts;
pub mod keys;
pub mod link;
pub mod menu;
pub mod minibuffer;
mod palette;
mod phone;
mod search;
pub mod surface;
pub mod system;
pub mod thread;
pub mod when;

pub use deal::dealer_policy_snapshot;
pub use frame::{Frame, Point};
pub use keys::normal_contexts;
pub use link::FrameLink;
pub use menu::{Row, Rows};
pub use minibuffer::{CancelHandler, Candidate, CandidateSource, ChangeHandler, SubmitHandler};
pub use rho_agent_types::EndpointId;
pub use rho_dealer::{Address, Addressed, Marks};
pub use surface::{AnySurface, Subject, Surface};
pub use system::{AnySystem, Found, Reversal, RoomRow, Standing, System};

/// The key context every surface is drawn in. A system binds its keys
/// under `RhoFrame` for the frame and under `RhoFrame > Editor` for keys
/// that must beat the editor's own: the bundled keymaps bind under plain
/// `Editor`, and gpui prefers the deeper match.
pub const KEY_CONTEXT: &str = "RhoFrame";
