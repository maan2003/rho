//! Inputs waiting to reach the model.
//!
//! Every source works the same way: it accumulates on its own, reports how it
//! is doing, and is *pulled* by the core at a moment the core chooses.
//! Nothing outside the core decides when a request happens; the core never
//! decides what a source has to say.

use std::borrow::Cow;

use rho_core::{AgentId as PeerId, ContentPart, ContextBlock, MessageSender, UnixMs};
use senax_encoder::{Decode, Encode};

use crate::tool::Rhythm;

/// The only scheduling lever a sender has: whether this input is worth
/// throwing away an in-flight request for.
///
/// There is deliberately no "deliver after the current task" mode. Prose says
/// that better than an enum can — "once you've finished the edits, run the
/// tests" is a boundary no variant could express.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum Delivery {
    /// Abort the in-flight request so this lands now.
    Interrupt,
    /// Ride along with the next request, whenever the core makes one.
    #[default]
    NextRequest,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum InputKind {
    Message {
        content: Vec<ContentPart>,
    },
    /// The user explicitly asked to compact. Automatic compaction is not an
    /// input at all — it happens while building a request.
    Compaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum InputSource {
    User,
    Mail { peer: PeerId },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedInput {
    pub source: InputSource,
    pub kind: InputKind,
    pub delivery: Delivery,
    pub at: UnixMs,
}

/// A source with something for the model, and how impatient it is.
///
/// Three kinds of source reduce to one shape, because scheduling only ever
/// asks two things: how impatient are you, and might you have more to say.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingSource {
    pub rhythm: Rhythm,
    /// When it last produced something, or `None` if it certainly will not
    /// produce again. A typed message is whole on arrival and an exited tool is
    /// done for good, so both are settled at once: quiet is only a guess, but
    /// "there is no more" is certain.
    pub still_talking: Option<UnixMs>,
}

impl PendingSource {
    /// Nothing more is coming.
    pub fn done(rhythm: Rhythm) -> Self {
        Self {
            rhythm,
            still_talking: None,
        }
    }

    pub fn talking(rhythm: Rhythm, last_output_at: UnixMs) -> Self {
        Self {
            rhythm,
            still_talking: Some(last_output_at),
        }
    }

    /// Whether waiting longer is unlikely to improve the request.
    pub fn settled(self, now: UnixMs) -> bool {
        self.still_talking.is_none_or(|last| {
            now.saturating_duration_since(last) >= self.rhythm.quiet_after.as_millis() as u64
        })
    }

    /// The next instant at which [`PendingSource::settled`] could change
    /// answer.
    pub fn quiet_deadline(self) -> Option<UnixMs> {
        self.still_talking
            .map(|last| UnixMs(last.0 + self.rhythm.quiet_after.as_millis() as u64))
    }

    /// When this source insists on being heard, measured from the model's last
    /// word rather than from its own arrival — impatience is a property of the
    /// conversation's cadence, not of any one item.
    pub fn hold_deadline(self, last_response_at: UnixMs) -> UnixMs {
        UnixMs(last_response_at.0 + self.rhythm.max_hold.as_millis() as u64)
    }
}

/// Messages typed by the user. Discrete, never merged or summarised, and
/// always drained in arrival order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserSource {
    items: Vec<QueuedInput>,
}

impl UserSource {
    pub fn push(&mut self, input: QueuedInput) {
        self.items.push(input);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn wants_interrupt(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.delivery == Delivery::Interrupt)
    }

    pub fn oldest(&self) -> Option<UnixMs> {
        self.items.first().map(|item| item.at)
    }

    pub(crate) fn pending_source(&self) -> Option<PendingSource> {
        (!self.items.is_empty()).then(|| PendingSource::done(Rhythm::USER))
    }

    /// Every queued item is eligible at every boundary, so a drain is total.
    pub(crate) fn take(&mut self) -> Vec<ContextBlock> {
        std::mem::take(&mut self.items)
            .into_iter()
            .map(|item| match item.kind {
                InputKind::Message { content } => ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content,
                },
                InputKind::Compaction => ContextBlock::CompactionTrigger,
            })
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }
}

/// Mail from one peer agent. Several messages from the same peer collapse into
/// a single block, so a chatty peer costs one request rather than five.
#[derive(Clone, Debug, PartialEq)]
pub struct MailSource {
    peer: PeerId,
    parts: Vec<ContentPart>,
    first_at: UnixMs,
    last_at: UnixMs,
}

impl MailSource {
    pub fn new(peer: PeerId, at: UnixMs) -> Self {
        Self {
            peer,
            parts: Vec::new(),
            first_at: at,
            last_at: at,
        }
    }

    pub fn push(&mut self, content: Vec<ContentPart>, at: UnixMs) {
        if self.parts.is_empty() {
            self.first_at = at;
        }
        self.parts.extend(content);
        self.last_at = at;
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    pub(crate) fn pending_source(&self) -> Option<PendingSource> {
        (!self.parts.is_empty()).then(|| PendingSource::talking(Rhythm::MAIL, self.last_at))
    }

    pub(crate) fn take(&mut self) -> Option<ContextBlock> {
        (!self.parts.is_empty()).then(|| ContextBlock::UserMessage {
            sender: MessageSender::Agent { id: self.peer },
            content: std::mem::take(&mut self.parts),
        })
    }

    pub(crate) fn clear(&mut self) {
        self.parts.clear();
    }

    pub(crate) fn preview(&self) -> QueuePreview {
        QueuePreview {
            pending: self.parts.len() as u32,
            since: self.first_at,
        }
    }
}

senax_encoder::declare_senax_tagged_trait!(
    pub trait PreviewData,
    unknown = UnknownPreviewData,
);

/// Non-consuming view of what a source is holding, for UIs.
///
/// Signals to the core carry no payload, so this is the only way to show
/// pending content before it is pulled. The payload is open the same way
/// provider data is: a shell tool shows a terminal buffer, a search shows
/// match counts, and neither has to be describable as one summary string.
#[derive(Clone, Debug)]
pub struct Preview {
    pub label: Cow<'static, str>,
    pub data: Box<dyn PreviewData>,
}

// Written out rather than derived: the derive cannot see through the tagged
// trait object's hand-written `PartialEq`.
impl PartialEq for Preview {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label && self.data.dyn_eq(&*other.data)
    }
}

/// Preview for the sources the core owns itself: the user queue and peer mail.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct QueuePreview {
    pub pending: u32,
    /// When this source first had something waiting, so a UI can render
    /// "waiting 3s" without a parallel status struct saying the same thing.
    pub since: UnixMs,
}

senax_encoder::register_senax_tagged!(
    trait = PreviewData,
    type = QueuePreview,
    tag = "rho-agent2.preview.queue",
);

/// Default preview for a tool that has nothing richer to show.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ToolPreview {
    pub exited: bool,
    pub pending: bool,
    pub last_output_at: UnixMs,
}

senax_encoder::register_senax_tagged!(
    trait = PreviewData,
    type = ToolPreview,
    tag = "rho-agent2.preview.tool",
);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn message(text: &str, delivery: Delivery, at: u64) -> QueuedInput {
        QueuedInput {
            source: InputSource::User,
            kind: InputKind::Message {
                content: vec![ContentPart::Text {
                    text: text.to_owned(),
                }],
            },
            delivery,
            at: UnixMs(at),
        }
    }

    #[test]
    fn user_drain_is_total_and_ordered() {
        let mut user = UserSource::default();
        user.push(message("first", Delivery::NextRequest, 1));
        user.push(message("second", Delivery::Interrupt, 2));

        let blocks = user.take();
        assert_eq!(blocks.len(), 2);
        assert!(user.is_empty(), "no item is ever left behind");
        assert_eq!(
            blocks[0],
            ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text {
                    text: "first".to_owned()
                }],
            }
        );
    }

    #[test]
    fn user_input_is_settled_on_arrival() {
        let user = PendingSource::done(Rhythm::USER);
        assert!(user.settled(UnixMs(0)));
        assert_eq!(user.quiet_deadline(), None);
    }

    #[test]
    fn a_source_settles_by_going_quiet_or_by_being_finished() {
        let rhythm = Rhythm {
            quiet_after: Duration::from_millis(250),
            max_hold: Duration::from_secs(10),
        };
        let chatty = PendingSource::talking(rhythm, UnixMs(1_000));
        assert!(!chatty.settled(UnixMs(1_100)));
        assert!(chatty.settled(UnixMs(1_250)));

        let done = PendingSource::done(rhythm);
        assert!(done.settled(UnixMs(0)), "certainty needs no quiet window");
        assert_eq!(done.quiet_deadline(), None);
    }

    #[test]
    fn mail_from_one_peer_collapses_into_a_single_block() {
        let peer = PeerId::from_counter(1, &rho_core::AgentIdDomain(7)).unwrap();
        let mut mail = MailSource::new(peer, UnixMs(0));
        mail.push(
            vec![ContentPart::Text {
                text: "a".to_owned(),
            }],
            UnixMs(10),
        );
        mail.push(
            vec![ContentPart::Text {
                text: "b".to_owned(),
            }],
            UnixMs(20),
        );

        let ContextBlock::UserMessage { sender, content } = mail.take().unwrap() else {
            panic!("expected a user message block")
        };
        assert_eq!(sender, MessageSender::Agent { id: peer });
        assert_eq!(content.len(), 2, "one block, both parts");
        assert!(mail.is_empty());
    }
}
