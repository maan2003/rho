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

/// One thread rho is tracking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub reason: Reason,
    /// The newest message in the thread. The tree keys a thread node's
    /// verdicts on it: a newer one voids a skip or a done, exactly like an
    /// agent's reply.
    pub latest: Ts,
    pub latest_from_you: bool,
    /// First line of the newest message, for the card.
    pub summary: String,
    /// When rho first saw this thread, so a card's age is rho's own clock
    /// and cannot be moved by a doctored message timestamp.
    pub first_seen_ms: i64,
}

impl Thread {
    pub fn waiting(&self) -> Waiting {
        match self.latest_from_you {
            true => Waiting::OnThem,
            false => Waiting::OnYou,
        }
    }
}

/// What a change to the model means for the inbox. The GUI translates these
/// into appends, updates, and retirements; the model never touches storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A thread that owes the user an answer, and was not owing one before.
    Raised(ThreadKey),
    /// A thread already raised whose newest message changed.
    Updated(ThreadKey),
    /// The user answered. The card stays and says `replied`: verdicts are
    /// the user's keys only, so nothing here closes it and nothing binds.
    Replied(ThreadKey),
    /// The thread stopped being the user's, because they ignored it here or
    /// unfollowed it in another client. Slack's own verdict, so the card
    /// goes; nothing rho stores says otherwise.
    Discarded(ThreadKey),
}

/// A dealer card's worth of a thread, with no ids and no raw timestamps.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadCard {
    pub key: ThreadKey,
    /// `#design` or `@ada`.
    pub conversation: String,
    pub summary: String,
    pub waiting: Waiting,
    pub wait_days: f64,
    /// The newest message; a change here is what re-raises the card.
    pub latest: Ts,
}

/// Everything one `mark read before` touches: the conversations to mark and
/// the followed threads to mark, each with the message to mark up to. The
/// count shown before acting and the calls made after are this same list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkPlan {
    pub conversations: Vec<(ChannelId, Ts)>,
    pub threads: Vec<(ThreadKey, Ts)>,
}

/// One line of the conversation list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationRow {
    pub id: ChannelId,
    pub label: String,
    pub unread: bool,
    pub mention_count: u32,
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
    threads: BTreeMap<ThreadKey, Thread>,
    /// Every (channel, timestamp) the model has already accounted for. This
    /// is the whole of the deduplication between the feed and the socket.
    seen: BTreeSet<(ChannelId, Ts)>,
    /// The threads Slack follows for the user, which is what makes a later
    /// reply in one an obligation rather than channel traffic. Slack owns
    /// this list: it subscribes a thread the user posts in or is mentioned
    /// in, from any client, so rho never has to remember what it watched.
    followed: BTreeSet<ThreadKey>,
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
            threads: BTreeMap::new(),
            seen: BTreeSet::new(),
            followed: BTreeSet::new(),
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

    /// Moves the list's own counters for a message off the socket. This is
    /// every message, not only the ones that raise a card: the list names
    /// the whole workspace, and without this its badges sit at whatever
    /// `client.counts` said at connect until the next restart.
    pub fn note_counts(&mut self, message: &Message) {
        let from_you = message.user.as_ref() == Some(&self.self_id);
        let is_dm = self
            .conversations
            .get(&message.channel)
            .is_some_and(|conversation| conversation.kind == ConversationKind::DirectMessage);
        let pings_you = is_dm || self.mentions_you(message);
        let count = self
            .counts
            .entry(message.channel.clone())
            .or_insert_with(|| ConversationCount {
                channel: message.channel.clone(),
                has_unreads: false,
                mention_count: 0,
                latest: None,
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
            return;
        }
        count.has_unreads = true;
        if pings_you {
            count.mention_count += 1;
        }
    }

    /// The conversation list: unread first with their counts, then the rest
    /// by recency. Within a group, the noisier conversation sorts first.
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
                    latest: count.and_then(|count| count.latest.clone()),
                }
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            right
                .unread
                .cmp(&left.unread)
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
            .threads
            .iter()
            .filter(|(key, _)| self.followed.contains(key))
            .filter(|(_, thread)| thread.latest.epoch_seconds() < before)
            .map(|(key, thread)| (key.clone(), thread.latest.clone()))
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
            .filter(|key| self.threads.contains_key(key))
            .cloned()
            .collect::<Vec<_>>();
        self.followed = now;
        for key in &dropped {
            self.threads.remove(key);
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
    /// discard follows it again and the card comes back exactly as it was,
    /// which an unfollow from Slack's side cannot promise.
    pub fn ignore(&mut self, key: &ThreadKey) {
        self.followed.remove(key);
    }

    /// Returns whether rho is tracking the thread, which is the difference
    /// between an unfollow that discards a card and one that changes
    /// nothing the user can see.
    pub fn unfollow(&mut self, channel: &ChannelId, thread_ts: &Ts) -> bool {
        let key = self.key(channel, thread_ts);
        self.followed.remove(&key);
        // The thread goes with the follow. Nothing here remembers that it
        // was ever raised, so following it again in Slack raises it only
        // when somebody writes in it.
        self.threads.remove(&key).is_some()
    }

    pub fn follows(&self, key: &ThreadKey) -> bool {
        self.followed.contains(key)
    }

    /// Marks a conversation read locally. Called both when rho reads one and
    /// when Slack says another client did. Reading is not a verdict: this
    /// clears the unread counts and leaves every card exactly where it was.
    pub fn mark_read(&mut self, channel: &ChannelId, ts: &Ts) {
        let _ = ts;
        if let Some(count) = self.counts.get_mut(channel) {
            count.has_unreads = false;
            count.mention_count = 0;
        }
    }

    /// Fills in the body of a message rho already knows about. The feed
    /// raises a thread before any body exists, and the message that then
    /// arrives is a duplicate by timestamp, so without this the card would
    /// keep the blank summary the feed gave it.
    pub fn note_loaded(&mut self, message: &Message) -> Option<Change> {
        let key = self.key(&message.channel, &message.thread_root());
        let summary = summarize(&self.render(message));
        let thread = self.threads.get_mut(&key)?;
        if summary.is_empty() || thread.latest != message.ts || thread.summary == summary {
            return None;
        }
        thread.summary = summary;
        thread.latest_from_you = message.user.as_ref() == Some(&self.self_id);
        Some(match thread.waiting() {
            Waiting::OnYou => Change::Updated(key),
            Waiting::OnThem => Change::Replied(key),
        })
    }

    /// Every thread rho is tracking, for a caller that has to revisit them
    /// all — the roster arriving is the case: names known late change what a
    /// card says.
    pub fn tracked(&self) -> Vec<ThreadKey> {
        self.threads.keys().cloned().collect()
    }

    pub fn thread(&self, key: &ThreadKey) -> Option<&Thread> {
        self.threads.get(key)
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
        let key = self.key(&message.channel, &message.thread_root());
        let from_you = message.user.as_ref() == Some(&self.self_id);
        // The reason is decided before the message is marked seen: channel
        // traffic is never "seen", so a live message rho drops cannot poison
        // the dedup and swallow the feed item for the same `ts` that would
        // have raised it.
        let reason = self.reason_for(message, &key, from_you)?;
        if !self
            .seen
            .insert((message.channel.clone(), message.ts.clone()))
        {
            return None;
        }
        self.record(key, reason, message, from_you, now_ms)
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
        let thread_ts = item.thread_ts.clone().unwrap_or_else(|| item.ts.clone());
        let key = self.key(&item.channel, &thread_ts);
        let message = Message {
            ts: item.ts.clone(),
            thread_ts: item.thread_ts.clone(),
            channel: item.channel.clone(),
            user: None,
            bot_name: None,
            blocks: Vec::new(),
            text: String::new(),
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 0,
            latest_reply: None,
            edited: false,
            reactions: Vec::new(),
        };
        self.record(key, reason, &message, false, now_ms)
    }

    /// Why this message obliges the user, or `None` for channel traffic.
    fn reason_for(&self, message: &Message, key: &ThreadKey, from_you: bool) -> Option<Reason> {
        // A thread rho already tracks stays rho's business whoever spoke: a
        // follow-up moves the verdict key, and the user's own reply is the
        // message that flips whose turn it is.
        if let Some(thread) = self.threads.get(key) {
            return Some(thread.reason);
        }
        let followed = self.followed.contains(key);
        if from_you {
            return followed.then_some(Reason::Thread);
        }
        let is_dm = self
            .conversations
            .get(&message.channel)
            .is_some_and(|conversation| conversation.kind == ConversationKind::DirectMessage);
        if is_dm {
            return Some(Reason::DirectMessage);
        }
        if self.mentions_you(message) {
            return Some(Reason::Mention);
        }
        followed.then_some(Reason::Thread)
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

    fn record(
        &mut self,
        key: ThreadKey,
        reason: Reason,
        message: &Message,
        from_you: bool,
        now_ms: i64,
    ) -> Option<Change> {
        let summary = summarize(&self.render(message));
        let existing = self.threads.get(&key);
        // An out-of-order arrival (a feed page after the socket already had
        // the newer reply) must not walk the thread backwards.
        if existing.is_some_and(|thread| thread.latest.is_newer_than(&message.ts)) {
            return None;
        }
        let was_waiting = existing.map(Thread::waiting);
        let first_seen_ms = existing.map_or(now_ms, |thread| thread.first_seen_ms);
        let summary = match summary.is_empty() {
            true => existing
                .map(|thread| thread.summary.clone())
                .unwrap_or_default(),
            false => summary,
        };
        self.threads.insert(
            key.clone(),
            Thread {
                reason,
                latest: message.ts.clone(),
                latest_from_you: from_you,
                summary,
                first_seen_ms,
            },
        );
        Some(match (was_waiting, from_you) {
            // Answering is not closing. The card keeps its place in the tree
            // and drops onto the fyi curve; only the user's own `d` ends it.
            (_, true) => Change::Replied(key),
            (Some(Waiting::OnYou), false) => Change::Updated(key),
            // Either brand new, or answered-then-answered-again: both are a
            // fresh obligation, which is what re-raises a card after a done.
            (_, false) => Change::Raised(key),
        })
    }

    pub fn card(&self, key: &ThreadKey, now_ms: i64) -> Option<ThreadCard> {
        let thread = self.threads.get(key)?;
        Some(ThreadCard {
            key: key.clone(),
            conversation: self.label(&key.channel),
            summary: thread.summary.clone(),
            waiting: thread.waiting(),
            wait_days: wait_days(thread, now_ms),
            latest: thread.latest.clone(),
        })
    }
}

/// How long the thread has been waiting, counted from the newest message.
fn wait_days(thread: &Thread, now_ms: i64) -> f64 {
    let since = now_ms.saturating_sub(thread.latest.millis().max(thread.first_seen_ms.min(now_ms)));
    (since as f64 / 86_400_000.0).max(0.0)
}

fn summarize(rendered: &str) -> String {
    rendered
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
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

    /// The cards a thread node would be raised for: what the dealer used to
    /// ask the model for directly, now derived here because the tree is what
    /// deals.
    fn owed(model: &Model, now_ms: i64) -> Vec<ThreadCard> {
        let mut cards = model
            .tracked()
            .into_iter()
            .filter_map(|key| model.card(&key, now_ms))
            .filter(|card| card.waiting == Waiting::OnYou)
            .collect::<Vec<_>>();
        cards.sort_by(|left, right| right.wait_days.total_cmp(&left.wait_days));
        cards
    }

    #[test]
    fn a_thread_raised_by_the_feed_gains_its_summary_when_it_is_loaded() {
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
        let key = model.key(&ChannelId("C1".into()), &Ts("100.0".into()));
        assert_eq!(model.thread(&key).unwrap().summary, "");

        let message = crate::api::parse_message(
            &serde_json::json!({"ts": "100.0", "user": "U1", "text": "look at the deploy"}),
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
            Some(Change::Updated(_))
        ));
        assert_eq!(model.thread(&key).unwrap().summary, "look at the deploy");
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
            Some(Change::Raised(
                model.key(&ChannelId("C1".into()), &Ts("101".into()))
            ))
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
        let key = model.key(&ChannelId("D1".into()), &Ts("400".into()));
        assert_eq!(model.thread(&key).unwrap().waiting(), Waiting::OnYou);
        assert_eq!(owed(&model, now).len(), 1);

        // Answering is not a verdict: the thread is still tracked, the ball
        // is theirs, and the card the dealer builds says so.
        assert_eq!(
            model.note_message(&reply("D1", "401", "400", "ME", "tomorrow"), now),
            Some(Change::Replied(key.clone()))
        );
        assert_eq!(model.thread(&key).unwrap().waiting(), Waiting::OnThem);
        assert_eq!(model.card(&key, now).unwrap().waiting, Waiting::OnThem);

        // Their answer brings it back, keyed on the newer message.
        assert_eq!(
            model.note_message(&reply("D1", "402", "400", "U1", "thanks!"), now),
            Some(Change::Raised(key.clone()))
        );
        let card = model.card(&key, now).unwrap();
        assert_eq!(card.waiting, Waiting::OnYou);
        assert_eq!(card.latest, Ts("402".into()));
        assert_eq!(card.summary, "thanks!");
        assert_eq!(card.conversation, "@ada");
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
        let key = model.key(&ChannelId("D1".into()), &Ts("500".into()));
        assert_eq!(model.thread(&key).unwrap().waiting(), Waiting::OnThem);
    }

    #[test]
    fn reading_elsewhere_clears_the_badge_and_keeps_the_card() {
        // Verdicts are the user's keys only. Reading on the phone means the
        // unread badge is stale, nothing more: the ping still owes an answer.
        let mut model = model();
        model.note_message(&message("D1", "600", "U1", "ping"), 0);
        let key = model.key(&ChannelId("D1".into()), &Ts("600".into()));
        model.mark_read(&ChannelId("D1".into()), &Ts("600".into()));
        assert_eq!(model.thread(&key).unwrap().waiting(), Waiting::OnYou);
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
                latest: Some(Ts("10".into())),
            },
            ConversationCount {
                channel: ChannelId("D1".into()),
                has_unreads: true,
                mention_count: 3,
                latest: Some(Ts("5".into())),
            },
            ConversationCount {
                channel: ChannelId("C2".into()),
                has_unreads: false,
                mention_count: 0,
                latest: Some(Ts("99".into())),
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
        let mut model = model();
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
}
