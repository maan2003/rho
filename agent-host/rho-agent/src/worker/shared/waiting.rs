//! What waits to reach the model: inputs and notebook output, in memory. Each
//! request pulls what it can carry into its row
//! ([DECISION-pull-based-sources](../../../specs/DECISION-pull-based-sources.
//! md)). A crash loses what waits; the chat hands its messages again.

use rho_agent_types::{ContentPart, MessageId, Role, UnixMs};

use crate::entry::add_output;
use crate::record::Input;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    input: Input,
    at: UnixMs,
    /// It can still wake the model. A request with no call open told the
    /// model of `tool` text it left, and a stop quiets what came before it.
    news: bool,
}

/// What one request took, and puts back if it cannot be sent.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Taken {
    pub output: Vec<ContentPart>,
    pub inputs: Vec<Input>,
    held: Vec<Held>,
}

#[derive(Debug, Default)]
pub(crate) struct Waiting {
    output: Vec<ContentPart>,
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
            input: Input {
                role,
                message,
                content,
            },
            at,
            news: role != Role::System,
        });
    }

    /// The notebook's report, which its own facts wake the model for.
    pub fn output(&mut self, content: Vec<ContentPart>) {
        add_output(&mut self.output, content);
    }

    pub fn oldest(&self, role: Role) -> Option<UnixMs> {
        self.held
            .iter()
            .filter(|held| held.news && held.input.role == role)
            .map(|held| held.at)
            .min()
    }

    /// Nothing waits, not even what the model was told of.
    pub fn is_empty(&self) -> bool {
        self.output.is_empty() && self.held.is_empty()
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
        (call_open && !self.output.is_empty())
            || self
                .held
                .iter()
                .any(|held| call_open || held.input.role != Role::Tool)
    }

    /// Whether output or `tool` text waits for a call.
    pub fn holds_tool(&self) -> bool {
        !self.output.is_empty() || self.held.iter().any(|held| held.input.role == Role::Tool)
    }

    /// Everything one request carries, oldest first. With no call open output
    /// and `tool` text stay: only a tool result reads as a tool's
    /// ([DECISION-roles](../../../specs/DECISION-roles.md)), and the model has
    /// been told of them.
    pub fn take(&mut self, call_open: bool) -> Taken {
        let (held, left): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|held| call_open || held.input.role != Role::Tool);
        self.held = left;
        self.quiet();
        Taken {
            output: if call_open {
                std::mem::take(&mut self.output)
            } else {
                Vec::new()
            },
            inputs: held.iter().map(|held| held.input.clone()).collect(),
            held,
        }
    }

    /// A send failed: what it took goes first next time. It wakes nobody.
    pub fn put_back(&mut self, taken: Taken) {
        let mut output = taken.output;
        add_output(&mut output, std::mem::take(&mut self.output));
        self.output = output;
        let mut held = taken.held;
        for held in &mut held {
            held.news = false;
        }
        held.append(&mut self.held);
        self.held = held;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    fn texts(taken: &Taken) -> Vec<String> {
        taken
            .inputs
            .iter()
            .map(|input| rho_agent_types::transcript::text_content(&input.content))
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

    /// Output needs a call, wakes nobody and is not news. It waits across a
    /// request with no call, and joins what the notebook says next.
    #[test]
    fn output_waits_for_a_call_and_wakes_nobody() {
        let mut waiting = Waiting::default();
        waiting.output(text("first"));
        assert_eq!(waiting.oldest(Role::Tool), None);
        assert!(!waiting.has_news());
        assert!(waiting.holds_tool());
        assert!(!waiting.can_go(false) && waiting.can_go(true));

        assert!(waiting.take(false).output.is_empty());
        waiting.output(text("second"));
        assert_eq!(waiting.take(true).output, text("first\n\nsecond"));
        assert!(!waiting.holds_tool());
    }

    /// A failed send puts back what it took, before what came since, and none
    /// of it wakes the model again.
    #[test]
    fn a_failed_send_keeps_the_order() {
        let mut waiting = Waiting::default();
        waiting.output(text("printed"));
        waiting.receive(Role::User, None, text("person"), UnixMs(2));
        let taken = waiting.take(true);
        waiting.output(text("printed later"));
        waiting.receive(Role::User, None, text("later"), UnixMs(3));
        waiting.put_back(taken);
        assert_eq!(waiting.oldest(Role::User), Some(UnixMs(3)));
        let taken = waiting.take(true);
        assert_eq!(taken.output, text("printed\n\nprinted later"));
        assert_eq!(texts(&taken), ["person", "later"]);
    }
}
