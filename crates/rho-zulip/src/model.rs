use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Default)]
pub struct Model {
    me: Option<u64>,
    users: HashMap<u64, String>,
    streams: HashMap<u64, StreamInfo>,
    topics: HashMap<(u64, String), BTreeSet<u64>>,
    dms: HashMap<Vec<u64>, BTreeSet<u64>>,
    mentions: HashSet<u64>,
}

pub struct StreamInfo {
    pub stream_id: u64,
    pub name: String,
    pub muted: bool,
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    pub kind: InboxRowKind,
    /// Display text without counts; the view appends those itself.
    pub label: String,
    pub unread: u32,
    pub mentions: u32,
    /// What `enter` on this row opens. `None` for a pure header.
    pub narrow: Option<crate::Narrow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxRowKind {
    Section,
    Stream,
    Topic,
    Dm,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub inbox: bool,
    pub message: Option<(crate::Narrow, u64)>,
    pub updated: Vec<u64>,
}

impl Model {
    pub fn apply_register(&mut self, response: crate::types::RegisterResponse) {
        self.me = response.user_id;
        self.users = response
            .realm_users
            .into_iter()
            .map(|user| (user.user_id, user.full_name))
            .collect();
        self.streams = response
            .subscriptions
            .into_iter()
            .map(|subscription| {
                (
                    subscription.stream_id,
                    StreamInfo {
                        stream_id: subscription.stream_id,
                        name: subscription.name,
                        muted: subscription.is_muted,
                        pinned: subscription.pin_to_top,
                    },
                )
            })
            .collect();
        self.topics.clear();
        for unread in response.unread_msgs.streams {
            self.topics.insert(
                (unread.stream_id, unread.topic),
                unread.unread_message_ids.into_iter().collect(),
            );
        }
        self.dms.clear();
        for unread in response.unread_msgs.dms {
            let key = self.dm_key_from_other(unread.other_user_id);
            self.dms
                .entry(key)
                .or_default()
                .extend(unread.unread_message_ids);
        }
        self.mentions = response.unread_msgs.mentions.into_iter().collect();
    }

    pub fn apply_event(&mut self, event: &crate::types::Event) -> Change {
        match event {
            crate::types::Event::Message { message, flags, .. } => {
                let narrow = self.narrow_of(message);
                let message_flags = if flags.is_empty() {
                    &message.flags
                } else {
                    flags
                };
                let unread = !message_flags.iter().any(|flag| flag == "read")
                    && Some(message.sender_id) != self.me;
                let mut change = Change {
                    inbox: false,
                    message: narrow.clone().map(|narrow| (narrow, message.id)),
                    updated: Vec::new(),
                };
                if unread && let Some(narrow) = narrow {
                    self.insert_unread(&narrow, message.id);
                    if message_flags
                        .iter()
                        .any(|flag| flag == "mentioned" || flag == "wildcard_mentioned")
                    {
                        self.mentions.insert(message.id);
                    }
                    change.inbox = true;
                }
                change
            }
            crate::types::Event::UpdateMessageFlags {
                op, flag, messages, ..
            } => {
                let mut change = Change {
                    inbox: false,
                    message: None,
                    updated: messages.clone(),
                };
                if op == "add" && flag == "read" {
                    for message_id in messages {
                        change.inbox |= self.remove_unread(*message_id);
                        change.inbox |= self.mentions.remove(message_id);
                    }
                }
                change
            }
            crate::types::Event::UpdateMessage { message_id, .. }
            | crate::types::Event::Reaction { message_id, .. } => Change {
                inbox: false,
                message: None,
                updated: vec![*message_id],
            },
            crate::types::Event::Other => Change::default(),
        }
    }

    pub fn me(&self) -> Option<u64> {
        self.me
    }

    pub fn user_name(&self, user_id: u64) -> Option<&str> {
        self.users.get(&user_id).map(String::as_str)
    }

    pub fn stream(&self, stream_id: u64) -> Option<&StreamInfo> {
        self.streams.get(&stream_id)
    }

    pub fn narrow_candidates(&self) -> Vec<crate::Narrow> {
        let mut candidates = Vec::new();
        for row in self.inbox_rows() {
            if matches!(
                row.kind,
                InboxRowKind::Stream | InboxRowKind::Topic | InboxRowKind::Dm
            ) && let Some(narrow) = row.narrow
            {
                candidates.push(narrow);
            }
        }
        // A zero-unread stream has no inbox row, but remains a valid place
        // to start reading and therefore must remain completable.
        let seen: HashSet<u64> = candidates
            .iter()
            .filter_map(|narrow| match narrow {
                crate::Narrow::Stream { stream_id, .. } => Some(*stream_id),
                _ => None,
            })
            .collect();
        let mut remaining: Vec<_> = self
            .streams
            .values()
            .filter(|stream| !seen.contains(&stream.stream_id))
            .collect();
        remaining.sort_by(|left, right| left.name.cmp(&right.name));
        candidates.extend(remaining.into_iter().map(|stream| crate::Narrow::Stream {
            stream_id: stream.stream_id,
            stream: stream.name.clone(),
        }));
        candidates
    }

    pub fn inbox_rows(&self) -> Vec<InboxRow> {
        let mut rows = Vec::new();
        if !self.mentions.is_empty() {
            rows.push(section("Mentions"));
            rows.push(InboxRow {
                kind: InboxRowKind::Topic,
                label: "Mentions".to_owned(),
                unread: self.mentions.len() as u32,
                mentions: self.mentions.len() as u32,
                narrow: Some(crate::Narrow::Mentions),
            });
        }

        let mut dms: Vec<_> = self
            .dms
            .iter()
            .filter(|(_, unread)| !unread.is_empty())
            .collect();
        dms.sort_by_key(|(user_ids, _)| self.dm_label(user_ids));
        if !dms.is_empty() {
            rows.push(section("Direct messages"));
            rows.extend(dms.into_iter().map(|(user_ids, unread)| InboxRow {
                kind: InboxRowKind::Dm,
                label: self.dm_label(user_ids),
                unread: unread.len() as u32,
                mentions: 0,
                narrow: Some(crate::Narrow::Dm {
                    user_ids: user_ids.clone(),
                    label: self.dm_label(user_ids),
                }),
            }));
        }

        let mut streams: Vec<_> = self
            .streams
            .values()
            .filter(|stream| {
                !stream.muted && (stream.pinned || self.stream_unread(stream.stream_id) > 0)
            })
            .collect();
        streams.sort_by(|left, right| {
            right
                .pinned
                .cmp(&left.pinned)
                .then_with(|| left.name.cmp(&right.name))
        });
        if !streams.is_empty() {
            rows.push(section("Streams"));
            for stream in streams {
                let unread = self.stream_unread(stream.stream_id);
                rows.push(InboxRow {
                    kind: InboxRowKind::Stream,
                    label: stream.name.clone(),
                    unread,
                    mentions: 0,
                    narrow: Some(crate::Narrow::Stream {
                        stream_id: stream.stream_id,
                        stream: stream.name.clone(),
                    }),
                });
                let mut topics: Vec<_> = self
                    .topics
                    .iter()
                    .filter(|((stream_id, _), unread)| {
                        *stream_id == stream.stream_id && !unread.is_empty()
                    })
                    .collect();
                topics.sort_by(|((_, left), _), ((_, right), _)| left.cmp(right));
                rows.extend(topics.into_iter().map(|((_, topic), unread)| InboxRow {
                    kind: InboxRowKind::Topic,
                    label: topic.clone(),
                    unread: unread.len() as u32,
                    mentions: 0,
                    narrow: Some(crate::Narrow::Topic {
                        stream_id: stream.stream_id,
                        stream: stream.name.clone(),
                        topic: topic.clone(),
                    }),
                }));
            }
        }
        rows
    }

    pub fn next_unread(&self, current: Option<&crate::Narrow>) -> Option<crate::Narrow> {
        // Only leaf conversations are stops on the reading loop: a stream
        // row is a superset of the topic rows beneath it, so visiting it
        // would show the same messages twice.
        let unread: Vec<_> = self
            .inbox_rows()
            .into_iter()
            .filter(|row| matches!(row.kind, InboxRowKind::Topic | InboxRowKind::Dm))
            .filter_map(|row| (row.unread > 0).then_some(row.narrow).flatten())
            .collect();
        if unread.is_empty() {
            return None;
        }
        let next = current
            .and_then(|current| unread.iter().position(|narrow| narrow == current))
            .map(|index| (index + 1) % unread.len())
            .unwrap_or(0);
        Some(unread[next].clone())
    }

    pub fn unread_in(&self, narrow: &crate::Narrow) -> Vec<u64> {
        let mut ids = BTreeSet::new();
        match narrow {
            crate::Narrow::Topic {
                stream_id, topic, ..
            } => {
                if let Some(unread) = self.topics.get(&(*stream_id, topic.clone())) {
                    ids.extend(unread);
                }
            }
            crate::Narrow::Stream { stream_id, .. } => {
                for ((topic_stream, _), unread) in &self.topics {
                    if topic_stream == stream_id {
                        ids.extend(unread);
                    }
                }
            }
            crate::Narrow::Dm { user_ids, .. } => {
                if let Some(unread) = self.dms.get(user_ids) {
                    ids.extend(unread);
                }
            }
            crate::Narrow::Mentions => ids.extend(&self.mentions),
            crate::Narrow::Combined => {
                for unread in self.topics.values().chain(self.dms.values()) {
                    ids.extend(unread);
                }
            }
            crate::Narrow::Starred => {}
        }
        ids.into_iter().collect()
    }

    pub fn total_unread(&self) -> u32 {
        self.topics
            .values()
            .map(|unread| unread.len() as u32)
            .sum::<u32>()
            + self
                .dms
                .values()
                .map(|unread| unread.len() as u32)
                .sum::<u32>()
    }

    pub fn total_mentions(&self) -> u32 {
        self.mentions.len() as u32
    }

    pub fn narrow_of(&self, message: &crate::types::Message) -> Option<crate::Narrow> {
        if message.is_stream() {
            let stream_id = message.stream_id?;
            let stream = self.streams.get(&stream_id)?;
            return Some(crate::Narrow::Topic {
                stream_id,
                stream: stream.name.clone(),
                topic: message.topic.clone(),
            });
        }
        let key = self.dm_key(&message.dm_recipients())?;
        Some(crate::Narrow::Dm {
            label: self.dm_label(&key),
            user_ids: key,
        })
    }

    fn dm_key(&self, recipients: &[u64]) -> Option<Vec<u64>> {
        let me = self.me?;
        if recipients.is_empty() {
            return None;
        }
        let mut key: Vec<_> = recipients.iter().copied().filter(|id| *id != me).collect();
        key.sort_unstable();
        key.dedup();
        if key.is_empty() {
            key.push(me);
        }
        Some(key)
    }

    fn dm_key_from_other(&self, other_user_id: u64) -> Vec<u64> {
        vec![other_user_id]
    }

    fn dm_label(&self, user_ids: &[u64]) -> String {
        user_ids
            .iter()
            .map(|user_id| {
                self.users
                    .get(user_id)
                    .cloned()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| user_id.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn insert_unread(&mut self, narrow: &crate::Narrow, message_id: u64) {
        match narrow {
            crate::Narrow::Topic {
                stream_id, topic, ..
            } => {
                self.topics
                    .entry((*stream_id, topic.clone()))
                    .or_default()
                    .insert(message_id);
            }
            crate::Narrow::Dm { user_ids, .. } => {
                self.dms
                    .entry(user_ids.clone())
                    .or_default()
                    .insert(message_id);
            }
            _ => {}
        }
    }

    fn remove_unread(&mut self, message_id: u64) -> bool {
        let mut removed = false;
        for unread in self.topics.values_mut().chain(self.dms.values_mut()) {
            removed |= unread.remove(&message_id);
        }
        removed
    }

    fn stream_unread(&self, stream_id: u64) -> u32 {
        self.topics
            .iter()
            .filter(|((topic_stream, _), _)| *topic_stream == stream_id)
            .map(|(_, unread)| unread.len() as u32)
            .sum()
    }
}

fn section(label: &str) -> InboxRow {
    InboxRow {
        kind: InboxRowKind::Section,
        label: label.to_owned(),
        unread: 0,
        mentions: 0,
        narrow: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn model() -> Model {
        let mut model = Model::default();
        model.apply_register(
            serde_json::from_value(json!({
                "queue_id": "queue", "last_event_id": 0, "user_id": 1,
                "realm_users": [
                    {"user_id": 1, "full_name": "Me"},
                    {"user_id": 2, "full_name": "Ada"},
                    {"user_id": 3, "full_name": "Bert"}
                ],
                "subscriptions": [
                    {"stream_id": 10, "name": "alpha", "pin_to_top": true},
                    {"stream_id": 11, "name": "beta"},
                    {"stream_id": 12, "name": "muted", "is_muted": true}
                ],
                "unread_msgs": {
                    "streams": [
                        {"stream_id": 10, "topic": "z", "unread_message_ids": [101]},
                        {"stream_id": 11, "topic": "a", "unread_message_ids": [102, 103]},
                        {"stream_id": 12, "topic": "hidden", "unread_message_ids": [104]}
                    ],
                    "dms": [{"other_user_id": 2, "unread_message_ids": [201]}],
                    "mentions": [101]
                }
            }))
            .unwrap(),
        );
        model
    }

    fn message_event(message: serde_json::Value, flags: &[&str]) -> crate::types::Event {
        serde_json::from_value(
            json!({"type":"message", "id": 1, "message": message, "flags": flags}),
        )
        .unwrap()
    }

    #[test]
    fn register_builds_group_buffer_in_required_order() {
        let model = model();
        let rows = model.inbox_rows();
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Mentions",
                "Mentions",
                "Direct messages",
                "Ada",
                "Streams",
                "alpha",
                "z",
                "beta",
                "a"
            ]
        );
        assert_eq!(model.total_unread(), 5);
        assert_eq!(model.total_mentions(), 1);
    }

    #[test]
    fn message_events_increment_only_other_unread_messages() {
        let mut model = model();
        let event = message_event(
            json!({"id": 105, "sender_id": 2, "timestamp": 0, "type": "stream", "stream_id": 10, "subject": "new"}),
            &["mentioned"],
        );
        let change = model.apply_event(&event);
        assert!(change.inbox);
        assert_eq!(change.message.unwrap().1, 105);
        assert_eq!(
            model.unread_in(&crate::Narrow::Topic {
                stream_id: 10,
                stream: "alpha".into(),
                topic: "new".into()
            }),
            vec![105]
        );
        assert_eq!(model.total_mentions(), 2);

        let own = message_event(
            json!({"id": 106, "sender_id": 1, "timestamp": 0, "type": "stream", "stream_id": 10, "subject": "new"}),
            &[],
        );
        assert!(!model.apply_event(&own).inbox);
        assert_eq!(model.total_unread(), 6);
    }

    #[test]
    fn read_flags_remove_exact_ids_without_underflow() {
        let mut model = model();
        let flags: crate::types::Event = serde_json::from_value(json!({"type":"update_message_flags", "id": 2, "op":"add", "flag":"read", "messages":[101, 201, 999]})).unwrap();
        let change = model.apply_event(&flags);
        assert!(change.inbox);
        assert_eq!(change.updated, vec![101, 201, 999]);
        assert_eq!(model.total_unread(), 3);
        assert_eq!(model.total_mentions(), 0);
        assert!(!model.apply_event(&flags).inbox);
        assert_eq!(model.total_unread(), 3);
    }

    #[test]
    fn direct_messages_use_sorted_recipients_and_named_label() {
        let mut model = model();
        let event = message_event(
            json!({"id": 202, "sender_id": 3, "timestamp": 0, "type": "private", "display_recipient": [{"id": 3}, {"id": 1}, {"id": 2}]}),
            &[],
        );
        let narrow = model
            .narrow_of(match &event {
                crate::types::Event::Message { message, .. } => message,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(
            narrow,
            crate::Narrow::Dm {
                user_ids: vec![2, 3],
                label: "Ada, Bert".into()
            }
        );
        model.apply_event(&event);
        assert_eq!(model.unread_in(&narrow), vec![202]);
    }

    #[test]
    fn self_dm_uses_our_own_id_as_its_key() {
        let model = model();
        let message: crate::types::Message = serde_json::from_value(json!({
            "id": 203, "sender_id": 1, "timestamp": 0, "type": "private",
            "display_recipient": [{"id": 1}]
        }))
        .unwrap();
        assert_eq!(
            model.narrow_of(&message),
            Some(crate::Narrow::Dm {
                user_ids: vec![1],
                label: "Me".into(),
            })
        );
    }

    #[test]
    fn next_unread_wraps_in_row_order() {
        let model = model();
        let first = model.next_unread(None).unwrap();
        assert_eq!(first, crate::Narrow::Mentions);
        let next = model.next_unread(Some(&first)).unwrap();
        assert!(matches!(next, crate::Narrow::Dm { .. }));
        let last = model
            .next_unread(Some(&crate::Narrow::Topic {
                stream_id: 11,
                stream: "beta".into(),
                topic: "a".into(),
            }))
            .unwrap();
        assert_eq!(last, crate::Narrow::Mentions);
    }

    #[test]
    fn candidates_include_zero_unread_subscriptions() {
        let model = model();
        assert!(
            model
                .narrow_candidates()
                .iter()
                .any(|narrow| matches!(narrow, crate::Narrow::Stream { stream_id: 12, .. }))
        );
    }

    #[test]
    fn routing_rejects_unknown_or_recipientless_messages() {
        let model = model();
        let unknown: crate::types::Message = serde_json::from_value(
            json!({"id": 1,"sender_id":2,"timestamp":0,"type":"stream","stream_id":99}),
        )
        .unwrap();
        let empty_dm: crate::types::Message =
            serde_json::from_value(json!({"id": 1,"sender_id":2,"timestamp":0,"type":"private"}))
                .unwrap();
        assert!(model.narrow_of(&unknown).is_none());
        assert!(model.narrow_of(&empty_dm).is_none());
    }
}
