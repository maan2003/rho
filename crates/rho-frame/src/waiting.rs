//! The screen an address shows while the hosts that would hold it have
//! not yet said what they hold, such as an agent's terminal right after a
//! restart. The way back keeps the address; once a host answers, the frame
//! puts the real surface in this one's place, or passes over an address
//! no host holds ([`crate::Opening`]).

use gpui::prelude::*;
use gpui::{App, Context, FocusHandle, Focusable, Window, div};
use theme::ActiveTheme as _;

use crate::surface::{Subject, Surface};

pub(crate) struct Waiting {
    /// The hosts it waits for, by name: `rho`, or `rho, lab`.
    hosts: String,
    focus: FocusHandle,
}

impl Waiting {
    pub(crate) fn new(hosts: String, cx: &mut Context<Self>) -> Self {
        Self {
            hosts,
            focus: cx.focus_handle(),
        }
    }
}

impl Surface for Waiting {
    fn title(&self, _cx: &App) -> String {
        format!("waiting for {}", self.hosts)
    }

    /// Nothing is here yet for a verdict to be about.
    fn subject(&self, _card: Option<rho_dealer::facts::Seen>, _cx: &App) -> Subject {
        Subject::None
    }
}

impl Focusable for Waiting {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Waiting {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .key_context(crate::keys::PLACE)
            .track_focus(&self.focus)
            .size_full()
            .p_2()
            .bg(colors.editor_background)
            .text_color(colors.text_muted)
            .child(format!(
                "waiting for {} to connect… this screen opens when it does",
                self.hosts
            ))
    }
}
