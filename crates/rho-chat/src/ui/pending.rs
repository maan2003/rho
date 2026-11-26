//! Read a queued idea before its host assigns it a thread address.

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window,
};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use rho_dealer::Address;
use rho_frame::Surface;

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

pub(super) struct PendingIdea {
    project: String,
    editor: Entity<Editor>,
}

impl PendingIdea {
    pub(super) fn new(
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
        Self { project, editor }
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
    fn title(&self, _: &App) -> String {
        format!("queued for {}", self.project)
    }

    fn phone_titles(&self, cx: &App) -> Vec<String> {
        vec![self.title(cx), "queued".to_owned()]
    }

    fn editor(&self, _: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }
}
