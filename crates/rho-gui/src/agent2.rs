//! The agent2 chat surface: a read-only editor over the host's chat log.

use editor::Editor;
use gpui::{AppContext as _, Context, Entity, Window};
use language::{Buffer, Capability};
use rho_agents2_client::protocol::{AgentInfo, ChatKind, Party};

#[derive(Clone)]
pub(crate) struct ChatView {
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    composer: Entity<Buffer>,
    composer_editor: Entity<Editor>,
}

impl ChatView {
    pub(crate) fn new(
        info: &AgentInfo,
        window: &mut Window,
        cx: &mut Context<super::workspace::Workspace>,
    ) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            editor
        });
        let composer = cx.new(|cx| Buffer::local("", cx));
        let composer_editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(composer.clone(), None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        let view = Self {
            buffer,
            editor,
            composer,
            composer_editor,
        };
        view.refresh(info, cx);
        view
    }

    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub(crate) fn composer_editor(&self) -> &Entity<Editor> {
        &self.composer_editor
    }

    pub(crate) fn composition(&self, cx: &gpui::App) -> String {
        self.composer.read(cx).text()
    }

    pub(crate) fn clear_if_sent(&self, sent: &str, cx: &mut gpui::App) {
        self.composer.update(cx, |buffer, cx| {
            if buffer.text() == sent {
                let old = buffer.len();
                buffer.edit([(0..old, "")], None, cx);
            }
        });
    }

    pub(crate) fn refresh(&self, info: &AgentInfo, cx: &mut gpui::App) {
        let text = chat_text(info);
        self.buffer.update(cx, |buffer, cx| {
            if buffer.text() == text {
                return;
            }
            let old = buffer.len();
            buffer.edit([(0..old, text.as_str())], None, cx);
        });
    }
}

fn chat_text(info: &AgentInfo) -> String {
    let mut text = format!(
        "agent2 {}\n{} · {} · {:?}{}\n",
        info.id.as_str(),
        info.workdir,
        info.model,
        info.effort,
        if info.archived { " · archived" } else { "" }
    );
    if let Some(status) = &info.status {
        text.push_str(&format!("status: {status}\n"));
    }
    text.push_str(
        "\nEnter: send · Shift+Enter: newline · Space h x: archive · Space h o: open another\n\n",
    );
    for event in &info.chat {
        match &event.kind {
            ChatKind::Message {
                from,
                to,
                text: body,
                ..
            } => {
                text.push_str(&format!("{} → {}:\n", party_label(from), party_label(to)));
                for line in body.lines() {
                    text.push_str("  ");
                    text.push_str(line);
                    text.push('\n');
                }
                text.push('\n');
            }
            ChatKind::Status(status) => text.push_str(&format!("[{status}]\n\n")),
        }
    }
    text
}

fn party_label(party: &Party) -> &str {
    match party {
        Party::Human => "you",
        Party::Agent(_) => "agent",
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::UnixMs;
    use rho_agents2_client::protocol::{AgentId, ChatEvent, Effort, MessageId};

    use super::*;

    #[test]
    fn chat_keeps_direction_multiline_text_and_archived_status() {
        let id = AgentId::new("eng-case").unwrap();
        let info = AgentInfo {
            id: id.clone(),
            workdir: "/src/project".into(),
            model: "gpt-6-sol".into(),
            effort: Effort::High,
            archived: true,
            status: Some("waiting".into()),
            chat: vec![
                ChatEvent {
                    seq: 1,
                    at: UnixMs(1),
                    kind: ChatKind::Message {
                        id: MessageId(1),
                        from: Party::Human,
                        to: Party::Agent(id.clone()),
                        text: "first line\nsecond line".into(),
                    },
                },
                ChatEvent {
                    seq: 2,
                    at: UnixMs(2),
                    kind: ChatKind::Message {
                        id: MessageId(2),
                        from: Party::Agent(id),
                        to: Party::Human,
                        text: "answer".into(),
                    },
                },
            ],
        };
        let text = chat_text(&info);
        assert!(text.contains("/src/project · gpt-6-sol · High · archived"));
        assert!(text.contains("status: waiting"));
        assert!(text.contains("you → agent:\n  first line\n  second line\n"));
        assert!(text.contains("agent → you:\n  answer\n"));
    }
}
