//! The local mirror: what rho knows about a workspace, on disk.
//!
//! Surfaces render from here first and refresh behind it, so a restart shows
//! the conversation before the socket is up and an offline workspace is
//! fully readable. These are tables in the client's own database
//! (`~/.local/state/rho/rho-client.redb`, mode 0600), which the client
//! opens once and hands in; nothing else reads them. A mirror in a file
//! of its own is still what a test, an example or a tool reading a copy
//! gets.
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

use crate::model::{Unit, UnitFacts};
use crate::types::{
    Attachment, ChannelId, Conversation, ConversationKind, FileSummary, Message, Reaction, Reason,
    Ts, User, UserId,
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
/// What rho knows about each unit it tracks: one row per conversation or
/// followed thread, not one per message.
///
/// This is what makes a start cost the units rather than the history. The
/// facts are derived from messages exactly once, when a mirror that predates
/// this table is first opened or when a rebuild is asked for; after that
/// they are written on the event that moves them and read back whole.
const UNITS: TableDefinition<&str, Sen<StoredUnit>> = TableDefinition::new("rho_slack_units_v1");
/// The channels the reader opted into. Its own table rather than a flag in
/// `CURSORS`, because the question asked of it is "which ones", and that is
/// a range scan over a workspace rather than a lookup per channel.
/// The emoji the reader has reacted with, most recent first. One row per
/// workspace: the question asked of it is "which ones, in what order",
/// which is one list and not a scan. Persisted because a picker that
/// forgets what the reader always uses is a picker they stop using.
const REACTED_WITH: TableDefinition<&str, Sen<StoredReactedWith>> =
    TableDefinition::new("rho_slack_reacted_with_v1");

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
    /// A mirror in a file of its own, at `path`. For tests, examples and
    /// the tools that read a copy; a rho client's own mirror is tables in
    /// the client's one database and comes through [`Mirror::open_on`].
    pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mirror = Self::open_on(RhoDb::open(path))?;
        // The file holds the user's messages, so it is theirs alone to
        // read. The client's own database is made the same way where it
        // is opened.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(mirror)
    }

    /// The mirror's tables in a database somebody else opened. The file
    /// holds every other kind of client state too, under its own names;
    /// this touches Slack's and nothing else.
    pub fn open_on(db: RhoDb) -> anyhow::Result<Self> {
        // Tables are created up front so a read on a fresh mirror is a miss
        // rather than a panic.
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write.open_table(MESSAGES);
            write.open_table(GAPS);
            write.open_table(USERS);
            write.open_table(CONVERSATIONS);
            write.open_table(CURSORS);
            write.open_table(REACTED_WITH);
            write.open_table(UNITS);
            write.commit();
        });
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

    /// Puts one arriving message into every scope it belongs to, in one
    /// transaction.
    ///
    /// A top-level message belongs in two: the channel, and the thread its
    /// own timestamp roots, so that a reader who opens a thread on it sees
    /// the message they opened even before Slack answers. Written scope by
    /// scope that is two durable commits for one message, and a durable
    /// commit is the floor of what an arriving message costs -- 88 µs
    /// against a microsecond of model work -- so the second one is half the
    /// bill of every message rho receives, for no fact the first does not
    /// already carry.
    ///
    /// The island rule is the same as a single write's and is applied in
    /// the same transaction: a scope the mirror holds nothing for has no
    /// telling what sits under this message, so it gets a gap of its own
    /// until a page fills it. Which scopes those are is read before the
    /// write, or the message would find itself.
    pub fn insert_live(&self, scopes: &[&Scope], message: &Message) {
        let islands = scopes
            .iter()
            .filter(|scope| self.newest_chunk(scope, 1).is_empty() && !self.history_begins(scope))
            .map(|scope| scope.key(&message.ts))
            .collect::<Vec<_>>();
        let mut txn = self.write();
        {
            let mut table = txn.open_table(MESSAGES);
            for scope in scopes {
                table.insert(
                    scope.key(&message.ts).as_str(),
                    SenValue::owned(StoredMessage::from(message)),
                );
            }
        }
        if !islands.is_empty() {
            let mut table = txn.open_table(GAPS);
            for key in &islands {
                table.insert(
                    key.as_str(),
                    SenValue::owned(StoredGap {
                        page_before: message.ts.0.clone(),
                    }),
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

    /// The run `ts` sits in: bounded below by the gap under its chunk and
    /// above by the next gap over it. This is what a deal opens on — the
    /// messages around the one being answered, not the newest ones, which
    /// may be a different chunk entirely.
    pub fn chunk_containing(&self, scope: &Scope, ts: &Ts, limit: usize) -> Vec<Message> {
        let floor = self.gap_at_or_below(scope, ts);
        let ceiling = self.gap_above(scope, ts);
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        let end = match &ceiling {
            Some(ceiling) => scope.key(ceiling),
            None => scope.end(),
        };
        let mut messages = Vec::new();
        let mut iter = table.range(scope.prefix().as_str()..end.as_str());
        while let Some((key, value)) = iter.next_back() {
            if let Some(floor) = &floor
                && ts_of(key.value()).is_some_and(|held| floor.is_newer_than(&held))
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

    /// The gap at or under `ts`: the bottom of the chunk `ts` belongs to.
    pub fn gap_at_or_below(&self, scope: &Scope, ts: &Ts) -> Option<Ts> {
        let txn = self.db.read();
        let table = txn.open_table(GAPS);
        let mut iter = table.range(scope.prefix().as_str()..=scope.key(ts).as_str());
        iter.next_back().and_then(|(key, _)| ts_of(key.value()))
    }

    /// The next gap over `ts`, which is where its chunk stops.
    pub fn gap_above(&self, scope: &Scope, ts: &Ts) -> Option<Ts> {
        let txn = self.db.read();
        let table = txn.open_table(GAPS);
        let mut iter = table.range(scope.key(ts).as_str()..scope.end().as_str());
        loop {
            let (key, _) = iter.next()?;
            let at = ts_of(key.value())?;
            if at.is_newer_than(ts) {
                return Some(at);
            }
        }
    }

    /// The oldest message held over `ts`. A run written under an existing
    /// one starts a chunk there, and that is where the record of the hole
    /// between them belongs.
    pub fn next_newer(&self, scope: &Scope, ts: &Ts) -> Option<Ts> {
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        let mut iter = table.range(scope.key(ts).as_str()..scope.end().as_str());
        loop {
            let (key, _) = iter.next()?;
            let held = ts_of(key.value())?;
            if held.is_newer_than(ts) {
                return Some(held);
            }
        }
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

    /// How far the reader has said they are done in this unit, in rho's own
    /// words. The other half of the same question is Slack's read mark, and
    /// what has been dealt with is the later of the two; this is the half
    /// that is rho's to keep, so it lives here beside the other rather than
    /// in the store. `SLACK-DESIGN.md`, "How a Slack unit sits in rho".
    pub fn handled(&self, scope: &Scope) -> Option<Ts> {
        match self.cursor(&format!("{}handled", scope.prefix())) {
            Some(StoredCursor::Stamp(ts)) => Some(Ts(ts)),
            _ => None,
        }
    }

    pub fn set_handled(&self, scope: &Scope, ts: &Ts) {
        self.put_cursor(
            &format!("{}handled", scope.prefix()),
            StoredCursor::Stamp(ts.0.clone()),
        );
    }

    /// Puts the cursor back where an undone verdict found it. `None` is a
    /// unit that had none, which is not the same as one at the oldest
    /// message: the row goes, so the join is back to Slack's mark alone.
    pub fn clear_handled(&self, scope: &Scope) {
        self.drop_cursor(&format!("{}handled", scope.prefix()));
    }

    /// How far Slack itself has been told, of what rho's cursor says. The
    /// outbox is the difference: a unit whose handled cursor is past this
    /// is one Slack has not heard about yet, and the push is retried at the
    /// next start whatever happened to the last one.
    pub fn pushed(&self, scope: &Scope) -> Option<Ts> {
        match self.cursor(&format!("{}pushed", scope.prefix())) {
            Some(StoredCursor::Stamp(ts)) => Some(Ts(ts)),
            _ => None,
        }
    }

    pub fn set_pushed(&self, scope: &Scope, ts: &Ts) {
        self.put_cursor(
            &format!("{}pushed", scope.prefix()),
            StoredCursor::Stamp(ts.0.clone()),
        );
    }

    /// Whether the local cursors have been seeded from the store's old
    /// `handled_through` cells. Once, at the first start that has both, and
    /// never again: the cells stay where they are and nothing reads them
    /// after this says yes.
    pub fn handled_seeded(&self, workspace: &str) -> bool {
        matches!(
            self.cursor(&format!("{workspace}{SEPARATOR}handled-seeded")),
            Some(StoredCursor::Flag(true))
        )
    }

    pub fn set_handled_seeded(&self, workspace: &str) {
        self.put_cursor(
            &format!("{workspace}{SEPARATOR}handled-seeded"),
            StoredCursor::Flag(true),
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

    /// Every workspace the mirror holds conversations for. A reader that was
    /// handed the file rather than the session — the QA rig feeding its fake
    /// Slack from a copy of this mirror — has no other way to know whose
    /// workspace it is looking at.
    pub fn workspaces(&self) -> Vec<String> {
        let txn = self.db.read();
        let table = txn.open_table(CONVERSATIONS);
        let mut names: Vec<String> = table
            .range::<&str>(..)
            .filter_map(|(key, _)| {
                key.value()
                    .split(SEPARATOR)
                    .next()
                    .map(std::borrow::ToOwned::to_owned)
            })
            .collect();
        names.dedup();
        names
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

    /// The emoji the reader reacted with, most recent first.
    pub fn reacted_with(&self, workspace: &str) -> Vec<String> {
        let txn = self.db.read();
        let table = txn.open_table(REACTED_WITH);
        table
            .get(workspace)
            .map(|stored| stored.value().as_ref().names.clone())
            .unwrap_or_default()
    }

    /// Writes the list back whole. It is capped at what the picker shows,
    /// so this is a handful of short strings and not a growing row.
    pub fn set_reacted_with(&self, workspace: &str, names: &[String]) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(REACTED_WITH);
            table.insert(
                workspace,
                SenValue::owned(StoredReactedWith {
                    names: names.to_vec(),
                }),
            );
        }
        txn.commit();
    }

    /// Writes one unit's facts. Called on the event that moved them, and
    /// on nothing else: the cost of a message is the unit it landed in.
    pub fn put_unit(&self, workspace: &str, unit: &Unit, facts: &UnitFacts) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(UNITS);
            table.insert(
                unit_key(workspace, unit).as_str(),
                SenValue::owned(StoredUnit::of(unit, facts)),
            );
        }
        txn.commit();
    }

    /// Forgets a unit: a thread unfollowed here or in another client. The
    /// messages stay — the reader can still find them — but rho has no
    /// standing claim to make about the thread any more.
    pub fn remove_unit(&self, workspace: &str, unit: &Unit) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(UNITS);
            table.remove(unit_key(workspace, unit).as_str());
        }
        txn.commit();
    }

    /// Every unit the last run left, for the start that installs them. One
    /// range scan over one workspace, and it reads units rather than
    /// messages: a mirror holding a million messages and four hundred units
    /// costs four hundred rows here.
    pub fn units(&self, workspace: &str) -> Vec<(Unit, UnitFacts)> {
        let txn = self.db.read();
        let table = txn.open_table(UNITS);
        let prefix = format!("{workspace}{SEPARATOR}");
        let end = format!(
            "{workspace}{}",
            char::from_u32(SEPARATOR as u32 + 1).unwrap()
        );
        table
            .range(prefix.as_str()..end.as_str())
            .filter_map(|(_, value)| value.value().as_ref().restore())
            .collect()
    }

    /// Whether the units table has been filled in at all. A mirror written
    /// before it existed has messages and no units, and says so here rather
    /// than by being empty — a workspace genuinely without units would look
    /// the same and be re-derived on every start.
    pub fn units_derived(&self, workspace: &str) -> bool {
        matches!(
            self.cursor(&format!("{workspace}{SEPARATOR}units")),
            Some(StoredCursor::Flag(true))
        )
    }

    pub fn set_units_derived(&self, workspace: &str) {
        self.put_cursor(
            &format!("{workspace}{SEPARATOR}units"),
            StoredCursor::Flag(true),
        );
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

    fn drop_cursor(&self, key: &str) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(CURSORS);
            table.remove(key);
        }
        txn.commit();
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

/// One tracked unit's facts, as they go on disk. The reason is a number
/// rather than a name because it is a closed set the code owns, and an
/// unknown number is a row from a newer rho, which is dropped rather than
/// guessed at.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredUnit {
    channel: String,
    thread: Option<String>,
    reason: u8,
    newest: String,
    newest_from_other: Option<String>,
    newest_from_you: bool,
    first_seen_ms: i64,
}

impl StoredUnit {
    fn of(unit: &Unit, facts: &UnitFacts) -> Self {
        Self {
            channel: unit.channel.0.clone(),
            thread: unit.thread.as_ref().map(|ts| ts.0.clone()),
            reason: match facts.reason {
                Reason::Mention => 0,
                Reason::DirectMessage => 1,
                Reason::Thread => 2,
                Reason::Channel => 3,
            },
            newest: facts.newest.0.clone(),
            newest_from_other: facts.newest_from_other.as_ref().map(|ts| ts.0.clone()),
            newest_from_you: facts.newest_from_you,
            first_seen_ms: facts.first_seen_ms,
        }
    }

    fn restore(&self) -> Option<(Unit, UnitFacts)> {
        let reason = match self.reason {
            0 => Reason::Mention,
            1 => Reason::DirectMessage,
            2 => Reason::Thread,
            3 => Reason::Channel,
            _ => return None,
        };
        Some((
            Unit {
                channel: ChannelId(self.channel.clone()),
                thread: self.thread.as_ref().map(|ts| Ts(ts.clone())),
            },
            UnitFacts {
                reason,
                newest: Ts(self.newest.clone()),
                newest_from_other: self.newest_from_other.as_ref().map(|ts| Ts(ts.clone())),
                newest_from_you: self.newest_from_you,
                // Whether somebody else had already answered is about the
                // run the reader is in the middle of, not about the unit,
                // so a restart starts it again rather than storing it.
                others_replied: false,
                first_seen_ms: self.first_seen_ms,
            },
        ))
    }
}

/// A unit's key: the conversation, then the thread when there is one. A
/// conversation and a thread whose root is empty cannot collide, because a
/// timestamp is never empty.
fn unit_key(workspace: &str, unit: &Unit) -> String {
    let thread = unit.thread.as_ref().map(Ts::as_str).unwrap_or("");
    format!(
        "{workspace}{SEPARATOR}{}{SEPARATOR}{thread}",
        unit.channel.as_str()
    )
}

/// The reader's own reaction history: shortcodes without colons, most
/// recent first, capped where the picker stops showing them.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredReactedWith {
    names: Vec<String>,
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
    url: Option<String>,
    service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredFile {
    id: String,
    title: String,
    filetype: String,
    size: u64,
    url: String,
    // Written since the picture's box learned to size itself; a row from
    // before that decodes without them and gets the capped box.
    #[senax(default)]
    original_w: u32,
    #[senax(default)]
    original_h: u32,
    #[senax(default)]
    thumb_url: String,
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
                    url: attachment.url.clone(),
                    service: attachment.service.clone(),
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
                    original_w: file.original_w,
                    original_h: file.original_h,
                    thumb_url: file.thumb_url.clone(),
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
                    url: attachment.url.clone(),
                    service: attachment.service.clone(),
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
                    original_w: file.original_w,
                    original_h: file.original_h,
                    thumb_url: file.thumb_url.clone(),
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
