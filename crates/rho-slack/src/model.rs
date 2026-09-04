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
    /// `#design` or `@ada`.
    pub conversation: String,
    pub waiting: Waiting,
    pub wait_days: f64,
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

/// Whether a character can be part of a handle or a channel name, which is
/// what bounds a mention in typed text.
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
    pub latest: Option<Ts>,
}

pub struct Model {
    workspace: WorkspaceName,
    self_id: UserId,
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

impl Model {
    pub fn new(workspace: WorkspaceName) -> Self {
        Self {
            workspace,
            self_id: UserId(String::new()),
            users: BTreeMap::new(),
            conversations: BTreeMap::new(),
            counts: BTreeMap::new(),
            custom_emoji: BTreeSet::new(),
            units: BTreeMap::new(),
            seen: BTreeSet::new(),
            followed: BTreeSet::new(),
            muted: BTreeSet::new(),
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

    pub fn add_users(&mut self, users: impl IntoIterator<Item = User>) {
        for user in users {
            self.users.insert(user.id.clone(), user);
        }
    }

    /// Registers conversations. Names are not stored: a label is built from
    /// the roster every time it is read, so a conversation that arrives
    /// before the roster is still named once the roster lands.
    pub fn add_conversations(&mut self, conversations: impl IntoIterator<Item = Conversation>) {
        for conversation in conversations {
            self.conversations
                .insert(conversation.id.clone(), conversation);
        }
    }

    pub fn conversation(&self, channel: &ChannelId) -> Option<&Conversation> {
        self.conversations.get(channel)
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

    pub fn set_counts(&mut self, counts: impl IntoIterator<Item = ConversationCount>) {
        for count in counts {
            self.counts.insert(count.channel.clone(), count);
        }
    }

    /// Slack's muted list, replacing whatever rho held: unmuting elsewhere
    /// has to bring a conversation back up out of the muted section.
    pub fn set_muted(&mut self, muted: impl IntoIterator<Item = ChannelId>) {
        self.muted = muted.into_iter().collect();
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
        if from_you {
            count.has_unreads = false;
            count.mention_count = 0;
            count.unread_count = 0;
            return;
        }
        count.has_unreads = true;
        count.unread_count += 1;
        if pings_you {
            count.mention_count += 1;
        }
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
        let mut rows = self
            .conversations
            .values()
            .map(|conversation| {
                let count = self.counts.get(&conversation.id);
                ConversationRow {
                    id: conversation.id.clone(),
                    label: self.label(&conversation.id),
                    unread: count.is_some_and(|count| count.has_unreads),
                    mention_count: count.map_or(0, |count| count.mention_count),
                    unread_count: count.map_or(0, |count| count.unread_count),
                    muted: self.muted.contains(&conversation.id),
                    latest: count.and_then(|count| count.latest.clone()),
                }
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            // Muted goes to the bottom whatever it holds: the whole point of
            // muting is that its traffic stops competing for the top.
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
        rows
    }

    /// What the composer offers for the token being typed. `@` is the
    /// people the reader could be talking to, `#` the channels, `:` the
    /// emoji, custom ones included. The value is what the editor shows and
    /// what a yank of the line carries; `encode` turns it into the wire
    /// form on the way out.
    pub fn suggestions(&self, channel: &ChannelId, sigil: char, needle: &str) -> Vec<Suggestion> {
        let needle = needle.to_lowercase();
        let mut found = match sigil {
            '@' => self
                .members_of(channel)
                .into_iter()
                .filter(|user| user.id != self.self_id)
                .map(|user| Suggestion {
                    value: format!("@{}", user.handle),
                    detail: user.name.clone(),
                })
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

    fn wire_form(&self, sigil: char, name: &str) -> Option<String> {
        match sigil {
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
    pub fn next_unread(&self, from: Option<&ChannelId>) -> Option<ChannelId> {
        let rows = self.conversation_rows();
        let unread = |row: &ConversationRow| !row.muted && (row.unread || row.mention_count > 0);
        let at = from
            .and_then(|from| rows.iter().position(|row| &row.id == from))
            .map_or(0, |at| at + 1);
        rows.iter()
            .skip(at)
            .chain(rows.iter().take(at))
            .find(|row| unread(row) && Some(&row.id) != from)
            .map(|row| row.id.clone())
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
    /// Returns the tracked threads the list no longer names: unfollowed in
    /// another client, possibly while rho was away. Their cards are Slack's
    /// to discard.
    pub fn set_followed(
        &mut self,
        threads: impl IntoIterator<Item = (ChannelId, Ts)>,
    ) -> Vec<ThreadKey> {
        let now = threads
            .into_iter()
            .map(|(channel, thread_ts)| self.key(&channel, &thread_ts))
            .collect::<BTreeSet<_>>();
        let dropped = self
            .followed
            .difference(&now)
            .filter(|key| self.units.contains_key(&unit_of(key)))
            .cloned()
            .collect::<Vec<_>>();
        self.followed = now;
        for key in &dropped {
            self.units.remove(&unit_of(key));
        }
        dropped
    }

    pub fn follow(&mut self, channel: &ChannelId, thread_ts: &Ts) {
        let key = self.key(channel, thread_ts);
        self.followed.insert(key);
    }

    /// A thread unfollowed anywhere stops being the user's business. The
    /// card it raised is left to the verdict that closes it; what goes is
    /// the standing claim that its next reply is theirs.
    /// rho's own ignore: the follow goes, the thread stays. Undoing the
    /// mute follows it again and the card comes back exactly as it was,
    /// which an unfollow from Slack's side cannot promise.
    pub fn ignore(&mut self, key: &ThreadKey) {
        self.followed.remove(key);
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

    /// Marks a conversation read locally. Called both when rho reads one and
    /// when Slack says another client did. Reading is not a verdict: this
    /// clears the unread counts and leaves every card exactly where it was.
    pub fn mark_read(&mut self, channel: &ChannelId, ts: &Ts) {
        if let Some(count) = self.counts.get_mut(channel) {
            count.has_unreads = false;
            count.mention_count = 0;
            count.unread_count = 0;
            // The cursor moves with the badge. A surface already open keeps
            // its own copy of where the rule goes, so reading here never
            // moves a rule out from under the reader.
            count.last_read = Some(ts.clone());
        }
    }

    /// Slack's read cursor for the conversation: the message the unread
    /// rule sits under.
    pub fn last_read(&self, channel: &ChannelId) -> Option<&Ts> {
        self.counts.get(channel)?.last_read.as_ref()
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
        } else {
            None
        };
        // The user's own message counts in any unit rho already tracks: it
        // is what flips whose turn it is, whatever it says.
        let existing = self.units.get(unit).map(|facts| facts.reason);
        match from_you {
            true => existing.or(reason),
            false => reason.and(existing.or(reason)),
        }
    }

    /// Whether a message is one the user is meant to answer: any message
    /// in a direct message, a mention in a channel, a reply in a thread they
    /// follow. This is what `newest_from_other` counts, so it is also what a
    /// dealt card lands on: the ordinary chatter in a channel is not what
    /// the reader was brought here for.
    pub fn concerns_you(&self, message: &Message, unit: &Unit) -> bool {
        unit.thread.is_some() || self.is_dm(&message.channel) || self.mentions_you(message)
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
            conversation: self.label(&unit.channel),
            waiting: facts.waiting(),
            wait_days: wait_days(facts, now_ms),
            newest: facts.newest.clone(),
            newest_from_other: facts.newest_from_other.clone(),
        })
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
        let mut cards = model
            .tracked()
            .into_iter()
            .filter_map(|unit| model.card(&unit, now_ms))
            .filter(|card| card.waiting == Waiting::OnYou)
            .collect::<Vec<_>>();
        cards.sort_by(|left, right| right.wait_days.total_cmp(&left.wait_days));
        cards
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
        followed.set_followed([(ChannelId("C1".into()), Ts("500".into()))]);
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
        live_first.set_followed([(ChannelId("C1".into()), Ts("800".into()))]);
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

    #[test]
    fn reading_elsewhere_clears_the_badge_and_keeps_the_card() {
        // Verdicts are the user's keys only. Reading on the phone means the
        // unread badge is stale, nothing more: the ping still owes an answer.
        let mut model = model();
        model.note_message(&message("D1", "600", "U1", "ping"), 0);
        let key = Unit::conversation(&ChannelId("D1".into()));
        model.mark_read(&ChannelId("D1".into()), &Ts("600".into()));
        assert_eq!(model.unit(&key).unwrap().waiting(), Waiting::OnYou);
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
        assert_eq!(model.next_unread(None), Some(first.clone()));
        assert_eq!(model.next_unread(Some(&first)), Some(second.clone()));
        // Round again: one key, pressed until there is nothing left.
        assert_eq!(model.next_unread(Some(&second)), Some(first.clone()));

        // The one the reader is in does not count, however unread Slack
        // still thinks it is: they are looking at it.
        model.set_counts([count("C1", false), count("D1", true)]);
        assert_eq!(model.next_unread(Some(&ChannelId("D1".into()))), None);
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
        assert_eq!(model.next_unread(None), None);

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
        // The reader is never a mention of themselves.
        assert_eq!(values('@', ""), vec!["@ada"]);
        assert_eq!(values('@', "ad"), vec!["@ada"]);
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
}
