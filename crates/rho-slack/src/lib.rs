//! A Slack client: wire protocol, client-side model, and the thread surface.
//!
//! The client is entirely client-side. It talks to slack.com from the GUI
//! process with the user's own web session — the `xoxc` token and the `d`
//! cookie, exactly as emacs-slack has for years — and no part of it passes
//! through a Rho daemon: a Slack session belongs to the person, and the
//! person sits at the client.
//!
//! Two sources feed the same model. `activity.feed` is the truth: a stable,
//! paged list of the mentions, DMs, and thread replies that concern the user,
//! so a missed frame is never a missed mention. The websocket exists only so
//! the lamp lights within a second. Both land in [`model::Model`], which
//! deduplicates on the message timestamp, so the two never deal a thread
//! twice.
//!
//! Only mentions, direct messages, and threads the user has posted in ever
//! become items; ordinary channel traffic never does.

pub mod api;
pub mod block;
pub mod config;
pub mod emoji;
pub mod events;
#[cfg(feature = "fake")]
pub mod fake;
pub mod health;
#[cfg(feature = "ui")]
pub mod mirror;
pub mod model;
#[cfg(feature = "ui")]
pub mod session;
pub mod socket;
pub mod types;
#[cfg(feature = "ui")]
pub mod ui;

pub use config::{CredentialStore, Credentials, WorkspaceName};
pub use types::{ChannelId, Reason, ThreadKey, Ts, UserId};
