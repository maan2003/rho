//! Read a queued idea before its host assigns it a thread address, or one
//! its host refused, until the user takes it back into the draft.

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window,
};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use rho_dealer::Address;
use rho_frame::{Row, Surface};

use super::ChatSystem;
use crate::outbox::OutgoingState;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingIdeaId(pub(super) u64);

impl PendingIdeaId {
    pub(super) const KIND: &str = "pending-idea";

    pub(super) fn address(self) -> Address {
        Address::new(Self::KIND, &self.0.to_string())
    }

    pub(super) fn from_address(address: &Address) -> Option<Self> {
        if address.kind() != Self::KIND {
            return None;
        }
        address.rest().parse().ok().map(Self)
    }
}

pub(super) fn title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// What the palette says of an idea the outbox holds.
pub(super) fn status(state: &OutgoingState) -> &'static str {
    match state {
        OutgoingState::Queued => "queued",
        OutgoingState::Refused { .. } => "not sent",
    }
}

pub(super) struct PendingIdea {
    system: Entity<ChatSystem>,
    id: PendingIdeaId,
    project: String,
    editor: Entity<Editor>,
}

impl PendingIdea {
    pub(super) fn new(
        system: Entity<ChatSystem>,
        id: PendingIdeaId,
        project: String,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(text, cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let view = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                view,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        Self {
            system,
            id,
            project,
            editor,
        }
    }

    /// Why the host refused it, while it is refused.
    fn refusal(&self, cx: &App) -> Option<String> {
        self.system
            .read(cx)
            .outbox
            .ideas()
            .find(|(_, id, _)| *id == self.id.0)
            .and_then(|(_, _, outgoing)| match &outgoing.state {
                OutgoingState::Refused { reason } => Some(reason.clone()),
                OutgoingState::Queued => None,
            })
    }
}

impl Focusable for PendingIdea {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for PendingIdea {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

impl Surface for PendingIdea {
    fn title(&self, cx: &App) -> String {
        match self.refusal(cx) {
            Some(_) => format!("not sent to {}", self.project),
            None => format!("queued for {}", self.project),
        }
    }

    fn phone_titles(&self, cx: &App) -> Vec<String> {
        let state = match self.refusal(cx) {
            Some(_) => "not sent",
            None => "queued",
        };
        vec![self.title(cx), state.to_owned()]
    }

    fn status(&self, cx: &App) -> Option<String> {
        self.refusal(cx)
            .map(|reason| rho_agent_types::one_line(&reason).to_string())
    }

    /// A refused idea goes back into the draft to change and send again.
    fn rows(
        &mut self,
        _: Option<rho_frame::surface::Seen>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Row> {
        if self.refusal(cx).is_none() {
            return Vec::new();
        }
        let (system, id) = (self.system.clone(), self.id);
        vec![Row::run(
            "",
            "write this idea again",
            move |frame, window, cx| {
                let Some(idea) = system.read(cx).refused_idea(id) else {
                    return;
                };
                if frame.write_idea_again(idea, window, cx) {
                    system.update(cx, |system, cx| system.withdraw_refused_idea(id, cx));
                }
            },
        )]
    }

    fn editor(&self, _: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }
}
