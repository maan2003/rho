//! Time. The workspace does not sit still while a client is connected: other
//! people post, reply in threads, react, edit what they said and read what
//! was said to them, on a schedule that comes out of the same seed the world
//! did.
//!
//! Two ways to run it, one sequence. `advance` applies the next N happenings
//! immediately, which is what a test wants — no sleeping, no flake, and the
//! same N happenings every run. A rate runs the same sequence on a timer,
//! which is what the rig wants. Both draw from one schedule, so a run that
//! did 500 happenings did the same 500 either way.
//!
//! Cost, per happening: one draw, a binary search to find the message it
//! lands on, and an append or an in-place write. Nothing walks a
//! conversation and nothing walks the workspace. The one pass over the
//! conversations is when the schedule is built, to find where the clock
//! starts.

use crate::socket::Wire;
use crate::store::Store;
use crate::types::{ChannelId, Kind, Message, Ts, UserId};
use crate::world::Random;

/// One thing that happened, typed. The in-process caller gets these back
/// from `advance`, so a test can assert against what the server did rather
/// than against what it guessed the server did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Happening {
    Posted {
        channel: ChannelId,
        ts: Ts,
        user: UserId,
    },
    Replied {
        channel: ChannelId,
        parent: Ts,
        ts: Ts,
        user: UserId,
    },
    Reacted {
        channel: ChannelId,
        ts: Ts,
        user: UserId,
        name: String,
    },
    Edited {
        channel: ChannelId,
        ts: Ts,
    },
    Read {
        channel: ChannelId,
        user: UserId,
        ts: Ts,
    },
    /// The draw landed somewhere nothing could happen — an empty
    /// conversation, a thread that is gone. Reported rather than retried, so
    /// that the number of draws and the number of happenings stay the same
    /// and the sequence stays replayable.
    Nothing,
}

/// The seeded schedule. Holds a snapshot of who and where, taken once, so a
/// happening costs a draw and not a scan.
pub struct Schedule {
    random: Random,
    channels: Vec<(ChannelId, Kind)>,
    people: Vec<UserId>,
    emoji: Vec<String>,
    self_id: UserId,
}

impl Schedule {
    /// Builds the schedule from the store it will act on, taking the
    /// snapshot of who and where once. The clock it writes with is the
    /// store's, so client writes and scheduled writes interleave without
    /// either landing behind the other.
    pub fn new(store: &Store, seed: u64) -> Self {
        let channels: Vec<(ChannelId, Kind)> = store
            .conversations()
            .map(|conversation| (conversation.id.clone(), conversation.kind))
            .collect();
        Self {
            random: Random::new(seed ^ 0x5DEECE66D),
            channels,
            people: store
                .users()
                .map(|user| user.id.clone())
                .filter(|id| *id != store.self_id)
                .collect(),
            emoji: store
                .emoji()
                .map(|(name, _)| name.clone())
                .take(64)
                .collect(),
            self_id: store.self_id.clone(),
        }
    }

    /// Applies one happening to the store and puts its frame on the wire.
    pub fn step(&mut self, store: &mut Store, wire: &Wire) -> Happening {
        if self.channels.is_empty() || self.people.is_empty() {
            return Happening::Nothing;
        }
        let (channel, kind) =
            self.channels[self.random.below(self.channels.len() as u64) as usize].clone();
        let user = self.people[self.random.below(self.people.len() as u64) as usize].clone();
        // The mix is what a workspace actually does in an hour: mostly
        // people talking, some of it in threads, a steady trickle of emoji,
        // the occasional correction, and reading.
        match self.random.below(100) {
            0..=54 => self.post(store, wire, channel, user),
            55..=69 => self.reply(store, wire, channel, user),
            70..=84 => self.react(store, wire, channel, user),
            85..=91 => self.edit(store, wire, channel),
            _ => self.read(store, wire, channel, kind, user),
        }
    }

    fn post(
        &mut self,
        store: &mut Store,
        wire: &Wire,
        channel: ChannelId,
        user: UserId,
    ) -> Happening {
        let ts = store.tick();
        // One message in eight names the reader, which is what keeps the
        // mention badge moving while a client watches it.
        let mentions_self = self.random.below(8) == 0;
        let message = self.compose(ts, None, &user, mentions_self);
        if !store.post(&channel, message.clone()) {
            return Happening::Nothing;
        }
        wire.publish(crate::socket::message(&channel, &message));
        Happening::Posted { channel, ts, user }
    }

    fn reply(
        &mut self,
        store: &mut Store,
        wire: &Wire,
        channel: ChannelId,
        user: UserId,
    ) -> Happening {
        // Replying to a followed thread is what the reader notices, so the
        // schedule prefers those and falls back to the newest message.
        let followed = store.followed();
        let parent = match followed.is_empty() || self.random.below(2) == 0 {
            true => store.latest(&channel),
            false => {
                let (found, parent) =
                    followed[self.random.below(followed.len() as u64) as usize].clone();
                if found != channel {
                    return self.reply_in(store, wire, found, parent, user);
                }
                Some(parent)
            }
        };
        let Some(parent) = parent else {
            return Happening::Nothing;
        };
        self.reply_in(store, wire, channel, parent, user)
    }

    fn reply_in(
        &mut self,
        store: &mut Store,
        wire: &Wire,
        channel: ChannelId,
        parent: Ts,
        user: UserId,
    ) -> Happening {
        let ts = store.tick();
        let mentions_self = self.random.below(4) == 0;
        let message = self.compose(ts, Some(parent), &user, mentions_self);
        if !store.post(&channel, message.clone()) {
            return Happening::Nothing;
        }
        wire.publish(crate::socket::message(&channel, &message));
        Happening::Replied {
            channel,
            parent,
            ts,
            user,
        }
    }

    fn react(
        &mut self,
        store: &mut Store,
        wire: &Wire,
        channel: ChannelId,
        user: UserId,
    ) -> Happening {
        let Some(ts) = store.latest(&channel) else {
            return Happening::Nothing;
        };
        if self.emoji.is_empty() {
            return Happening::Nothing;
        }
        let name = self.emoji[self.random.below(self.emoji.len() as u64) as usize].clone();
        if !store.react(&channel, ts, &user, &name, true) {
            return Happening::Nothing;
        }
        wire.publish(crate::socket::reaction(true, &channel, ts, &user, &name));
        Happening::Reacted {
            channel,
            ts,
            user,
            name,
        }
    }

    fn edit(&mut self, store: &mut Store, wire: &Wire, channel: ChannelId) -> Happening {
        let Some(ts) = store.latest(&channel) else {
            return Happening::Nothing;
        };
        let text = format!("{} (edited)", self.sentence());
        if !store.edit(&channel, ts, text, false) {
            return Happening::Nothing;
        }
        let at = store.tick();
        let Some((_, edited)) = store.message(&channel, ts) else {
            return Happening::Nothing;
        };
        wire.publish(crate::socket::edited(&channel, at, edited));
        Happening::Edited { channel, ts }
    }

    fn read(
        &mut self,
        store: &mut Store,
        wire: &Wire,
        channel: ChannelId,
        kind: Kind,
        user: UserId,
    ) -> Happening {
        let Some(ts) = store.latest(&channel) else {
            return Happening::Nothing;
        };
        store.set_read(&channel, user.clone(), ts);
        // Only the signed-in user's own cursor is a frame: Slack does not
        // tell a client what other people have read.
        if user == self.self_id {
            wire.publish(crate::socket::marked(kind, &channel, ts));
        }
        Happening::Read { channel, user, ts }
    }

    fn compose(
        &mut self,
        ts: Ts,
        thread_ts: Option<Ts>,
        user: &UserId,
        mentions_self: bool,
    ) -> Message {
        let mut text = self.sentence().to_owned();
        if mentions_self {
            text = format!("<@{}> {text}", self.self_id.0);
        }
        Message {
            ts,
            thread_ts,
            user: user.clone(),
            text,
            edited: false,
            reply_count: 0,
            latest_reply: None,
            reactions: Vec::new(),
            mentions_self,
            deleted: false,
        }
    }

    fn sentence(&mut self) -> &'static str {
        SAID[self.random.below(SAID.len() as u64) as usize]
    }
}

/// What people say. Enough of it that a transcript does not read as one line
/// repeated, and fixed so that a seed replays word for word.
const SAID: [&str; 12] = [
    "shipping it",
    "that is the second time today",
    "can you take a look when you get a minute",
    "rebased and green",
    "I think the cursor is off by one",
    "meeting moved to the hour",
    "found it, it was the cache",
    "reverted for now",
    "numbers are in the note",
    "nice catch",
    "reading it back this looks right",
    "one more pass and it lands",
];
