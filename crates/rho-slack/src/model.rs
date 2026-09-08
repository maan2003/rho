//! What rho knows about a workspace, and which of it is an obligation.
//!
//! Two sources feed this: the activity feed (truth) and the websocket
//! (latency). Both funnel through [`Model::note_message`] and
//! [`Model::note_activity`], which deduplicate on (channel, timestamp), so a
//! thread announced by both is raised exactly once — the "dealt twice"
//! symptom the design warns about cannot happen here.
//!
//! Only mentions, direct messages, and threads the user has posted in become
//! obligations. Channel traffic is kept for reading and never raised.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::api::{ActivityItem, ActivityKind, ConversationCount};
use crate::block::{Names, render_message};
use crate::config::WorkspaceName;
use crate::types::{
    ChannelId, Conversation, ConversationKind, Message, Reason, ThreadKey, Ts, User, UserId,
};

/// Whether the last word in a thread is theirs or yours. Yours is the done
/// verdict: the obligation is discharged until somebody answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiting {
    OnYou,
    OnThem,
}

/// Why a unit is asking for the reader right now: the fact behind a card,
/// never the sentence.
///
/// This is Slack's own notion of attention, which is the whole of the rule.
/// Slack badges a DM, a mention, and a reply in a thread it follows for the
/// user; a channel with ordinary unread traffic it counts but does not
/// press, and neither does rho. The one addition is a channel the reader
/// opted into here, which is rho's own fact and no less the reader's word
/// for it.
///
/// The words are rendered from this at the moment a card is drawn, by
/// [`reason_text`], so a conversation named after the message landed reads
/// as `#design` rather than by its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    /// The reader was named, by handle, group, or a channel-wide broadcast.
    Mentioned,
    /// Something unread in a direct or group message. Being in the room is
    /// the address: nobody is in a group DM by accident.
    DirectMessage,
    /// A reply, since the reader last looked, in a thread Slack follows for
    /// them.
    FollowedThread,
    /// Unread traffic in a channel the reader asked rho to hand them. The
    /// only one of the four Slack itself would not badge.
    WatchedChannel,
}

/// What a card says it is for, in the words the reader reads. Built when
/// the card is drawn and never stored: `conversation` is the label the
/// roster gives now, so a name learned late is not baked into a stale
/// sentence.
pub fn reason_text(reason: Attention, conversation: &str) -> String {
    match reason {
        Attention::Mentioned => format!("mentioned in {conversation}"),
        Attention::DirectMessage => format!("unread in {conversation}"),
        Attention::FollowedThread => {
            format!("a reply in a followed thread in {conversation}")
        }
        Attention::WatchedChannel => format!("unread in {conversation}, watched here"),
    }
}

/// The thing rho deals: a conversation or a followed thread, never a
/// message. Slack keeps the identity, so rho only has to say which ones
/// matter. One card per unit at most, so a channel with three unhandled
/// mentions is one card rather than three.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unit {
    pub channel: ChannelId,
    /// `None` for a conversation: a direct or group message, or a channel
    /// the user was mentioned in. A followed thread carries its root.
    pub thread: Option<Ts>,
}

impl Unit {
    pub fn conversation(channel: &ChannelId) -> Self {
        Self {
            channel: channel.clone(),
            thread: None,
        }
    }

    pub fn thread(channel: &ChannelId, root: &Ts) -> Self {
        Self {
            channel: channel.clone(),
            thread: Some(root.clone()),
        }
    }
}

/// What the mirror says about one unit.
///
/// Every timestamp here only ever rises. A live frame, a feed poll, a
/// history page, a reconnect and a restart are all the same kind of
/// evidence, and none of them may lower a fact, which is what keeps a card
/// the user closed from coming back on an older message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitFacts {
    pub reason: Reason,
    /// The newest message in the unit, whoever wrote it.
    pub newest: Ts,
    /// The newest message from someone else that concerns the user: any
    /// message in a direct message, a mention in a channel, a reply in a
    /// followed thread. This is what a verdict cursor is compared against.
    pub newest_from_other: Option<Ts>,
    /// Who wrote `newest`. The word on the card, and which curve it takes.
    pub newest_from_you: bool,
    /// When rho first saw this unit, so a card's age is rho's own clock and
    /// cannot be moved by a doctored message timestamp.
    pub first_seen_ms: i64,
}

impl UnitFacts {
    pub fn waiting(&self) -> Waiting {
        match self.newest_from_you {
            true => Waiting::OnThem,
            false => Waiting::OnYou,
        }
    }
}

/// What a change to the model means for the inbox. The GUI translates these
/// into appends, updates, and retirements; the model never touches storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A unit that owes the user an answer, and was not owing one before.
    Raised(Unit),
    /// A unit already raised whose newest message changed.
    Updated(Unit),
    /// The user answered. The card stays and says `replied`: verdicts are
    /// the user's keys only, so nothing here closes it and nothing binds.
    Replied(Unit),
    /// The thread stopped being the user's, because they ignored it here or
    /// unfollowed it in another client. Slack's own verdict, so the card
    /// goes; nothing rho stores says otherwise.
    Muted(Unit),
}

/// A dealer card's worth of a thread, with no ids and no raw timestamps.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitCard {
    pub unit: Unit,
    /// What the unit is about, in the words the mirror has now — a name the
    /// roster only supplied after the message landed reads as `@ada` rather
    /// than as `<@U123>`. Empty from [`Model::card`], which does not hold
    /// the mirror; [`crate::session::Session::cards`] is where it is filled.
    pub title: String,
    /// `#design` or `@ada`.
    pub conversation: String,
    /// Why this unit is asking now, or `None` when nothing is: a unit rho
    /// tracks whose messages have all been read is still a unit — Find
    /// reaches it — but it is not a card, and this is the one field that
    /// says which.
    pub attention: Option<Attention>,
    pub waiting: Waiting,
    pub wait_days: f64,
    /// When rho first saw this unit, in milliseconds. What a card is raised
    /// at, which is not the same as when its newest message landed.
    pub first_seen_ms: i64,
    /// The newest message; a change here is what re-raises the card.
    pub newest: Ts,
    /// Where a dealt card lands the reader: the oldest message from someone
    /// else the user has not handled, or the newest when there is none.
    pub newest_from_other: Option<Ts>,
}

/// Everything one `mark read before` touches: the conversations to mark and
/// the followed threads to mark, each with the message to mark up to. The
/// count shown before acting and the calls made after are this same list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkPlan {
    pub conversations: Vec<(ChannelId, Ts)>,
    pub threads: Vec<(ThreadKey, Ts)>,
}

/// How many completions the composer offers at once. There are thousands of
/// emoji; a list longer than this is not read, it is scrolled past.
const SUGGESTION_LIMIT: usize = 20;

/// How many of the reader's own emoji the reaction menu remembers. A
/// menu is read at a glance, so this is a short row of keys and not a
/// history.
pub const REACTIONS_REMEMBERED: usize = 9;

/// What the menu offers before the reader has reacted to anything. Not a
/// preference and not a ranking: a first menu with nothing in it teaches
/// nobody what the key does, and one use puts the reader's own choice at
/// the front of the row for good.
const FIRST_REACTIONS: [&str; 6] = [
    "thumbsup",
    "white_check_mark",
    "eyes",
    "tada",
    "heart",
    "pray",
];

/// Whether a character can be part of a handle or a channel name, which is
/// what bounds a mention in typed text.
/// The channel-wide mentions, which name everyone rather than anyone: what
/// the composer offers for `@` beside the members, and what `encode` puts on
/// the wire as `<!here>` and `<!channel>`.
const BROADCASTS: [&str; 2] = ["here", "channel"];

fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.')
}

/// One thing the composer offers for the token being typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// What replaces the token: `@ada`, `#design`, `:tada:`.
    pub value: String,
    /// What it is, beside it: a display name, or the glyph itself.
    pub detail: String,
}

/// Where one conversation sits in the list.
///
/// Every field is stored the way it sorts, so the map's own order *is* the
/// list's order and drawing never compares anything. `Reverse` is what puts
/// unread, then the loudest, then the newest at the top; `muted` is plain,
/// because muted belongs at the bottom.
///
/// The label is in the key because the list is alphabetical between
/// conversations that are otherwise equal, and the id is last so that two
/// conversations with the same name still have distinct keys.
///
/// Both strings are shared rather than owned. A key is copied every time a
/// query is answered — once per match — and a name is long enough that
/// copying it there was most of what narrowing cost.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RowKey {
    muted: bool,
    unread: Reverse<bool>,
    mentions: Reverse<u32>,
    latest: Reverse<i64>,
    label: std::sync::Arc<str>,
    id: std::sync::Arc<str>,
}

/// One thing that happened to the list: a row left the place it was in, a
/// row arrived at a place, or both, which is a row that moved.
///
/// A badge changing without moving the row is `from` and `at` being the
/// same place, and is one line rewritten. A conversation rho has just heard
/// of has no `from`; one that has gone has no `at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowEdit {
    pub channel: ChannelId,
    /// The conversation had a line on screen and that line has to come
    /// out. A place is not given: a number would cost the distance down
    /// the list to count, and the drawer already knows which line it put
    /// this conversation on. While a query stands this is about the
    /// narrowed list, which is the list the drawer holds.
    pub was_shown: bool,
    /// The conversation the row now sits above, or `None` for the end of
    /// the list. A neighbour rather than a place, for the same reason:
    /// the next key is one step from this one, whereas how many rows are
    /// above it is a walk.
    pub before: Option<ChannelId>,
    /// The row to draw, so a drawer never has to ask again.
    pub row: Option<ConversationRow>,
}

/// How many unread list edits are kept before the log is thrown away and
/// the next drawer is told to rebuild. Well past a screenful of traffic,
/// and far below the point where replaying them beats redrawing.
const ROW_EDIT_CAP: usize = 4_096;

/// One line of the conversation list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationRow {
    pub id: ChannelId,
    pub label: String,
    pub unread: bool,
    pub mention_count: u32,
    /// How many messages are waiting, when Slack counts them or rho has
    /// watched them land. Zero with `unread` set means "something is here"
    /// and no number to put on it.
    pub unread_count: u32,
    /// Muted in Slack, from any client. These sit at the bottom of the list
    /// and never pull the reader with `shift-n`.
    pub muted: bool,
    /// The reader opted into this channel, so its traffic is handed to them
    /// rather than left here with a count. Shown so the opt-in is visible
    /// where it was made.
    pub watched: bool,
    pub latest: Option<Ts>,
}

/// What the next-unread key found.
///
/// Three cases and not an Option, because "nothing here" and "nothing
/// here, and this much waiting outside what you are looking at" are
/// different things to be told, and the reader is owed the difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextUnread {
    /// Open this one.
    Go(ChannelId),
    /// Nothing unread the narrowing reaches, and this many outside it.
    /// Never zero, and never raised when no query stands.
    Outside(usize),
    /// Nothing unread anywhere the key would go.
    Nothing,
}

/// Why a narrowed list is empty, for the one line that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Empty {
    /// The query has not reached a conversation since it was typed.
    Never,
    /// It reached conversations, and they have been renamed, archived or
    /// left since.
    Gone,
}

pub struct Model {
    workspace: WorkspaceName,
    self_id: UserId,
    /// The emoji the reader has reacted with, most recent first and
    /// capped at what a picker shows. Kept here so the menu is built from
    /// a list rather than worked out from the conversation.
    reacted_with: Vec<String>,
    users: BTreeMap<UserId, User>,
    conversations: BTreeMap<ChannelId, Conversation>,
    counts: BTreeMap<ChannelId, ConversationCount>,
    /// The workspace's own emoji names, which stay shortcodes on screen.
    custom_emoji: BTreeSet<String>,
    units: BTreeMap<Unit, UnitFacts>,
    /// Every (channel, timestamp) the model has already accounted for. This
    /// is the whole of the deduplication between the feed and the socket.
    seen: BTreeSet<(ChannelId, Ts)>,
    /// The threads Slack follows for the user, which is what makes a later
    /// reply in one an obligation rather than channel traffic. Slack owns
    /// this list: it subscribes a thread the user posts in or is mentioned
    /// in, from any client, so rho never has to remember what it watched.
    followed: BTreeSet<ThreadKey>,
    /// The channels the user muted, whichever client they muted them in.
    muted: BTreeSet<ChannelId>,
    /// Where the reader has read to in each conversation, and in each
    /// followed thread. Slack keeps these two apart and so does rho: they
    /// answer different questions, and folding them together marks a
    /// channel read that nobody has looked at.
    conversation_read: BTreeMap<ChannelId, Ts>,
    thread_read: BTreeMap<ThreadKey, Ts>,
    /// The channels the reader opted into: the ones whose ordinary traffic
    /// they asked to be handed rather than left in the list. rho's own
    /// fact, not Slack's, so it lives in rho's own file and comes back off
    /// it at startup.
    watched: BTreeSet<ChannelId>,
    /// Every word start in every conversation's name, paired with the
    /// conversation it belongs to. This is what a reader typing narrows
    /// against: the set is ordered by the word, so a query is a range scan
    /// from the query to the first word that does not begin with it, and
    /// the cost is the depth of the tree plus the matches, never the list.
    words: BTreeSet<(String, ChannelId)>,
    /// The words each conversation currently has in that set. Kept because
    /// a name that changes has to have its old words taken out, and the
    /// old words cannot be worked backwards from the new name.
    worded: BTreeMap<ChannelId, Vec<String>>,
    /// What the reader has typed to narrow the list, already split into
    /// words. Empty means the whole list is shown.
    query: Vec<String>,
    /// The conversations the query matches, in the list's own order. This
    /// is the list on screen while a query stands. A sorted vector rather
    /// than a tree: it is rebuilt whole on a keystroke and walked whole to
    /// draw, and neither wants a tree's per-node cost.
    narrowed: Vec<(RowKey, ChannelId)>,
    /// Whether the standing query has ever had a conversation on screen.
    /// A query that empties because the mirror moved and one that never
    /// answered are the same empty list, and this is the only thing that
    /// tells them apart; cleared by the reader typing, and by nothing
    /// else.
    reached_once: bool,
    /// The conversation list, in the order it is read, and the key each
    /// conversation currently sits at.
    ///
    /// Kept rather than sorted, because sorting is O(n log n) and a frame
    /// may not cost that. Every event that can move a row — a message,
    /// Slack's counts, a mark, a mute, an opt-in, the roster — puts that
    /// row back in its place and leaves the rest standing.
    order: BTreeMap<RowKey, ConversationRow>,
    placed: BTreeMap<ChannelId, RowKey>,
    /// What the list has done since a drawer last asked: one entry per
    /// reindex, in the order they happened, each saying where a row left
    /// and where it arrived.
    ///
    /// Replaying these against a buffer that held the last list reproduces
    /// this one exactly, which is what lets a message cost two line edits
    /// instead of a redraw. They are in order and each position is the
    /// position *at that moment*, so they must be applied in order and
    /// none may be skipped.
    ///
    /// Taken rather than read, because a drawer that has been told is
    /// caught up. That makes one drawer the owner of this; there is one
    /// today, the conversation list. A drawer holding nothing rebuilds in
    /// full, so the failure of a second one forgetting this is a slow list
    /// and never a wrong one.
    edits: Vec<RowEdit>,
    /// Set when the log was dropped for growing too long — nobody has drawn
    /// the list in a long time — and the next drawer must rebuild rather
    /// than replay.
    resync: bool,
    /// The units asking for the reader right now, and what for.
    ///
    /// Kept rather than worked out, because working it out is a pass over
    /// every unit and drawing a frame may not cost that. Every event that
    /// can change the answer — a message, a mark, a mute, a follow, an
    /// opt-in, Slack's counts — rewrites the units it touches here and no
    /// others, through [`Model::refresh_attention`], and every one of those
    /// events already knows which units those are.
    asking: BTreeMap<Unit, Attention>,
}

impl Names for Model {
    fn user(&self, id: &UserId) -> Option<String> {
        // The reader is a person in the conversation like anyone else: they
        // read their own name, and the class is what marks it as theirs. A
        // transcript that says "you" cannot be quoted to anybody.
        self.users.get(id).map(|user| user.name.clone())
    }

    fn channel(&self, id: &ChannelId) -> Option<String> {
        self.conversations
            .get(id)
            .map(|conversation| conversation.name.clone())
    }
}

/// The handles baked into a group DM's machine name:
/// `mpdm-david--manmeet--keith-1` is three handles separated by `--`, with a
/// disambiguating suffix Slack appends.
fn mpdm_handles(name: &str) -> Vec<String> {
    let Some(rest) = name.strip_prefix("mpdm-") else {
        return Vec::new();
    };
    let rest = rest.rsplit_once('-').map_or(rest, |(head, _)| head);
    rest.split("--")
        .filter(|handle| !handle.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The words a query asks for, split exactly as a name is split.
///
/// The reader types what they see: `#design`, `dev-ops`, a name pasted
/// from a completion. Splitting the query the same way the index splits
/// names is what makes those reach — `#design` asks for `design`, and
/// `dev-ops` asks for `dev` and `ops`, both of which the name answers.
/// Any other rule would offer a completion that then matches nothing.
fn typed_words(query: &str) -> Vec<String> {
    Model::word_starts(query)
}

impl Model {
    pub fn new(workspace: WorkspaceName) -> Self {
        Self {
            workspace,
            self_id: UserId(String::new()),
            reacted_with: FIRST_REACTIONS
                .iter()
                .map(|name| name.to_string())
                .collect(),
            users: BTreeMap::new(),
            conversations: BTreeMap::new(),
            counts: BTreeMap::new(),
            custom_emoji: BTreeSet::new(),
            units: BTreeMap::new(),
            seen: BTreeSet::new(),
            followed: BTreeSet::new(),
            muted: BTreeSet::new(),
            conversation_read: BTreeMap::new(),
            thread_read: BTreeMap::new(),
            watched: BTreeSet::new(),
            words: BTreeSet::new(),
            worded: BTreeMap::new(),
            query: Vec::new(),
            narrowed: Vec::new(),
            reached_once: false,
            order: BTreeMap::new(),
            placed: BTreeMap::new(),
            edits: Vec::new(),
            resync: false,
            asking: BTreeMap::new(),
        }
    }

    pub fn workspace(&self) -> &WorkspaceName {
        &self.workspace
    }

    pub fn set_self(&mut self, id: UserId) {
        self.self_id = id;
    }

    pub fn self_id(&self) -> &UserId {
        &self.self_id
    }

    /// The emoji the reader reaches for, most recent first. What the
    /// reaction menu offers before anything else.
    pub fn reacted_with(&self) -> &[String] {
        &self.reacted_with
    }

    /// What a previous run remembered. An empty list is a workspace the
    /// reader has not reacted in yet, which leaves the starting row
    /// standing rather than emptying the menu.
    pub fn set_reacted_with(&mut self, names: Vec<String>) {
        if names.is_empty() {
            return;
        }
        self.reacted_with = names;
        self.reacted_with.truncate(REACTIONS_REMEMBERED);
    }

    /// Remembers an emoji the reader just used, newest first.
    ///
    /// Answers whether the list changed, so a caller writes it back only
    /// when it did. The cost is the length of the list, which is capped at
    /// what the menu shows.
    pub fn note_reaction_used(&mut self, name: &str) -> bool {
        if self.reacted_with.first().map(String::as_str) == Some(name) {
            return false;
        }
        self.reacted_with.retain(|held| held != name);
        self.reacted_with.insert(0, name.to_owned());
        self.reacted_with.truncate(REACTIONS_REMEMBERED);
        true
    }

    pub fn add_users(&mut self, users: impl IntoIterator<Item = User>) {
        for user in users {
            self.users.insert(user.id.clone(), user);
        }
        // A direct message is named after the person in it, so the roster
        // landing renames rows and moves the alphabetical ones among them.
        self.reindex_all();
    }

    /// Registers conversations. Names are not stored: a label is built from
    /// the roster every time it is read, so a conversation that arrives
    /// before the roster is still named once the roster lands.
    pub fn add_conversations(&mut self, conversations: impl IntoIterator<Item = Conversation>) {
        for conversation in conversations {
            let id = conversation.id.clone();
            self.conversations.insert(id.clone(), conversation);
            self.reindex(&id);
        }
    }

    pub fn conversation(&self, channel: &ChannelId) -> Option<&Conversation> {
        self.conversations.get(channel)
    }

    /// Every conversation rho knows of, for a caller that has to visit them
    /// all: seeding the read cursors off the mirror at startup is the case.
    pub fn conversations(&self) -> Vec<ChannelId> {
        self.conversations.keys().cloned().collect()
    }

    /// How a conversation reads everywhere the user meets it: the list, the
    /// surface title, the status bar, a dealer card, a filed heading.
    /// `#design`, `@ada`, or, for a group DM, the people in it. Unknown
    /// channels read as an unnamed conversation rather than as their id.
    pub fn label(&self, channel: &ChannelId) -> String {
        let Some(conversation) = self.conversations.get(channel) else {
            return "#a conversation".to_owned();
        };
        // A display name can carry an emoji, and the list is a place a
        // reader scans: a shortcode there is noise.
        crate::emoji::render(&self.raw_label(conversation))
    }

    fn raw_label(&self, conversation: &Conversation) -> String {
        match conversation.kind {
            ConversationKind::Channel => format!("#{}", conversation.name),
            ConversationKind::DirectMessage => {
                let name = conversation
                    .user
                    .as_ref()
                    .and_then(|user| self.users.get(user))
                    .map(|user| user.name.clone())
                    .unwrap_or_else(|| conversation.name.clone());
                format!("@{name}")
            }
            ConversationKind::Group => self.group_label(conversation),
        }
    }

    /// A group DM reads as the people in it, the way Slack's own client
    /// shows one. Slack's name for it, `mpdm-david--manmeet--keith-1`, is a
    /// machine string and never reaches the user.
    fn group_label(&self, conversation: &Conversation) -> String {
        let mut names: Vec<String> = conversation
            .members
            .iter()
            .filter(|member| *member != &self.self_id)
            .map(|member| {
                self.users
                    .get(member)
                    .map(|user| user.name.clone())
                    .unwrap_or_else(|| "someone".to_owned())
            })
            .collect();
        // `users.conversations` does not carry members, so the handles baked
        // into the machine name are all there is; they still name people.
        if names.is_empty() {
            let own = self
                .users
                .get(&self.self_id)
                .map(|user| user.handle.clone());
            names = mpdm_handles(&conversation.name)
                .into_iter()
                .filter(|handle| own.as_deref() != Some(handle.as_str()))
                .map(|handle| self.display_of(&handle))
                .collect();
        }
        if names.is_empty() {
            return "a group".to_owned();
        }
        names.join(", ")
    }

    fn display_of(&self, handle: &str) -> String {
        self.users
            .values()
            .find(|user| user.handle == handle)
            .map(|user| user.name.clone())
            .unwrap_or_else(|| handle.to_owned())
    }

    pub fn set_custom_emoji(&mut self, names: impl IntoIterator<Item = String>) {
        self.custom_emoji.extend(names);
    }

    /// Whether `name` is a workspace emoji, which is the difference between
    /// muting a shortcode and leaving a word alone.
    pub fn is_custom_emoji(&self, name: &str) -> bool {
        self.custom_emoji.contains(name)
    }

    /// Slack's unread bookkeeping, replacing what rho held.
    ///
    /// The cursor that comes with it is evidence like any other and goes
    /// through the one rule, so an answer prepared before rho's own mark
    /// reached the server cannot walk the unread rule back over messages
    /// the reader has been through. A reconnect asks for this again, and
    /// re-badging a conversation the reader read a second ago is the whole
    /// of "mark read does not stick".
    pub fn set_counts(&mut self, counts: impl IntoIterator<Item = ConversationCount>) {
        for count in counts {
            let (channel, cursor) = (count.channel.clone(), count.last_read.clone());
            self.counts.insert(channel.clone(), count);
            if let Some(cursor) = cursor {
                self.mark_read(&channel, &cursor);
            }
            // The badge against whichever cursor won, because the one rho
            // already had may be the newer of the two and `mark_read` says
            // nothing about a badge when the cursor did not move.
            self.refresh_badge(&channel);
            self.refresh_channel(&channel);
            self.reindex(&channel);
        }
    }

    /// Slack's muted list, replacing whatever rho held: unmuting elsewhere
    /// has to bring a conversation back up out of the muted section.
    pub fn set_muted(&mut self, muted: impl IntoIterator<Item = ChannelId>) {
        let now = muted.into_iter().collect::<BTreeSet<_>>();
        // Only the conversations whose mute changed: muting is a verdict
        // about one room, and a roster of four thousand that says the same
        // as last time costs nothing.
        let touched = self
            .muted
            .symmetric_difference(&now)
            .cloned()
            .collect::<Vec<_>>();
        self.muted = now;
        for channel in touched {
            self.refresh_channel(&channel);
            self.reindex(&channel);
        }
    }

    /// Moves the list's own counters for a message off the socket. This is
    /// every message, not only the ones that raise a card: the list names
    /// the whole workspace, and without this its badges sit at whatever
    /// `client.counts` said at connect until the next restart.
    pub fn note_counts(&mut self, message: &Message) {
        let from_you = message.user.as_ref() == Some(&self.self_id);
        let pings_you = self.is_dm(&message.channel) || self.mentions_you(message);
        let count = self
            .counts
            .entry(message.channel.clone())
            .or_insert_with(|| ConversationCount {
                channel: message.channel.clone(),
                has_unreads: false,
                mention_count: 0,
                unread_count: 0,
                latest: None,
                last_read: None,
            });
        if count
            .latest
            .as_ref()
            .is_none_or(|latest| message.ts.is_newer_than(latest))
        {
            count.latest = Some(message.ts.clone());
        }
        // Posting is reading: a message the user sent from any client marks
        // the conversation read in Slack, so rho must not badge it here.
        match from_you {
            true => {
                count.has_unreads = false;
                count.mention_count = 0;
                count.unread_count = 0;
            }
            false => {
                count.has_unreads = true;
                count.unread_count += 1;
                if pings_you {
                    count.mention_count += 1;
                }
            }
        }
        // Every message moves the row: its badge, its count, and the time
        // it last spoke are all in what the list is ordered by. One row.
        let channel = message.channel.clone();
        self.reindex(&channel);
    }

    /// The DMs Slack says are unread, raised as the cards they would have
    /// been had rho been running. `activity.feed` carries mentions, thread
    /// replies and reactions but never a DM, so without this a message sent
    /// while rho was off is in the list and nowhere else.
    ///
    /// Deduplication is the ordinary one: a DM the socket or the feed has
    /// already accounted for is not raised twice.
    pub fn unread_dms(&mut self, now_ms: i64) -> Vec<Change> {
        let unread = self
            .counts
            .values()
            .filter(|count| count.has_unreads || count.mention_count > 0)
            .filter(|count| self.is_dm(&count.channel))
            .filter_map(|count| {
                Some(ActivityItem {
                    channel: count.channel.clone(),
                    ts: count.latest.clone()?,
                    thread_ts: None,
                    kind: ActivityKind::DirectMessage,
                    unread: true,
                })
            })
            .collect::<Vec<_>>();
        unread
            .iter()
            .filter_map(|item| self.note_activity(item, now_ms))
            .collect()
    }

    /// The conversation list: unread first with their counts, then the rest
    /// by recency, and the muted ones under both. Within a group, the
    /// noisier conversation sorts first.
    pub fn conversation_rows(&self) -> Vec<ConversationRow> {
        match self.query.is_empty() {
            true => self.order.values().cloned().collect(),
            // While a query stands the list is the narrowed one, and it
            // costs the matches rather than the workspace.
            false => self
                .narrowed
                .iter()
                .filter_map(|(key, _)| self.order.get(key).cloned())
                .collect(),
        }
    }

    /// A slice of the list, for a drawer that only has a screen to fill.
    /// The rows come out already in order, so this costs the rows asked
    /// for and the walk to reach them, and never the workspace.
    pub fn conversation_window(&self, from: usize, count: usize) -> Vec<ConversationRow> {
        self.order
            .values()
            .skip(from)
            .take(count)
            .cloned()
            .collect()
    }

    /// How many conversations the workspace holds, whatever a query
    /// reaches. What the list's narrowing line counts against, so the
    /// reader can see how much is being kept off the screen.
    pub fn conversation_count(&self) -> usize {
        self.order.len()
    }

    /// What the list has done since this was last asked, in order, and
    /// forgets it. `None` means the log was dropped and the caller has to
    /// draw the list again from scratch.
    pub fn take_row_edits(&mut self) -> Option<Vec<RowEdit>> {
        match std::mem::take(&mut self.resync) {
            true => {
                self.edits = Vec::new();
                None
            }
            false => Some(std::mem::take(&mut self.edits)),
        }
    }

    /// Forgets the log without applying it: for a drawer that has just
    /// built the list in full and is therefore already caught up.
    pub fn forget_row_edits(&mut self) {
        self.edits = Vec::new();
        self.resync = false;
    }

    /// Where one conversation sits now, and the row to draw there. `None`
    /// for a conversation that has gone, which is the drawer's cue to take
    /// its line out.
    ///
    /// The place is counted, so this costs the distance from the top of
    /// the list. Nothing on an event path calls it; it is here for a test
    /// or a caller that genuinely wants a number.
    pub fn row_position(&self, channel: &ChannelId) -> Option<(usize, ConversationRow)> {
        let key = self.placed.get(channel)?;
        let at = self.order.range(..key).count();
        Some((at, self.order.get(key)?.clone()))
    }

    /// Puts one conversation back in its place in the list, and nothing
    /// else: the cost of a message, a mark or a mute is the conversation it
    /// happened in.
    ///
    /// Both halves are needed. The key is what the list is ordered by, and
    /// it has to come out of the set before it changes or the set would be
    /// ordered by a key that is no longer there; `placed` is what remembers
    /// which key a conversation currently has, since the key itself cannot
    /// be worked backwards from the conversation once its counts have
    /// moved.
    fn reindex(&mut self, channel: &ChannelId) {
        // Shown, not merely listed: while a query stands the list on
        // screen is the narrowed one, so what a drawer has to be told
        // about is whether this conversation had a line there.
        let mut was_shown = false;
        if let Some(held) = self.placed.remove(channel) {
            self.order.remove(&held);
            was_shown = match self.query.is_empty() {
                true => true,
                false => match self.narrowed.binary_search_by(|(key, _)| key.cmp(&held)) {
                    Ok(at) => {
                        self.narrowed.remove(at);
                        true
                    }
                    Err(_) => false,
                },
            };
        }
        let Some(conversation) = self.conversations.get(channel) else {
            self.index_words(channel, None);
            self.note_row_edit(RowEdit {
                channel: channel.clone(),
                was_shown,
                before: None,
                row: None,
            });
            return;
        };
        let count = self.counts.get(channel);
        let label = crate::emoji::render(&self.raw_label(conversation));
        let key = RowKey {
            // Muted goes to the bottom whatever it holds: the whole point
            // of muting is that its traffic stops competing for the top.
            muted: self.muted.contains(channel),
            unread: Reverse(count.is_some_and(|count| count.has_unreads)),
            mentions: Reverse(count.map_or(0, |count| count.mention_count)),
            // Milliseconds rather than the timestamp itself, because a
            // timestamp is a string and the integer part grows a digit
            // every few years: byte order and time order agree today and
            // would quietly stop agreeing.
            latest: Reverse(
                count
                    .and_then(|count| count.latest.as_ref())
                    .map_or(0, Ts::millis),
            ),
            label: std::sync::Arc::from(label.as_str()),
            id: std::sync::Arc::from(channel.0.as_str()),
        };
        let row = ConversationRow {
            id: channel.clone(),
            label,
            unread: count.is_some_and(|count| count.has_unreads),
            mention_count: count.map_or(0, |count| count.mention_count),
            unread_count: count.map_or(0, |count| count.unread_count),
            muted: self.muted.contains(channel),
            watched: self.watched.contains(channel),
            latest: count.and_then(|count| count.latest.clone()),
        };
        self.index_words(channel, Some(&row.label));
        // The neighbour, not the place: one step past this key in the
        // order, which the tree finds in the depth of the tree. Counting
        // how many rows are above it instead would cost the distance, and
        // doing that on every message is what the list is here to avoid.
        // While a query stands the neighbour is read from the narrowed
        // list, because that is the list the drawer holds.
        let shown = self.query.is_empty() || self.matches_query(channel);
        let before = match self.query.is_empty() {
            true => self
                .order
                .range(&key..)
                .next()
                .map(|(next, _)| ChannelId(next.id.to_string())),
            false => {
                // Where this row goes in the narrowed list, and so which
                // row it sits above. The key is not in the list yet, so the
                // search lands on the successor either way.
                let at = self
                    .narrowed
                    .binary_search_by(|(held, _)| held.cmp(&key))
                    .unwrap_or_else(|at| at);
                self.narrowed.get(at).map(|(_, next)| next.clone())
            }
        };
        self.placed.insert(channel.clone(), key.clone());
        self.order.insert(key.clone(), row.clone());
        if shown && !self.query.is_empty() {
            let at = self
                .narrowed
                .binary_search_by(|(held, _)| held.cmp(&key))
                .unwrap_or_else(|at| at);
            self.narrowed.insert(at, (key, channel.clone()));
            self.reached_once = true;
        }
        // Traffic in a conversation the query does not reach changes no
        // line on screen, so there is nothing to tell the drawer and
        // nothing to grow the log with.
        if !was_shown && !shown {
            return;
        }
        self.note_row_edit(RowEdit {
            channel: channel.clone(),
            was_shown,
            before: match shown {
                true => before,
                false => None,
            },
            row: match shown {
                true => Some(row),
                false => None,
            },
        });
    }

    /// The word starts in a name, folded to lower case.
    ///
    /// A name breaks at a dash, an underscore, a space, a dot and at a
    /// change from lower case to upper, so `#design-ops`, `@Ada Lovelace`
    /// and `mpdm-devOps` all offer the words a reader would type at them.
    /// The whole name is a word too, sigil stripped, so `#design` is
    /// reached by `design` and by `des`.
    fn word_starts(label: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut previous_lower = false;
        for character in label.chars() {
            let breaks = matches!(
                character,
                '-' | '_' | ' ' | '.' | ',' | '/' | '#' | '@' | ':'
            );
            let humps = previous_lower && character.is_uppercase();
            if breaks || humps {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                if breaks {
                    previous_lower = false;
                    continue;
                }
            }
            previous_lower = character.is_lowercase();
            word.extend(character.to_lowercase());
        }
        if !word.is_empty() {
            words.push(word);
        }
        words.sort();
        words.dedup();
        words
    }

    /// Puts one conversation's words in the index, and takes its old ones
    /// out. The cost is the words of the one name that changed, which is
    /// why the words it had are remembered rather than searched for.
    fn index_words(&mut self, channel: &ChannelId, label: Option<&str>) {
        let fresh = label.map(Self::word_starts).unwrap_or_default();
        if let Some(stale) = self.worded.get(channel)
            && stale == &fresh
        {
            return;
        }
        if let Some(stale) = self.worded.remove(channel) {
            for word in stale {
                self.words.remove(&(word, channel.clone()));
            }
        }
        if fresh.is_empty() {
            return;
        }
        for word in &fresh {
            self.words.insert((word.clone(), channel.clone()));
        }
        self.worded.insert(channel.clone(), fresh);
    }

    /// Whether a conversation's name answers every typed word. Asked of
    /// the words already indexed for it, so this costs that one name.
    fn matches_query(&self, channel: &ChannelId) -> bool {
        let Some(words) = self.worded.get(channel) else {
            return false;
        };
        self.query
            .iter()
            .all(|typed| words.iter().any(|word| word.starts_with(typed)))
    }

    /// What the reader has typed, or the empty string when the list is not
    /// narrowed.
    pub fn query(&self) -> String {
        self.query.join(" ")
    }

    /// Whether a query stands, which is what tells a drawer the list it is
    /// looking at is the narrowed one.
    pub fn is_narrowed(&self) -> bool {
        !self.query.is_empty()
    }

    /// The conversations a query reaches, in the list's own order, at most
    /// `most` of them. For a caller that wants to show the matches without
    /// narrowing anything: the minibuffer offering names as the reader
    /// types is the case.
    ///
    /// Costs the depth of the tree per typed word plus the conversations
    /// they reach, and never the list.
    pub fn reached_by(&self, query: &str, most: usize) -> Vec<ConversationRow> {
        let typed = typed_words(query);
        if typed.is_empty() {
            return self.conversation_window(0, most);
        }
        let mut found = self
            .reached_for(&typed)
            .into_iter()
            .filter_map(|channel| self.placed.get(&channel).cloned())
            .collect::<Vec<_>>();
        // Only the first `most` are shown, so only they are put in order:
        // the rest are partitioned away in one pass rather than sorted.
        // A letter reaching a tenth of a large workspace was paying to
        // order thousands of names to show sixty-four of them.
        match found.len() > most {
            true => {
                found.select_nth_unstable(most);
                found.truncate(most);
                found.sort_unstable();
            }
            false => found.sort_unstable(),
        }
        found
            .into_iter()
            .take(most)
            .filter_map(|key| self.order.get(&key).cloned())
            .collect()
    }

    /// Narrows the list to the conversations the query reaches, and logs
    /// the difference from the list that was on screen so a drawer edits
    /// rows out and back in rather than drawing the list again.
    ///
    /// Two typed words intersect: `des ops` reaches only the names with a
    /// word starting `des` and a word starting `ops`. Each word is its own
    /// range scan and the smallest is walked first, so the cost is the
    /// depth of the tree per word plus the conversations they reach, never
    /// the list.
    pub fn narrow(&mut self, query: &str) {
        let typed = typed_words(query);
        if typed == self.query {
            return;
        }
        let was = std::mem::take(&mut self.narrowed);
        let was_narrowed = !self.query.is_empty();
        self.query = typed;

        // Drawn in the list's own order, not in the index's: the key a
        // conversation already sits at is what orders it, and the reader's
        // list does not reshuffle because they typed. Sorted in one pass
        // over a flat vector rather than by inserting each match into a
        // tree, because the key carries a name and comparing names is the
        // expensive part.
        let mut now = self
            .reached()
            .into_iter()
            .filter_map(|channel| Some((self.placed.get(&channel)?.clone(), channel)))
            .collect::<Vec<_>>();
        now.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

        match (was_narrowed, self.query.is_empty()) {
            // Into or out of a narrowing from the whole list: the two
            // lists have no common shape to diff, so the drawer is told to
            // draw once rather than handed n edits.
            (false, _) | (_, true) => self.resync_rows(),
            // Narrowing further or widening by a letter: the difference is
            // what changed, walked over the two ordered sets at once.
            (true, false) => self.note_narrow_edits(&was, &now),
        }
        self.reached_once = !now.is_empty();
        self.narrowed = now;
    }

    /// Why the narrowed list has nothing on it, or `None` when it has rows
    /// or no query stands at all.
    ///
    /// The two are not the same fact and must not read the same: a word
    /// nothing answers is the reader's last keystroke, and a list that
    /// emptied under them is Slack's doing. Only the second has to name
    /// the way out, because only in the second did the reader do nothing
    /// to get there.
    pub fn empty_narrowing(&self) -> Option<Empty> {
        if self.query.is_empty() || !self.narrowed.is_empty() {
            return None;
        }
        match self.reached_once {
            true => Some(Empty::Gone),
            false => Some(Empty::Never),
        }
    }

    /// The conversations the standing query reaches, or none at all when
    /// nothing is typed.
    ///
    /// One typed word is answered straight off the range scan, with no
    /// set built to throw away. More than one is an intersection, and the
    /// rarest word is walked first so the work is that word's matches and
    /// not the commonest word's.
    fn reached(&self) -> Vec<ChannelId> {
        self.reached_for(&self.query)
    }

    /// The conversations a list of typed words reaches.
    fn reached_for(&self, query: &[String]) -> Vec<ChannelId> {
        let Some((first, rest)) = query.split_first() else {
            return Vec::new();
        };
        if rest.is_empty() {
            return self.reaching_iter(first).cloned().collect();
        }
        let mut postings = query
            .iter()
            .map(|word| (self.reaching_iter(word).count(), word))
            .collect::<Vec<_>>();
        postings.sort();
        let Some(((_, rarest), others)) = postings.split_first() else {
            return Vec::new();
        };
        self.reaching_iter(rarest)
            .filter(|channel| {
                let Some(words) = self.worded.get(*channel) else {
                    return false;
                };
                others
                    .iter()
                    .all(|(_, typed)| words.iter().any(|word| word.starts_with(typed.as_str())))
            })
            .cloned()
            .collect()
    }

    /// The conversations one typed word reaches, as a walk rather than a
    /// set: every name with a word starting with it. A range scan from the
    /// word to the first word that does not begin with it, so the cost is
    /// the depth of the tree plus the matches.
    fn reaching_iter<'a>(&'a self, word: &'a str) -> impl Iterator<Item = &'a ChannelId> + 'a {
        let start = (word.to_owned(), ChannelId(String::new()));
        self.words
            .range(start..)
            .take_while(move |(indexed, _)| indexed.starts_with(word))
            .map(|(_, channel)| channel)
    }

    /// The edits between two narrowings, walked over both in key order so
    /// the cost is the two match sets and nothing else.
    ///
    /// Rows leaving go first, then rows arriving in descending order.
    /// Descending is what makes a neighbour namable: by the time a row is
    /// put back, every row below it in the new list is already on screen,
    /// so its successor there is a line the drawer holds.
    fn note_narrow_edits(&mut self, was: &[(RowKey, ChannelId)], now: &[(RowKey, ChannelId)]) {
        // Both lists are in the same order, so the difference is one walk
        // down the two of them: no key is looked up in a tree and no name
        // is compared more than once.
        let (mut here, mut there) = (0, 0);
        let mut leaving = Vec::new();
        let mut arriving = Vec::new();
        while here < was.len() || there < now.len() {
            match (was.get(here), now.get(there)) {
                (Some((left, channel)), Some((right, _))) if left < right => {
                    leaving.push(channel.clone());
                    here += 1;
                }
                (Some((left, _)), Some((right, channel))) if right < left => {
                    arriving.push((there, channel.clone()));
                    there += 1;
                }
                (Some(_), Some(_)) => {
                    here += 1;
                    there += 1;
                }
                (Some((_, channel)), None) => {
                    leaving.push(channel.clone());
                    here += 1;
                }
                (None, Some((_, channel))) => {
                    arriving.push((there, channel.clone()));
                    there += 1;
                }
                (None, None) => break,
            }
        }
        for channel in leaving {
            self.note_row_edit(RowEdit {
                channel,
                was_shown: true,
                before: None,
                row: None,
            });
        }
        // Rows arriving go back in descending order, which is what makes a
        // neighbour namable: by the time a row is put back, every row
        // below it in the new list is already on screen, so its successor
        // there is a line the drawer holds.
        for (at, channel) in arriving.into_iter().rev() {
            let row = self.order.get(&now[at].0).cloned();
            self.note_row_edit(RowEdit {
                channel,
                was_shown: false,
                before: now.get(at + 1).map(|(_, next)| next.clone()),
                row,
            });
        }
    }

    /// Tells the drawer to draw the list again and drops the log, for the
    /// changes where no diff against what is on screen exists.
    fn resync_rows(&mut self) {
        self.resync = true;
        self.edits = Vec::new();
    }

    /// Notes one list edit, or gives up on the log. Giving up is the right
    /// answer when nobody has drawn the list in a long time: replaying ten
    /// thousand edits is slower than drawing the list once, and holding
    /// them costs memory for a screen nobody is looking at.
    fn note_row_edit(&mut self, edit: RowEdit) {
        if self.resync {
            return;
        }
        if self.edits.len() >= ROW_EDIT_CAP {
            self.edits = Vec::new();
            self.resync = true;
            return;
        }
        self.edits.push(edit);
    }

    /// Every conversation put back in its place. For the events that move
    /// all of them at once and genuinely have to: the roster landing, which
    /// renames every direct message, and the workspace's own emoji
    /// arriving, which re-renders every name that carries one. Those happen
    /// once a session, not on a keypress and not on a frame.
    fn reindex_all(&mut self) {
        // Every row moved, so there is nothing for a log of moved rows to
        // say. Telling the drawer to draw the list again is both cheaper
        // than n edits and what it would decide for itself on reading
        // them.
        self.resync = true;
        self.edits = Vec::new();
        for channel in self.conversations.keys().cloned().collect::<Vec<_>>() {
            self.reindex(&channel);
        }
    }

    /// What the composer offers for the token being typed. `@` is the
    /// people the reader could be talking to, `#` the channels, `:` the
    /// emoji, custom ones included. The value is what the editor shows and
    /// what a yank of the line carries; `encode` turns it into the wire
    /// form on the way out.
    pub fn suggestions(&self, channel: &ChannelId, sigil: char, needle: &str) -> Vec<Suggestion> {
        let needle = needle.to_lowercase();
        let mut found = match sigil {
            // The broadcasts are offered beside the members. They are in no
            // member list, so a composer that had only the roster to draw on
            // gave the reader no way to find out they exist.
            '@' => BROADCASTS
                .iter()
                .map(|name| Suggestion {
                    value: format!("@{name}"),
                    detail: match *name {
                        "here" => "everyone here now".to_owned(),
                        _ => "everyone in the channel".to_owned(),
                    },
                })
                .chain(
                    self.members_of(channel)
                        .into_iter()
                        .filter(|user| user.id != self.self_id)
                        .map(|user| Suggestion {
                            value: format!("@{}", user.handle),
                            detail: user.name.clone(),
                        }),
                )
                .collect::<Vec<_>>(),
            '#' => self
                .conversations
                .values()
                .filter(|conversation| conversation.kind == ConversationKind::Channel)
                .map(|conversation| Suggestion {
                    value: format!("#{}", conversation.name),
                    detail: String::new(),
                })
                .collect(),
            ':' => self
                .custom_emoji
                .iter()
                .map(|name| Suggestion {
                    value: format!(":{name}:"),
                    // Nowhere but Slack has a glyph for one of these, which
                    // is why it stays a shortcode on screen too.
                    detail: "custom".to_owned(),
                })
                .chain(emojis::iter().filter_map(|emoji| {
                    Some(Suggestion {
                        value: format!(":{}:", emoji.shortcode()?),
                        detail: emoji.as_str().to_owned(),
                    })
                }))
                .collect(),
            _ => Vec::new(),
        };
        found.retain(|found| found.value.to_lowercase().contains(&needle));
        // What starts with what was typed comes first: a reader who has
        // typed `@ad` means Ada before they mean anyone merely containing it.
        found.sort_by_key(|found| {
            let value = found.value.to_lowercase();
            let starts = !value[1..].starts_with(&needle);
            (starts, value)
        });
        found.truncate(SUGGESTION_LIMIT);
        found
    }

    /// Everyone the conversation could mean. Slack only lists members for a
    /// group DM, so a channel falls back to the workspace roster, which is
    /// what its member list would mostly be anyway.
    fn members_of(&self, channel: &ChannelId) -> Vec<&User> {
        let members = self
            .conversations
            .get(channel)
            .map(|conversation| conversation.members.as_slice())
            .unwrap_or_default();
        match members.is_empty() {
            true => self.users.values().collect(),
            false => members
                .iter()
                .filter_map(|member| self.users.get(member))
                .collect(),
        }
    }

    /// The wire form of what the reader typed: `@ada` becomes `<@U1>` and
    /// `#design` becomes `<#C1|design>`, which is what every other client
    /// sends and what makes the mention count for the person named. A name
    /// nobody answers to is left exactly as typed: it was prose.
    pub fn encode(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(['@', '#']) {
            let (before, from) = rest.split_at(at);
            out.push_str(before);
            let sigil = from.chars().next().expect("the sigil was just found");
            let name = from[1..]
                .split(|character: char| !is_name_char(character))
                .next()
                .unwrap_or_default();
            // A sigil mid-word is an email address or a fragment, not a
            // mention: only one starting a word can name anybody.
            let starts_word = before
                .chars()
                .last()
                .is_none_or(|character| !is_name_char(character));
            match self.wire_form(sigil, name).filter(|_| starts_word) {
                Some(wire) => out.push_str(&wire),
                None => {
                    out.push(sigil);
                    out.push_str(name);
                }
            }
            rest = &from[1 + name.len()..];
        }
        out.push_str(rest);
        out
    }

    /// The inverse of [`Model::encode`]: a wire string as the reader would
    /// have typed it, for a composer that is about to send it back.
    ///
    /// An escape is rewritten only when `encode` would turn the result into
    /// a mention of the same person or channel. Everything else stays
    /// exactly as the wire wrote it — a link, a subteam, an id the roster
    /// does not know. The rendered form is not typeable: `@Ada Lovelace`
    /// would go back out as prose, because `encode` stops a handle at the
    /// space and finds nobody, and `docs` would go out with the address
    /// gone. A rewrite of a message is not a chance to lose the link in it,
    /// so the reader edits a link in its wire form and keeps it.
    ///
    /// Cost: one roster lookup per escape in the one message, which is what
    /// `encode` already pays to send it. This is opening an edit, not a
    /// frame and not an event.
    pub fn decode(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('<') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            // An unclosed `<` is the reader's own character, not an escape.
            let Some(end) = after.find('>') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let escape = &after[..end];
            match self.typed_form(escape) {
                Some(typed) => out.push_str(&typed),
                None => {
                    out.push('<');
                    out.push_str(escape);
                    out.push('>');
                }
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// What the reader would have typed for one escape, or `None` when
    /// nothing they could type comes back as it.
    ///
    /// The test is not that the bytes match — Slack writes `<#C1>` and
    /// `<#C1|whatever-it-was-called-then>` for the same channel — but that
    /// `encode` names the same one coming back. Two people can carry the
    /// same handle, and a rewrite must not quietly change who it tells.
    fn typed_form(&self, escape: &str) -> Option<String> {
        let target = escape.split('|').next().unwrap_or(escape);
        let (sigil, id) = target.split_at_checked(1)?;
        match sigil {
            "@" => {
                let handle = self.users.get(&UserId(id.to_owned()))?.handle.clone();
                (self.wire_form('@', &handle)? == format!("<@{id}>")).then(|| format!("@{handle}"))
            }
            "#" => {
                let name = self
                    .conversations
                    .get(&ChannelId(id.to_owned()))?
                    .name
                    .clone();
                (self.wire_form('#', &name)? == format!("<#{id}|{name}>"))
                    .then(|| format!("#{name}"))
            }
            // The broadcasts name nobody in particular, so there is nothing
            // to resolve and nothing to get wrong.
            "!" if BROADCASTS.contains(&id) => Some(format!("@{id}")),
            _ => None,
        }
    }

    fn wire_form(&self, sigil: char, name: &str) -> Option<String> {
        match sigil {
            // The broadcasts are not users and are in no member list, so the
            // user table can never answer for them. Slack's wire form is its
            // own, and rho has always read one coming in — a broadcast earns
            // a card as surely as the reader's own handle does. Until now it
            // could read one and not send one.
            '@' if BROADCASTS.contains(&name) => Some(format!("<!{name}>")),
            '@' => {
                let user = self.users.values().find(|user| user.handle == name)?;
                Some(format!("<@{}>", user.id.0))
            }
            _ => {
                let conversation = self
                    .conversations
                    .values()
                    .find(|conversation| conversation.name == name)?;
                Some(format!("<#{}|{}>", conversation.id.0, conversation.name))
            }
        }
    }

    /// The next conversation with something unread, in the order the list
    /// shows them, starting after `from`. Wraps, so reading through the
    /// unread ones is one key pressed repeatedly; `None` when there is
    /// nothing left, which is what sends the reader back to the list.
    ///
    /// The list the reader is looking at, which is the narrowed one while a
    /// query stands. A key that moves them through the list has to move
    /// them through *this* list: taking them to a conversation the rows
    /// cannot show, under a banner still describing the rows, is the list
    /// and the key disagreeing about where the reader is. The narrowing
    /// lasts as long as they are in it and no longer -- it is a motion, not
    /// a setting -- so there is no hour-old query left to answer this
    /// wrongly.
    ///
    /// This is why it no longer reads the same set `mark_plan` does. That
    /// one is a question about the workspace, and stays one; this is a
    /// question about the list on screen. Two questions, two answers, and
    /// the difference is deliberate.
    pub fn next_unread(&self, from: Option<&ChannelId>) -> NextUnread {
        let rows = self.walked();
        let at = from
            .and_then(|from| rows.iter().position(|row| &row.id == from))
            .map_or(0, |at| at + 1);
        let found = rows
            .iter()
            .skip(at)
            .chain(rows.iter().take(at))
            .find(|row| Self::unread(row) && Some(&row.id) != from)
            .map(|row| row.id.clone());
        if let Some(channel) = found {
            return NextUnread::Go(channel);
        }
        // Nothing left on screen. Whether that is the end of the unread or
        // only the end of what the query reaches is the difference between
        // "you are done" and "you are done in here", and the reader is owed
        // it: the same walk, once, and only at the edge.
        match self.query.is_empty() {
            true => NextUnread::Nothing,
            false => {
                let inside = rows.iter().map(|row| &row.id).collect::<BTreeSet<_>>();
                let outside = self
                    .order
                    .values()
                    .filter(|row| Self::unread(row) && !inside.contains(&row.id))
                    .count();
                match outside {
                    0 => NextUnread::Nothing,
                    outside => NextUnread::Outside(outside),
                }
            }
        }
    }

    /// Whether a conversation is somewhere the next-unread key will take
    /// the reader: unread or holding a mention, and not muted. Muted is
    /// the reader saying "not this one", which the key obeys.
    fn unread(row: &&ConversationRow) -> bool {
        !row.muted && (row.unread || row.mention_count > 0)
    }

    /// The rows a key walks: the narrowed ones while a query stands, the
    /// whole list otherwise. Costs the matches rather than the workspace
    /// when narrowed, since the narrowed set is already held in list order.
    fn walked(&self) -> Vec<&ConversationRow> {
        match self.query.is_empty() {
            true => self.order.values().collect(),
            false => self
                .narrowed
                .iter()
                .filter_map(|(key, _)| self.order.get(key))
                .collect(),
        }
    }

    /// What `mark read before` would touch, as a plan the caller can count
    /// before it acts and then act on unchanged. `before` is a cutoff in
    /// epoch seconds; nothing newer than it is ever in here.
    ///
    /// Conversations are the unread ones only. A conversation with nothing
    /// unread is not backlog, and marking it would spend a request to change
    /// nothing.
    pub fn mark_plan(&self, before: f64) -> MarkPlan {
        let conversations = self
            .counts
            .values()
            .filter(|count| count.has_unreads || count.mention_count > 0)
            .filter_map(|count| {
                let latest = count.latest.clone()?;
                (latest.epoch_seconds() < before).then_some((count.channel.clone(), latest))
            })
            .collect();
        let threads = self
            .units
            .iter()
            .filter_map(|(unit, facts)| {
                Some((self.key(&unit.channel, unit.thread.as_ref()?), facts))
            })
            .filter(|(key, _)| self.followed.contains(key))
            .filter(|(_, facts)| facts.newest.epoch_seconds() < before)
            .map(|(key, facts)| (key, facts.newest.clone()))
            .collect();
        MarkPlan {
            conversations,
            threads,
        }
    }

    /// The follow list as Slack has it, from `subscriptions.thread.getView`
    /// on connect. It replaces whatever rho held: Slack is the truth, and a
    /// thread missing from it is one the user has unfollowed somewhere else.
    /// Each thread arrives carrying the cursor Slack keeps inside it, which
    /// is where its unread rule goes: a thread read on the phone is read
    /// here after a restart because of this and nothing else.
    ///
    /// Returns the tracked threads the list no longer names: unfollowed in
    /// another client, possibly while rho was away. Their cards are Slack's
    /// to discard.
    pub fn set_followed(
        &mut self,
        threads: impl IntoIterator<Item = (ChannelId, Ts, Option<Ts>)>,
    ) -> Vec<ThreadKey> {
        let mut now = BTreeSet::new();
        for (channel, thread_ts, last_read) in threads {
            let key = self.key(&channel, &thread_ts);
            if let Some(last_read) = last_read {
                self.mark_thread_read(&key, &last_read);
            }
            now.insert(key);
        }
        let dropped = self
            .followed
            .difference(&now)
            .filter(|key| self.units.contains_key(&unit_of(key)))
            .cloned()
            .collect::<Vec<_>>();
        // Slack's follow list is one of the five facts the rule reads, so a
        // thread that has just become followed asks from now on. Only the
        // threads whose follow actually changed are visited: the list comes
        // again on every reconnect and is nearly always the same list.
        let added = now
            .difference(&self.followed)
            .map(unit_of)
            .collect::<Vec<_>>();
        self.followed = now;
        for key in &dropped {
            self.asking.remove(&unit_of(key));
            self.units.remove(&unit_of(key));
        }
        for unit in added {
            self.refresh_attention(&unit);
        }
        dropped
    }

    pub fn follow(&mut self, channel: &ChannelId, thread_ts: &Ts) {
        let key = self.key(channel, thread_ts);
        self.followed.insert(key.clone());
        self.refresh_attention(&unit_of(&key));
    }

    /// A thread unfollowed anywhere stops being the user's business. The
    /// card it raised is left to the verdict that closes it; what goes is
    /// the standing claim that its next reply is theirs.
    /// rho's own ignore: the follow goes, the thread stays. Undoing the
    /// mute follows it again and the card comes back exactly as it was,
    /// which an unfollow from Slack's side cannot promise.
    pub fn ignore(&mut self, key: &ThreadKey) {
        self.followed.remove(key);
        self.refresh_attention(&unit_of(key));
    }

    /// Returns whether rho is tracking the thread, which is the difference
    /// between an unfollow that mutes a card and one that changes
    /// nothing the user can see.
    pub fn unfollow(&mut self, channel: &ChannelId, thread_ts: &Ts) -> bool {
        let key = self.key(channel, thread_ts);
        self.followed.remove(&key);
        // The thread goes with the follow. Nothing here remembers that it
        // was ever raised, so following it again in Slack raises it only
        // when somebody writes in it.
        self.asking.remove(&unit_of(&key));
        self.units.remove(&unit_of(&key)).is_some()
    }

    /// The threads the user follows, for a caller that has to walk them:
    /// they are units in their own right, so anything re-deriving units from
    /// history needs to know which threads to look in.
    pub fn followed(&self) -> Vec<ThreadKey> {
        self.followed.iter().cloned().collect()
    }

    pub fn follows(&self, key: &ThreadKey) -> bool {
        self.followed.contains(key)
    }

    /// The channels the reader opted into, as the mirror handed them back.
    /// Replaces whatever the model held, because the file is the record.
    pub fn set_watched(&mut self, channels: impl IntoIterator<Item = ChannelId>) {
        let now = channels.into_iter().collect::<BTreeSet<_>>();
        let touched = self
            .watched
            .symmetric_difference(&now)
            .cloned()
            .collect::<Vec<_>>();
        self.watched = now;
        for channel in touched {
            self.refresh_channel(&channel);
            self.reindex(&channel);
        }
    }

    /// Opts into a channel, or out of it, and says whether that changed
    /// anything — the caller writes the file only when it did.
    ///
    /// Opting out stops the channel asking, and leaves the unit it raised
    /// standing: Find still reaches it, and the desk row the reader may
    /// have half filed is not pulled out from under them. What goes is the
    /// standing claim on their attention, which is what they said.
    pub fn set_watching(&mut self, channel: &ChannelId, watching: bool) -> bool {
        let moved = match watching {
            true => self.watched.insert(channel.clone()),
            false => self.watched.remove(channel),
        };
        if moved {
            self.refresh_channel(channel);
            self.reindex(channel);
        }
        moved
    }

    pub fn watches(&self, channel: &ChannelId) -> bool {
        self.watched.contains(channel)
    }

    /// Every channel the reader opted into, for the caller that writes them
    /// down and the list that marks them.
    pub fn watched(&self) -> Vec<ChannelId> {
        self.watched.iter().cloned().collect()
    }

    /// Marks a conversation read locally. Called when rho reads one, when
    /// Slack says another client did, and when the mirror hands back what
    /// the last run knew. Reading is not a verdict: this moves the cursor
    /// and the badge and leaves every card exactly where it was.
    ///
    /// The cursor only ever rises, like every other fact here. A mark is
    /// evidence that the user read this far, and a `channel_marked` frame
    /// that overtakes rho's own request, or a `client.counts` answered
    /// before it, would otherwise pull the rule back over messages the
    /// reader has already dealt with.
    ///
    /// Returns whether the cursor moved, because the callers that have to
    /// write it down should not write down a mark that changed nothing.
    pub fn mark_read(&mut self, channel: &ChannelId, ts: &Ts) -> bool {
        if self
            .conversation_read
            .get(channel)
            .is_some_and(|held| !ts.is_newer_than(held))
        {
            return false;
        }
        self.conversation_read.insert(channel.clone(), ts.clone());
        self.refresh_badge(channel);
        // The cursor moved past somebody's message, so the units in this
        // conversation may have stopped asking, and its badge may have gone
        // out from under its place in the list. Only this conversation's.
        self.refresh_channel(channel);
        self.reindex(channel);
        true
    }

    /// Clears a conversation's badge once its cursor has caught up with the
    /// newest message Slack knows about.
    ///
    /// The badge says what is still above the cursor, so a mark that did not
    /// reach the newest message leaves it standing: `mark read before` marks
    /// at a cutoff, and a conversation with something newer than the cutoff
    /// has genuinely not been read. Separate from [`Model::mark_read`]
    /// because a badge can need clearing when the cursor did not move — a
    /// reconnect re-badging a conversation the reader is already through is
    /// exactly that case.
    fn refresh_badge(&mut self, channel: &ChannelId) {
        let Some(cursor) = self.conversation_read.get(channel).cloned() else {
            return;
        };
        let Some(count) = self.counts.get_mut(channel) else {
            return;
        };
        let caught_up = count
            .latest
            .as_ref()
            .is_none_or(|latest| !latest.is_newer_than(&cursor));
        if caught_up {
            count.has_unreads = false;
            count.mention_count = 0;
            count.unread_count = 0;
        }
    }

    /// Slack's read cursor for the conversation: the message the unread
    /// rule sits under. Kept apart from the counts because it outlives
    /// them — it comes back off the mirror at startup, before Slack has
    /// said anything, which is what puts the rule in the right place on a
    /// restart and offline.
    pub fn last_read(&self, channel: &ChannelId) -> Option<&Ts> {
        self.conversation_read.get(channel)
    }

    /// Marks a thread read, at its own cursor. Slack keeps one per followed
    /// thread and rho keeps it in the same shape: a thread read here or on
    /// the phone says nothing about the conversation around it, and a
    /// channel marked read says nothing about the threads hanging in it.
    ///
    /// Monotonic for the same reason the conversation's cursor is, and
    /// answers the same question: did anything move?
    pub fn mark_thread_read(&mut self, key: &ThreadKey, ts: &Ts) -> bool {
        match self.thread_read.get(key) {
            Some(held) if !ts.is_newer_than(held) => false,
            _ => {
                self.thread_read.insert(key.clone(), ts.clone());
                self.refresh_attention(&unit_of(key));
                true
            }
        }
    }

    /// The read cursor inside a followed thread, where its unread rule goes.
    pub fn thread_last_read(&self, key: &ThreadKey) -> Option<&Ts> {
        self.thread_read.get(key)
    }

    /// Fills in the author of a message rho already knows about. The feed
    /// says a message landed without saying who wrote it, so the body that
    /// arrives afterwards is what settles whose turn it is. The words are
    /// never stored: a card renders them from the mirror when it is drawn.
    pub fn note_loaded(&mut self, message: &Message) -> Option<Change> {
        let from_you = message.user.as_ref() == Some(&self.self_id);
        let unit = self.unit_for(&message.channel, message.thread_ts.as_ref());
        let facts = self.units.get_mut(&unit)?;
        if facts.newest != message.ts || facts.newest_from_you == from_you {
            return None;
        }
        facts.newest_from_you = from_you;
        Some(match facts.waiting() {
            Waiting::OnYou => Change::Updated(unit),
            Waiting::OnThem => Change::Replied(unit),
        })
    }

    /// Every unit rho is tracking, for a caller that has to revisit them
    /// all — the roster arriving is the case: names known late change what a
    /// card says.
    pub fn tracked(&self) -> Vec<Unit> {
        self.units.keys().cloned().collect()
    }

    pub fn unit(&self, unit: &Unit) -> Option<&UnitFacts> {
        self.units.get(unit)
    }

    /// Installs a unit the last run wrote down, without going near a
    /// message. The facts are already the answer; deriving them again from
    /// history is the pass over the mirror the cost rule forbids.
    ///
    /// The message ids are marked seen for the same reason they would have
    /// been when they landed: the feed replaying an item rho already has
    /// must not raise it a second time.
    pub fn restore_unit(&mut self, unit: Unit, facts: UnitFacts) {
        self.seen
            .insert((unit.channel.clone(), facts.newest.clone()));
        if let Some(other) = facts.newest_from_other.clone() {
            self.seen.insert((unit.channel.clone(), other));
        }
        self.units.insert(unit.clone(), facts);
        self.refresh_attention(&unit);
    }

    /// The unit a message belongs to: a followed thread when Slack says the
    /// message is a reply in one, and the conversation otherwise. A reply in
    /// a thread nobody follows is traffic in the conversation, not a unit of
    /// its own, which is what keeps a channel with three mentions to one
    /// card.
    pub fn unit_for(&self, channel: &ChannelId, thread_ts: Option<&Ts>) -> Unit {
        match thread_ts {
            Some(root) if self.followed.contains(&self.key(channel, root)) => {
                Unit::thread(channel, root)
            }
            _ => Unit::conversation(channel),
        }
    }

    pub fn key(&self, channel: &ChannelId, thread_ts: &Ts) -> ThreadKey {
        ThreadKey {
            workspace: self.workspace.clone(),
            channel: channel.clone(),
            thread_ts: thread_ts.clone(),
        }
    }

    /// Renders a message the way the surface shows it, with whatever names
    /// are known right now.
    /// The message's own words and the lines the renderer hangs under
    /// them, kept apart for a surface that puts the time at the end of the
    /// words rather than at the end of the chrome.
    pub fn render_parts(&self, message: &Message) -> (String, Vec<String>) {
        crate::block::render_parts(
            &message.blocks,
            &message.text,
            &message.attachments,
            &message.files,
            self,
        )
    }

    pub fn render(&self, message: &Message) -> String {
        render_message(
            &message.blocks,
            &message.text,
            &message.attachments,
            &message.files,
            self,
        )
    }

    /// Who a message is from, as a name.
    /// How a mention of the reader appears in a rendered body: the same
    /// `@name` anyone else's mention gets, which is what the UI looks for
    /// when it decides which text is theirs.
    pub fn self_mention(&self) -> Option<String> {
        self.users
            .get(&self.self_id)
            .map(|user| format!("@{}", user.name))
    }

    /// Whether the roster has a name for this id. What `author` falls back
    /// to is not a name, and the session has to be able to tell the
    /// difference to know whether there is anything to ask Slack.
    pub fn knows_user(&self, id: &UserId) -> bool {
        self.user(id).is_some()
    }

    pub fn author(&self, message: &Message) -> String {
        message
            .user
            .as_ref()
            .and_then(|id| self.user(id))
            .or_else(|| message.bot_name.clone())
            .unwrap_or_else(|| "someone".to_owned())
    }

    /// Takes one message, from either source. Returns what it changed, or
    /// nothing at all when it is channel traffic or a duplicate.
    pub fn note_message(&mut self, message: &Message, now_ms: i64) -> Option<Change> {
        let unit = self.unit_for(&message.channel, message.thread_ts.as_ref());
        let from_you = message.user.as_ref() == Some(&self.self_id);
        // The reason is decided before the message is marked seen: channel
        // traffic is never "seen", so a live message rho drops cannot poison
        // the dedup and swallow the feed item for the same `ts` that would
        // have raised it.
        let reason = self.reason_for(message, &unit, from_you)?;
        if !self
            .seen
            .insert((message.channel.clone(), message.ts.clone()))
        {
            return None;
        }
        self.record(unit, reason, &message.ts, from_you, now_ms)
    }

    /// Takes one activity-feed entry. The feed says *that* something
    /// happened; the message body arrives separately, so a thread raised
    /// from the feed carries no summary until it is loaded.
    pub fn note_activity(&mut self, item: &ActivityItem, now_ms: i64) -> Option<Change> {
        let reason = match item.kind {
            ActivityKind::Mention => Reason::Mention,
            ActivityKind::ThreadReply => Reason::Thread,
            ActivityKind::DirectMessage => Reason::DirectMessage,
            ActivityKind::Other => return None,
        };
        if !self.seen.insert((item.channel.clone(), item.ts.clone())) {
            return None;
        }
        let unit = self.unit_for(&item.channel, item.thread_ts.as_ref());
        self.record(unit, reason, &item.ts, false, now_ms)
    }

    /// Why this message obliges the user, or `None` when it is traffic.
    ///
    /// A channel the user was mentioned in is a unit, but the twenty
    /// unrelated messages that follow the mention are not about them: they
    /// move nothing on the card, or the wait would reset every time anyone
    /// said anything. What counts is a message addressed to them, a reply
    /// in a thread they follow, and their own answer.
    fn reason_for(&self, message: &Message, unit: &Unit, from_you: bool) -> Option<Reason> {
        let reason = if self.is_dm(&message.channel) {
            Some(Reason::DirectMessage)
        } else if self.mentions_you(message) {
            Some(Reason::Mention)
        } else if unit.thread.is_some() {
            Some(Reason::Thread)
        } else if self.watches(&message.channel) {
            // The opt-in is the address. Without it this line is where the
            // twenty unrelated messages after a mention stop being anyone's
            // business, and that is the whole of the flood.
            Some(Reason::Watched)
        } else {
            None
        };
        // The user's own message counts in any unit rho already tracks: it
        // is what flips whose turn it is, whatever it says.
        let existing = self.units.get(unit).map(|facts| facts.reason);
        let held = match from_you {
            true => existing.or(reason),
            false => reason.and(existing.or(reason)),
        };
        // A watched channel the reader is then named in is a mention from
        // then on: the reason a unit carries is the strongest thing that
        // has happened in it, not the first.
        Some(match (held?, reason) {
            (Reason::Watched, Some(stronger)) => stronger,
            (held, _) => held,
        })
    }

    /// Whether a message is one the user is meant to answer: any message
    /// in a direct message, a mention in a channel, a reply in a thread they
    /// follow. This is what `newest_from_other` counts, so it is also what a
    /// dealt card lands on: the ordinary chatter in a channel is not what
    /// the reader was brought here for.
    pub fn concerns_you(&self, message: &Message, unit: &Unit) -> bool {
        unit.thread.is_some()
            || self.is_dm(&message.channel)
            || self.mentions_you(message)
            || self.watches(&message.channel)
    }

    /// Whether the conversation is a DM: one person or a group of them.
    /// A group DM is a room, but it is a room the user was put in by name,
    /// so a message in it is addressed to them the way a one-to-one is and
    /// not the way `#design` is.
    fn is_dm(&self, channel: &ChannelId) -> bool {
        self.conversations.get(channel).is_some_and(|conversation| {
            matches!(
                conversation.kind,
                ConversationKind::DirectMessage | ConversationKind::Group
            )
        })
    }

    fn mentions_you(&self, message: &Message) -> bool {
        if self.self_id.0.is_empty() {
            return false;
        }
        if message.text.contains(&format!("<@{}>", self.self_id.0)) {
            return true;
        }
        message
            .blocks
            .iter()
            .any(|block| mentions_in(block, &self.self_id))
    }

    /// Folds one message into a unit's facts.
    ///
    /// Nothing here ever goes backwards. A history page, a reconnect, a feed
    /// poll and a restart all replay messages the model has already seen, and
    /// any of them lowering `newest_from_other` would reopen a card the user
    /// has closed. So every timestamp is raised by `max` and never assigned.
    fn record(
        &mut self,
        unit: Unit,
        reason: Reason,
        ts: &Ts,
        from_you: bool,
        now_ms: i64,
    ) -> Option<Change> {
        let existing = self.units.get(&unit);
        // An out-of-order arrival (a feed page after the socket already had
        // the newer reply) is evidence about the past, not news.
        if existing.is_some_and(|facts| facts.newest.is_newer_than(ts)) {
            return None;
        }
        let was_waiting = existing.map(UnitFacts::waiting);
        let first_seen_ms = existing.map_or(now_ms, |facts| facts.first_seen_ms);
        let newest_from_other = match from_you {
            true => existing.and_then(|facts| facts.newest_from_other.clone()),
            false => Some(ts.clone()),
        };
        self.units.insert(
            unit.clone(),
            UnitFacts {
                reason,
                newest: ts.clone(),
                newest_from_other,
                newest_from_you: from_you,
                first_seen_ms,
            },
        );
        self.refresh_attention(&unit);
        Some(match (was_waiting, from_you) {
            // Answering is not closing. The card keeps its place in the tree
            // and drops onto the fyi curve; only the user's own `d` ends it.
            (_, true) => Change::Replied(unit),
            (Some(Waiting::OnYou), false) => Change::Updated(unit),
            // Either brand new, or answered-then-answered-again: both are a
            // fresh obligation, which is what re-raises a card after a done.
            (_, false) => Change::Raised(unit),
        })
    }

    /// What a unit looks like right now. The words are not in here: a card's
    /// text is rendered from the mirror when it is drawn, so a name learned
    /// after the message landed shows as `@ada` rather than `<@U123>`.
    pub fn card(&self, unit: &Unit, now_ms: i64) -> Option<UnitCard> {
        let facts = self.units.get(unit)?;
        Some(UnitCard {
            unit: unit.clone(),
            title: String::new(),
            conversation: self.label(&unit.channel),
            attention: self.attention(unit),
            waiting: facts.waiting(),
            wait_days: wait_days(facts, now_ms),
            first_seen_ms: facts.first_seen_ms,
            newest: facts.newest.clone(),
            newest_from_other: facts.newest_from_other.clone(),
        })
    }

    /// Whether a `mark read before` cutoff closes this unit, and the
    /// message it closes at: its newest message is older than the cutoff.
    /// A unit the model has nothing to say about is not closed, which is
    /// what keeps a cutoff from touching a card the mirror never saw.
    pub fn closed_by(&self, unit: &Unit, before_epoch_seconds: f64) -> Option<Ts> {
        let facts = self.units.get(unit)?;
        (facts.newest.epoch_seconds() < before_epoch_seconds).then(|| facts.newest.clone())
    }

    /// Whether Slack itself would be badging this unit right now, and what
    /// for. This is the one place the question is answered, and it is the
    /// difference between an inbox and a firehose.
    ///
    /// Everything here is asked of Slack's own read state rather than of
    /// rho's dealing cursor: a mention read on the phone this morning is
    /// not asking for anything, and the flood was rho going on asking about
    /// it. A channel with plain unread traffic and nothing addressed to the
    /// reader is in the list with its count and is not a card, unless the
    /// reader opted into it.
    pub fn attention(&self, unit: &Unit) -> Option<Attention> {
        let facts = self.units.get(unit)?;
        // Muted is the reader's standing "not this room", said in Slack and
        // honoured here. Nothing in a muted conversation is a card, however
        // unread it is.
        if self.muted.contains(&unit.channel) {
            return None;
        }
        // Nothing from anyone else since the reader last looked: whoever
        // else has written, they have read it.
        let newest = facts.newest_from_other.as_ref()?;
        match &unit.thread {
            Some(root) => {
                let key = self.key(&unit.channel, root);
                // Slack owns the follow list. A thread it stopped following
                // is not the reader's business, whatever rho once raised.
                if !self.followed.contains(&key) {
                    return None;
                }
                self.thread_read
                    .get(&key)
                    .is_none_or(|read| newest.is_newer_than(read))
                    .then_some(Attention::FollowedThread)
            }
            None => {
                if self
                    .conversation_read
                    .get(&unit.channel)
                    .is_some_and(|read| !newest.is_newer_than(read))
                {
                    return None;
                }
                match facts.reason {
                    Reason::DirectMessage => Some(Attention::DirectMessage),
                    Reason::Mention => Some(Attention::Mentioned),
                    // A thread reply that landed before the follow list
                    // did, so it is filed against the conversation. The feed
                    // only carries threads that are the reader's, so it asks
                    // for them as surely as one filed against its thread.
                    Reason::Thread => Some(Attention::FollowedThread),
                    Reason::Watched => self
                        .watched
                        .contains(&unit.channel)
                        .then_some(Attention::WatchedChannel),
                }
            }
        }
    }

    /// Every unit asking for the reader, longest wait first. The dealer's
    /// whole input: a unit missing from here is one Slack is not badging,
    /// and a unit in here carries the words for why.
    ///
    /// Costs the cards, not the units: the set is maintained on the events
    /// that change it, so a workspace with four thousand conversations and
    /// nine cards draws nine.
    pub fn cards(&self, now_ms: i64) -> Vec<UnitCard> {
        let mut cards = self
            .asking
            .keys()
            .filter_map(|unit| self.card(unit, now_ms))
            .collect::<Vec<_>>();
        cards.sort_by(|left, right| right.wait_days.total_cmp(&left.wait_days));
        cards
    }

    /// Works the rule out for one unit and records the answer. The only
    /// way into `asking`, so the kept set and the rule cannot drift.
    fn refresh_attention(&mut self, unit: &Unit) {
        match self.attention(unit) {
            Some(reason) => {
                self.asking.insert(unit.clone(), reason);
            }
            None => {
                self.asking.remove(unit);
            }
        }
    }

    /// The same, for every unit in one conversation: the conversation
    /// itself and the followed threads hanging in it, and nothing else.
    ///
    /// Units are keyed by (channel, thread), so this is a range scan over
    /// one channel's own entries — the cost of a mark is the threads in the
    /// channel that was marked, not the workspace.
    fn refresh_channel(&mut self, channel: &ChannelId) {
        let units = self.units_in(channel).cloned().collect::<Vec<_>>();
        for unit in units {
            self.refresh_attention(&unit);
        }
    }

    /// Every unit rho tracks in one conversation, in key order. The upper
    /// bound is the channel id with a null byte after it, which no channel
    /// id can be and every unit in this channel sorts before.
    fn units_in(&self, channel: &ChannelId) -> impl Iterator<Item = &Unit> {
        let start = Unit::conversation(channel);
        let end = Unit::conversation(&ChannelId(format!("{}\0", channel.as_str())));
        self.units.range(start..end).map(|(unit, _)| unit)
    }
}

/// The unit a followed thread's key names.
fn unit_of(key: &ThreadKey) -> Unit {
    Unit::thread(&key.channel, &key.thread_ts)
}

/// How long the unit has been waiting, counted from the newest message.
fn wait_days(facts: &UnitFacts, now_ms: i64) -> f64 {
    let since = now_ms.saturating_sub(facts.newest.millis().max(facts.first_seen_ms.min(now_ms)));
    (since as f64 / 86_400_000.0).max(0.0)
}

fn mentions_in(block: &Value, self_id: &UserId) -> bool {
    match block {
        Value::Object(map) => {
            let is_you = map.get("type").and_then(Value::as_str) == Some("user")
                && map.get("user_id").and_then(Value::as_str) == Some(self_id.0.as_str());
            // A channel-wide broadcast addresses the user as surely as their
            // own handle does; that is why `@here` earns a card.
            let is_broadcast = map.get("type").and_then(Value::as_str) == Some("broadcast");
            is_you || is_broadcast || map.values().any(|value| mentions_in(value, self_id))
        }
        Value::Array(values) => values.iter().any(|value| mentions_in(value, self_id)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cards a unit would be raised for: what the dealer used to ask
    /// the model for directly, now derived here because the store is what
    /// deals.
    fn owed(model: &Model, now_ms: i64) -> Vec<UnitCard> {
        model
            .cards(now_ms)
            .into_iter()
            .filter(|card| card.waiting == Waiting::OnYou)
            .collect()
    }

    /// The cutoff rule, which used to live in the GUI beside the desk's
    /// card ids: whether a `mark read before` reaches a unit is a question
    /// about the unit's newest message, so the crate answers it.
    #[test]
    fn a_cutoff_closes_what_is_older_than_it_and_nothing_it_has_never_seen() {
        let mut model = model();
        model.note_message(&message("C1", "100.0", "U1", "hey <@ME>"), 0);
        model.note_message(&message("C1", "900.0", "U1", "<@ME> again"), 0);
        let channel = Unit::conversation(&ChannelId("C1".into()));
        let never_seen = Unit::conversation(&ChannelId("C9".into()));
        assert_eq!(
            model.closed_by(&channel, 1_000.0),
            Some(Ts("900.0".into())),
            "a unit older than the cutoff is closed at its own newest message"
        );
        assert_eq!(
            model.closed_by(&channel, 500.0),
            None,
            "and one whose newest message is newer than the cutoff stays open"
        );
        assert_eq!(
            model.closed_by(&never_seen, 1_000.0),
            None,
            "a unit the model has nothing to say about is left alone"
        );
    }

    /// What the host needs to draw a card and what the model can say without
    /// the mirror: everything but the title, which needs the words the
    /// mirror holds and is filled by the session.
    #[test]
    fn a_card_carries_when_it_was_first_seen_and_leaves_the_title_to_the_session() {
        let mut model = model();
        model.note_message(&message("C1", "100.0", "U1", "hey <@ME>"), 7 * DAY);
        let unit = Unit::conversation(&ChannelId("C1".into()));
        let card = model.card(&unit, 7 * DAY).expect("a mention is a card");
        assert_eq!(card.first_seen_ms, 7 * DAY, "raised when it was first seen");
        assert_eq!(card.conversation, "#design");
        assert_eq!(card.attention, Some(Attention::Mentioned));
        assert!(
            card.title.is_empty(),
            "the model does not hold the mirror, so it does not invent the words"
        );
    }

    #[test]
    fn a_unit_raised_by_the_feed_learns_its_author_when_the_body_lands() {
        let mut model = model();
        let item = ActivityItem {
            channel: ChannelId("C1".into()),
            ts: Ts("100.0".into()),
            thread_ts: None,
            kind: ActivityKind::Mention,
            unread: true,
        };
        assert!(matches!(
            model.note_activity(&item, 0),
            Some(Change::Raised(_))
        ));
        let unit = Unit::conversation(&ChannelId("C1".into()));
        assert_eq!(model.unit(&unit).unwrap().newest, Ts("100.0".into()));

        // The feed says only that something landed. The body arriving is a
        // duplicate by timestamp, and what it settles is who wrote it.
        let message = crate::api::parse_message(
            &serde_json::json!({"ts": "100.0", "user": "ME", "text": "look at the deploy"}),
            &ChannelId("C1".into()),
        )
        .unwrap();
        assert_eq!(
            model.note_message(&message, 0),
            None,
            "the feed already counted this timestamp"
        );
        assert!(matches!(
            model.note_loaded(&message),
            Some(Change::Replied(_))
        ));
        assert_eq!(model.unit(&unit).unwrap().waiting(), Waiting::OnThem);
    }

    use serde_json::json;

    const DAY: i64 = 86_400_000;

    fn model() -> Model {
        let mut model = Model::new(WorkspaceName("acme".into()));
        model.set_self(UserId("ME".into()));
        model.add_users([
            User {
                id: UserId("ME".into()),
                name: "Manmeet".to_owned(),
                handle: "manmeet".to_owned(),
            },
            User {
                id: UserId("U1".into()),
                name: "ada".to_owned(),
                handle: "ada".to_owned(),
            },
        ]);
        model.add_conversations([
            Conversation {
                id: ChannelId("C1".into()),
                kind: ConversationKind::Channel,
                name: "design".to_owned(),
                user: None,
                members: Vec::new(),
            },
            Conversation {
                id: ChannelId("D1".into()),
                kind: ConversationKind::DirectMessage,
                name: "someone".to_owned(),
                user: Some(UserId("U1".into())),
                members: Vec::new(),
            },
        ]);
        model
    }

    fn message(channel: &str, ts: &str, user: &str, text: &str) -> Message {
        Message {
            ts: Ts(ts.into()),
            thread_ts: None,
            channel: ChannelId(channel.into()),
            user: Some(UserId(user.into())),
            bot_name: None,
            blocks: Vec::new(),
            text: text.into(),
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 0,
            latest_reply: None,
            edited: false,
            reactions: Vec::new(),
        }
    }

    fn reply(channel: &str, ts: &str, thread: &str, user: &str, text: &str) -> Message {
        Message {
            thread_ts: Some(Ts(thread.into())),
            ..message(channel, ts, user, text)
        }
    }

    /// The flood, and the fix. A channel nobody addressed the reader in is
    /// in the list with its count, where they can go and read it; it never
    /// asks. Opting into it is the reader's own word that it should, and
    /// opting back out takes only the asking away.
    #[test]
    fn a_channel_with_plain_unreads_is_in_the_list_and_never_a_card() {
        let mut model = model();
        let design = ChannelId("C1".into());
        assert_eq!(
            model.note_message(&message("C1", "100", "U1", "shipping today"), 0),
            None,
            "traffic in a channel is not a unit at all"
        );
        model.set_counts([ConversationCount {
            channel: design.clone(),
            has_unreads: true,
            mention_count: 0,
            unread_count: 1,
            latest: Some(Ts("100".into())),
            last_read: None,
        }]);
        let row = model
            .conversation_rows()
            .into_iter()
            .find(|row| row.id == design)
            .expect("the channel is in the list");
        assert!(row.unread, "with its count, which is where unreads belong");
        assert_eq!(row.unread_count, 1);
        assert!(!row.watched);
        assert!(model.cards(0).is_empty(), "and nothing is handed over");

        // The reader opts in. What already landed is theirs from now on.
        assert!(model.set_watching(&design, true));
        assert_eq!(
            model.note_message(&message("C1", "101", "U1", "and again"), 0),
            Some(Change::Raised(Unit::conversation(&design)))
        );
        let cards = model.cards(0);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].attention, Some(Attention::WatchedChannel));
        assert_eq!(
            reason_text(cards[0].attention.unwrap(), &cards[0].conversation),
            "unread in #design, watched here"
        );
        assert!(
            model
                .conversation_rows()
                .into_iter()
                .any(|row| row.id == design && row.watched),
            "and the opt-in reads on the line it was made on"
        );

        // Opting out stops the asking. The unit stays, so Find still
        // reaches it and a row the reader filed is not pulled away.
        assert!(model.set_watching(&design, false));
        assert!(model.cards(0).is_empty());
        assert!(model.unit(&Unit::conversation(&design)).is_some());
    }

    /// The other half of the flood: a mention the reader read on their
    /// phone this morning. Slack's cursor is past it, so Slack is not
    /// badging it, so neither is rho — whatever rho's own dealing cursor
    /// says, which is a different question about a different thing.
    #[test]
    fn a_mention_read_in_another_client_stops_asking() {
        let mut model = model();
        let design = ChannelId("C1".into());
        model.note_message(&message("C1", "100", "U1", "hey <@ME> look"), 0);
        assert_eq!(
            model.cards(0).first().and_then(|card| card.attention),
            Some(Attention::Mentioned)
        );
        assert_eq!(
            reason_text(Attention::Mentioned, "#design"),
            "mentioned in #design"
        );

        // The phone marked it read at the mention itself.
        model.mark_read(&design, &Ts("100".into()));
        assert!(model.cards(0).is_empty());

        // And a later mention asks again: the cursor is a place, not a
        // verdict on the channel.
        model.note_message(&message("C1", "200", "U1", "<@ME> still?"), 0);
        assert_eq!(
            model.cards(0).first().and_then(|card| card.attention),
            Some(Attention::Mentioned)
        );
    }

    /// A followed thread answers to its own cursor. Reading the channel
    /// around it says nothing about it, which is the whole reason Slack
    /// keeps the two apart.
    #[test]
    fn a_followed_thread_asks_until_its_own_cursor_passes_the_reply() {
        let mut model = model();
        let design = ChannelId("C1".into());
        model.follow(&design, &Ts("100".into()));
        model.note_message(&reply("C1", "150", "100", "U1", "one more thing"), 0);
        let key = model.key(&design, &Ts("100".into()));
        let unit = Unit::thread(&design, &Ts("100".into()));
        assert_eq!(model.attention(&unit), Some(Attention::FollowedThread));
        assert_eq!(
            reason_text(Attention::FollowedThread, "#design"),
            "a reply in a followed thread in #design"
        );

        model.mark_read(&design, &Ts("999".into()));
        assert_eq!(
            model.attention(&unit),
            Some(Attention::FollowedThread),
            "reading the channel is not reading the thread"
        );
        model.mark_thread_read(&key, &Ts("150".into()));
        assert_eq!(model.attention(&unit), None);
    }

    /// Muting is the reader saying "not this room", in Slack, from any
    /// client. Nothing in a muted conversation is handed over, however
    /// unread it is.
    #[test]
    fn a_muted_conversation_asks_for_nothing() {
        let mut model = model();
        model.note_message(&message("D1", "100", "U1", "lunch?"), 0);
        assert_eq!(
            model.cards(0).first().and_then(|card| card.attention),
            Some(Attention::DirectMessage)
        );
        assert_eq!(
            reason_text(Attention::DirectMessage, "@ada"),
            "unread in @ada"
        );
        model.set_muted([ChannelId("D1".into())]);
        assert!(model.cards(0).is_empty());
    }

    /// A channel the reader was named in and then opted into says the
    /// stronger of the two things: the mention is what they will want to
    /// answer, and it stands until the cursor passes it.
    #[test]
    fn a_mention_in_a_watched_channel_still_reads_as_a_mention() {
        let mut model = model();
        let design = ChannelId("C1".into());
        model.set_watching(&design, true);
        model.note_message(&message("C1", "100", "U1", "shipping today"), 0);
        assert_eq!(
            model.attention(&Unit::conversation(&design)),
            Some(Attention::WatchedChannel)
        );
        model.note_message(&message("C1", "200", "U1", "<@ME> can you look?"), 0);
        assert_eq!(
            model.attention(&Unit::conversation(&design)),
            Some(Attention::Mentioned)
        );
    }

    /// Replaying the log against the list a drawer already had must give
    /// the list the model holds now. This is the whole contract the
    /// incremental redraw rests on: if it can drift, the list on screen
    /// drifts from Slack and nothing ever corrects it.
    #[test]
    fn replaying_the_edits_rebuilds_the_list_the_model_holds() {
        let mut model = model();
        // The drawer starts caught up, holding the list as it stands.
        let mut held = model
            .conversation_rows()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        model.forget_row_edits();

        let replay = |model: &mut Model, held: &mut Vec<ChannelId>, at: &str| {
            let edits = model.take_row_edits().expect("the log was not dropped");
            for edit in edits {
                if edit.was_shown {
                    let from = held
                        .iter()
                        .position(|line| line == &edit.channel)
                        .unwrap_or_else(|| panic!("{at}: no line to take out"));
                    held.remove(from);
                }
                if edit.row.is_some() {
                    let to = match &edit.before {
                        Some(before) => held
                            .iter()
                            .position(|line| line == before)
                            .unwrap_or_else(|| panic!("{at}: no neighbour to sit above")),
                        None => held.len(),
                    };
                    held.insert(to, edit.channel.clone());
                }
            }
            assert_eq!(
                *held,
                model
                    .conversation_rows()
                    .into_iter()
                    .map(|row| row.id)
                    .collect::<Vec<_>>(),
                "{at}"
            );
        };

        model.note_counts(&message("C1", "100", "U1", "morning"));
        replay(&mut model, &mut held, "a message lands");
        model.note_counts(&message("D1", "110", "U1", "lunch?"));
        replay(&mut model, &mut held, "and one in a direct message");
        // A badge changing without the row moving: the line comes out and
        // goes back above the same neighbour, which the rope handles as a
        // rewrite of one line.
        model.note_counts(&message("D1", "111", "U1", "still lunch?"));
        replay(&mut model, &mut held, "the same conversation speaks again");
        model.mark_read(&ChannelId("D1".into()), &Ts("111".into()));
        replay(&mut model, &mut held, "it is read");
        model.set_muted([ChannelId("C1".into())]);
        replay(&mut model, &mut held, "a channel is muted to the bottom");
        model.set_muted([]);
        replay(&mut model, &mut held, "and comes back up");
        model.add_conversations([Conversation {
            id: ChannelId("C9".into()),
            kind: ConversationKind::Channel,
            name: "new".to_owned(),
            user: None,
            members: Vec::new(),
        }]);
        replay(&mut model, &mut held, "a conversation rho had not heard of");
        // The roster is the one event that moves every row, and it asks
        // for the list again rather than handing over an edit per row: n
        // edits cost more to replay than one draw, and the drawer would
        // reach that conclusion itself on reading them.
        model.add_users([User {
            id: UserId("U1".into()),
            name: "Zara".to_owned(),
            handle: "zara".to_owned(),
        }]);
        assert!(
            model.take_row_edits().is_none(),
            "the roster landing asks for the list again"
        );
    }

    /// The promise the reader is given: a word of a name reaches it, and a
    /// mid-word run does not. `des` reaches `#design`, `ops` reaches both
    /// `#dev-ops` and `#ops-alerts`, `sign` reaches nothing. This is
    /// Emacs completion's style for names, and it is the promise that
    /// makes a query a range scan rather than a walk.
    #[test]
    fn a_word_of_a_name_reaches_it_and_a_run_inside_one_does_not() {
        let mut model = model();
        model.add_conversations(
            ["dev-ops", "ops-alerts", "random"]
                .into_iter()
                .enumerate()
                .map(|(at, name)| Conversation {
                    id: ChannelId(format!("K{at}")),
                    kind: ConversationKind::Channel,
                    name: name.to_owned(),
                    user: None,
                    members: Vec::new(),
                }),
        );
        let reached = |model: &Model, query: &str| {
            model
                .reached_by(query, 50)
                .into_iter()
                .map(|row| row.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(reached(&model, "des"), vec!["#design".to_owned()]);
        assert_eq!(
            reached(&model, "ops"),
            vec!["#dev-ops".to_owned(), "#ops-alerts".to_owned()],
            "a word start anywhere in the name, not only the first"
        );
        assert!(
            reached(&model, "sign").is_empty(),
            "a run inside a word is not a word start, and is not promised"
        );
        assert_eq!(
            reached(&model, "#dev-ops"),
            vec!["#dev-ops".to_owned()],
            "a name typed or completed whole reaches itself: the query is              split exactly as a name is, so the sigil and the dash ask for              the words either side of them"
        );
        assert_eq!(
            reached(&model, "ops al"),
            vec!["#ops-alerts".to_owned()],
            "a second typed word intersects"
        );
    }

    /// A name breaks where a reader would break it: on the punctuation
    /// Slack's own names are full of, and at a hump in a name that has no
    /// punctuation at all.
    #[test]
    fn a_name_breaks_on_punctuation_and_on_a_hump() {
        assert_eq!(
            Model::word_starts("#design-ops"),
            vec!["design".to_owned(), "ops".to_owned()]
        );
        assert_eq!(
            Model::word_starts("@Ada Lovelace"),
            vec!["ada".to_owned(), "lovelace".to_owned()]
        );
        assert_eq!(
            Model::word_starts("mpdm-devOps"),
            vec!["dev".to_owned(), "mpdm".to_owned(), "ops".to_owned()]
        );
    }

    /// Narrowing is the list, so a conversation that arrives while a query
    /// stands is drawn only if it answers the query, and one that stops
    /// answering it goes. Otherwise a message would put a row on screen
    /// that the reader has just filtered away.
    #[test]
    fn traffic_while_narrowed_only_reaches_the_rows_the_query_holds() {
        let mut model = model();
        model.add_conversations([Conversation {
            id: ChannelId("C9".into()),
            kind: ConversationKind::Channel,
            name: "random".to_owned(),
            user: None,
            members: Vec::new(),
        }]);
        model.narrow("random");
        model.forget_row_edits();
        assert_eq!(
            model
                .conversation_rows()
                .into_iter()
                .map(|row| row.label)
                .collect::<Vec<_>>(),
            vec!["#random".to_owned()],
            "only what the query reaches is the list"
        );

        // Something lands in a conversation the query does not reach.
        model.note_counts(&message("C1", "500", "U1", "morning"));
        let edits = model.take_row_edits().expect("the log stands");
        assert!(
            edits.is_empty(),
            "a row the query does not reach changes no line, so there is \
             nothing to tell the drawer: {edits:?}"
        );
        assert_eq!(
            model
                .conversation_rows()
                .into_iter()
                .map(|row| row.label)
                .collect::<Vec<_>>(),
            vec!["#random".to_owned()],
            "and the narrowed list is what it was"
        );
    }

    /// An empty narrowing has two causes and the list must not read the
    /// same for both: a word nothing ever answered is the reader's own
    /// keystroke, and a list that emptied under them afterwards is Slack's
    /// doing. Only what the model remembers of the query's own history can
    /// tell them apart, and the reader typing is the only thing that
    /// forgets it.
    #[test]
    fn an_empty_narrowing_says_which_of_the_two_emptied_it() {
        let mut model = model();
        assert_eq!(model.empty_narrowing(), None, "no query stands");

        model.narrow("design");
        assert_eq!(
            model.empty_narrowing(),
            None,
            "a query with rows under it is not empty"
        );

        // The channel is renamed out from under the query.
        model.add_conversations([Conversation {
            id: ChannelId("C1".into()),
            kind: ConversationKind::Channel,
            name: "product".to_owned(),
            user: None,
            members: Vec::new(),
        }]);
        assert!(model.conversation_rows().is_empty(), "nothing left to draw");
        assert_eq!(
            model.empty_narrowing(),
            Some(Empty::Gone),
            "the reader did nothing to empty this and is owed the way out"
        );

        // A conversation answering the standing query arrives: the list is
        // a list again, and the reason it was empty is not raised.
        model.add_conversations([Conversation {
            id: ChannelId("C7".into()),
            kind: ConversationKind::Channel,
            name: "design-review".to_owned(),
            user: None,
            members: Vec::new(),
        }]);
        assert_eq!(model.empty_narrowing(), None, "rows again");

        model.narrow("zzz");
        assert_eq!(
            model.empty_narrowing(),
            Some(Empty::Never),
            "a fresh query that reaches nothing is the reader's own keystroke"
        );

        model.narrow("");
        assert_eq!(model.empty_narrowing(), None, "widening ends the question");
    }

    /// Widening by deleting a letter puts rows back, and the model says
    /// which ones rather than telling the drawer to start again.
    #[test]
    fn deleting_a_letter_puts_the_rows_it_widens_to_back() {
        let mut model = model();
        model.add_conversations([Conversation {
            id: ChannelId("K0".into()),
            kind: ConversationKind::Channel,
            name: "desks".to_owned(),
            user: None,
            members: Vec::new(),
        }]);
        model.narrow("desi");
        model.forget_row_edits();
        assert_eq!(model.conversation_rows().len(), 1, "#design alone");

        model.narrow("des");
        let edits = model
            .take_row_edits()
            .expect("widening is a diff, not a redraw");
        let arriving = edits.iter().filter(|edit| edit.row.is_some()).count();
        assert_eq!(arriving, 1, "#desks came back and #design did not move");
        assert_eq!(model.conversation_rows().len(), 2);
    }

    /// A log nobody drains is dropped rather than grown without bound, and
    /// the drawer is told to start again instead of being handed a log that
    /// no longer begins where its list does.
    #[test]
    fn a_list_nobody_draws_gives_up_its_log_rather_than_growing() {
        let mut model = model();
        model.forget_row_edits();
        for at in 0..(ROW_EDIT_CAP + 10) {
            model.note_counts(&message("C1", &format!("{at}"), "U1", "traffic"));
        }
        assert!(
            model.take_row_edits().is_none(),
            "the drawer is told to rebuild"
        );
        assert!(
            model.take_row_edits().is_some_and(|edits| edits.is_empty()),
            "and is caught up again once it has"
        );
    }

    /// The kept order and a sort must say the same thing after anything
    /// that can move a row. The list is the way in to Slack; a row in the
    /// wrong place is a conversation the reader cannot find, and an index
    /// that drifts is worse than a sort because nothing recomputes it.
    #[test]
    fn the_kept_order_and_a_sort_never_drift() {
        /// What the list used to do on every draw, kept here as the thing
        /// the index has to agree with.
        fn sorted(model: &Model) -> Vec<ChannelId> {
            let mut rows = model.conversation_rows();
            rows.sort_by(|left, right| {
                left.muted
                    .cmp(&right.muted)
                    .then_with(|| right.unread.cmp(&left.unread))
                    .then_with(|| right.mention_count.cmp(&left.mention_count))
                    .then_with(|| {
                        let latest = |row: &ConversationRow| {
                            row.latest
                                .as_ref()
                                .map(Ts::epoch_seconds)
                                .unwrap_or_default()
                        };
                        latest(right).total_cmp(&latest(left))
                    })
                    .then_with(|| left.label.cmp(&right.label))
            });
            rows.into_iter().map(|row| row.id).collect()
        }
        let mut model = model();
        let agrees = |model: &Model, at: &str| {
            assert_eq!(
                model
                    .conversation_rows()
                    .into_iter()
                    .map(|row| row.id)
                    .collect::<Vec<_>>(),
                sorted(model),
                "{at}"
            );
        };

        agrees(&model, "the roster has just landed");
        model.note_counts(&message("C1", "100", "U1", "morning"));
        agrees(&model, "a message lands in a channel");
        model.note_counts(&message("D1", "110", "U1", "lunch?"));
        agrees(&model, "and one in a direct message");
        model.note_counts(&message("C1", "120", "U1", "hey <@ME>"));
        agrees(&model, "and a mention, which is louder");
        model.mark_read(&ChannelId("C1".into()), &Ts("120".into()));
        agrees(&model, "the channel is read");
        model.set_muted([ChannelId("D1".into())]);
        agrees(&model, "the direct message is muted");
        model.set_muted([]);
        agrees(&model, "and unmuted");
        model.set_watching(&ChannelId("C1".into()), true);
        agrees(&model, "a channel is opted into");
        model.add_users([User {
            id: UserId("U1".into()),
            name: "Zara".to_owned(),
            handle: "zara".to_owned(),
        }]);
        agrees(&model, "and the roster renames the person the DM is with");
        model.set_counts([ConversationCount {
            channel: ChannelId("D1".into()),
            has_unreads: true,
            mention_count: 3,
            unread_count: 9,
            latest: Some(Ts("900".into())),
            last_read: None,
        }]);
        agrees(&model, "and Slack's own counts arrive");
    }

    /// The kept set and the rule must say the same thing after anything
    /// that can move either. A card that is asked for and never kept is a
    /// card that never appears; one that is kept and no longer asked for is
    /// the flood coming back the other way.
    #[test]
    fn what_is_kept_and_what_the_rule_says_never_drift() {
        let mut model = model();
        let design = ChannelId("C1".into());
        let direct = ChannelId("D1".into());
        let agrees = |model: &Model, at: &str| {
            let kept = model.cards(0).into_iter().map(|card| card.unit);
            let asked = model
                .tracked()
                .into_iter()
                .filter(|unit| model.attention(unit).is_some());
            let mut kept = kept.collect::<Vec<_>>();
            let mut asked = asked.collect::<Vec<_>>();
            kept.sort();
            asked.sort();
            assert_eq!(kept, asked, "{at}");
        };

        model.note_message(&message("C1", "100", "U1", "hey <@ME>"), 0);
        agrees(&model, "a mention arrives");
        model.note_message(&message("D1", "110", "U1", "lunch?"), 0);
        agrees(&model, "a direct message arrives");
        model.follow(&design, &Ts("100".into()));
        model.note_message(&reply("C1", "120", "100", "U1", "and?"), 0);
        agrees(&model, "a followed thread gets a reply");
        model.mark_read(&design, &Ts("100".into()));
        agrees(&model, "the channel is read");
        model.mark_thread_read(&model.key(&design, &Ts("100".into())), &Ts("120".into()));
        agrees(&model, "the thread is read at its own cursor");
        model.set_watching(&design, true);
        model.note_message(&message("C1", "200", "U1", "unrelated"), 0);
        agrees(&model, "the channel is opted into");
        model.set_muted([design.clone()]);
        agrees(&model, "and then muted");
        model.set_muted([]);
        agrees(&model, "and unmuted somewhere else");
        model.set_watching(&design, false);
        agrees(&model, "and opted back out");
        model.set_counts([ConversationCount {
            channel: direct.clone(),
            has_unreads: false,
            mention_count: 0,
            unread_count: 0,
            latest: Some(Ts("110".into())),
            last_read: Some(Ts("110".into())),
        }]);
        agrees(
            &model,
            "and Slack says the direct message was read elsewhere",
        );
        model.unfollow(&design, &Ts("100".into()));
        agrees(&model, "and the thread is unfollowed");
    }

    #[test]
    fn only_mentions_dms_and_joined_threads_are_raised() {
        let mut model = model();
        // Ordinary channel traffic is read, never raised.
        assert_eq!(
            model.note_message(&message("C1", "100", "U1", "shipping today"), 0),
            None
        );
        // A mention is.
        assert_eq!(
            model.note_message(&message("C1", "101", "U1", "hey <@ME> look"), 0),
            Some(Change::Raised(Unit::conversation(&ChannelId("C1".into()))))
        );
        // A DM is.
        assert!(matches!(
            model.note_message(&message("D1", "102", "U1", "hello"), 0),
            Some(Change::Raised(_))
        ));
        // A reply in a thread the user never touched is not.
        assert_eq!(
            model.note_message(&reply("C1", "104", "103", "U1", "and another"), 0),
            None
        );
    }

    #[test]
    fn a_reply_in_a_thread_slack_follows_for_the_user_raises() {
        // The thread the user answered from their phone: Slack follows it,
        // rho has never seen a message in it, and the reply is still theirs.
        let mut followed = model();
        followed.set_followed([(ChannelId("C1".into()), Ts("500".into()), None)]);
        assert!(matches!(
            followed.note_message(&reply("C1", "501", "500", "U1", "any update?"), 0),
            Some(Change::Raised(_))
        ));

        // The same reply in a thread Slack does not follow is channel
        // traffic, and unfollowing puts a thread back in that state.
        let mut stranger = model();
        assert_eq!(
            stranger.note_message(&reply("C1", "601", "600", "U1", "any update?"), 0),
            None
        );
        stranger.follow(&ChannelId("C1".into()), &Ts("700".into()));
        stranger.unfollow(&ChannelId("C1".into()), &Ts("700".into()));
        assert_eq!(
            stranger.note_message(&reply("C1", "701", "700", "U1", "any update?"), 0),
            None
        );
    }

    #[test]
    fn channel_traffic_is_never_seen_so_the_feed_can_still_raise_it() {
        // The bug this guards: the live message was marked seen before its
        // reason was decided, so the feed's item for the same `ts` — the one
        // that knew the thread was the user's — was dropped as a duplicate.
        let mut poisoned = model();
        assert_eq!(
            poisoned.note_message(&reply("C1", "801", "800", "U1", "any update?"), 0),
            None
        );
        let item = ActivityItem {
            channel: ChannelId("C1".into()),
            ts: Ts("801".into()),
            thread_ts: Some(Ts("800".into())),
            kind: ActivityKind::ThreadReply,
            unread: true,
        };
        assert!(matches!(
            poisoned.note_activity(&item, 0),
            Some(Change::Raised(_))
        ));

        // And when the live reply did raise it, the feed item for the same
        // message is a no-op: one thread, raised once.
        let mut live_first = model();
        live_first.set_followed([(ChannelId("C1".into()), Ts("800".into()), None)]);
        assert!(matches!(
            live_first.note_message(&reply("C1", "801", "800", "U1", "any update?"), 0),
            Some(Change::Raised(_))
        ));
        assert_eq!(live_first.note_activity(&item, 0), None);
        assert_eq!(owed(&live_first, 0).len(), 1);
    }

    #[test]
    fn a_broadcast_earns_a_card_and_a_stranger_thread_does_not() {
        let mut model = model();
        let here = Message {
            blocks: vec![json!({
                "type": "rich_text",
                "elements": [{"type": "rich_text_section", "elements": [
                    {"type": "broadcast", "range": "here"},
                    {"type": "text", "text": " standup"},
                ]}],
            })],
            ..message("C1", "200", "U1", "")
        };
        assert!(matches!(
            model.note_message(&here, 0),
            Some(Change::Raised(_))
        ));

        let other = Message {
            blocks: vec![json!({
                "type": "rich_text",
                "elements": [{"type": "rich_text_section", "elements": [
                    {"type": "user", "user_id": "U9"},
                ]}],
            })],
            ..message("C1", "201", "U1", "")
        };
        assert_eq!(model.note_message(&other, 0), None);
    }

    #[test]
    fn the_feed_and_the_socket_never_raise_a_thread_twice() {
        let mut model = model();
        let item = ActivityItem {
            channel: ChannelId("C1".into()),
            ts: Ts("300".into()),
            thread_ts: None,
            kind: ActivityKind::Mention,
            unread: true,
        };
        assert!(matches!(
            model.note_activity(&item, 0),
            Some(Change::Raised(_))
        ));
        // The same event arriving over the websocket changes nothing.
        assert_eq!(
            model.note_message(&message("C1", "300", "U1", "hey <@ME>"), 0),
            None
        );
        // And a repeat poll of the same feed page changes nothing either.
        assert_eq!(model.note_activity(&item, 0), None);
        assert_eq!(owed(&model, 0).len(), 1);
    }

    #[test]
    fn your_reply_relabels_the_card_and_a_later_answer_re_raises() {
        let mut model = model();
        let now = 10 * DAY;
        assert!(matches!(
            model.note_message(&message("D1", "400", "U1", "any update?"), now),
            Some(Change::Raised(_))
        ));
        let key = Unit::conversation(&ChannelId("D1".into()));
        assert_eq!(model.unit(&key).unwrap().waiting(), Waiting::OnYou);
        assert_eq!(owed(&model, now).len(), 1);

        // Answering is not a verdict: the thread is still tracked, the ball
        // is theirs, and the card the dealer builds says so.
        assert_eq!(
            model.note_message(&reply("D1", "401", "400", "ME", "tomorrow"), now),
            Some(Change::Replied(key.clone()))
        );
        assert_eq!(model.unit(&key).unwrap().waiting(), Waiting::OnThem);
        assert_eq!(model.card(&key, now).unwrap().waiting, Waiting::OnThem);

        // Their answer brings it back, keyed on the newer message.
        assert_eq!(
            model.note_message(&reply("D1", "402", "400", "U1", "thanks!"), now),
            Some(Change::Raised(key.clone()))
        );
        let card = model.card(&key, now).unwrap();
        assert_eq!(card.waiting, Waiting::OnYou);
        assert_eq!(card.newest, Ts("402".into()));
        assert_eq!(card.conversation, "@ada");
    }

    /// One card per unit. Three mentions in `#design` are one row on the
    /// desk, not three, and five messages in a direct message are one card
    /// that says how long it has been waiting.
    #[test]
    fn a_channel_of_mentions_and_a_busy_dm_are_each_one_card() {
        let mut model = model();
        for ts in ["100", "101", "102"] {
            model.note_message(&message("C1", ts, "U1", "hey <@ME> look"), 0);
        }
        for ts in ["200", "201", "202", "203", "204"] {
            model.note_message(&message("D1", ts, "U1", "ping"), 0);
        }
        assert_eq!(
            model.tracked(),
            vec![
                Unit::conversation(&ChannelId("C1".into())),
                Unit::conversation(&ChannelId("D1".into())),
            ],
            "eight messages, two units"
        );
        assert_eq!(owed(&model, 0).len(), 2);
        // The card stands for the whole unit, so what it reports is where
        // the unit is, not where the message that made it was.
        let channel = model
            .card(&Unit::conversation(&ChannelId("C1".into())), 0)
            .unwrap();
        assert_eq!(channel.newest, Ts("102".into()));
        assert_eq!(channel.newest_from_other, Some(Ts("102".into())));
    }

    /// Every source is evidence about the same unit, and none of them may
    /// take back what another said: the live socket, a feed poll, a history
    /// page and the roster read at startup all only raise the facts.
    #[test]
    fn no_source_can_lower_what_the_mirror_has_already_said() {
        let mut model = model();
        model.note_message(&message("D1", "500", "U1", "the newest"), 0);

        // A history page loading under the card.
        assert_eq!(
            model.note_message(&message("D1", "300", "U1", "older"), 0),
            None
        );
        // A feed poll repeating an older item after a reconnect.
        let stale = ActivityItem {
            channel: ChannelId("D1".into()),
            ts: Ts("400".into()),
            thread_ts: None,
            kind: ActivityKind::DirectMessage,
            unread: true,
        };
        assert_eq!(model.note_activity(&stale, 0), None);
        // And what Slack says is unread when rho starts again.
        model.set_counts([ConversationCount {
            channel: ChannelId("D1".into()),
            has_unreads: true,
            mention_count: 0,
            unread_count: 0,
            latest: Some(Ts("450".into())),
            last_read: None,
        }]);
        assert!(model.unread_dms(0).is_empty());

        let facts = model
            .unit(&Unit::conversation(&ChannelId("D1".into())))
            .unwrap();
        assert_eq!(facts.newest, Ts("500".into()));
        assert_eq!(facts.newest_from_other, Some(Ts("500".into())));
    }

    /// Answering flips the word on the card and the curve it takes. It is
    /// not a verdict: what closes a unit is the cursor, and a reply leaves
    /// the newest message from someone else exactly where it was.
    #[test]
    fn your_reply_flips_the_word_and_not_what_closes_the_card() {
        let mut model = model();
        model.note_message(&message("D1", "600", "U1", "any update?"), 0);
        let unit = Unit::conversation(&ChannelId("D1".into()));
        assert_eq!(model.unit(&unit).unwrap().waiting(), Waiting::OnYou);

        model.note_message(&message("D1", "601", "ME", "tomorrow"), 0);
        let facts = model.unit(&unit).unwrap();
        assert_eq!(facts.waiting(), Waiting::OnThem);
        assert_eq!(facts.newest, Ts("601".into()));
        assert_eq!(
            facts.newest_from_other,
            Some(Ts("600".into())),
            "the card is still open on their message until the user closes it"
        );
    }

    #[test]
    fn wait_days_count_from_the_newest_message() {
        let mut model = model();
        let sent_ms = 100 * DAY;
        model.note_message(
            &message("D1", &format!("{}", sent_ms / 1000), "U1", "ping"),
            sent_ms,
        );
        let now = sent_ms + 2 * DAY;
        let card = &owed(&model, now)[0];
        assert!((card.wait_days - 2.0).abs() < 0.01, "{}", card.wait_days);
        assert_eq!(card.waiting, Waiting::OnYou);
    }

    #[test]
    fn an_out_of_order_page_cannot_walk_a_thread_backwards() {
        let mut model = model();
        model.note_message(&message("D1", "500", "U1", "first"), 0);
        model.note_message(&reply("D1", "502", "500", "ME", "answered"), 0);
        // A feed poll arriving late with the older reply must not undo the
        // done verdict the newer one recorded.
        let stale = ActivityItem {
            channel: ChannelId("D1".into()),
            ts: Ts("501".into()),
            thread_ts: Some(Ts("500".into())),
            kind: ActivityKind::ThreadReply,
            unread: true,
        };
        assert_eq!(model.note_activity(&stale, 0), None);
        let key = Unit::conversation(&ChannelId("D1".into()));
        assert_eq!(model.unit(&key).unwrap().waiting(), Waiting::OnThem);
    }

    /// Reading is not a verdict, and this is where the two stop being
    /// confused. A verdict — done, skip, defer — is the reader's own key
    /// and only theirs, and the desk keeps those cursors untouched. Reading
    /// is a fact about a message, Slack records it from whichever client
    /// did it, and a message everyone can see the reader has read is not
    /// something to go on handing them.
    ///
    /// The unit stays: it is still a conversation, Find still reaches it,
    /// and the facts on it are unchanged. What goes is the asking.
    #[test]
    fn reading_elsewhere_clears_the_badge_and_stops_the_asking() {
        let mut model = model();
        model.note_message(&message("D1", "600", "U1", "ping"), 0);
        let key = Unit::conversation(&ChannelId("D1".into()));
        assert_eq!(owed(&model, 0).len(), 1);

        model.mark_read(&ChannelId("D1".into()), &Ts("600".into()));
        assert_eq!(
            model.unit(&key).unwrap().waiting(),
            Waiting::OnYou,
            "whose turn it is has not changed; nobody answered"
        );
        assert!(model.attention(&key).is_none());
        assert!(owed(&model, 0).is_empty());

        // And the next thing they say asks again.
        model.note_message(&message("D1", "700", "U1", "still there?"), 0);
        assert_eq!(owed(&model, 0).len(), 1);
    }

    #[test]
    fn a_group_dm_reads_as_the_people_in_it() {
        let mut model = model();
        model.add_users([User {
            id: UserId("UK".into()),
            name: "Keith".to_owned(),
            handle: "keith".to_owned(),
        }]);
        model.add_conversations([
            Conversation {
                id: ChannelId("G1".into()),
                kind: ConversationKind::Group,
                name: "mpdm-manmeet--ada--keith-1".to_owned(),
                user: None,
                members: vec![
                    UserId("ME".into()),
                    UserId("U1".into()),
                    UserId("UK".into()),
                ],
            },
            // The same group as `users.conversations` sends it: a machine
            // name and nothing else.
            Conversation {
                id: ChannelId("G2".into()),
                kind: ConversationKind::Group,
                name: "mpdm-manmeet--ada--keith-1".to_owned(),
                user: None,
                members: Vec::new(),
            },
        ]);

        assert_eq!(model.label(&ChannelId("G1".into())), "ada, Keith");
        assert_eq!(
            model.label(&ChannelId("G2".into())),
            "ada, Keith",
            "the handles in the machine name still name people"
        );
        assert!(
            !model.label(&ChannelId("G2".into())).contains("mpdm"),
            "a machine name never reaches the user"
        );
    }

    #[test]
    fn a_conversation_that_arrives_before_the_roster_is_named_once_it_lands() {
        let mut model = Model::new(WorkspaceName("acme".into()));
        model.set_self(UserId("ME".into()));
        model.add_conversations([Conversation {
            id: ChannelId("D1".into()),
            kind: ConversationKind::DirectMessage,
            name: "someone".to_owned(),
            user: Some(UserId("U1".into())),
            members: Vec::new(),
        }]);
        assert_eq!(model.label(&ChannelId("D1".into())), "@someone");

        model.add_users([User {
            id: UserId("U1".into()),
            name: "Ada Lovelace".to_owned(),
            handle: "ada".to_owned(),
        }]);
        assert_eq!(model.label(&ChannelId("D1".into())), "@Ada Lovelace");
    }

    #[test]
    fn the_conversation_list_puts_unread_first_then_recency() {
        let mut model = model();
        model.add_conversations([Conversation {
            id: ChannelId("C2".into()),
            kind: ConversationKind::Channel,
            name: "quiet".to_owned(),
            user: None,
            members: Vec::new(),
        }]);
        model.set_counts([
            ConversationCount {
                channel: ChannelId("C1".into()),
                has_unreads: true,
                mention_count: 0,
                unread_count: 0,
                latest: Some(Ts("10".into())),
                last_read: None,
            },
            ConversationCount {
                channel: ChannelId("D1".into()),
                has_unreads: true,
                mention_count: 3,
                unread_count: 0,
                latest: Some(Ts("5".into())),
                last_read: None,
            },
            ConversationCount {
                channel: ChannelId("C2".into()),
                has_unreads: false,
                mention_count: 0,
                unread_count: 0,
                latest: Some(Ts("99".into())),
                last_read: None,
            },
        ]);
        let rows = model.conversation_rows();
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["@ada", "#design", "#quiet"],
            "mentions first, then other unread, then recency"
        );
        assert_eq!(rows[0].mention_count, 3);
        assert!(!rows[2].unread);
    }

    #[test]
    fn a_dm_takes_its_name_from_the_roster() {
        let model = model();
        assert_eq!(model.label(&ChannelId("D1".into())), "@ada");
        assert_eq!(model.label(&ChannelId("C1".into())), "#design");
        assert_eq!(
            model.label(&ChannelId("C404".into())),
            "#a conversation",
            "an unknown channel never reads as its id"
        );
    }

    #[test]
    fn the_list_counters_move_on_every_frame_not_only_the_raised_ones() {
        let mut model = model();
        let row = |model: &Model, channel: &str| {
            model
                .conversation_rows()
                .into_iter()
                .find(|row| row.id == ChannelId(channel.into()))
                .expect("the conversation is listed")
        };
        // Channel traffic raises nothing and still has to badge the list.
        model.note_counts(&message("C1", "100", "U1", "shipping today"));
        let listed = row(&model, "C1");
        assert!(listed.unread);
        assert_eq!(listed.mention_count, 0);
        assert_eq!(listed.latest, Some(Ts("100".into())));

        model.note_counts(&message("C1", "101", "U1", "hey <@ME> look"));
        assert_eq!(row(&model, "C1").mention_count, 1);
        // Two messages watched land, so the list can now say how many.
        assert_eq!(row(&model, "C1").unread_count, 2);
        // A DM counts as a mention the same way Slack counts it.
        model.note_counts(&message("D1", "102", "U1", "ping"));
        assert_eq!(row(&model, "D1").mention_count, 1);

        // Answering from any client is reading: the badge goes out here
        // rather than waiting for the read marker to come back round.
        model.note_counts(&message("C1", "103", "ME", "on it"));
        let listed = row(&model, "C1");
        assert!(!listed.unread);
        assert_eq!(listed.mention_count, 0);
        assert_eq!(listed.latest, Some(Ts("103".into())));
    }

    #[test]
    fn a_group_dm_is_a_dm_and_a_dm_unread_at_startup_is_a_card() {
        let mut model = model();
        model.add_conversations([Conversation {
            id: ChannelId("G1".into()),
            kind: ConversationKind::Group,
            name: "mpdm-ada--keith-1".to_owned(),
            user: None,
            members: vec![UserId("U1".into()), UserId("ME".into())],
        }]);
        // A group DM is a room the user was put in by name: a message in it
        // is addressed to them, the way a one-to-one is.
        let key = Unit::conversation(&ChannelId("G1".into()));
        assert_eq!(
            model.note_message(&message("G1", "200", "U1", "are you both free?"), 0),
            Some(Change::Raised(key.clone()))
        );
        assert_eq!(model.unit(&key).unwrap().reason, Reason::DirectMessage);

        // What Slack says is unread when rho starts: a DM that arrived while
        // it was off, which the feed never carries.
        model.set_counts([
            ConversationCount {
                channel: ChannelId("D1".into()),
                has_unreads: true,
                mention_count: 1,
                unread_count: 0,
                latest: Some(Ts("300".into())),
                last_read: None,
            },
            // A channel with unreads is backlog, not an obligation.
            ConversationCount {
                channel: ChannelId("C1".into()),
                has_unreads: true,
                mention_count: 0,
                unread_count: 0,
                latest: Some(Ts("301".into())),
                last_read: None,
            },
        ]);
        let raised = model.unread_dms(0);
        assert_eq!(
            raised,
            vec![Change::Raised(Unit::conversation(&ChannelId("D1".into())))],
            "the DM is raised and the channel is not"
        );
        assert!(
            model.unread_dms(0).is_empty(),
            "a second roster fetch raises nothing again"
        );
    }

    #[test]
    fn the_next_unread_conversation_wraps_and_never_lands_where_it_started() {
        let mut model = model();
        let count = |channel: &str, unread: bool| ConversationCount {
            channel: ChannelId(channel.into()),
            has_unreads: unread,
            mention_count: 0,
            unread_count: 0,
            latest: Some(Ts("100".into())),
            last_read: None,
        };
        model.set_counts([count("C1", true), count("D1", true)]);
        let order = model
            .conversation_rows()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        let (first, second) = (order[0].clone(), order[1].clone());
        assert_eq!(model.next_unread(None), NextUnread::Go(first.clone()));
        assert_eq!(
            model.next_unread(Some(&first)),
            NextUnread::Go(second.clone())
        );
        // Round again: one key, pressed until there is nothing left.
        assert_eq!(
            model.next_unread(Some(&second)),
            NextUnread::Go(first.clone())
        );

        // The one the reader is in does not count, however unread Slack
        // still thinks it is: they are looking at it.
        model.set_counts([count("C1", false), count("D1", true)]);
        assert_eq!(
            model.next_unread(Some(&ChannelId("D1".into()))),
            NextUnread::Nothing
        );
    }

    /// `shift-n` walks the list the reader is looking at, which is the
    /// narrowed one while a query stands.
    ///
    /// This has been decided both ways and the history is worth keeping.
    /// It first walked the narrowed rows; that was changed because a query
    /// left standing answered "nothing unread" while a DM outside it
    /// waited, and the key that exists to find unread messages was the one
    /// thing that could not see them. What settles it is the rule that a
    /// narrowing is a motion and not a setting: it lasts while the reader
    /// is in it and does not survive a restart, so there is no hour-old
    /// query to be trapped by, and a key that moved them somewhere the
    /// rows cannot show -- under a banner still describing the rows --
    /// would leave the list and the key disagreeing about where they are.
    ///
    /// What remains true from the other direction is asserted below: the
    /// backlog is still counted over the whole workspace, so nothing
    /// waiting is ever hidden from the reader, only from this one key
    /// while they are narrowed.
    #[test]
    fn the_next_unread_key_stays_inside_the_list_the_reader_is_looking_at() {
        let mut model = model();
        let count = |channel: &str| ConversationCount {
            channel: ChannelId(channel.into()),
            has_unreads: true,
            mention_count: 0,
            unread_count: 0,
            latest: Some(Ts("100".into())),
            last_read: None,
        };
        // Unread in the DM, which is the one the query will not reach, and
        // in the channel it will.
        model.set_counts([count("D1"), count("C1")]);
        model.narrow("design");
        assert_eq!(
            model
                .conversation_rows()
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>(),
            vec![ChannelId("C1".into())],
            "the list is narrowed to #design, which is what the reader sees"
        );
        assert_eq!(
            model.next_unread(None),
            NextUnread::Go(ChannelId("C1".into())),
            "the key goes to the unread one on screen, not the one off it"
        );

        // Nothing unread left inside the narrowing: the key stops rather
        // than jumping to a conversation the rows cannot show.
        model.set_counts([
            count("D1"),
            ConversationCount {
                has_unreads: false,
                ..count("C1")
            },
        ]);
        assert_eq!(
            model.next_unread(None),
            NextUnread::Outside(1),
            "at the edge it stops, and says how much waits outside rather \
             than jumping there or claiming there is nothing"
        );
        assert_eq!(
            model.mark_plan(f64::MAX).conversations.len(),
            1,
            "the backlog is still the workspace's, so nothing is hidden \
             from the reader -- only from this key while they are narrowed"
        );

        // Out of the narrowing, the key sees the whole list again.
        model.narrow("");
        assert_eq!(
            model.next_unread(None),
            NextUnread::Go(ChannelId("D1".into()))
        );

        // Read everything, narrowed: nothing outside either, so the key
        // says nothing waits rather than counting zero.
        model.narrow("design");
        model.set_counts([
            ConversationCount {
                has_unreads: false,
                ..count("D1")
            },
            ConversationCount {
                has_unreads: false,
                ..count("C1")
            },
        ]);
        assert_eq!(model.next_unread(None), NextUnread::Nothing);
    }

    #[test]
    fn a_muted_conversation_sinks_and_never_pulls_the_reader() {
        let mut model = model();
        model.set_counts([ConversationCount {
            channel: ChannelId("C1".into()),
            has_unreads: true,
            mention_count: 0,
            unread_count: 4,
            latest: Some(Ts("300".into())),
            last_read: None,
        }]);
        assert_eq!(model.conversation_rows()[0].id, ChannelId("C1".into()));

        // Muted in another client: it keeps its count, loses its place, and
        // stops being somewhere `shift-n` will take the reader.
        model.set_muted([ChannelId("C1".into())]);
        let rows = model.conversation_rows();
        assert_eq!(
            rows.last().map(|row| &row.id),
            Some(&ChannelId("C1".into()))
        );
        assert!(
            rows.last()
                .is_some_and(|row| row.muted && row.unread_count == 4)
        );
        assert_eq!(model.next_unread(None), NextUnread::Nothing);

        // Unmuted again, and it comes straight back up.
        model.set_muted([]);
        assert_eq!(model.conversation_rows()[0].id, ChannelId("C1".into()));
    }

    #[test]
    fn the_composer_completes_people_channels_and_emoji() {
        let model = model();
        let design = ChannelId("C1".into());
        let values = |sigil, needle: &str| {
            model
                .suggestions(&design, sigil, needle)
                .into_iter()
                .map(|found| found.value)
                .collect::<Vec<_>>()
        };
        // The reader is never a mention of themselves, and the two channel
        // -wide mentions are offered beside the people.
        assert_eq!(values('@', ""), vec!["@ada", "@channel", "@here"]);
        assert_eq!(values('@', "ad"), vec!["@ada"]);
        assert_eq!(values('@', "her"), vec!["@here"]);
        assert!(values('@', "zzz").is_empty());
        assert_eq!(values('#', "des"), vec!["#design"]);

        let tada = model.suggestions(&design, ':', "tada");
        assert_eq!(tada[0].value, ":tada:");
        assert_eq!(tada[0].detail, "🎉", "the glyph is what says which one");
        // A workspace's own emoji has no glyph anywhere else, and is offered
        // the same as any other.
        let mut model = model;
        model.set_custom_emoji(["forrest_gump_wave".to_owned()]);
        assert_eq!(
            model.suggestions(&design, ':', "forrest")[0].value,
            ":forrest_gump_wave:"
        );
    }

    #[test]
    fn a_sent_mention_goes_out_in_the_form_slack_counts() {
        let model = model();
        assert_eq!(
            model.encode("morning @ada, see #design"),
            "morning <@U1>, see <#C1|design>"
        );
        // Nobody by that name, and a sigil inside a word: both are prose.
        assert_eq!(
            model.encode("mail me@example.com about @nobody"),
            "mail me@example.com about @nobody"
        );
        assert_eq!(model.encode(":tada: ships"), ":tada: ships");
    }

    /// A broadcast is not a user and is in no member list, so the user table
    /// can never answer for it. rho has always read one coming in — that is
    /// what earns a card from an `@here` somebody else sent — and without
    /// this it could read one and not send one: `@here` went out as the four
    /// characters, reaching nobody at all.
    #[test]
    fn the_two_channel_wide_mentions_go_out_as_broadcasts() {
        let model = model();
        assert_eq!(
            model.encode("@here standup in five"),
            "<!here> standup in five"
        );
        assert_eq!(model.encode("ship it @channel"), "ship it <!channel>");
        // The same rules as any other mention: one starting a word, and the
        // word is the whole of it.
        assert_eq!(model.encode("over@here.example"), "over@here.example");
        assert_eq!(model.encode("@herero"), "@herero");
    }

    /// Opening an edit puts what was sent in the composer, and what was sent
    /// is the wire form. Without this the reader rewrites `<@U1> can you
    /// look?` — and if they retyped it as the name they see drawn, `@Ada
    /// Lovelace`, `encode` would stop at the space, find nobody, and send it
    /// as prose.
    #[test]
    fn opening_an_edit_gives_back_the_words_that_were_typed() {
        let model = model();
        assert_eq!(
            model.decode("<@U1> see <#C1|design> and <!here>"),
            "@ada see #design and @here"
        );
        // Slack writes a channel both ways for the same channel, and both
        // come back as the name it has now.
        assert_eq!(model.decode("<#C1>"), "#design");
        assert_eq!(model.decode("<#C1|what-it-was-called-then>"), "#design");
    }

    /// An escape with no typed form is left exactly as the wire wrote it. A
    /// rewrite is not a chance to lose the link in a message: rendering
    /// `<https://x.example|docs>` down to `docs` would send it back with the
    /// address gone, and there is no typed form that puts one back.
    #[test]
    fn an_escape_the_reader_could_not_have_typed_survives_the_rewrite() {
        let model = model();
        for wire in [
            "<https://x.example|docs>",
            "<https://x.example>",
            "<!subteam^S1|@team>",
            "<@U404>",
            "<#C404>",
            "<>",
            "a < b and c > d",
            "unclosed <@U1 stays",
        ] {
            assert_eq!(model.decode(wire), wire, "left as the wire wrote it");
            assert_eq!(
                model.encode(wire),
                wire,
                "and encode does not touch it either, so the rewrite is lossless"
            );
        }
    }

    /// The round trip, both ways, on the forms each side produces.
    #[test]
    fn what_the_reader_types_and_what_goes_on_the_wire_are_inverses() {
        let model = model();
        let typed = "morning @ada, see #design, @here, @channel";
        assert_eq!(model.decode(&model.encode(typed)), typed);
        let wire = "<@U1> <#C1|design> <!here> <!channel> <https://x.example|docs>";
        assert_eq!(model.encode(&model.decode(wire)), wire);
    }
}
