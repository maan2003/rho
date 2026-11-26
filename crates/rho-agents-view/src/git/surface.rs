//! The screen of one request that waits: who asks, where it goes, and
//! exactly what it would change, as a buffer the user reads like any other.

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use jiff::Timestamp;
use rho_frame::{CardHere, Subject, Surface};
use theme::ActiveTheme as _;

/// What the screen shows, as the system read it when the card opened. A
/// request never changes while it waits; one that ends closes its screen.
pub(crate) struct Shown {
    pub title: String,
    pub asker: String,
    pub is_push: bool,
    pub destination: String,
    pub refs: Vec<String>,
    pub asked: Timestamp,
}

impl Shown {
    /// The screen's text, one fact a line.
    pub(crate) fn text(&self) -> String {
        let action = match self.is_push {
            true => "push",
            false => "fetch",
        };
        let mut lines = vec![
            format!("{} asks to {action} with your ssh login", self.asker),
            String::new(),
            format!("repository {}", self.destination),
            "changes".to_owned(),
        ];
        // The screen's font is proportional, so nothing lines up in
        // columns: each ref sits indented on its own line.
        lines.extend(self.refs.iter().map(|reference| format!("  {reference}")));
        let asked = self
            .asked
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%a %H:%M");
        lines.push(format!("asked {asked}"));
        lines.push(String::new());
        lines.push("space a approves · space d denies".to_owned());
        lines.join("\n")
    }
}

pub struct GitAskSurface {
    title: String,
    editor: Entity<Editor>,
}

impl GitAskSurface {
    pub(crate) fn new(shown: Shown, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = shown.text();
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(text, cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            editor
        });
        Self {
            title: shown.title,
            editor,
        }
    }
}

impl Surface for GitAskSurface {
    fn title(&self, _cx: &App) -> String {
        self.title.clone()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn subject(&self, _card: Option<rho_frame::surface::Seen>, _cx: &App) -> Subject {
        Subject::Own
    }

    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Itself
    }
}

impl Focusable for GitAskSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for GitAskSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoGitAsk")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}
