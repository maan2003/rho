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

const FAVORITES: TableDefinition<&str, bool> = TableDefinition::new("rho_slack_favorites_v1");

/// Cached Slack message fields changed without changing Slack message identity.
/// Unlike drafts and read state, the cache is disposable: a generation change
/// drops only the tables and cursor facts derived from fetched history.
const CACHE_GENERATION: TableDefinition<(), String> =
    TableDefinition::new("rho_slack_cache_generation_v1");
const CURRENT_CACHE_GENERATION: &str = "a73c9f21";

const MESSAGES_TABLE: &str = "rho_slack_messages_v1";
const MESSAGES: TableDefinition<&str, Sen<StoredMessage>> = TableDefinition::new(MESSAGES_TABLE);
const GAPS_TABLE: &str = "rho_slack_gaps_v1";
const GAPS: TableDefinition<&str, Sen<StoredGap>> = TableDefinition::new(GAPS_TABLE);
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
/// Messages the reader explicitly saved for later in rho. Slack does not
/// expose its current Later list through a supported API, so this is honest
/// client-local state in the same durable mirror as the message it names.
const SAVED: TableDefinition<&str, Sen<StoredSaved>> = TableDefinition::new("rho_slack_saved_v1");
/// The channels the reader opted into. Its own table rather than a flag in
/// `CURSORS`, because the question asked of it is "which ones", and that is
/// a range scan over a workspace rather than a lookup per channel.
/// The emoji the reader has reacted with, most recent first. One row per
/// workspace: the question asked of it is "which ones, in what order",
/// which is one list and not a scan. Persisted because a picker that
/// forgets what the reader always uses is a picker they stop using.
const REACTED_WITH: TableDefinition<&str, Sen<StoredReactedWith>> =
    TableDefinition::new("rho_slack_reacted_with_v1");
/// Unsent composer contents, split so a text edit never rewrites file bytes.
/// These are new tables rather than changed record shapes, so existing
/// mirrors open without a format migration.
const DRAFT_TEXT: TableDefinition<&str, &str> = TableDefinition::new("rho_slack_draft_text_v1");
const DRAFT_FILES: TableDefinition<&str, Sen<StoredDraftFiles>> =
    TableDefinition::new("rho_slack_draft_files_v1");
const PENDING_DRAFTS: TableDefinition<&str, Sen<StoredPendingDraft>> =
    TableDefinition::new("rho_slack_pending_drafts_v1");

/// One locally saved message. Its source is retained so opening a saved
/// thread reply returns to the thread rather than the channel timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub channel: ChannelId,
    pub thread: Option<Ts>,
    pub ts: Ts,
    /// A human-readable snapshot retained if the mirrored message is later
    /// deleted or its cached run is collected.
    pub summary: String,
}

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

/// An unsent composer, including every file waiting with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    pub files: Vec<DraftFile>,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.files.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftFile {
    pub name: String,
    pub bytes: Vec<u8>,
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
            write.open_table(FAVORITES);
            write.open_table(SAVED);
            write.open_table(MESSAGES);
            write.open_table(GAPS);
            write.open_table(USERS);
            write.open_table(CONVERSATIONS);
            write.open_table(CURSORS);
            write.open_table(REACTED_WITH);
            write.open_table(UNITS);
            write.open_table(DRAFT_TEXT);
            write.open_table(DRAFT_FILES);
            write.open_table(PENDING_DRAFTS);
            write.open_table(CACHE_GENERATION);
            invalidate_stale_history(&mut write);
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

    /// One exact Slack message identity, when its cached bytes still exist.
    pub fn message(&self, scope: &Scope, ts: &Ts) -> Option<Message> {
        let txn = self.db.read();
        let table = txn.open_table(MESSAGES);
        table
            .get(scope.key(ts).as_str())
            .map(|value| value.value().as_ref().into())
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

    /// Slack's authoritative archive subdomain, retained so cached messages
    /// keep native permalink behavior while the workspace is offline.
    pub fn archive_domain(&self, workspace: &str) -> Option<String> {
        match self.cursor(&format!("{workspace}{SEPARATOR}archive-domain")) {
            Some(StoredCursor::Stamp(domain)) => Some(domain),
            _ => None,
        }
    }

    pub fn set_archive_domain(&self, workspace: &str, domain: &str) {
        self.put_cursor(
            &format!("{workspace}{SEPARATOR}archive-domain"),
            StoredCursor::Stamp(domain.to_owned()),
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

    pub fn favorite(&self, workspace: &str, channel: &ChannelId) -> bool {
        self.db
            .read()
            .open_table(FAVORITES)
            .get(format!("{workspace}{SEPARATOR}{}", channel.as_str()).as_str())
            .is_some()
    }

    pub fn set_favorite(&self, workspace: &str, channel: &ChannelId, favorite: bool) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(FAVORITES);
            let key = format!("{workspace}{SEPARATOR}{}", channel.as_str());
            if favorite {
                table.insert(key.as_str(), true);
            } else {
                table.remove(key.as_str());
            }
        }
        txn.commit();
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

    /// Saves one message idempotently. The key is Slack's message identity,
    /// so pressing save twice cannot duplicate an inventory row.
    pub fn save(&self, workspace: &str, saved: &Saved) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(SAVED);
            table.insert(
                saved_key(workspace, saved).as_str(),
                SenValue::owned(StoredSaved {
                    channel: saved.channel.0.clone(),
                    thread: saved.thread.as_ref().map(|ts| ts.0.clone()),
                    ts: saved.ts.0.clone(),
                    summary: saved.summary.clone(),
                }),
            );
        }
        txn.commit();
    }

    pub fn unsave(&self, workspace: &str, saved: &Saved) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(SAVED);
            table.remove(saved_key(workspace, saved).as_str());
        }
        txn.commit();
    }

    /// Newest first, as a Later inventory is read.
    pub fn saved(&self, workspace: &str) -> Vec<Saved> {
        let txn = self.db.read();
        let table = txn.open_table(SAVED);
        let prefix = format!("{workspace}{SEPARATOR}");
        let end = format!(
            "{workspace}{}",
            char::from_u32(SEPARATOR as u32 + 1).unwrap()
        );
        let mut saved = table
            .range(prefix.as_str()..end.as_str())
            .map(|(_, value)| {
                let value = value.value();
                let value = value.as_ref();
                Saved {
                    channel: ChannelId(value.channel.clone()),
                    thread: value.thread.as_ref().map(|ts| Ts(ts.clone())),
                    ts: Ts(value.ts.clone()),
                    summary: value.summary.clone(),
                }
            })
            .collect::<Vec<_>>();
        saved.sort_by(|left, right| right.ts.epoch_seconds().total_cmp(&left.ts.epoch_seconds()));
        saved
    }

    /// Reads the draft for one source.
    pub fn draft(&self, scope: &Scope) -> Option<Draft> {
        self.read_draft(scope, true)
    }

    pub fn next_draft(&self, scope: &Scope) -> Option<Draft> {
        self.read_draft(scope, false)
    }

    fn read_draft(&self, scope: &Scope, include_pending: bool) -> Option<Draft> {
        let txn = self.db.read();
        let key = scope.prefix();
        let text = txn
            .open_table(DRAFT_TEXT)
            .get(key.as_str())
            .map(|value| value.value().to_owned())
            .unwrap_or_default();
        let files = txn
            .open_table(DRAFT_FILES)
            .get(key.as_str())
            .map(|value| {
                value
                    .value()
                    .as_ref()
                    .files
                    .iter()
                    .map(|file| DraftFile {
                        name: file.name.clone(),
                        bytes: file.bytes.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let draft = Draft { text, files };
        let pending = if include_pending {
            txn.open_table(PENDING_DRAFTS)
                .get(key.as_str())
                .map(|value| {
                    let value = value.value();
                    let value = value.as_ref();
                    Draft {
                        text: value.text.clone(),
                        files: value
                            .files
                            .iter()
                            .map(|file| DraftFile {
                                name: file.name.clone(),
                                bytes: file.bytes.clone(),
                            })
                            .collect(),
                    }
                })
        } else {
            None
        };
        let draft = match pending {
            Some(pending) => merge_drafts(pending, draft),
            None => draft,
        };
        (!draft.is_empty()).then_some(draft)
    }

    /// Replaces both halves of one draft. Normal text edits use
    /// `put_draft_text`, so attachment bytes are not decoded and rewritten
    /// for every keystroke.
    pub fn put_draft(&self, scope: &Scope, draft: &Draft) {
        let key = scope.prefix();
        let mut txn = self.write();
        {
            let mut text = txn.open_table(DRAFT_TEXT);
            if draft.text.is_empty() {
                text.remove(key.as_str());
            } else {
                text.insert(key.as_str(), draft.text.as_str());
            }
        }
        {
            let mut files = txn.open_table(DRAFT_FILES);
            if draft.files.is_empty() {
                files.remove(key.as_str());
            } else {
                files.insert(
                    key.as_str(),
                    SenValue::owned(StoredDraftFiles {
                        files: draft
                            .files
                            .iter()
                            .map(|file| StoredDraftFile {
                                name: file.name.clone(),
                                bytes: file.bytes.clone(),
                            })
                            .collect(),
                    }),
                );
            }
        }
        txn.commit();
    }

    pub fn put_draft_text(&self, scope: &Scope, text: &str) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(DRAFT_TEXT);
            if text.is_empty() {
                table.remove(scope.prefix().as_str());
            } else {
                table.insert(scope.prefix().as_str(), text);
            }
        }
        txn.commit();
    }

    pub fn put_draft_files(&self, scope: &Scope, files: &[DraftFile]) {
        let mut txn = self.write();
        {
            let mut table = txn.open_table(DRAFT_FILES);
            if files.is_empty() {
                table.remove(scope.prefix().as_str());
            } else {
                table.insert(
                    scope.prefix().as_str(),
                    SenValue::owned(StoredDraftFiles {
                        files: files
                            .iter()
                            .map(|file| StoredDraftFile {
                                name: file.name.clone(),
                                bytes: file.bytes.clone(),
                            })
                            .collect(),
                    }),
                );
            }
        }
        txn.commit();
    }

    /// Persists the immutable snapshot currently being sent. A restart
    /// exposes it as part of the draft inventory but never sends it.
    pub fn put_pending_draft(&self, scope: &Scope, draft: &Draft) {
        let mut txn = self.write();
        {
            txn.open_table(PENDING_DRAFTS).insert(
                scope.prefix().as_str(),
                SenValue::owned(StoredPendingDraft::from(draft)),
            );
        }
        txn.commit();
    }

    pub fn finish_pending_from_store(&self, scope: &Scope, sent: bool) -> Draft {
        let next = self.next_draft(scope).unwrap_or_default();
        self.finish_pending_draft(scope, &next, sent)
    }

    /// Resolves the pending snapshot and the next composer in one transaction.
    /// Success keeps only `next`; failure restores pending before it.
    pub fn finish_pending_draft(&self, scope: &Scope, next: &Draft, sent: bool) -> Draft {
        let key = scope.prefix();
        let mut txn = self.write();
        let pending = txn
            .open_table(PENDING_DRAFTS)
            .get(key.as_str())
            .map(|value| {
                let value = value.value();
                let value = value.as_ref();
                Draft {
                    text: value.text.clone(),
                    files: value
                        .files
                        .iter()
                        .map(|file| DraftFile {
                            name: file.name.clone(),
                            bytes: file.bytes.clone(),
                        })
                        .collect(),
                }
            });
        let resolved = match (sent, pending) {
            (false, Some(pending)) => merge_drafts(pending, next.clone()),
            _ => next.clone(),
        };
        {
            let mut text = txn.open_table(DRAFT_TEXT);
            if resolved.text.is_empty() {
                text.remove(key.as_str());
            } else {
                text.insert(key.as_str(), resolved.text.as_str());
            }
        }
        {
            let mut files = txn.open_table(DRAFT_FILES);
            if resolved.files.is_empty() {
                files.remove(key.as_str());
            } else {
                files.insert(
                    key.as_str(),
                    SenValue::owned(StoredDraftFiles {
                        files: resolved
                            .files
                            .iter()
                            .map(|file| StoredDraftFile {
                                name: file.name.clone(),
                                bytes: file.bytes.clone(),
                            })
                            .collect(),
                    }),
                );
            }
        }
        txn.open_table(PENDING_DRAFTS).remove(key.as_str());
        txn.commit();
        resolved
    }

    /// Every resumable draft in a workspace.
    pub fn drafts(&self, workspace: &str) -> Vec<(Scope, Draft)> {
        let txn = self.db.read();
        let prefix = format!("{workspace}{SEPARATOR}");
        let end = format!(
            "{workspace}{}",
            char::from_u32(SEPARATOR as u32 + 1).unwrap()
        );
        let mut drafts = std::collections::BTreeMap::<String, Draft>::new();
        for (key, value) in txn
            .open_table(DRAFT_TEXT)
            .range(prefix.as_str()..end.as_str())
        {
            drafts.entry(key.value().to_owned()).or_default().text = value.value().to_owned();
        }
        for (key, value) in txn
            .open_table(DRAFT_FILES)
            .range(prefix.as_str()..end.as_str())
        {
            drafts.entry(key.value().to_owned()).or_default().files = value
                .value()
                .as_ref()
                .files
                .iter()
                .map(|file| DraftFile {
                    name: file.name.clone(),
                    bytes: file.bytes.clone(),
                })
                .collect();
        }
        for (key, value) in txn
            .open_table(PENDING_DRAFTS)
            .range(prefix.as_str()..end.as_str())
        {
            let value = value.value();
            let value = value.as_ref();
            let pending = Draft {
                text: value.text.clone(),
                files: value
                    .files
                    .iter()
                    .map(|file| DraftFile {
                        name: file.name.clone(),
                        bytes: file.bytes.clone(),
                    })
                    .collect(),
            };
            let next = drafts.remove(key.value()).unwrap_or_default();
            drafts.insert(key.value().to_owned(), merge_drafts(pending, next));
        }
        drafts
            .into_iter()
            .filter_map(|(key, draft)| {
                let mut parts = key.split(SEPARATOR);
                let workspace = parts.next()?.to_owned();
                let channel = ChannelId(parts.next()?.to_owned());
                let thread = parts
                    .next()
                    .filter(|it| !it.is_empty())
                    .map(|it| Ts(it.to_owned()));
                Some((
                    Scope {
                        workspace,
                        channel,
                        thread,
                    },
                    draft,
                ))
            })
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

/// Invalidates only network-rebuildable history when its cached shape changes.
///
/// `CURSORS` also owns durable read and workflow state, so the table cannot be
/// dropped whole. Only `…␟begins` is derived from a cached history run and
/// must go with that run. A missing marker is the one legacy generation; an
/// unknown marker is likewise a disposable cache from another build.
fn invalidate_stale_history(write: &mut rho_db::WriteTxn) {
    let current = write
        .open_table(CACHE_GENERATION)
        .get(&())
        .map(|generation| generation.value());
    if current.as_deref() == Some(CURRENT_CACHE_GENERATION) {
        return;
    }

    write.delete_table(MESSAGES_TABLE);
    write.delete_table(GAPS_TABLE);
    write.open_table(MESSAGES);
    write.open_table(GAPS);

    let begins = {
        let cursors = write.open_table(CURSORS);
        cursors
            .iter()
            .map(|(key, _)| key.value().to_owned())
            .filter(|key| key.ends_with(&format!("{SEPARATOR}begins")))
            .collect::<Vec<_>>()
    };
    {
        let mut cursors = write.open_table(CURSORS);
        for key in begins {
            cursors.remove(key.as_str());
        }
    }
    write
        .open_table(CACHE_GENERATION)
        .insert(&(), &CURRENT_CACHE_GENERATION.to_owned());
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

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredSaved {
    channel: String,
    thread: Option<String>,
    ts: String,
    summary: String,
}

fn saved_key(workspace: &str, saved: &Saved) -> String {
    format!(
        "{workspace}{SEPARATOR}{}{SEPARATOR}{}",
        saved.channel.as_str(),
        saved.ts.as_str()
    )
}

/// The reader's own reaction history: shortcodes without colons, most
/// recent first, capped where the picker stops showing them.

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredDraftFiles {
    files: Vec<StoredDraftFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredDraftFile {
    name: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct StoredPendingDraft {
    text: String,
    files: Vec<StoredDraftFile>,
}

impl From<&Draft> for StoredPendingDraft {
    fn from(draft: &Draft) -> Self {
        Self {
            text: draft.text.clone(),
            files: draft
                .files
                .iter()
                .map(|file| StoredDraftFile {
                    name: file.name.clone(),
                    bytes: file.bytes.clone(),
                })
                .collect(),
        }
    }
}

fn merge_drafts(pending: Draft, next: Draft) -> Draft {
    let text = match (pending.text.is_empty(), next.text.is_empty()) {
        (true, _) => next.text,
        (_, true) => pending.text,
        (false, false) => format!("{}\n\n{}", pending.text, next.text),
    };
    let files = pending.files.into_iter().chain(next.files).collect();
    Draft { text, files }
}

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
    #[senax(default)]
    bot_id: Option<String>,
    blocks: Vec<String>,
    text: String,
    attachments: Vec<StoredAttachment>,
    files: Vec<StoredFile>,
    subtype: Option<String>,
    reply_count: u32,
    latest_reply: Option<String>,
    #[senax(default)]
    reply_users: Vec<String>,
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
    #[senax(default)]
    author_name: Option<String>,
    #[senax(default)]
    author_id: Option<String>,
    #[senax(default)]
    channel_id: Option<String>,
    #[senax(default)]
    blocks: Vec<String>,
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
            bot_id: message.bot_id.clone(),
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
                    author_name: attachment.author_name.clone(),
                    author_id: attachment.author_id.as_ref().map(|id| id.0.clone()),
                    channel_id: attachment.channel_id.as_ref().map(|id| id.0.clone()),
                    blocks: attachment.blocks.iter().map(ToString::to_string).collect(),
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
            reply_users: message
                .reply_users
                .iter()
                .map(|user| user.0.clone())
                .collect(),
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
            bot_id: stored.bot_id.clone(),
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
                    author_name: attachment.author_name.clone(),
                    author_id: attachment.author_id.clone().map(UserId),
                    channel_id: attachment.channel_id.clone().map(ChannelId),
                    blocks: attachment
                        .blocks
                        .iter()
                        .filter_map(|block| serde_json::from_str(block).ok())
                        .collect(),
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
            reply_users: stored.reply_users.iter().cloned().map(UserId).collect(),
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

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    const OTHER_CLIENT_STATE: TableDefinition<(), &str> =
        TableDefinition::new("rho_slack_test_credentials");

    #[test]
    fn stale_history_is_invalidated_once_without_touching_local_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("rho.redb"));
        let scope = Scope::conversation("acme", &ChannelId::from("C1"));
        let cached = Message {
            ts: Ts::from("100.000000"),
            thread_ts: None,
            channel: ChannelId::from("C1"),
            user: Some(UserId::from("U1")),
            bot_name: None,
            bot_id: None,
            blocks: Vec::new(),
            text: "legacy cache".into(),
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 3,
            reply_users: Vec::new(),
            latest_reply: Some(Ts::from("103.000000")),
            edited: false,
            reactions: Vec::new(),
        };
        let unit = Unit::conversation(&ChannelId::from("C1"));
        let facts = UnitFacts {
            reason: Reason::Mention,
            newest: cached.ts.clone(),
            newest_from_other: Some(cached.ts.clone()),
            newest_from_you: false,
            others_replied: false,
            first_seen_ms: 42,
        };
        let saved = Saved {
            channel: ChannelId::from("C1"),
            thread: None,
            ts: cached.ts.clone(),
            summary: "kept summary".into(),
        };

        // This is the pre-generation database: history and its shape facts
        // exist, as do local state and a table owned by the surrounding
        // client database, but CACHE_GENERATION does not.
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write.open_table(MESSAGES).insert(
                scope.key(&cached.ts).as_str(),
                SenValue::owned(StoredMessage::from(&cached)),
            );
            write.open_table(GAPS).insert(
                scope.key(&cached.ts).as_str(),
                SenValue::owned(StoredGap {
                    page_before: cached.ts.0.clone(),
                }),
            );
            {
                let mut cursors = write.open_table(CURSORS);
                cursors.insert(
                    format!("{}begins", scope.prefix()).as_str(),
                    SenValue::owned(StoredCursor::Flag(true)),
                );
                cursors.insert(
                    format!("{}read", scope.prefix()).as_str(),
                    SenValue::owned(StoredCursor::Stamp("90.000000".into())),
                );
            }
            write.open_table(UNITS).insert(
                unit_key("acme", &unit).as_str(),
                SenValue::owned(StoredUnit::of(&unit, &facts)),
            );
            write
                .open_table(DRAFT_TEXT)
                .insert(scope.prefix().as_str(), "unfinished");
            write
                .open_table(FAVORITES)
                .insert(format!("acme{SEPARATOR}C1").as_str(), true);
            write.open_table(SAVED).insert(
                saved_key("acme", &saved).as_str(),
                SenValue::owned(StoredSaved {
                    channel: "C1".into(),
                    thread: None,
                    ts: cached.ts.0.clone(),
                    summary: saved.summary.clone(),
                }),
            );
            write
                .open_table(OTHER_CLIENT_STATE)
                .insert(&(), "credential sentinel");
            write.commit();
        });

        let mirror = Mirror::open_on(db.clone()).unwrap();
        assert!(mirror.all_messages(&scope).is_empty());
        assert!(mirror.gap_below(&scope, None).is_none());
        assert!(!mirror.history_begins(&scope));
        assert_eq!(
            mirror.last_read(&scope),
            Some(Ts::from("90.000000")),
            "read state is not cache shape"
        );
        assert_eq!(mirror.units("acme"), vec![(unit, facts)]);
        assert_eq!(
            mirror.draft(&scope),
            Some(Draft {
                text: "unfinished".into(),
                files: Vec::new(),
            })
        );
        assert!(mirror.favorite("acme", &ChannelId::from("C1")));
        assert_eq!(mirror.saved("acme"), vec![saved]);
        assert_eq!(
            db.read()
                .open_table(OTHER_CLIENT_STATE)
                .get(&())
                .map(|value| value.value().to_owned()),
            Some("credential sentinel".to_owned()),
            "opening Slack's mirror does not wipe the shared client database"
        );

        let mut fresh = cached;
        fresh.text = "fresh cache".into();
        fresh.reply_users = vec![UserId::from("U2")];
        mirror.insert_messages(&scope, std::slice::from_ref(&fresh));
        mirror.put_gap(&scope, &fresh.ts, &fresh.ts);
        mirror.set_history_begins(&scope);
        drop(mirror);

        let reopened = Mirror::open_on(db).unwrap();
        assert_eq!(reopened.all_messages(&scope), vec![fresh.clone()]);
        assert_eq!(
            reopened
                .gap_below(&scope, None)
                .map(|(at, gap)| (at, gap.page_before)),
            Some((fresh.ts.clone(), fresh.ts.clone()))
        );
        assert!(
            reopened.history_begins(&scope),
            "the current generation survives subsequent opens"
        );
    }

    #[derive(Encode)]
    struct LegacyMessage {
        ts: String,
        thread_ts: Option<String>,
        channel: String,
        user: Option<String>,
        bot_name: Option<String>,
        #[senax(default)]
        bot_id: Option<String>,
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

    #[derive(Encode)]
    struct LegacyAttachment {
        title: Option<String>,
        text: Option<String>,
        fallback: Option<String>,
        pretext: Option<String>,
        fields: Vec<(String, String)>,
        is_unfurl: bool,
        url: Option<String>,
        service: Option<String>,
    }

    #[test]
    fn cached_attachments_without_rich_metadata_still_decode() {
        let old = LegacyAttachment {
            title: Some("Old preview".into()),
            text: Some("old body".into()),
            fallback: None,
            pretext: None,
            fields: Vec::new(),
            is_unfurl: true,
            url: Some("https://example.com".into()),
            service: Some("example.com".into()),
        };
        let mut bytes = senax_encoder::encode(&old).unwrap();
        let stored: StoredAttachment = senax_encoder::decode(&mut bytes).unwrap();
        assert_eq!(stored.title.as_deref(), Some("Old preview"));
        assert_eq!(stored.text.as_deref(), Some("old body"));
        assert!(stored.author_name.is_none());
        assert!(stored.author_id.is_none());
        assert!(stored.channel_id.is_none());
        assert!(stored.blocks.is_empty());
    }

    #[test]
    fn attachment_rich_metadata_round_trips() {
        let stored = StoredAttachment {
            title: Some("Discussion".into()),
            text: Some("fallback".into()),
            fallback: None,
            pretext: None,
            fields: Vec::new(),
            is_unfurl: true,
            url: Some("https://example.com".into()),
            service: None,
            author_name: Some("Ada".into()),
            author_id: Some("U1".into()),
            channel_id: Some("C1".into()),
            blocks: vec![
                serde_json::json!({
                    "type": "rich_text",
                    "elements": []
                })
                .to_string(),
            ],
        };
        let mut bytes = senax_encoder::encode(&stored).unwrap();
        let decoded: StoredAttachment = senax_encoder::decode(&mut bytes).unwrap();
        assert_eq!(decoded, stored);
    }

    #[test]
    fn cached_messages_without_participants_still_decode() {
        let old = LegacyMessage {
            ts: "123.456789".into(),
            thread_ts: None,
            channel: "C1".into(),
            user: Some("UA".into()),
            bot_name: None,
            bot_id: None,
            blocks: Vec::new(),
            text: "old cached message".into(),
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 7,
            latest_reply: Some("124.123456".into()),
            edited: true,
            reactions: Vec::new(),
        };
        let mut bytes = senax_encoder::encode(&old).unwrap();
        let stored: StoredMessage = senax_encoder::decode(&mut bytes).unwrap();
        let message = Message::from(&stored);
        assert!(message.reply_users.is_empty());
        assert_eq!(message.reply_count, 7);
        assert_eq!(message.latest_reply.unwrap().0, "124.123456");
        assert!(message.edited);
        assert_eq!(message.text, "old cached message");
    }
}
