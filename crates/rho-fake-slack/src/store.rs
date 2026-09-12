//! The whole workspace, in memory, and every read a client can ask for.
//!
//! The cost rule holds here as it does in the client: a request costs the
//! rows it touches plus a lookup to place them, never a pass over the
//! workspace. A conversation's history is one sorted vector, so a window of
//! it is two binary searches and a slice; the unread counts a client asks for
//! on every poll are an index subtraction and one lookup in a prefix sum,
//! never a walk of the messages behind the cursor.
//!
//! Unread counts are derived rather than stored. A server that keeps a count
//! beside a cursor can disagree with itself — `client.counts` saying four
//! while `conversations.history` shows three — and a client that has to cope
//! with that is being taught something Slack does not actually do.

use std::collections::BTreeMap;

use crate::types::{ChannelId, Conversation, Kind, Message, Reaction, Ts, User, UserId};

/// One conversation and everything under it.
struct Held {
    conversation: Conversation,
    /// Top-level messages, ascending by `ts`. Replies live in `threads`, as
    /// they do at Slack: `conversations.history` does not return them.
    messages: Vec<Message>,
    /// How many of `messages[..i]` name the signed-in user. A Fenwick tree
    /// rather than a plain prefix sum, because the world is alive: a message
    /// arriving is one append and an edit that gains or loses a mention is
    /// one update, both in log time, where a prefix sum would rewrite the
    /// tail of the conversation on every edit.
    mentions: Mentions,
    threads: BTreeMap<Ts, Vec<Message>>,
    /// Where each person has read to in the conversation.
    read: BTreeMap<UserId, Ts>,
    /// Where each person has read to inside a thread. Slack keeps this apart
    /// from the conversation's own cursor, and so does this: reading a thread
    /// must not mark the channel around it.
    thread_read: BTreeMap<(UserId, Ts), Ts>,
}

/// The workspace. Everything a request can ask about is here, and nothing
/// about a connection is.
pub struct Store {
    /// The next timestamp anything new gets. One clock for the server, so a
    /// message the schedule wrote and a message a client sent cannot land on
    /// the same second or out of order in a conversation both touched.
    clock: Ts,
    /// Who the client signs in as.
    pub self_id: UserId,
    users: BTreeMap<UserId, User>,
    /// The order `users.list` returns people in.
    user_order: Vec<UserId>,
    held: BTreeMap<ChannelId, Held>,
    /// Every message that names the signed-in user, ascending, as (when,
    /// where, which row). The activity feed is the tail of this, so serving
    /// it is a slice rather than a search of the workspace.
    mention_index: Vec<(Ts, ChannelId, usize)>,
    /// The threads Slack follows for the signed-in user, in the order
    /// `subscriptions.thread.getView` lists them.
    followed: Vec<(ChannelId, Ts)>,
    /// The order `users.conversations` returns conversations in: most
    /// recently active first, which is the order the client's list starts in.
    order: Vec<ChannelId>,
    emoji: BTreeMap<String, String>,
}

/// A window of a conversation, newest first, and whether older messages are
/// behind it. What `conversations.history` is made of.
pub struct Window<'a> {
    pub messages: &'a [Message],
    pub has_more: bool,
}

/// What one conversation is worth to the badge on the client's list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    pub messages: u32,
    pub mentions: u32,
}

impl Store {
    pub fn new(self_id: UserId) -> Self {
        Self {
            clock: Ts::new(0, 0),
            self_id,
            users: BTreeMap::new(),
            user_order: Vec::new(),
            held: BTreeMap::new(),
            mention_index: Vec::new(),
            followed: Vec::new(),
            order: Vec::new(),
            emoji: BTreeMap::new(),
        }
    }

    pub fn add_user(&mut self, user: User) {
        if self.users.insert(user.id.clone(), user.clone()).is_none() {
            self.user_order.push(user.id);
        }
    }

    pub fn add_emoji(&mut self, name: &str, url: &str) {
        self.emoji.insert(name.to_owned(), url.to_owned());
    }

    /// Adds a conversation with its history already sorted by `ts`. Taking
    /// the whole history at once is what lets the mention prefix be built in
    /// one pass here rather than being rebuilt on every count later.
    pub fn add_conversation(&mut self, conversation: Conversation, messages: Vec<Message>) {
        let id = conversation.id.clone();
        let mut mentions = Mentions::default();
        for (at, message) in messages.iter().enumerate() {
            mentions.push(u32::from(message.mentions_self));
            if message.mentions_self {
                self.mention_index.push((message.ts, id.clone(), at));
            }
        }
        self.held.insert(
            id.clone(),
            Held {
                conversation,
                messages,
                mentions,
                threads: BTreeMap::new(),
                read: BTreeMap::new(),
                thread_read: BTreeMap::new(),
            },
        );
        self.order.push(id);
    }

    /// Replies under one message, ascending. The parent is not repeated here;
    /// `replies` puts it back at the front, as Slack does.
    pub fn add_thread(&mut self, channel: &ChannelId, parent: Ts, replies: Vec<Message>) {
        let Some(held) = self.held.get_mut(channel) else {
            return;
        };
        held.threads.insert(parent, replies);
    }

    /// The newest messages that name the signed-in user, newest first. What
    /// the activity feed is made of.
    pub fn mentions(&self, most: usize) -> Vec<(&ChannelId, &Message)> {
        self.mention_index
            .iter()
            .rev()
            .take(most)
            .filter_map(|(_, channel, at)| {
                let held = self.held.get(channel)?;
                Some((&held.conversation.id, held.messages.get(*at)?))
            })
            .collect()
    }

    /// Puts the mention index in order after the world is built. The world
    /// arrives conversation by conversation, so the index is only sorted by
    /// time once they are all in.
    pub fn settle(&mut self) {
        self.mention_index.sort_by_key(|(ts, _, _)| *ts);
        // Where time starts: just after the newest thing in the world. The
        // one pass over conversations the live side makes, made once here
        // rather than per happening.
        let newest = self
            .held
            .values()
            .filter_map(|held| held.messages.last().map(|message| message.ts))
            .max()
            .unwrap_or(Ts::new(0, 0));
        self.clock = Ts::new(newest.seconds + 1, 0);
    }

    /// The next timestamp, strictly after every one handed out before it.
    /// Everything that writes takes its `ts` from here — the schedule and
    /// the clients both — so a conversation's history is append-only no
    /// matter who is writing to it.
    pub fn tick(&mut self) -> Ts {
        self.clock = Ts::new(self.clock.seconds + 1, self.clock.micros);
        self.clock
    }

    pub fn follow_thread(&mut self, channel: ChannelId, parent: Ts) {
        self.followed.push((channel, parent));
    }

    /// Stops following a thread, and says whether it was followed at all.
    pub fn unfollow_thread(&mut self, channel: &ChannelId, parent: Ts) -> bool {
        let before = self.followed.len();
        self.followed
            .retain(|(id, ts)| id != channel || *ts != parent);
        self.followed.len() != before
    }

    /// One message by its timestamp, and where it sits. A binary search, not
    /// a scan: this is on the path of every reaction and every edit.
    pub fn message(&self, channel: &ChannelId, ts: Ts) -> Option<(usize, &Message)> {
        let held = self.held.get(channel)?;
        let at = held
            .messages
            .binary_search_by(|message| message.ts.cmp(&ts))
            .ok()?;
        Some((at, &held.messages[at]))
    }

    /// A reply by its timestamp, inside the thread it belongs to.
    pub fn reply(&self, channel: &ChannelId, parent: Ts, ts: Ts) -> Option<&Message> {
        let held = self.held.get(channel)?;
        held.threads
            .get(&parent)?
            .iter()
            .rev()
            .find(|reply| reply.ts == ts)
    }

    pub fn followed(&self) -> &[(ChannelId, Ts)] {
        &self.followed
    }

    pub fn set_read(&mut self, channel: &ChannelId, user: UserId, cursor: Ts) {
        if let Some(held) = self.held.get_mut(channel) {
            held.read.insert(user, cursor);
        }
    }

    pub fn set_thread_read(&mut self, channel: &ChannelId, parent: Ts, user: UserId, cursor: Ts) {
        if let Some(held) = self.held.get_mut(channel) {
            held.thread_read.insert((user, parent), cursor);
        }
    }

    pub fn user(&self, id: &UserId) -> Option<&User> {
        self.users.get(id)
    }

    pub fn users(&self) -> impl Iterator<Item = &User> {
        self.user_order.iter().filter_map(|id| self.users.get(id))
    }

    pub fn user_count(&self) -> usize {
        self.user_order.len()
    }

    pub fn emoji(&self) -> impl Iterator<Item = (&String, &String)> {
        self.emoji.iter()
    }

    pub fn conversation(&self, id: &ChannelId) -> Option<&Conversation> {
        self.held.get(id).map(|held| &held.conversation)
    }

    /// The conversations the signed-in user is in, in list order.
    pub fn conversations(&self) -> impl Iterator<Item = &Conversation> {
        self.order
            .iter()
            .filter_map(|id| self.held.get(id).map(|held| &held.conversation))
    }

    pub fn conversation_count(&self) -> usize {
        self.order.len()
    }

    pub fn message_count(&self) -> usize {
        self.held
            .values()
            .map(|held| held.messages.len() + held.threads.values().map(Vec::len).sum::<usize>())
            .sum()
    }

    /// A window of history, newest first, the way `conversations.history`
    /// asks for it: everything strictly after `oldest` and up to `latest`,
    /// at most `limit` of them, and whether there are older ones behind.
    pub fn history(
        &self,
        channel: &ChannelId,
        oldest: Option<Ts>,
        latest: Option<Ts>,
        inclusive: bool,
        limit: usize,
    ) -> Option<Window<'_>> {
        let held = self.held.get(channel)?;
        let start = match oldest {
            // Slack's `oldest` is exclusive unless `inclusive` is set.
            Some(oldest) if inclusive => held.messages.partition_point(|m| m.ts < oldest),
            Some(oldest) => held.messages.partition_point(|m| m.ts <= oldest),
            None => 0,
        };
        let end = match latest {
            Some(latest) if inclusive => held.messages.partition_point(|m| m.ts <= latest),
            Some(latest) => held.messages.partition_point(|m| m.ts < latest),
            None => held.messages.len(),
        };
        let end = end.max(start);
        // Newest first, so the window is taken from the far end and what is
        // left behind it is what `has_more` reports.
        let taken = (end - start).min(limit);
        Some(Window {
            messages: &held.messages[end - taken..end],
            has_more: end - taken > start,
        })
    }

    /// One thread, parent first, the way `conversations.replies` returns it.
    pub fn replies(&self, channel: &ChannelId, parent: Ts) -> Option<(&Message, &[Message])> {
        let held = self.held.get(channel)?;
        let at = held.messages.binary_search_by(|m| m.ts.cmp(&parent)).ok()?;
        let replies = held.threads.get(&parent).map_or(&[][..], Vec::as_slice);
        Some((&held.messages[at], replies))
    }

    /// What a person has left to read in one conversation. Two lookups and a
    /// subtraction: no walk of the messages behind the cursor.
    pub fn unread(&self, channel: &ChannelId, user: &UserId) -> Unread {
        let Some(held) = self.held.get(channel) else {
            return Unread::default();
        };
        let read = held.read.get(user);
        let at = match read {
            Some(cursor) => held.messages.partition_point(|m| m.ts <= *cursor),
            None => 0,
        };
        let total = held.messages.len();
        Unread {
            messages: (total - at) as u32,
            mentions: held.mentions.after(at),
        }
    }

    pub fn read_cursor(&self, channel: &ChannelId, user: &UserId) -> Option<Ts> {
        self.held.get(channel)?.read.get(user).copied()
    }

    pub fn thread_read_cursor(&self, channel: &ChannelId, parent: Ts, user: &UserId) -> Option<Ts> {
        self.held
            .get(channel)?
            .thread_read
            .get(&(user.clone(), parent))
            .copied()
    }

    /// Puts a message at the end of a conversation. Appending is what time
    /// does to a conversation, so it is the cheap case on purpose: a push, a
    /// log-time mention update, and nothing else moves.
    pub fn post(&mut self, channel: &ChannelId, message: Message) -> bool {
        let Some(held) = self.held.get_mut(channel) else {
            return false;
        };
        match message.thread_ts {
            // A reply belongs to its thread, and bumps the parent's counters
            // the way Slack does rather than appearing in the history.
            Some(parent) => {
                let replies = held.threads.entry(parent).or_default();
                replies.push(message.clone());
                let Ok(at) = held.messages.binary_search_by(|held| held.ts.cmp(&parent)) else {
                    return false;
                };
                held.messages[at].reply_count += 1;
                held.messages[at].latest_reply = Some(message.ts);
            }
            None => {
                if message.mentions_self {
                    self.mention_index
                        .push((message.ts, channel.clone(), held.messages.len()));
                }
                held.mentions.push(u32::from(message.mentions_self));
                held.messages.push(message);
            }
        }
        true
    }

    /// Rewrites a message's text. The mention count follows the new text, so
    /// an edit that takes the reader's name out of a message takes it out of
    /// their badge as well.
    pub fn edit(&mut self, channel: &ChannelId, ts: Ts, text: String, mentions_self: bool) -> bool {
        let Some(held) = self.held.get_mut(channel) else {
            return false;
        };
        let Ok(at) = held.messages.binary_search_by(|held| held.ts.cmp(&ts)) else {
            return false;
        };
        let was = held.messages[at].mentions_self;
        held.messages[at].text = text;
        held.messages[at].edited = true;
        held.messages[at].mentions_self = mentions_self;
        if was != mentions_self {
            held.mentions.set(at, u32::from(mentions_self));
            match mentions_self {
                true => self.mention_index.push((ts, channel.clone(), at)),
                false => self
                    .mention_index
                    .retain(|(_, id, row)| *row != at || id != channel),
            }
        }
        true
    }

    /// A deletion is a tombstone rather than a removal: every row after it
    /// would otherwise move, and the indexes that make the counts cheap are
    /// positions in that list.
    pub fn delete(&mut self, channel: &ChannelId, ts: Ts) -> bool {
        let Some(held) = self.held.get_mut(channel) else {
            return false;
        };
        let Ok(at) = held.messages.binary_search_by(|held| held.ts.cmp(&ts)) else {
            return false;
        };
        held.messages[at].deleted = true;
        held.messages[at].text = String::new();
        if held.messages[at].mentions_self {
            held.messages[at].mentions_self = false;
            held.mentions.set(at, 0);
        }
        true
    }

    /// Puts an emoji on a message or takes it off, and says whether anything
    /// changed — pressing a reaction twice is not two reactions.
    pub fn react(
        &mut self,
        channel: &ChannelId,
        ts: Ts,
        user: &UserId,
        name: &str,
        added: bool,
    ) -> bool {
        let Some(held) = self.held.get_mut(channel) else {
            return false;
        };
        let Ok(at) = held.messages.binary_search_by(|held| held.ts.cmp(&ts)) else {
            return false;
        };
        let reactions = &mut held.messages[at].reactions;
        let found = reactions.iter().position(|reaction| reaction.name == name);
        match (added, found) {
            (true, Some(at)) => {
                if reactions[at].users.contains(user) {
                    return false;
                }
                reactions[at].users.push(user.clone());
            }
            (true, None) => reactions.push(Reaction {
                name: name.to_owned(),
                users: Vec::from([user.clone()]),
            }),
            (false, Some(at)) => {
                let before = reactions[at].users.len();
                reactions[at].users.retain(|had| had != user);
                if reactions[at].users.len() == before {
                    return false;
                }
                if reactions[at].users.is_empty() {
                    reactions.remove(at);
                }
            }
            (false, None) => return false,
        }
        true
    }

    /// The newest message in a conversation, which is what the list sorts on
    /// and what `client.counts` reports as `latest`.
    pub fn latest(&self, channel: &ChannelId) -> Option<Ts> {
        self.held.get(channel)?.messages.last().map(|m| m.ts)
    }

    /// Whether Slack counts messages for this conversation at all: it counts
    /// them for DMs and group DMs, and reports only mentions for channels.
    pub fn counts_messages(&self, channel: &ChannelId) -> bool {
        self.held
            .get(channel)
            .is_some_and(|held| matches!(held.conversation.kind, Kind::Dm | Kind::Group))
    }
}

/// A Fenwick tree over "does this message name the signed-in user": append
/// and update in log time, and the count after a cursor in log time. The
/// alternative, a plain prefix sum, is one line shorter and rewrites the tail
/// of a conversation whenever an edit changes a mention.
#[derive(Default)]
struct Mentions {
    /// 1-based, each cell holding the sum of a power-of-two run below it.
    tree: Vec<u32>,
}

impl Mentions {
    fn push(&mut self, value: u32) {
        let at = self.tree.len() + 1;
        let mut sum = value;
        let mut step = 1;
        while step < (at & at.wrapping_neg()) {
            sum += self.tree[at - step - 1];
            step <<= 1;
        }
        self.tree.push(sum);
    }

    /// The sum of the first `count` messages.
    fn upto(&self, count: usize) -> u32 {
        let mut at = count.min(self.tree.len());
        let mut sum = 0;
        while at > 0 {
            sum += self.tree[at - 1];
            at -= at & at.wrapping_neg();
        }
        sum
    }

    /// How many mentions are after the first `count` messages.
    fn after(&self, count: usize) -> u32 {
        self.upto(self.tree.len()) - self.upto(count)
    }

    /// Sets one message's value, which is what an edit does.
    fn set(&mut self, at: usize, value: u32) {
        let was = self.upto(at + 1) - self.upto(at);
        if was == value {
            return;
        }
        let mut cursor = at + 1;
        while cursor <= self.tree.len() {
            self.tree[cursor - 1] = self.tree[cursor - 1] + value - was;
            cursor += cursor & cursor.wrapping_neg();
        }
    }
}
