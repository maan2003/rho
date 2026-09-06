//! What a loop tells clients about its in-flight response: the phase,
//! and each item as it first appears or grows. The loop is the only
//! place that knows what changed, so it says so here instead of a
//! reader diffing snapshots.

use rho_core::{AStr, Diff, StreamingContextItem, StreamingContextItemState};
use rho_ui_proto::mirror::{Item, Live, TextPhase};

use crate::AgentStateKind;

/// Remembers what was last told so the next tell is only the change.
/// `reset` forgets it all; the next tell then says everything again,
/// which is how a joiner is brought up to date.
#[derive(Default)]
pub struct Teller {
    phase: Option<Phase>,
    /// Temporary failures already told this request.
    failures: u64,
    /// Per index, the item as last told; `None` where nothing was.
    items: Vec<Option<StreamingContextItem>>,
}

#[derive(PartialEq, Eq)]
enum Phase {
    Requesting,
    Waiting(Option<rho_core::UnixMs>),
    Idle,
}

impl Teller {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// What changed since the last tell. Every phase message empties the
    /// tail on the client: `Requesting` because a request starts empty,
    /// `Waiting` and `Idle` because the row now carries the response.
    pub fn tell(&mut self, kind: &AgentStateKind) -> Vec<Live> {
        let mut out = Vec::new();
        match kind {
            AgentStateKind::ApiStreaming {
                pending_response,
                previous_attempt,
            } => {
                let mut fresh = false;
                if self.phase != Some(Phase::Requesting) {
                    self.phase = Some(Phase::Requesting);
                    self.items.clear();
                    self.failures = 0;
                    out.push(Live::Requesting);
                    fresh = true;
                }
                // A retry is a fresh request: the partial response went
                // to the log as a `Failed` row before this tell.
                if let Some(failure) = previous_attempt
                    && failure.attempt_count.get() > self.failures
                {
                    self.failures = failure.attempt_count.get();
                    if !fresh {
                        self.items.clear();
                        out.push(Live::Requesting);
                    }
                }
                // A response the loop started over within the request
                // (Claude's next message, whose rows now carry the last
                // one) has fewer items than were told. The client would
                // keep the stale ones under the rows, so empty its tail
                // and say the response again.
                if self.items.len() > pending_response.items.len() {
                    self.items.clear();
                    out.push(Live::Requesting);
                }
                for (index, slot) in pending_response.items.iter().enumerate() {
                    let (StreamingContextItemState::Pending(item)
                    | StreamingContextItemState::Finished(item)) = slot
                    else {
                        continue;
                    };
                    if self.items.len() <= index {
                        self.items.resize(index + 1, None);
                    }
                    let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
                    match self.items[index].as_ref().map(|told| appended(told, item)) {
                        Some(Some(text)) => {
                            if !text.is_empty() {
                                out.push(Live::Appended {
                                    index: index_u32,
                                    text,
                                });
                            }
                        }
                        _ => {
                            if let Some(item) = to_item(item) {
                                out.push(Live::Item {
                                    index: index_u32,
                                    item,
                                });
                            }
                        }
                    }
                    self.items[index] = Some(item.clone());
                }
            }
            AgentStateKind::ToolCalling { waiting, .. } => {
                if self.phase != Some(Phase::Waiting(*waiting)) {
                    self.phase = Some(Phase::Waiting(*waiting));
                    self.items.clear();
                    out.push(Live::Waiting { until: *waiting });
                }
            }
            AgentStateKind::Idle
            | AgentStateKind::UnfinishedTurn { .. }
            | AgentStateKind::Error(_) => {
                if self.phase != Some(Phase::Idle) {
                    self.phase = Some(Phase::Idle);
                    self.items.clear();
                    out.push(Live::Idle);
                }
            }
        }
        out
    }
}

/// The item as a client draws it. Compaction and unknown items have no
/// face; their index is never told.
pub fn to_item(item: &StreamingContextItem) -> Option<Item> {
    Some(match item {
        StreamingContextItem::AssistantMessage { content, phase, .. } => Item::Text {
            text: content.iter().map(ToString::to_string).collect(),
            phase: phase.map(text_phase),
        },
        StreamingContextItem::RawReasoning {
            content, summary, ..
        } => Item::Reasoning {
            text: reasoning_text(content, summary),
        },
        StreamingContextItem::EncryptedReasoning { summary, .. } => {
            if summary.is_empty() {
                return None;
            }
            Item::Reasoning {
                text: join(summary),
            }
        }
        StreamingContextItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Item::ToolCall {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.to_string(),
        },
        StreamingContextItem::Compaction { .. } | StreamingContextItem::Unknown { .. } => {
            return None;
        }
    })
}

pub fn text_phase(phase: rho_core::MessagePhase) -> TextPhase {
    match phase {
        rho_core::MessagePhase::Commentary => TextPhase::Commentary,
        rho_core::MessagePhase::FinalAnswer => TextPhase::FinalAnswer,
    }
}

/// What `new` shows past `old` when the only change is text growing at
/// the end; `None` when anything else changed and the item must be told
/// whole.
fn appended(old: &StreamingContextItem, new: &StreamingContextItem) -> Option<String> {
    use StreamingContextItem as S;
    match (old, new) {
        (
            S::AssistantMessage {
                content: a,
                phase: pa,
                ..
            },
            S::AssistantMessage {
                content: b,
                phase: pb,
                ..
            },
        ) if pa == pb => parts_appended(a, b, ""),
        (
            S::RawReasoning {
                content: ca,
                summary: sa,
                ..
            },
            S::RawReasoning {
                content: cb,
                summary: sb,
                ..
            },
        ) => {
            if sa.is_empty() && sb.is_empty() {
                parts_appended(std::slice::from_ref(ca), std::slice::from_ref(cb), "")
            } else if sa.is_empty() {
                None
            } else {
                parts_appended(sa, sb, "\n")
            }
        }
        (S::EncryptedReasoning { summary: a, .. }, S::EncryptedReasoning { summary: b, .. }) => {
            if a.is_empty() {
                None
            } else {
                parts_appended(a, b, "\n")
            }
        }
        (
            S::ToolCall {
                id: ia,
                name: na,
                arguments: a,
                ..
            },
            S::ToolCall {
                id: ib,
                name: nb,
                arguments: b,
                ..
            },
        ) if ia == ib && na == nb => {
            parts_appended(std::slice::from_ref(a), std::slice::from_ref(b), "")
        }
        _ => None,
    }
}

/// The text `new` adds to `old` when every old part is a prefix of the
/// same-numbered new part, the last old part being the only one allowed
/// to have grown, and any further parts are new.
fn parts_appended(old: &[AStr], new: &[AStr], separator: &str) -> Option<String> {
    if new.len() < old.len() || old.is_empty() {
        return None;
    }
    let mut text = String::new();
    for (index, (a, b)) in old.iter().zip(new).enumerate() {
        match a.diff(b) {
            Diff::LeftIsPrefix if index + 1 == old.len() => {
                b.with_str(|b| text.push_str(&b[a.len()..]));
            }
            Diff::LeftIsPrefix if a.len() == b.len() => {}
            _ => return None,
        }
    }
    for part in &new[old.len()..] {
        text.push_str(separator);
        part.with_str(|part| text.push_str(part));
    }
    Some(text)
}

fn reasoning_text(content: &AStr, summary: &[AStr]) -> String {
    if summary.is_empty() {
        content.to_string()
    } else {
        join(summary)
    }
}

fn join(parts: &[AStr]) -> String {
    parts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Arc;

    use rho_core::{AppendString, MessagePhase, PendingInferenceResponse};
    use senax_encoder::{Decode, Encode};

    use super::*;
    use crate::FailedInferenceResponse;

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct TestProviderData;

    impl senax_encoder::TaggedSenax for TestProviderData {
        const TAG: &'static str = "rho-agent-live-test.provider-data";
    }

    fn message(parts: &[&AppendString], phase: Option<MessagePhase>) -> StreamingContextItem {
        StreamingContextItem::AssistantMessage {
            provider_specific: Box::new(TestProviderData),
            content: parts.iter().map(|part| part.snapshot()).collect(),
            phase,
        }
    }

    fn streaming(items: Vec<StreamingContextItem>) -> AgentStateKind {
        AgentStateKind::ApiStreaming {
            pending_response: PendingInferenceResponse {
                items: items
                    .into_iter()
                    .map(StreamingContextItemState::Pending)
                    .collect(),
            },
            previous_attempt: None,
        }
    }

    /// Claude starts a new message within one request once the last is
    /// in the log: the tail told for the last one must go, or the client
    /// shows it twice.
    #[test]
    fn a_response_started_over_empties_the_tail_first() {
        let mut teller = Teller::default();
        let first = AppendString::from("first".to_owned());
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&first], None)])),
            vec![
                Live::Requesting,
                Live::Item {
                    index: 0,
                    item: Item::Text {
                        text: "first".to_owned(),
                        phase: None
                    }
                }
            ]
        );
        assert_eq!(teller.tell(&streaming(vec![])), vec![Live::Requesting]);
        let second = AppendString::from("second".to_owned());
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&second], None)])),
            vec![Live::Item {
                index: 0,
                item: Item::Text {
                    text: "second".to_owned(),
                    phase: None
                }
            }]
        );
        // The same length told again is not a start-over.
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&second], None)])),
            Vec::<Live>::new()
        );
    }

    #[test]
    fn tells_first_sight_then_appends() {
        let mut teller = Teller::default();
        assert_eq!(teller.tell(&streaming(vec![])), vec![Live::Requesting]);
        let mut buffer = AppendString::from("hel".to_owned());
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&buffer], None)])),
            vec![Live::Item {
                index: 0,
                item: Item::Text {
                    text: "hel".to_owned(),
                    phase: None
                }
            }]
        );
        buffer.push_str("lo");
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&buffer], None)])),
            vec![Live::Appended {
                index: 0,
                text: "lo".to_owned()
            }]
        );
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&buffer], None)])),
            vec![]
        );
        let second = AppendString::from(" world".to_owned());
        assert_eq!(
            teller.tell(&streaming(vec![message(&[&buffer, &second], None)])),
            vec![Live::Appended {
                index: 0,
                text: " world".to_owned()
            }]
        );
        assert_eq!(
            teller.tell(&streaming(vec![message(
                &[&buffer, &second],
                Some(MessagePhase::FinalAnswer)
            )])),
            vec![Live::Item {
                index: 0,
                item: Item::Text {
                    text: "hello world".to_owned(),
                    phase: Some(TextPhase::FinalAnswer)
                }
            }]
        );
    }

    #[test]
    fn phases_and_retries_empty_the_tail() {
        let mut teller = Teller::default();
        let buffer = AppendString::from("partial".to_owned());
        teller.tell(&streaming(vec![message(&[&buffer], None)]));
        let retry = AgentStateKind::ApiStreaming {
            pending_response: PendingInferenceResponse::default(),
            previous_attempt: Some(FailedInferenceResponse {
                partial_response: PendingInferenceResponse::default(),
                attempt_count: NonZeroU64::new(1).unwrap(),
                error: Arc::new("boom".to_owned()),
            }),
        };
        assert_eq!(teller.tell(&retry), vec![Live::Requesting]);
        assert_eq!(teller.tell(&retry), vec![]);
        // The same buffer after a retry is a first sight again.
        let again = AgentStateKind::ApiStreaming {
            pending_response: PendingInferenceResponse {
                items: vec![StreamingContextItemState::Pending(message(
                    &[&buffer],
                    None,
                ))],
            },
            previous_attempt: Some(FailedInferenceResponse {
                partial_response: PendingInferenceResponse::default(),
                attempt_count: NonZeroU64::new(1).unwrap(),
                error: Arc::new("boom".to_owned()),
            }),
        };
        assert!(matches!(
            teller.tell(&again)[..],
            [Live::Item { index: 0, .. }]
        ));
        assert_eq!(
            teller.tell(&AgentStateKind::ToolCalling {
                previews: Default::default(),
                results: Vec::new(),
                waiting: None,
            }),
            vec![Live::Waiting { until: None }]
        );
        assert_eq!(teller.tell(&AgentStateKind::Idle), vec![Live::Idle]);
        assert_eq!(teller.tell(&AgentStateKind::Idle), vec![]);
        teller.reset();
        assert_eq!(teller.tell(&AgentStateKind::Idle), vec![Live::Idle]);
    }
}
