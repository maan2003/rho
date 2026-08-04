//! A narrow: the set of messages a conversation surface shows.
//!
//! Zulip narrows are search queries, so this is both the surface's identity
//! and its address bar — the same string the user types at the minibuffer
//! round-trips through [`Narrow::parse`].

use serde_json::{Value, json};

/// Which messages a surface shows. Stream ids are carried alongside names
/// because sends address the stream by id while the label reads by name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Narrow {
    /// Every subscribed message: the home view.
    Combined,
    /// Messages that mention you.
    Mentions,
    /// Starred messages.
    Starred,
    /// One topic in one stream — the unit of Zulip conversation.
    Topic {
        stream_id: u64,
        stream: String,
        topic: String,
    },
    /// Every topic in one stream.
    Stream { stream_id: u64, stream: String },
    /// A direct message conversation, group or one-to-one. `user_ids` are
    /// the recipients excluding you, sorted, so a conversation has one key.
    Dm { user_ids: Vec<u64>, label: String },
}

impl Narrow {
    /// The `narrow` parameter for `GET /messages`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Combined => json!([]),
            Self::Mentions => json!([{"operator": "is", "operand": "mentioned"}]),
            Self::Starred => json!([{"operator": "is", "operand": "starred"}]),
            Self::Topic {
                stream_id, topic, ..
            } => json!([
                {"operator": "stream", "operand": stream_id},
                {"operator": "topic", "operand": topic},
            ]),
            Self::Stream { stream_id, .. } => {
                json!([{"operator": "stream", "operand": stream_id}])
            }
            Self::Dm { user_ids, .. } => json!([{"operator": "dm", "operand": user_ids}]),
        }
    }

    /// How the narrow reads in chrome and in the minibuffer. Round-trips
    /// through [`Narrow::parse`].
    pub fn label(&self) -> String {
        match self {
            Self::Combined => "all".to_owned(),
            Self::Mentions => "is:mentioned".to_owned(),
            Self::Starred => "is:starred".to_owned(),
            Self::Topic { stream, topic, .. } => format!("#{stream} > {topic}"),
            Self::Stream { stream, .. } => format!("#{stream}"),
            Self::Dm { label, .. } => format!("dm:{label}"),
        }
    }

    /// Whether new messages in this narrow should land in an open surface.
    /// A stream narrow accepts its topics; the combined view accepts
    /// everything.
    pub fn accepts(&self, message_stream: Option<u64>, message_topic: &str) -> bool {
        match self {
            Self::Combined => true,
            Self::Stream { stream_id, .. } => message_stream == Some(*stream_id),
            Self::Topic {
                stream_id, topic, ..
            } => message_stream == Some(*stream_id) && topic == message_topic,
            // Flag-derived narrows only gain messages through their flags,
            // and DMs are matched by the model against the conversation key.
            Self::Mentions | Self::Starred | Self::Dm { .. } => false,
        }
    }

    /// Where a reply in this narrow goes, if the narrow names a single
    /// conversation. A stream-wide or combined narrow has no default
    /// destination, so its surface composes into a topic the user names.
    pub fn destination(&self) -> Option<Destination> {
        match self {
            Self::Topic {
                stream_id, topic, ..
            } => Some(Destination::Topic {
                stream_id: *stream_id,
                topic: topic.clone(),
            }),
            Self::Dm { user_ids, .. } => Some(Destination::Dm {
                user_ids: user_ids.clone(),
            }),
            Self::Combined | Self::Mentions | Self::Starred | Self::Stream { .. } => None,
        }
    }
}

/// Where a composed message is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    Topic { stream_id: u64, topic: String },
    Dm { user_ids: Vec<u64> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_narrow_addresses_stream_by_id() {
        let narrow = Narrow::Topic {
            stream_id: 7,
            stream: "design".to_owned(),
            topic: "colors".to_owned(),
        };
        assert_eq!(
            narrow.to_json(),
            json!([
                {"operator": "stream", "operand": 7},
                {"operator": "topic", "operand": "colors"},
            ])
        );
        assert_eq!(narrow.label(), "#design > colors");
    }

    #[test]
    fn stream_narrow_accepts_its_topics() {
        let narrow = Narrow::Stream {
            stream_id: 7,
            stream: "design".to_owned(),
        };
        assert!(narrow.accepts(Some(7), "anything"));
        assert!(!narrow.accepts(Some(8), "anything"));
        assert!(!narrow.accepts(None, ""));
    }

    #[test]
    fn topic_narrow_accepts_only_its_topic() {
        let narrow = Narrow::Topic {
            stream_id: 7,
            stream: "design".to_owned(),
            topic: "colors".to_owned(),
        };
        assert!(narrow.accepts(Some(7), "colors"));
        assert!(!narrow.accepts(Some(7), "fonts"));
    }
}
