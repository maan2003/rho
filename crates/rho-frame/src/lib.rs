//! The frame: rho's one window and everything every screen shares.
//!
//! rho is an editor the way Emacs is one. Every screen is a surface in a
//! buffer, keys do everything, and there are no modals: the minibuffer
//! asks, the echo line answers, and a menu under the point offers keys.
//! The frame is what holds that together and nothing else: the places
//! the reader has been, most recent first (`recent`), `Places` and
//! `Commands` on the minibuffer ([`minibuffer`], [`command`]), the leader
//! and snooze menus ([`menu`]), the keymap
//! ([`keys`]), the attached agent hosts ([`hosts`]), the user's own facts
//! in the ledger and the dealer that ranks what wants them
//! ([`attention`], [`deal`]), and Desk ([`desk`]). A window narrow
//! enough to be a phone's is laid out for touch by `phone`, over the
//! same places and the same surfaces.
//!
//! Everything the user looks at belongs to a [`System`]: chat,
//! agents, notes, pages. A system plugs in under address
//! kinds ([`rho_dealer::Address`]): it opens a surface for an address,
//! offers commands for what is in view, claims the user's attention for
//! the things of its own that want it, lists its places for `Places`, and
//! runs its own streams to each host. The frame
//! knows no system by name, so the list of systems is the binary's, and a
//! system is written once for the desk and the phone alike.
//!
//! One surface is in view at a time, on every device. There is no back
//! and forward: the user moves by recency (`Places`, `tab` for Desk, `q`
//! to close back to the place before) and by the deal (`space-j`).

pub mod attention;
mod awake;
pub mod command;
mod commands;
pub mod deal;
mod desk;
mod devices;
pub mod focus;
pub mod frame;
mod gccp;
pub mod hosts;
pub mod keys;
pub mod link;
mod matching;
pub mod menu;
pub mod minibuffer;
pub mod own;
mod phone;
mod place;
mod places;
mod recent;
mod search;
pub mod surface;
pub mod system;
mod text_search;
pub mod thread;
mod waiting;
pub mod when;

pub use command::{Command, Spelling};
pub use commands::rank_spellings;
pub use deal::dealer_policy_snapshot;
pub use frame::{CardInspection, Frame, Point};
pub use hosts::OnHost;
pub use keys::normal_contexts;
pub use link::FrameLink;
pub use menu::{Row, Rows};
pub use minibuffer::{CancelHandler, Candidate, CandidateSource, SubmitHandler};
pub use phone::{Layout, phone_editor_tap};
pub use rho_agent_types::EndpointId;
pub use rho_dealer::{Address, Addressed, Marks};
pub use surface::{AnySurface, CardHere, Mode, PhoneTool, Subject, Surface};
pub use system::{
    AnySystem, Found, FoundText, FoundThread, Opening, Project, SearchPoint, System, TextPresenter,
};
pub use text_search::{TextHit, TextIndex, TextKind, TextSearch, TextTarget};

/// How the window's own commands are spelled: those about the window, the
/// deal, this device and its hosts. Each system exports its own, so the
/// words for every command can be ranked together.
pub static SPELLINGS: &[&Spelling] = &[
    &own::WRITE_IDEA,
    &own::UNDO_VERDICT,
    &own::SNOOZE,
    &own::UNMUTE,
    &focus::SET_DEALER_FOCUS,
    &focus::CLEAR_DEALER_FOCUS,
    &own::PAIR_DEVICE,
    &own::SHOW_SECRET_PHRASE,
    &own::QUIT,
    &hosts::ATTACH_HOST,
    &hosts::DETACH_HOST,
    &hosts::LIST_HOSTS,
    &gccp::LIST_GCCP,
    &gccp::APPROVE_GCCP,
    &gccp::REMOVE_GCCP,
    &devices::LIST_DEVICES,
    &devices::REVOKE_DEVICE,
];

/// The key context every surface is drawn in. A system binds its keys
/// under `RhoFrame` for the frame and under `RhoFrame > Editor` for keys
/// that must beat the editor's own: the bundled keymaps bind under plain
/// `Editor`, and gpui prefers the deeper match.
pub const KEY_CONTEXT: &str = "RhoFrame";
