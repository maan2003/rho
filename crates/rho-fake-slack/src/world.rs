//! The workspace one number produces.
//!
//! A seed fixes the world: the same seed builds the same conversations, the
//! same people and the same messages, in the same order, so a run against
//! this server replays. The generator is deliberately a seam — it hands back
//! typed conversations and messages and the store takes them — so that
//! eng-8gpr's fixture generator can be called here instead, as a library
//! call, without this crate ever owning fixture data on disk.
//!
//! The defaults are the scale that matters, not the scale that is easy: 300
//! conversations and 450,000 messages, long-tailed, because the user's own
//! workspace is hundreds of conversations and about half a million messages
//! and the message count is what the client's cost is paid against.

use crate::store::Store;
use crate::types::{ChannelId, Conversation, Kind, Message, Reaction, Ts, User, UserId};

/// How big a world to make, and from which number.
#[derive(Clone, Copy, Debug)]
pub struct Seed {
    pub seed: u64,
    pub conversations: usize,
    pub messages: usize,
    pub people: usize,
    /// The wall-clock second the newest message lands on. Fixed rather than
    /// taken from the clock, because a world that depends on when it was made
    /// does not replay.
    pub now: i64,
}

impl Default for Seed {
    fn default() -> Self {
        Self {
            seed: 1,
            conversations: 300,
            messages: 450_000,
            people: 120,
            now: 1_760_000_000,
        }
    }
}

/// The signed-in user's id. The one id in the crate that is a constant,
/// because everything else is generated from the seed.
pub const SELF_ID: &str = "U0RHO";

/// Builds the whole workspace. One pass, no reallocation of a history after
/// it is built, and the store's mention prefix is built as each conversation
/// arrives.
pub fn build(seed: Seed) -> Store {
    let mut random = Random::new(seed.seed);
    let mut store = Store::new(UserId(SELF_ID.to_owned()));

    store.add_user(User {
        id: UserId(SELF_ID.to_owned()),
        handle: "manmeet".to_owned(),
        display: "Manmeet".to_owned(),
        avatar: "https://example.invalid/avatar/self.png".to_owned(),
        bot: false,
    });
    for at in 0..seed.people {
        let handle = format!("{}.{}", FIRST[at % FIRST.len()], LAST[at % LAST.len()]);
        store.add_user(User {
            id: UserId(format!("U{at:05}")),
            display: format!(
                "{} {}",
                capitalised(FIRST[at % FIRST.len()]),
                capitalised(LAST[at % LAST.len()])
            ),
            handle,
            avatar: format!("https://example.invalid/avatar/{at}.png"),
            bot: at % 40 == 39,
        });
    }
    for name in ["tada", "eyes", "thumbsup", "ship-it", "rho"] {
        store.add_emoji(name, &format!("https://example.invalid/emoji/{name}.png"));
    }

    let people: Vec<UserId> = (0..seed.people)
        .map(|at| UserId(format!("U{at:05}")))
        .collect();
    let shares = tail(seed.conversations, seed.messages);
    // The newest message of the busiest conversation is the newest message in
    // the workspace; the rest trail behind it, so the list has an order to
    // sort by that is not the order they were made in.
    for (at, share) in shares.iter().copied().enumerate() {
        let kind = match at % 10 {
            0 | 1 => Kind::Dm,
            2 => Kind::Group,
            3 => Kind::Private,
            _ => Kind::Channel,
        };
        let id = ChannelId(match kind {
            Kind::Dm => format!("D{at:05}"),
            Kind::Group => format!("G{at:05}"),
            _ => format!("C{at:05}"),
        });
        let members: Vec<UserId> = match kind {
            Kind::Dm => vec![people[at % people.len()].clone()],
            Kind::Group => (0..4)
                .map(|n| people[(at + n) % people.len()].clone())
                .collect(),
            _ => (0..12)
                .map(|n| people[(at * 3 + n) % people.len()].clone())
                .collect(),
        };
        let conversation = Conversation {
            id: id.clone(),
            kind,
            name: match kind {
                Kind::Dm => String::new(),
                _ => format!("{}-{}", TOPIC[at % TOPIC.len()], at),
            },
            user: match kind {
                Kind::Dm => Some(members[0].clone()),
                _ => None,
            },
            members: members.clone(),
            muted: at % 17 == 0,
        };
        let (messages, threads) = history(&mut random, &seed, at, share, &members);
        store.add_conversation(conversation, messages);
        let followed = threads.first().map(|(parent, _)| *parent);
        for (parent, replies) in threads {
            store.add_thread(&id, parent, replies);
        }
        // A reader follows some of the threads they are in, and has read part
        // of what is in them: an unread followed thread is the state the
        // client's thread list is interesting in.
        if at % 5 == 0
            && let Some(parent) = followed
        {
            store.follow_thread(id.clone(), parent);
            if at % 10 == 0 {
                store.set_thread_read(&id, parent, UserId(SELF_ID.to_owned()), parent);
            }
        }
        // Everyone has read most of what they have, and the tail of the newest
        // conversations is what is left unread — which is the state a client
        // actually starts in.
        let read_to = share.saturating_sub(random.below(6) as usize);
        if read_to > 0 {
            store.set_read(
                &id,
                UserId(SELF_ID.to_owned()),
                Ts::new(seed.now - (share - read_to) as i64 * 60, 0),
            );
        }
    }
    store.settle();
    store
}

/// One conversation's history, and the threads under it. `share` is the
/// conversation's whole budget: a reply is a message like any other, so a
/// thread of four comes out of the share rather than being added on top of
/// it, and the world holds exactly the number of messages that was asked for.
fn history(
    random: &mut Random,
    seed: &Seed,
    at: usize,
    share: usize,
    members: &[UserId],
) -> (Vec<Message>, Vec<(Ts, Vec<Message>)>) {
    let mut messages = Vec::new();
    let mut threads = Vec::new();
    let mut made = 0;
    // The busiest conversation ends at `now`; quieter ones end further back,
    // so "the conversation with the longest history" and "the conversation
    // that spoke last" are not the same one.
    let end = seed.now - (at as i64 % 30) * 3_600;
    while made < share {
        let step = messages.len();
        let ts = Ts::new(end - (share - made) as i64 * 60, (step % 1_000) as u32);
        let user = members[random.below(members.len() as u64) as usize].clone();
        let mentions_self = random.below(50) == 0;
        let mut message = Message {
            ts,
            thread_ts: None,
            user,
            text: line(random, mentions_self),
            edited: random.below(80) == 0,
            reply_count: 0,
            latest_reply: None,
            reactions: reactions(random, members),
            mentions_self,
        };
        made += 1;
        let room = share - made;
        if room > 0 && random.below(12) == 0 {
            let count = (1 + random.below(6) as usize).min(room);
            let replies: Vec<Message> = (0..count)
                .map(|n| Message {
                    ts: Ts::new(ts.seconds + (n as i64 + 1) * 7, ts.micros),
                    thread_ts: Some(ts),
                    user: members[random.below(members.len() as u64) as usize].clone(),
                    text: line(random, false),
                    edited: false,
                    reply_count: 0,
                    latest_reply: None,
                    reactions: Vec::new(),
                    mentions_self: false,
                })
                .collect();
            message.reply_count = count as u32;
            message.latest_reply = replies.last().map(|reply| reply.ts);
            made += count;
            threads.push((ts, replies));
        }
        messages.push(message);
    }
    (messages, threads)
}

fn reactions(random: &mut Random, members: &[UserId]) -> Vec<Reaction> {
    if random.below(4) != 0 {
        return Vec::new();
    }
    let name = ["tada", "eyes", "thumbsup", "heart"][random.below(4) as usize];
    Vec::from([Reaction {
        name: name.to_owned(),
        users: Vec::from([members[random.below(members.len() as u64) as usize].clone()]),
    }])
}

fn line(random: &mut Random, mentions_self: bool) -> String {
    let mut text = String::new();
    if mentions_self {
        text.push_str("<@");
        text.push_str(SELF_ID);
        text.push_str("> ");
    }
    let words = 4 + random.below(24) as usize;
    for at in 0..words {
        if at > 0 {
            text.push(' ');
        }
        text.push_str(WORDS[random.below(WORDS.len() as u64) as usize]);
    }
    text
}

/// How the messages are shared out: a long tail, so most conversations are
/// small and a few carry the history that costs anything to open. Every
/// conversation gets at least one message, and the shares add up to the total
/// asked for.
fn tail(conversations: usize, messages: usize) -> Vec<usize> {
    if conversations == 0 {
        return Vec::new();
    }
    let weights: Vec<f64> = (0..conversations)
        .map(|at| 1.0 / (at as f64 + 1.0))
        .collect();
    let total: f64 = weights.iter().sum();
    let mut shares: Vec<usize> = weights
        .iter()
        .map(|weight| ((weight / total) * messages as f64).round().max(1.0) as usize)
        .collect();
    // Round-off goes on the busiest conversation, so the total is exact.
    let made: usize = shares.iter().sum();
    match made.cmp(&messages) {
        std::cmp::Ordering::Less => shares[0] += messages - made,
        std::cmp::Ordering::Greater => shares[0] = shares[0].saturating_sub(made - messages).max(1),
        std::cmp::Ordering::Equal => {}
    }
    shares
}

fn capitalised(word: &str) -> String {
    let mut letters = word.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + letters.as_str(),
        None => String::new(),
    }
}

/// SplitMix64: three lines, no dependency, and the same numbers on every
/// machine — which is the whole requirement a replayable world has of it.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E3779B97F4A7C15))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        match bound {
            0 => 0,
            bound => self.next() % bound,
        }
    }
}

const FIRST: [&str; 12] = [
    "ada", "rob", "grace", "alan", "edsger", "barbara", "ken", "dennis", "niklaus", "leslie",
    "john", "margaret",
];
const LAST: [&str; 10] = [
    "lovelace", "pike", "hopper", "turing", "dijkstra", "liskov", "thompson", "ritchie", "wirth",
    "lamport",
];
const TOPIC: [&str; 12] = [
    "design",
    "eng",
    "release",
    "incidents",
    "hiring",
    "support",
    "infra",
    "product",
    "sales",
    "random",
    "reading",
    "onboarding",
];
const WORDS: [&str; 24] = [
    "deploy",
    "ship",
    "review",
    "rebase",
    "merge",
    "revert",
    "latency",
    "cursor",
    "thread",
    "mirror",
    "socket",
    "cache",
    "index",
    "release",
    "profile",
    "budget",
    "keystroke",
    "frame",
    "narrowing",
    "unread",
    "mention",
    "backfill",
    "handoff",
    "rollout",
];
