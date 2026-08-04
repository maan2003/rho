//! A Zulip client: wire protocol, client-side model, and its surfaces.
//!
//! The client is entirely client-side — it talks to the Zulip server from
//! the GUI process using the credentials in `~/.zuliprc`, and no part of it
//! passes through a Rho daemon.
//!
//! The reading model is Gnus'. Zulip's stream/topic split is a newsreader's
//! group/thread split, so [`ui::InboxView`] is a group buffer (streams and
//! their topics, with unread counts) and [`ui::NarrowView`] is the article
//! buffer for whichever conversation you entered — a Comint transcript with
//! a writable compose region at its end. `n` walks to the next unread
//! conversation and marks the one you left as read, which is the whole
//! reading loop.
//!
//! Message content is raw Markdown throughout: the client registers with
//! `apply_markdown: false` and renders through the host's Markdown
//! pipeline, so no HTML renderer exists here.

pub mod api;
pub mod config;
pub mod events;
pub mod model;
pub mod narrow;
pub mod types;

#[cfg(feature = "ui")]
pub mod session;
#[cfg(feature = "ui")]
pub mod ui;

pub use narrow::{Destination, Narrow};
