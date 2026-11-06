//! What waits to reach the model: inputs and notebook output, in memory. Each
//! request pulls what it can carry and writes its rows just before it
//! ([DECISION-pull-based-sources](../../../specs/DECISION-pull-based-sources.
//! md)). A crash loses what waits; the chat hands its messages again.

use rho_agent_types::{AgentId, ContentPart, MessageId, Role, UnixMs};

use crate::record::Event;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub role: Role,
    pub message: Option<MessageId>,
    pub content: Vec<ContentPart>,
    at: UnixMs,
    /// It can still wake the model. A request with no call open told the
    /// model of `tool` text it left, and a stop quiets what came before it.
    news: bool,
}

impl Held {
    /// The row that says a request carried it.
    pub fn row(self, agent: AgentId) -> Event<'static> {
        Event::Input {
            agent,
            message: self.message,
            content: self.content,
            role: self.role,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Waiting {
    held: Vec<Held>,
}

impl Waiting {
    pub fn receive(
        &mut self,
        role: Role,
        message: Option<MessageId>,
        content: Vec<ContentPart>,
        at: UnixMs,
    ) {
        self.held.push(Held {
            role,
            message,
            content,
            at,
            news: role != Role::System,
        });
    }

    /// The notebook's report, which its own facts wake the model for.
    pub fn output(&mut self, content: Vec<ContentPart>, at: UnixMs) {
        self.receive(Role::Tool, None, content, at);
        self.held.last_mut().expect("just pushed").news = false;
    }

    pub fn oldest(&self, role: Role) -> Option<UnixMs> {
        self.held
            .iter()
            .filter(|held| held.news && held.role == role)
            .map(|held| held.at)
            .min()
    }

    pub fn has_news(&self) -> bool {
        self.news() > 0
    }

    pub fn news(&self) -> usize {
        self.held.iter().filter(|held| held.news).count()
    }

    pub fn quiet(&mut self) {
        for held in &mut self.held {
            held.news = false;
        }
    }

    /// Whether a request could carry anything now.
    pub fn can_go(&self, call_open: bool) -> bool {
        self.held
            .iter()
            .any(|held| call_open || held.role != Role::Tool)
    }

    /// Whether `tool` text waits for a call.
    pub fn holds_tool(&self) -> bool {
        self.held.iter().any(|held| held.role == Role::Tool)
    }

    /// Everything one request carries, oldest first. With no call open `tool`
    /// text stays: only a tool result reads as a tool's
    /// ([DECISION-roles](../../../specs/DECISION-roles.md)), and the model has
    /// been told of it.
    pub fn take(&mut self, call_open: bool) -> Vec<Held> {
        let (taken, left): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|held| call_open || held.role != Role::Tool);
        self.held = left;
        self.quiet();
        taken
    }

    /// A send failed: what it took goes first next time. It wakes nobody.
    pub fn put_back(&mut self, mut taken: Vec<Held>) {
        for held in &mut taken {
            held.news = false;
        }
        taken.append(&mut self.held);
        self.held = taken;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    fn texts(held: &[Held]) -> Vec<String> {
        held.iter()
            .map(|held| rho_agent_types::transcript::text_content(&held.content))
            .collect()
    }

    /// With no call open `tool` text stays and wakes nobody again. The next
    /// call takes it, in order with what came later.
    #[test]
    fn tool_text_waits_for_a_call_and_is_told_once() {
        let mut waiting = Waiting::default();
        waiting.receive(Role::Tool, None, text("peer"), UnixMs(1));
        waiting.receive(Role::User, None, text("person"), UnixMs(2));
        waiting.receive(Role::System, None, text("rho"), UnixMs(3));
        assert_eq!(waiting.oldest(Role::Tool), Some(UnixMs(1)));
        assert_eq!(waiting.oldest(Role::User), Some(UnixMs(2)));
        assert_eq!(waiting.oldest(Role::System), None);

        assert_eq!(texts(&waiting.take(false)), ["person", "rho"]);
        assert!(waiting.holds_tool() && !waiting.has_news());
        assert!(!waiting.can_go(false) && waiting.can_go(true));

        waiting.receive(Role::Tool, None, text("later peer"), UnixMs(4));
        assert_eq!(waiting.oldest(Role::Tool), Some(UnixMs(4)));
        assert_eq!(texts(&waiting.take(true)), ["peer", "later peer"]);
        assert!(!waiting.holds_tool());
    }

    #[test]
    fn output_wakes_nobody_by_itself_and_a_failed_send_keeps_the_order() {
        let mut waiting = Waiting::default();
        waiting.output(text("printed"), UnixMs(1));
        assert_eq!(waiting.oldest(Role::Tool), None);
        waiting.receive(Role::User, None, text("person"), UnixMs(2));
        let taken = waiting.take(true);
        waiting.receive(Role::User, None, text("later"), UnixMs(3));
        waiting.put_back(taken);
        assert_eq!(waiting.oldest(Role::User), Some(UnixMs(3)));
        assert_eq!(texts(&waiting.take(true)), ["printed", "person", "later"]);
    }
}
