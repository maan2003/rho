//! Who in the workset hears a Slack message
//! ([ARCH-agent-host](../../../specs/ARCH-agent-host.md#slack)).
//!
//! The routes live in `slack.redb` in the workset's state directory: the
//! workset's owner, a rule for each channel an agent changed, and the owner
//! of each thread an agent took part in. Python polls the host's Slack
//! events and asks [`Routes::route`] which agent gets each one; it then hands
//! the event to that agent through `hooks.on_slack`.

use std::path::Path;

use redb::TableDefinition;
use rho_agent_types::AgentId;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use serde_json::Value;

/// The agent that gets what no channel or thread rule gives another. The
/// workset routes Slack only once it has one.
const OWNER: TableDefinition<(), Sen<AgentId>> = TableDefinition::new("slack_owner");
const CHANNELS: TableDefinition<&str, Sen<Channel>> = TableDefinition::new("slack_channels");
/// `(channel, thread root ts)` to the agent that owns the thread.
const THREADS: TableDefinition<(&str, &str), Sen<AgentId>> = TableDefinition::new("slack_threads");

/// What a channel delivers besides mentions and owned threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) enum Mode {
    /// Nothing more.
    Mentions,
    /// Each new top-level message too.
    TopLevel,
    /// Every message.
    All,
}

impl Mode {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "mentions" => Some(Self::Mentions),
            "top_level" => Some(Self::TopLevel),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Mentions => "mentions",
            Self::TopLevel => "top_level",
            Self::All => "all",
        }
    }
}

/// A channel an agent changed. A channel with no row is `Mentions` and
/// belongs to the workset's owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct Channel {
    pub owner: Option<AgentId>,
    pub mode: Mode,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            owner: None,
            mode: Mode::Mentions,
        }
    }
}

/// A Slack message as routing reads it.
#[derive(Debug, PartialEq, Eq)]
struct Message<'a> {
    channel: &'a str,
    /// The root of its thread: its own ts for a top-level message.
    thread: &'a str,
    top_level: bool,
    mentions_bot: bool,
}

impl<'a> Message<'a> {
    /// The new message `event` is, if it is one routing delivers: not an
    /// edit, deletion or reaction, not in a direct conversation, and not
    /// one of the bot's own.
    fn of(event: &'a Value, bot: &str) -> Option<Self> {
        let field = |name: &str| event.get(name).and_then(Value::as_str);
        if field("type") != Some("message") {
            return None;
        }
        if !matches!(
            field("subtype"),
            None | Some("thread_broadcast" | "file_share")
        ) {
            return None;
        }
        if matches!(field("channel_type"), Some("im" | "mpim")) {
            return None;
        }
        if field("user") == Some(bot) {
            return None;
        }
        let channel = field("channel")?;
        let ts = field("ts")?;
        let thread = field("thread_ts").unwrap_or(ts);
        let mention = format!("<@{bot}>");
        Some(Self {
            channel,
            thread,
            top_level: thread == ts,
            mentions_bot: field("text").is_some_and(|text| text.contains(&mention)),
        })
    }
}

/// Every route of the workset, for `rho.slack.routes()`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct All {
    pub owner: Option<AgentId>,
    pub channels: Vec<(String, Channel)>,
    pub threads: Vec<((String, String), AgentId)>,
}

pub(crate) struct Routes {
    db: RhoDb,
}

impl Routes {
    pub(crate) async fn open(path: &Path) -> Self {
        Self::with(RhoDb::open(path)).await
    }

    async fn with(db: RhoDb) -> Self {
        let mut write = db.write().await;
        write.open_table(OWNER);
        write.open_table(CHANNELS);
        write.open_table(THREADS);
        write.commit();
        Self { db }
    }

    pub(crate) fn owner(&self) -> Option<AgentId> {
        self.db
            .read()
            .open_table(OWNER)
            .get(())
            .map(|owner| owner.value().into_owned())
    }

    pub(crate) async fn set_owner(&self, agent: AgentId) {
        let mut write = self.db.write().await;
        write.open_table(OWNER).insert((), SenValue::owned(agent));
        write.commit();
    }

    /// The agent `event` goes to, if anyone hears it. A mention in a thread
    /// nobody owns gives the thread to the agent it goes to, so the replies
    /// that follow reach that agent before it answers.
    pub(crate) async fn route(&self, event: &Value, bot: &str) -> Option<AgentId> {
        let message = Message::of(event, bot)?;
        let mut write = self.db.write().await;
        let owner = write.open_table(OWNER).get(())?.value().into_owned();
        let channel = write
            .open_table(CHANNELS)
            .get(message.channel)
            .map(|channel| channel.value().into_owned())
            .unwrap_or_default();
        let key = (message.channel, message.thread);
        let thread = write
            .open_table(THREADS)
            .get(key)
            .map(|agent| agent.value().into_owned());
        let heard = thread.is_some()
            || message.mentions_bot
            || match channel.mode {
                Mode::Mentions => false,
                Mode::TopLevel => message.top_level,
                Mode::All => true,
            };
        if !heard {
            return None;
        }
        let to = thread.or(channel.owner).unwrap_or(owner);
        if thread.is_none() && message.mentions_bot {
            write.open_table(THREADS).insert(key, SenValue::owned(to));
            write.commit();
        }
        Some(to)
    }

    /// Give the thread to `agent` unless someone owns it. Returns its owner.
    pub(crate) async fn claim(&self, channel: &str, thread: &str, agent: AgentId) -> AgentId {
        let mut write = self.db.write().await;
        let mut threads = write.open_table(THREADS);
        if let Some(owner) = threads.get((channel, thread)) {
            return owner.value().into_owned();
        }
        threads.insert((channel, thread), SenValue::owned(agent));
        drop(threads);
        write.commit();
        agent
    }

    /// Give a thread, or with no thread the whole channel, to `agent`.
    pub(crate) async fn delegate(&self, channel: &str, thread: Option<&str>, agent: AgentId) {
        let mut write = self.db.write().await;
        match thread {
            Some(thread) => {
                write
                    .open_table(THREADS)
                    .insert((channel, thread), SenValue::owned(agent));
            }
            None => {
                let mut channels = write.open_table(CHANNELS);
                let mut rule = channels
                    .get(channel)
                    .map(|rule| rule.value().into_owned())
                    .unwrap_or_default();
                rule.owner = Some(agent);
                channels.insert(channel, SenValue::owned(rule));
            }
        }
        write.commit();
    }

    pub(crate) async fn set_mode(&self, channel: &str, mode: Mode) {
        let mut write = self.db.write().await;
        let mut channels = write.open_table(CHANNELS);
        let mut rule = channels
            .get(channel)
            .map(|rule| rule.value().into_owned())
            .unwrap_or_default();
        rule.mode = mode;
        channels.insert(channel, SenValue::owned(rule));
        drop(channels);
        write.commit();
    }

    pub(crate) fn all(&self) -> All {
        let read = self.db.read();
        All {
            owner: read
                .open_table(OWNER)
                .get(())
                .map(|owner| owner.value().into_owned()),
            channels: read
                .open_table(CHANNELS)
                .iter()
                .map(|(channel, rule)| (channel.value().to_owned(), rule.value().into_owned()))
                .collect(),
            threads: read
                .open_table(THREADS)
                .iter()
                .map(|(key, owner)| {
                    let (channel, thread) = key.value();
                    (
                        (channel.to_owned(), thread.to_owned()),
                        owner.value().into_owned(),
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
