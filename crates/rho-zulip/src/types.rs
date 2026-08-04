//! Zulip wire types, narrowed to what the client actually reads.
//!
//! Every struct is deliberately partial: Zulip's payloads are large and
//! grow between server versions, so unknown fields are ignored rather than
//! failing a whole event batch. Message content arrives as raw Markdown
//! (the client registers with `apply_markdown: false`), never as HTML.

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    pub id: u64,
    pub sender_id: u64,
    #[serde(default)]
    pub sender_full_name: String,
    /// Raw Markdown, as sent.
    #[serde(default)]
    pub content: String,
    pub timestamp: i64,
    /// The topic, for stream messages. Zulip's wire name is `subject`.
    #[serde(default, rename = "subject")]
    pub topic: String,
    /// `"stream"` or `"private"`.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub stream_id: Option<u64>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    /// For direct messages, everyone in the conversation including you.
    #[serde(default)]
    pub display_recipient: serde_json::Value,
}

impl Message {
    pub fn is_stream(&self) -> bool {
        self.kind == "stream"
    }

    pub fn is_read(&self) -> bool {
        self.flags.iter().any(|flag| flag == "read")
    }

    pub fn mentions_you(&self) -> bool {
        self.flags
            .iter()
            .any(|flag| flag == "mentioned" || flag == "wildcard_mentioned")
    }

    /// Recipient user ids for a direct message, as Zulip reports them
    /// (including you). Empty for stream messages.
    pub fn dm_recipients(&self) -> Vec<u64> {
        let Some(entries) = self.display_recipient.as_array() else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| entry.get("id")?.as_u64())
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Reaction {
    #[serde(default)]
    pub emoji_name: String,
    #[serde(default)]
    pub user_id: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Subscription {
    pub stream_id: u64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_muted: bool,
    #[serde(default)]
    pub pin_to_top: bool,
    #[serde(default)]
    pub invite_only: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RealmUser {
    pub user_id: u64,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub is_bot: bool,
}

/// The unread state Zulip hands out at register time: the counts the inbox
/// shows before a single message has been fetched.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UnreadMessages {
    #[serde(default)]
    pub streams: Vec<UnreadStream>,
    #[serde(default)]
    pub dms: Vec<UnreadDm>,
    #[serde(default)]
    pub mentions: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UnreadStream {
    pub stream_id: u64,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub unread_message_ids: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UnreadDm {
    /// The other party. Zulip's wire name is `other_user_id`.
    #[serde(default)]
    pub other_user_id: u64,
    #[serde(default)]
    pub unread_message_ids: Vec<u64>,
}

/// The `POST /register` response, minus everything the client ignores.
#[derive(Clone, Debug, Deserialize)]
pub struct RegisterResponse {
    pub queue_id: String,
    pub last_event_id: i64,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    #[serde(default)]
    pub realm_users: Vec<RealmUser>,
    #[serde(default)]
    pub unread_msgs: UnreadMessages,
    #[serde(default)]
    pub user_id: Option<u64>,
}

/// One event from the queue. Unrecognized types decode as [`Event::Other`]
/// so a new server-side event kind cannot stall the loop.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "message")]
    Message {
        id: i64,
        // Boxed: a message dwarfs every other event, and events travel in
        // batches.
        message: Box<Message>,
        #[serde(default)]
        flags: Vec<String>,
    },
    #[serde(rename = "update_message_flags")]
    UpdateMessageFlags {
        id: i64,
        /// `"add"` or `"remove"`.
        op: String,
        flag: String,
        #[serde(default)]
        messages: Vec<u64>,
    },
    #[serde(rename = "update_message")]
    UpdateMessage {
        id: i64,
        message_id: u64,
        #[serde(default)]
        rendered_content: Option<String>,
        #[serde(default)]
        content: Option<String>,
    },
    #[serde(rename = "reaction")]
    Reaction {
        id: i64,
        op: String,
        message_id: u64,
        #[serde(default)]
        emoji_name: String,
        #[serde(default)]
        user_id: u64,
    },
    #[serde(other)]
    Other,
}

impl Event {
    /// The queue id of the event, used to advance `last_event_id`.
    /// [`Event::Other`] carries none, so the loop tracks the batch maximum
    /// from the raw payload instead.
    pub fn id(&self) -> Option<i64> {
        match self {
            Self::Message { id, .. }
            | Self::UpdateMessageFlags { id, .. }
            | Self::UpdateMessage { id, .. }
            | Self::Reaction { id, .. } => Some(*id),
            Self::Other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_event_types_decode_as_other() {
        let event: Event =
            serde_json::from_str(r#"{"type": "presence", "id": 4, "server_timestamp": 1.0}"#)
                .expect("unknown event kinds must not fail the batch");
        assert!(matches!(event, Event::Other));
    }

    #[test]
    fn dm_recipients_read_from_display_recipient() {
        let message: Message = serde_json::from_str(
            r#"{"id": 1, "sender_id": 2, "timestamp": 0, "type": "private",
                "display_recipient": [{"id": 2}, {"id": 3}]}"#,
        )
        .expect("message decodes");
        assert_eq!(message.dm_recipients(), vec![2, 3]);
    }
}
