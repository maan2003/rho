//! The Zulip client the desk holds, and what the desk lends it.
//!
//! Zulip is one session for the whole client, started on first entry and
//! not before: a reader who never opens it never connects. The surfaces
//! over it are `rho-zulip`'s own; what is here is the session and the two
//! host services those surfaces borrow, so chat reads like every other
//! buffer in the frame rather than bringing its own chrome.

use gpui::prelude::*;
use gpui::{App, Entity};
use rho_zulip::session::Session;
use rho_zulip::ui::Hooks;

/// The Zulip session, once something has asked for it.
#[derive(Default)]
pub(crate) struct Zulip {
    session: Option<Entity<Session>>,
}

impl Zulip {
    /// The session, started the first time it is asked for.
    pub(crate) fn session(&mut self, cx: &mut App) -> Entity<Session> {
        self.session
            .get_or_insert_with(|| cx.new(Session::new))
            .clone()
    }

    /// The session if it has ever been started, for a reading command that
    /// means nothing before the first entry.
    pub(crate) fn started(&self) -> Option<Entity<Session>> {
        self.session.clone()
    }

    /// The host services the Zulip surfaces borrow: editor chrome and the
    /// transcript's Markdown pipeline.
    pub(crate) fn hooks() -> Hooks {
        Hooks {
            configure_editor: rho_window::editor_config::configure,
            configure_markdown: rho_window::markdown::configure_buffer,
        }
    }
}
