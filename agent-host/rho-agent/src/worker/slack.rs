//! Who in the workset hears a Slack message, or a reaction to one of the
//! bot's posts ([ARCH-agent-host](../../../specs/ARCH-agent-host.md#slack)).
//!
//! The routes are workset state, in the workset's own database: a route
//! gives a scope to one agent. The scope is the whole workspace, a channel,
//! or one thread; the most specific route wins. The database also keeps
//! which agent wrote each of the bot's posts. Python polls the host's
//! Slack events and asks [`Routes::who_hears`] which agent gets one.

use redb::TableDefinition;
use rho_agent_types::AgentId;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use serde_json::Value;

/// The workspace and channel routes, each with the agent and what it hears.
const WIDE: TableDefinition<Sen<Wide>, Sen<(AgentId, Mode)>> =
    TableDefinition::new("slack_wide_routes");
/// `(channel, thread root ts)` to the agent that hears the whole thread.
const THREADS: TableDefinition<(&str, &str), Sen<AgentId>> =
    TableDefinition::new("slack_thread_routes");
/// `(channel, ts)` of each of the bot's posts from an agent's notebook to
/// that agent and the post's thread root.
const POSTS: TableDefinition<(&str, &str), Sen<(AgentId, String)>> =
    TableDefinition::new("slack_posts");

/// A scope wider than a thread.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub(crate) enum Wide {
    Workspace,
    Channel(String),
}

/// What a workspace or channel route hears besides mentions of the bot.
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

/// A Slack message as routing reads it.
/// Who the bot is, as `auth.test` names it: its user and its bot id.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bot<'a> {
    pub user: &'a str,
    pub id: &'a str,
}

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
    fn of(event: &'a Value, bot: Bot<'_>) -> Option<Self> {
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
        if field("user") == Some(bot.user) || field("bot_id") == Some(bot.id) {
            return None;
        }
        let channel = field("channel")?;
        let ts = field("ts")?;
        let thread = field("thread_ts").unwrap_or(ts);
        let mention = format!("<@{}>", bot.user);
        Some(Self {
            channel,
            thread,
            top_level: thread == ts,
            mentions_bot: field("text").is_some_and(|text| text.contains(&mention)),
        })
    }
}

/// A reaction as routing reads it: someone's, not the bot's, to a message.
#[derive(Debug, PartialEq, Eq)]
struct Reaction<'a> {
    channel: &'a str,
    /// The ts of the message it is on.
    ts: &'a str,
}

impl<'a> Reaction<'a> {
    fn of(event: &'a Value, bot: Bot<'_>) -> Option<Self> {
        let field = |name: &str| event.get(name).and_then(Value::as_str);
        if field("type") != Some("reaction_added") || field("user") == Some(bot.user) {
            return None;
        }
        let item = event.get("item")?;
        let item = |name: &str| item.get(name).and_then(Value::as_str);
        if item("type") != Some("message") {
            return None;
        }
        Some(Self {
            channel: item("channel")?,
            ts: item("ts")?,
        })
    }
}

/// Every route of the workset, for `rho.slack.routes()`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct All {
    pub wide: Vec<(Wide, AgentId, Mode)>,
    pub threads: Vec<((String, String), AgentId)>,
}

pub(crate) struct Routes<'db> {
    db: &'db RhoDb,
}

impl<'db> Routes<'db> {
    /// Make the tables in the workset's database, if new. Once, when it
    /// opens.
    pub(crate) async fn create_tables(db: &RhoDb) {
        let mut write = db.write().await;
        write.open_table(WIDE);
        write.open_table(THREADS);
        write.open_table(POSTS);
        write.commit();
    }

    /// The routes in `db`, the workset's database, whose tables exist.
    pub(crate) fn of(db: &'db RhoDb) -> Self {
        Self { db }
    }

    /// The agent a new message, or a reaction to one of the bot's posts,
    /// goes to, if anyone hears it, with the root of the thread it is in.
    /// A reaction goes to the agent that wrote the post. A thread route
    /// hears every message of its thread; otherwise the channel's route,
    /// else the workspace's, hears mentions of the bot and what its mode
    /// adds. A mention in a thread with no route routes the thread to the
    /// agent it goes to, so the replies that follow reach that agent
    /// before it answers.
    pub(crate) async fn who_hears(&self, event: &Value, bot: Bot<'_>) -> Option<(AgentId, String)> {
        if let Some(reaction) = Reaction::of(event, bot) {
            let read = self.db.read();
            let posts = read.open_table(POSTS);
            let post = posts.get((reaction.channel, reaction.ts))?;
            return Some(post.value().into_owned());
        }
        let message = Message::of(event, bot)?;
        let thread = message.thread.to_owned();
        let key = (message.channel, message.thread);
        let mut write = self.db.write().await;
        if let Some(agent) = write.open_table(THREADS).get(key) {
            return Some((agent.value().into_owned(), thread));
        }
        let (agent, mode) = {
            let wide = write.open_table(WIDE);
            let channel = wide.get(SenValue::owned(Wide::Channel(message.channel.to_owned())));
            let route = match channel {
                Some(route) => route,
                None => wide.get(SenValue::owned(Wide::Workspace))?,
            };
            route.value().into_owned()
        };
        let heard = message.mentions_bot
            || match mode {
                Mode::Mentions => false,
                Mode::TopLevel => message.top_level,
                Mode::All => true,
            };
        if !heard {
            return None;
        }
        if message.mentions_bot {
            write
                .open_table(THREADS)
                .insert(key, SenValue::owned(agent));
            write.commit();
        }
        Some((agent, thread))
    }

    /// The agent of the most specific route that covers a thread, or with
    /// no thread a channel, or with neither the workspace.
    pub(crate) fn owner(&self, channel: Option<&str>, thread: Option<&str>) -> Option<AgentId> {
        let read = self.db.read();
        if let (Some(channel), Some(thread)) = (channel, thread)
            && let Some(agent) = read.open_table(THREADS).get((channel, thread))
        {
            return Some(agent.value().into_owned());
        }
        let wide = read.open_table(WIDE);
        channel
            .and_then(|channel| wide.get(SenValue::owned(Wide::Channel(channel.to_owned()))))
            .or_else(|| wide.get(SenValue::owned(Wide::Workspace)))
            .map(|route| route.value().into_owned().0)
    }

    pub(crate) async fn route_wide(&self, scope: Wide, agent: AgentId, mode: Mode) {
        let mut write = self.db.write().await;
        write
            .open_table(WIDE)
            .insert(SenValue::owned(scope), SenValue::owned((agent, mode)));
        write.commit();
    }

    pub(crate) async fn route_thread(&self, channel: &str, thread: &str, agent: AgentId) {
        let mut write = self.db.write().await;
        write
            .open_table(THREADS)
            .insert((channel, thread), SenValue::owned(agent));
        write.commit();
    }

    /// `agent` wrote the bot's post `ts` in `thread`: reactions to it go to
    /// `agent`, and the thread is routed to `agent` unless it has a route.
    /// Returns the agent the thread routes to.
    pub(crate) async fn posted(
        &self,
        channel: &str,
        ts: &str,
        thread: &str,
        agent: AgentId,
    ) -> AgentId {
        let mut write = self.db.write().await;
        write
            .open_table(POSTS)
            .insert((channel, ts), SenValue::owned((agent, thread.to_owned())));
        let mut threads = write.open_table(THREADS);
        let routed = threads
            .get((channel, thread))
            .map(|owner| owner.value().into_owned());
        let owner = routed.unwrap_or_else(|| {
            threads.insert((channel, thread), SenValue::owned(agent));
            agent
        });
        drop(threads);
        write.commit();
        owner
    }

    pub(crate) async fn unroute_wide(&self, scope: Wide) {
        let mut write = self.db.write().await;
        write.open_table(WIDE).remove(SenValue::owned(scope));
        write.commit();
    }

    pub(crate) async fn unroute_thread(&self, channel: &str, thread: &str) {
        let mut write = self.db.write().await;
        write.open_table(THREADS).remove((channel, thread));
        write.commit();
    }

    pub(crate) fn all(&self) -> All {
        let read = self.db.read();
        All {
            wide: read
                .open_table(WIDE)
                .iter()
                .map(|(scope, route)| {
                    let (agent, mode) = route.value().into_owned();
                    (scope.value().into_owned(), agent, mode)
                })
                .collect(),
            threads: read
                .open_table(THREADS)
                .iter()
                .map(|(key, agent)| {
                    let (channel, thread) = key.value();
                    (
                        (channel.to_owned(), thread.to_owned()),
                        agent.value().into_owned(),
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
