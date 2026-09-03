//! The local mirror: what rho knows about a workspace, on disk.
//!
//! Surfaces render from here first and refresh behind it, so a restart shows
//! the conversation before the socket is up and an offline workspace is
//! fully readable. The file is the GUI's own (`~/.local/state/rho/slack.redb`,
//! mode 0600); nothing else reads it.
//!
//! The shape is matrix-rust-sdk's event cache, simplified for Slack. There, a
//! room's history is a chain of chunks with explicit gaps between them. Slack
//! orders every message in a conversation by `ts` and has no state events, so
//! the chain does not need storing: messages live in one range-scannable run
//! and the only things written down are the discontinuities. A chunk is "the
//! run between two gap records", derived on read.
//!
//! A gap is a record, never an assumption. It carries the cursor needed to
//! fill it — for Slack, the `latest` timestamp to page back from — so
//! `shift-p` knows what to ask for and a hole is never mistaken for the
//! beginning of history. The beginning of history is its own fact, recorded
//! when a page comes back with `has_more: false`.

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

use crate::types::{
    Attachment, ChannelId, Conversation, ConversationKind, FileSummary, Message, Reaction, Ts,
    User, UserId,
};

/// Keys are composed strings rather than encoded tuples, because redb orders
/// them by bytes and a range scan over one conversation has to come out in
/// `ts` order. Slack's timestamps are fixed width (`1756800000.000000`), so
/// byte order and time order agree.
const SEPARATOR: char = '\u{1f}';

const MESSAGES: TableDefinition<&str, Sen<StoredMessage>> =
    TableDefinition::new("rho_slack_messages_v1");
const GAPS: TableDefinition<&str, Sen<StoredGap>> = TableDefinition::new("rho_slack_gaps_v1");
const USERS: TableDefinition<&str, Sen<StoredUser>> = TableDefinition::new("rho_slack_users_v1");
const CONVERSATIONS: TableDefinition<&str, Sen<StoredConversation>> =
    TableDefinition::new("rho_slack_conversations_v1");
const CURSORS: TableDefinition<&str, Sen<StoredCursor>> =
    TableDefinition::new("rho_slack_cursors_v1");

/// One run of history: a conversation, or one thread inside it. A thread is
/// its own run because Slack pages it separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub workspace: String,
    pub channel: ChannelId,
    /// The thread's parent, or `None` for the conversation itself.
    pub thread: Option<Ts>,
}

impl Scope {
    pub fn conversation(workspace: &str, channel: &ChannelId) -> Self {
        Self {
            workspace: workspace.to_owned(),
            channel: channel.clone(),
            thread: None,
        }
    }

    pub fn thread(workspace: &str, channel: &ChannelId, thread: &Ts) -> Self {
        Self {
            workspace: workspace.to_owned(),
            channel: channel.clone(),
            thread: Some(thread.clone()),
        }
    }

    fn prefix(&self) -> String {
        let thread = self.thread.as_ref().map(Ts::as_str).unwrap_or("");
        format!(
            "{}{SEPARATOR}{}{SEPARATOR}{thread}{SEPARATOR}",
            self.workspace,
            self.channel.as_str()
        )
    }

    fn key(&self, ts: &Ts) -> String {
        format!("{}{}", self.prefix(), ts.as_str())
    }

    /// The exclusive end of this scope's range. `\u{20}` is the first byte a
    /// timestamp can never start with, so it closes the prefix.
    fn end(&self) -> String {
        let mut end = self.prefix();
        end.pop();
        end.push('\u{20}');
        end
    }
}

/// A hole in a conversation's history, and the cursor that fills it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    /// Where to page back from: Slack's `latest` parameter.
    pub page_before: Ts,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredGap {
    page_before: String,
}

pub struct Mirror {
    db: RhoDb,
}

impl Mirror {
    /// Opens the mirror, creating it if this is the first run. The file holds
    /// the user's messages, so it is theirs alone to read.
    pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = RhoDb::open(path);
        // Tables are created up front so a read on a fresh mirror is a miss
        // rather than a panic.
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write.open_table(MESSAGES);
            write.open_table(GAPS);
            write.open_table(USERS);
            write.open_table(CONVERSATIONS);
            write.open_table(CURSORS);
            write.commit();
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { db })
    }

    /// Writes messages, newest wins. Two copies of one `ts` are one message:
    /// the key is the timestamp, so a re-fetched page overwrites rather than
    /// duplicating, which is Slack's own identity rule.
    pub fn insert_messages(&self, scope: &Scope, messages: &[Message]) {
        if messages.is_empty() {
            return;
        }
        let mut txn = self.write();
        {
            let mut table = txn.open_table(MESSAGES);
            for message in messages {
                table.insert(
                    scope.key(&message.ts).as_str(),
                    SenValue::owned(StoredMessage::from(message)),
                );
            }
        }
        txn.commit();
    }

    /// A `message_deleted` frame: the message is gone, and the mirror must
    /// forget it rather than keep a copy the user cannot see anywhere else.
    pub fn remove_message(&self, scope: &Scope, ts: &Ts) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(MESSAGES);
            table.remove(scope.key(ts).as_str());
        }
        txn.commit();
    }

    /// The newest messages, oldest first, stopping at the first gap below
    /// them: that run is the newest chunk, which is what opening shows.
    pub fn newest_chunk(&self, scope: &Scope, limit: usize) -> Vec<Message> {
        let floor = self.gap_below(scope, None).map(|(at, _)| at);
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        let mut messages = Vec::new();
        let mut iter = table.range(scope.prefix().as_str()..scope.end().as_str());
        while let Some((key, value)) = iter.next_back() {
            // The gap sits at the oldest message of this chunk: everything
            // below it is unknown, so the run stops there and includes it.
            if let Some(floor) = &floor
                && ts_of(key.value()).is_some_and(|ts| floor.is_newer_than(&ts))
            {
                break;
            }
            messages.push(value.value().as_ref().into());
            if messages.len() >= limit {
                break;
            }
        }
        messages.reverse();
        messages
    }

    /// Everything the mirror holds for a scope, oldest first. Gaps are not
    /// hidden here: a caller that has already filled them wants the lot.
    pub fn all_messages(&self, scope: &Scope) -> Vec<Message> {
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        table
            .range(scope.prefix().as_str()..scope.end().as_str())
            .map(|(_, value)| value.value().as_ref().into())
            .collect()
    }

    /// Whether a particular message is already on disk. A ping for something
    /// the mirror holds costs no request at all.
    pub fn holds(&self, scope: &Scope, ts: &Ts) -> bool {
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        table.get(scope.key(ts).as_str()).is_some()
    }

    /// The newest timestamp held, which is what a refresh asks Slack for
    /// messages newer than. Nothing already mirrored is fetched twice.
    pub fn newest_ts(&self, scope: &Scope) -> Option<Ts> {
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        let mut iter = table.range(scope.prefix().as_str()..scope.end().as_str());
        iter.next_back().and_then(|(key, _)| ts_of(key.value()))
    }

    pub fn oldest_ts(&self, scope: &Scope) -> Option<Ts> {
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        table
            .range(scope.prefix().as_str()..scope.end().as_str())
            .next()
            .and_then(|(key, _)| ts_of(key.value()))
    }

    /// Records a hole at `at`, to be filled by paging back from
    /// `page_before`. Writing one is how the mirror admits it does not know
    /// what came before, rather than joining two runs that never met.
    pub fn put_gap(&self, scope: &Scope, at: &Ts, page_before: &Ts) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(GAPS);
            table.insert(
                scope.key(at).as_str(),
                SenValue::owned(StoredGap {
                    page_before: page_before.0.clone(),
                }),
            );
        }
        txn.commit();
    }

    pub fn clear_gap(&self, scope: &Scope, at: &Ts) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(GAPS);
            table.remove(scope.key(at).as_str());
        }
        txn.commit();
    }

    /// The newest gap at or below `below`, which is the one `shift-p` fills.
    pub fn gap_below(&self, scope: &Scope, below: Option<&Ts>) -> Option<(Ts, Gap)> {
        let txn = self.db.read();
        let table = txn.open_table(GAPS);
        let end = match below {
            Some(below) => scope.key(below),
            None => scope.end(),
        };
        let mut iter = table.range(scope.prefix().as_str()..end.as_str());
        iter.next_back().and_then(|(key, value)| {
            let at = ts_of(key.value())?;
            let gap = Gap {
                page_before: Ts(value.value().as_ref().page_before.clone()),
            };
            Some((at, gap))
        })
    }

    /// Whether the mirror has reached the first message ever sent here.
    /// `shift-p` at the top is then an echo, not a request.
    pub fn history_begins(&self, scope: &Scope) -> bool {
        matches!(
            self.cursor(&format!("{}begins", scope.prefix())),
            Some(StoredCursor::Flag(true))
        )
    }

    /// Called when a page comes back with `has_more: false`: there is nothing
    /// older, so the gap at that boundary is not a hole but the start.
    pub fn set_history_begins(&self, scope: &Scope) {
        self.put_cursor(
            &format!("{}begins", scope.prefix()),
            StoredCursor::Flag(true),
        );
    }

    /// How far the activity feed has been read, so a restart does not deal
    /// the same pings again.
    pub fn activity_cursor(&self, workspace: &str) -> Option<Ts> {
        match self.cursor(&format!("{workspace}{SEPARATOR}activity")) {
            Some(StoredCursor::Stamp(ts)) => Some(Ts(ts)),
            _ => None,
        }
    }

    pub fn set_activity_cursor(&self, workspace: &str, ts: &Ts) {
        self.put_cursor(
            &format!("{workspace}{SEPARATOR}activity"),
            StoredCursor::Stamp(ts.0.clone()),
        );
    }

    /// Who the reader is. Kept because a group DM is named after everyone
    /// *else* in it: without this the mirror would name the reader to
    /// themselves until the socket connects.
    pub fn self_id(&self, workspace: &str) -> Option<UserId> {
        match self.cursor(&format!("{workspace}{SEPARATOR}self")) {
            Some(StoredCursor::Stamp(id)) => Some(UserId(id)),
            _ => None,
        }
    }

    pub fn set_self_id(&self, workspace: &str, id: &UserId) {
        self.put_cursor(
            &format!("{workspace}{SEPARATOR}self"),
            StoredCursor::Stamp(id.0.clone()),
        );
    }

    pub fn last_read(&self, scope: &Scope) -> Option<Ts> {
        match self.cursor(&format!("{}read", scope.prefix())) {
            Some(StoredCursor::Stamp(ts)) => Some(Ts(ts)),
            _ => None,
        }
    }

    pub fn set_last_read(&self, scope: &Scope, ts: &Ts) {
        self.put_cursor(
            &format!("{}read", scope.prefix()),
            StoredCursor::Stamp(ts.0.clone()),
        );
    }

    pub fn put_users(&self, workspace: &str, users: &[User]) {
        if users.is_empty() {
            return;
        }
        let mut txn = self.write();
        {
            let mut table = txn.open_table(USERS);
            for user in users {
                table.insert(
                    format!("{workspace}{SEPARATOR}{}", user.id.as_str()).as_str(),
                    SenValue::owned(StoredUser {
                        id: user.id.0.clone(),
                        name: user.name.clone(),
                        handle: user.handle.clone(),
                    }),
                );
            }
        }
        txn.commit();
    }

    pub fn users(&self, workspace: &str) -> Vec<User> {
        let txn = self.db.read();
        let table = txn.open_table(USERS);
        let prefix = format!("{workspace}{SEPARATOR}");
        let end = format!(
            "{workspace}{}",
            char::from_u32(SEPARATOR as u32 + 1).unwrap()
        );
        table
            .range(prefix.as_str()..end.as_str())
            .map(|(_, value)| {
                let stored = value.value();
                let stored = stored.as_ref();
                User {
                    id: UserId(stored.id.clone()),
                    name: stored.name.clone(),
                    handle: stored.handle.clone(),
                }
            })
            .collect()
    }

    pub fn put_conversations(&self, workspace: &str, conversations: &[Conversation]) {
        if conversations.is_empty() {
            return;
        }
        let mut txn = self.write();
        {
            let mut table = txn.open_table(CONVERSATIONS);
            for conversation in conversations {
                table.insert(
                    format!("{workspace}{SEPARATOR}{}", conversation.id.as_str()).as_str(),
                    SenValue::owned(StoredConversation::from(conversation)),
                );
            }
        }
        txn.commit();
    }

    pub fn conversations(&self, workspace: &str) -> Vec<Conversation> {
        let txn = self.db.read();
        let table = txn.open_table(CONVERSATIONS);
        let prefix = format!("{workspace}{SEPARATOR}");
        let end = format!(
            "{workspace}{}",
            char::from_u32(SEPARATOR as u32 + 1).unwrap()
        );
        table
            .range(prefix.as_str()..end.as_str())
            .map(|(_, value)| value.value().as_ref().into())
            .collect()
    }

    /// Every write goes through one lock; the mirror is small and the GUI is
    /// the only writer, so blocking on it is cheaper than threading async
    /// through every surface.
    fn write(&self) -> rho_db::WriteTxn {
        futures::executor::block_on(self.db.write())
    }

    fn cursor(&self, key: &str) -> Option<StoredCursor> {
        let txn = self.db.read();
        let table = txn.open_table(CURSORS);
        table.get(key).map(|value| value.value().into_owned())
    }

    fn put_cursor(&self, key: &str, cursor: StoredCursor) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(CURSORS);
            table.insert(key, SenValue::owned(cursor));
        }
        txn.commit();
    }
}

/// The timestamp part of a composed key.
fn ts_of(key: &str) -> Option<Ts> {
    key.rsplit(SEPARATOR).next().map(Ts::from)
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum StoredCursor {
    Stamp(String),
    Flag(bool),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredUser {
    id: String,
    name: String,
    handle: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredConversation {
    id: String,
    kind: u8,
    name: String,
    user: Option<String>,
    members: Vec<String>,
}

impl From<&Conversation> for StoredConversation {
    fn from(conversation: &Conversation) -> Self {
        Self {
            id: conversation.id.0.clone(),
            kind: match conversation.kind {
                ConversationKind::Channel => 0,
                ConversationKind::Group => 1,
                ConversationKind::DirectMessage => 2,
            },
            name: conversation.name.clone(),
            user: conversation.user.as_ref().map(|user| user.0.clone()),
            members: conversation
                .members
                .iter()
                .map(|member| member.0.clone())
                .collect(),
        }
    }
}

impl From<&StoredConversation> for Conversation {
    fn from(stored: &StoredConversation) -> Self {
        Self {
            id: ChannelId(stored.id.clone()),
            kind: match stored.kind {
                0 => ConversationKind::Channel,
                1 => ConversationKind::Group,
                _ => ConversationKind::DirectMessage,
            },
            name: stored.name.clone(),
            user: stored.user.clone().map(UserId),
            members: stored.members.iter().cloned().map(UserId).collect(),
        }
    }
}

/// A message as the mirror keeps it. Block Kit stays as the JSON text Slack
/// sent, because it is rendered late: a name that is unknown today resolves
/// the next time the conversation is drawn.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredMessage {
    ts: String,
    thread_ts: Option<String>,
    channel: String,
    user: Option<String>,
    bot_name: Option<String>,
    blocks: Vec<String>,
    text: String,
    attachments: Vec<StoredAttachment>,
    files: Vec<StoredFile>,
    subtype: Option<String>,
    reply_count: u32,
    latest_reply: Option<String>,
    edited: bool,
    reactions: Vec<StoredReaction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredAttachment {
    title: Option<String>,
    text: Option<String>,
    fallback: Option<String>,
    pretext: Option<String>,
    fields: Vec<(String, String)>,
    is_unfurl: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredFile {
    id: String,
    title: String,
    filetype: String,
    size: u64,
    url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredReaction {
    name: String,
    count: u32,
    users: Vec<String>,
}

impl From<&Message> for StoredMessage {
    fn from(message: &Message) -> Self {
        Self {
            ts: message.ts.0.clone(),
            thread_ts: message.thread_ts.as_ref().map(|ts| ts.0.clone()),
            channel: message.channel.0.clone(),
            user: message.user.as_ref().map(|user| user.0.clone()),
            bot_name: message.bot_name.clone(),
            blocks: message
                .blocks
                .iter()
                .map(|block| block.to_string())
                .collect(),
            text: message.text.clone(),
            attachments: message
                .attachments
                .iter()
                .map(|attachment| StoredAttachment {
                    title: attachment.title.clone(),
                    text: attachment.text.clone(),
                    fallback: attachment.fallback.clone(),
                    pretext: attachment.pretext.clone(),
                    fields: attachment.fields.clone(),
                    is_unfurl: attachment.is_unfurl,
                })
                .collect(),
            files: message
                .files
                .iter()
                .map(|file| StoredFile {
                    id: file.id.clone(),
                    title: file.title.clone(),
                    filetype: file.filetype.clone(),
                    size: file.size,
                    url: file.url.clone(),
                })
                .collect(),
            subtype: message.subtype.clone(),
            reply_count: message.reply_count,
            latest_reply: message.latest_reply.as_ref().map(|ts| ts.0.clone()),
            edited: message.edited,
            reactions: message
                .reactions
                .iter()
                .map(|reaction| StoredReaction {
                    name: reaction.name.clone(),
                    count: reaction.count,
                    users: reaction.users.iter().map(|user| user.0.clone()).collect(),
                })
                .collect(),
        }
    }
}

impl From<&StoredMessage> for Message {
    fn from(stored: &StoredMessage) -> Self {
        Self {
            ts: Ts(stored.ts.clone()),
            thread_ts: stored.thread_ts.clone().map(Ts),
            channel: ChannelId(stored.channel.clone()),
            user: stored.user.clone().map(UserId),
            bot_name: stored.bot_name.clone(),
            blocks: stored
                .blocks
                .iter()
                .filter_map(|block| serde_json::from_str(block).ok())
                .collect(),
            text: stored.text.clone(),
            attachments: stored
                .attachments
                .iter()
                .map(|attachment| Attachment {
                    title: attachment.title.clone(),
                    text: attachment.text.clone(),
                    fallback: attachment.fallback.clone(),
                    pretext: attachment.pretext.clone(),
                    fields: attachment.fields.clone(),
                    is_unfurl: attachment.is_unfurl,
                })
                .collect(),
            files: stored
                .files
                .iter()
                .map(|file| FileSummary {
                    id: file.id.clone(),
                    title: file.title.clone(),
                    filetype: file.filetype.clone(),
                    size: file.size,
                    url: file.url.clone(),
                })
                .collect(),
            subtype: stored.subtype.clone(),
            reply_count: stored.reply_count,
            latest_reply: stored.latest_reply.clone().map(Ts),
            edited: stored.edited,
            reactions: stored
                .reactions
                .iter()
                .map(|reaction| Reaction {
                    name: reaction.name.clone(),
                    count: reaction.count,
                    users: reaction.users.iter().cloned().map(UserId).collect(),
                })
                .collect(),
        }
    }
}
