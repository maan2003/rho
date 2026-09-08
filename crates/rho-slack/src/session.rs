//! The live Slack session: one workspace's socket, poll, model, and the
//! conversations the surfaces are reading.
//!
//! Every surface observes this entity and re-reads what it needs, so two
//! panes on the same conversation cannot disagree. The session owns no UI
//! and no storage: it emits [`SessionEvent`], and the host decides what a
//! raised thread means for its inbox, its journal, and its lamp.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AppContext as _, Context, EventEmitter, Task};
use tokio::sync::Notify;

use crate::api::{Client, SearchPage};
use crate::config::{Credentials, Paths};
use crate::events::WsEvent;
use crate::health::{Health, Signal};
use crate::mirror::{Mirror, Scope};
use crate::model::{Change, ConversationRow, Model, Unit, UnitCard};
use crate::socket::{Timings, Wire, poll_feed, run_feed, run_socket};
use crate::types::{ChannelId, Message, Reaction, ThreadKey, Ts, UserId};

/// How often health is re-examined. An outage produces no events at all, so
/// something has to look at the clock.
const TICK: Duration = Duration::from_secs(15);

/// What a surface is showing. A channel, a group, and a DM differ only in
/// their label; a thread differs in what a message sent from it becomes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Conversation(ChannelId),
    Thread(ThreadKey),
}

impl Source {
    pub fn channel(&self) -> &ChannelId {
        match self {
            Self::Conversation(channel) => channel,
            Self::Thread(key) => &key.channel,
        }
    }

    /// The thread a message composed here belongs to: a reply inside the
    /// thread, or a new message in the conversation.
    pub fn thread_ts(&self) -> Option<&Ts> {
        match self {
            Self::Conversation(_) => None,
            Self::Thread(key) => Some(&key.thread_ts),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected,
    /// Terminal: no credentials, or a token Slack refuses. The surface says
    /// so rather than sitting empty.
    Failed(String),
}

/// What the host has to act on. Everything else is read off the session.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    Connected,
    Disconnected(String),
    /// Threads whose obligation changed, for the inbox and the journal.
    Changed(Vec<Change>),
    /// The user's own reply landed in this thread.
    Replied(ThreadKey),
    /// Something the user should be told, once, in the message strip.
    Notice(String),
    Health(Signal),
    /// An answer to the last search the reader asked for. Only the last:
    /// an answer to a query they have already replaced is dropped before
    /// this is emitted.
    Found(Found),
}

/// What came back from a search, ready to be drawn.
///
/// The query is carried with the answer because the surface has to say what
/// it is showing the results *of*, and by the time an answer lands the
/// reader may have typed something else entirely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub query: String,
    pub page: Result<SearchPage, SearchRefused>,
}

/// Why a search did not answer, in the terms the reader is told it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchRefused {
    /// The Slack session rho holds is not allowed to search. Its own case
    /// because it is the one failure the reader can do something about, and
    /// what they do about it is not "try again".
    NotAllowed,
    /// Anything else: the network, Slack, a query Slack would not take.
    Failed,
}

#[derive(Default)]
pub struct Loaded {
    /// Oldest first, the order the surface renders.
    pub messages: Vec<Message>,
    pub loading: bool,
    pub reached_oldest: bool,
    /// The messages a hole sits over: everything newer than one of these,
    /// up to the next message loaded, is unknown. A deal opens on the chunk
    /// its message is in, which on a long history is not the newest chunk.
    pub holes: Vec<Ts>,
    /// Whether the newest message loaded is known not to be the newest
    /// there is: a run caught up one page at a time says so under its last
    /// message until it reaches the live end.
    pub behind_live: bool,
    older_cursor: Option<String>,
    /// Messages sent from here that Slack has not confirmed yet. They sit
    /// in `messages` like any other, shown muted until the echo replaces
    /// them, so the reader sees what they sent without being told it
    /// arrived before it did.
    pending: Vec<Ts>,
    /// Why the last attempt to load this conversation's history failed, if
    /// it did. Drawn as a line above the transcript, and cleared by the next
    /// page that lands — which is why nothing but a load may write it. A
    /// failed send has no page coming to clear it, so its error would sit
    /// there over history the reader has since scrolled past, still saying
    /// so after they retried and got through; a write says what happened in
    /// the notice line instead, once, beside the composer it happened at.
    pub error: Option<String>,
    /// Bumped by every change to `messages`, so a surface knows whether what
    /// it is showing is current.
    revision: u64,
    /// What changed, newest last, so a surface rewrites only those messages
    /// instead of re-rendering the conversation on every socket frame.
    log: Vec<(u64, Update)>,
}

/// One message-sized change to a loaded conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    Inserted(Ts),
    Replaced(Ts),
    Removed(Ts),
}

/// How far back a surface may fall behind and still catch up by applying
/// changes. Beyond it, rebuilding the whole transcript is the cheaper answer
/// anyway.
const LOG_LIMIT: usize = 512;

impl Loaded {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The changes since `revision`, or `None` when the log no longer
    /// reaches back that far and the surface must rebuild.
    pub fn updates_since(&self, revision: u64) -> Option<Vec<Update>> {
        if revision == self.revision {
            return Some(Vec::new());
        }
        if revision > self.revision {
            return None;
        }
        let oldest = self.log.first().map(|(at, _)| *at)?;
        (oldest <= revision + 1).then(|| {
            self.log
                .iter()
                .filter(|(at, _)| *at > revision)
                .map(|(_, update)| update.clone())
                .collect()
        })
    }

    fn record(&mut self, update: Update) {
        self.revision += 1;
        self.log.push((self.revision, update));
        if self.log.len() > LOG_LIMIT {
            self.log.remove(0);
        }
    }

    /// Inserts in timestamp order, ignoring one rho already holds. Both
    /// sources can deliver the same message, and a page can arrive after the
    /// socket.
    fn insert(&mut self, message: Message) {
        let ts = message.ts.clone();
        match self.messages.binary_search_by(|held| {
            held.ts
                .epoch_seconds()
                .total_cmp(&message.ts.epoch_seconds())
        }) {
            Ok(_) => {}
            Err(index) => {
                self.messages.insert(index, message);
                self.record(Update::Inserted(ts));
            }
        }
    }

    /// A message off the socket. A run that has not caught up with the live
    /// end is not next to what just arrived: the jump is a hole, and the
    /// arrival is the live end itself.
    fn insert_live(&mut self, message: Message) {
        if self.behind_live
            && let Some(last) = self.messages.last()
            && message.ts.is_newer_than(&last.ts)
        {
            self.holes.push(last.ts.clone());
            self.behind_live = false;
        }
        self.insert(message);
    }

    /// Overwrites a message in place: an edit, or a reply count that grew.
    fn replace(&mut self, message: Message) -> bool {
        let Some(held) = self.messages.iter_mut().find(|held| held.ts == message.ts) else {
            return false;
        };
        if *held == message {
            return false;
        }
        let ts = message.ts.clone();
        *held = message;
        self.record(Update::Replaced(ts));
        true
    }

    /// Whether this message is still on its way out.
    pub fn is_pending(&self, ts: &Ts) -> bool {
        self.pending.contains(ts)
    }

    /// Shows a message the moment it is sent, under a timestamp of rho's
    /// own. Slack's real one arrives with the echo and takes its place.
    fn hold_local(&mut self, message: Message) {
        self.pending.push(message.ts.clone());
        self.insert(message);
    }

    /// The echo of something sent from here, matched to the local copy by
    /// its text: the reply carries Slack's timestamp, never rho's, so the
    /// words are all the two have in common.
    fn settle_local(&mut self, message: &Message) {
        let Some(index) = self.pending.iter().position(|ts| {
            self.messages
                .iter()
                .any(|held| &held.ts == ts && held.text == message.text)
        }) else {
            return;
        };
        let ts = self.pending.remove(index);
        self.remove(&ts);
    }

    /// Takes back a message Slack refused. The words go back to the
    /// composer, so leaving the line on screen would show it twice.
    fn drop_local(&mut self, ts: &Ts) {
        self.pending.retain(|held| held != ts);
        self.remove(ts);
    }

    /// An emoji on a held message. Slack sends the reaction, never the
    /// message it landed on, so the count is kept here rather than paid for
    /// with a refetch of the whole conversation.
    fn react(&mut self, ts: &Ts, user: &UserId, name: &str, added: bool) -> bool {
        // The messages are in timestamp order, so the one a reaction names
        // is found the way `insert` places one: by search, not by walking
        // the loaded run.
        let Ok(index) = self
            .messages
            .binary_search_by(|held| held.ts.epoch_seconds().total_cmp(&ts.epoch_seconds()))
        else {
            return false;
        };
        let held = &mut self.messages[index];
        let at = held.reactions.iter().position(|held| held.name == name);
        match (at, added) {
            (None, true) => held.reactions.push(Reaction {
                name: name.to_owned(),
                count: 1,
                users: vec![user.clone()],
            }),
            (Some(index), true) => {
                let reaction = &mut held.reactions[index];
                if reaction.users.contains(user) {
                    return false;
                }
                reaction.users.push(user.clone());
                // Slack truncates the user list on a heavily reacted
                // message, so the count is its own number, not a length.
                reaction.count += 1;
            }
            (Some(index), false) => {
                let reaction = &mut held.reactions[index];
                reaction.users.retain(|held| held != user);
                reaction.count = reaction.count.saturating_sub(1);
                if reaction.count == 0 {
                    held.reactions.remove(index);
                }
            }
            (None, false) => return false,
        }
        self.record(Update::Replaced(ts.clone()));
        true
    }

    /// The held message with this timestamp, found by search over the run
    /// rather than by walking it.
    pub fn held(&self, ts: &Ts) -> Option<&Message> {
        let index = self
            .messages
            .binary_search_by(|held| held.ts.epoch_seconds().total_cmp(&ts.epoch_seconds()))
            .ok()?;
        self.messages.get(index)
    }

    fn remove(&mut self, ts: &Ts) -> bool {
        let Some(index) = self.messages.iter().position(|held| &held.ts == ts) else {
            return false;
        };
        self.messages.remove(index);
        self.record(Update::Removed(ts.clone()));
        true
    }
}

pub struct Session {
    client: Option<Arc<Client>>,
    model: Model,
    status: Status,
    health: Health,
    loaded: HashMap<Source, Loaded>,
    /// Fires a feed poll immediately, which is how a reconnect fills the gap
    /// the outage left before the lamp goes out.
    catch_up: Arc<Notify>,
    pending_sends: usize,
    /// Author ids `users.info` has already been asked about, whether or not
    /// it answered. One ask per person, so a channel full of a stranger's
    /// messages is one request, and a request that failed is not retried on
    /// every page.
    asked_names: HashSet<UserId>,
    /// How many searches the reader has asked for. The answer to any but
    /// the last is not shown: see `search`.
    asked: u64,
    /// Files already fetched into the state cache, by Slack file id. An
    /// image is shown from here, so a redraw never refetches.
    cached_files: HashMap<String, std::path::PathBuf>,
    /// What rho already knows, on disk. Surfaces render from here before the
    /// network answers, and a refresh asks only for what it does not hold.
    mirror: Option<Arc<Mirror>>,
    /// Where this rho's Slack files live. Handed in by the caller and
    /// never resolved here; see `config::Paths`.
    paths: Paths,
    /// The conversation last opened, which is the one on screen. Only this
    /// one re-syncs its tail: every conversation ever opened stays in
    /// `loaded`, and re-syncing all of them would spend a request a minute
    /// on each.
    focused: Option<Source>,
    /// Whether a connection has already been made. The roster fetch covers
    /// the first one; a later one is a reconnect, and what happened during
    /// the outage has to be asked for.
    connected_once: bool,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    /// The session for a registered workspace, keeping its files where
    /// `paths` says. Nothing in this crate knows where the user's state
    /// directory is, so nothing in it can open the user's files by
    /// accident: see `config::Paths`.
    pub fn new(credentials: Credentials, paths: Paths, cx: &mut Context<Self>) -> Self {
        match Client::new(credentials.clone()) {
            Ok(client) => Self::with_client(Arc::new(client), paths, cx),
            Err(error) => Self::without_client(credentials, paths, format!("{error:#}")),
        }
    }

    /// A session whose client could not be built at all. There is no
    /// reconnect from here: `client` is set once, in the constructor, and
    /// this workspace will make no request for as long as it exists.
    fn without_client(credentials: Credentials, paths: Paths, reason: String) -> Self {
        Self {
            client: None,
            model: Model::new(credentials.workspace),
            status: Status::Failed(reason),
            health: Health::default(),
            loaded: HashMap::new(),
            catch_up: Arc::new(Notify::new()),
            pending_sends: 0,
            asked_names: HashSet::new(),
            asked: 0,
            cached_files: HashMap::new(),
            mirror: open_mirror(&paths.mirror),
            paths,
            focused: None,
            connected_once: false,
            _tasks: Vec::new(),
        }
    }

    pub fn with_client(client: Arc<Client>, paths: Paths, cx: &mut Context<Self>) -> Self {
        let mut session = Self {
            model: Model::new(client.workspace().clone()),
            client: Some(client.clone()),
            status: Status::Connecting,
            health: Health::default(),
            loaded: HashMap::new(),
            catch_up: Arc::new(Notify::new()),
            pending_sends: 0,
            asked_names: HashSet::new(),
            asked: 0,
            cached_files: HashMap::new(),
            mirror: open_mirror(&paths.mirror),
            paths,
            focused: None,
            connected_once: false,
            _tasks: Vec::new(),
        };
        session.seed_from_mirror();
        session.start(client, Timings::default(), cx);
        session
    }

    /// Starts the two loops and the roster fetch. `timings` is a parameter so
    /// a test can run a whole reconnect inside a few milliseconds.
    pub fn start(&mut self, client: Arc<Client>, timings: Timings, cx: &mut Context<Self>) {
        let (sink, mut wire) = mpsc::unbounded();
        let catch_up = self.catch_up.clone();
        let socket_client = client.clone();
        let feed_client = client.clone();
        let socket_sink = sink.clone();
        let socket_catch_up = catch_up.clone();
        let feed_catch_up = catch_up.clone();
        // The websocket and the feed are tokio IO; GPUI's executor cannot
        // drive them, so they live on the shared runtime and speak to the
        // entity through this channel.
        let socket = gpui_tokio::Tokio::spawn(cx, async move {
            run_socket(socket_client, socket_sink, socket_catch_up, timings).await;
        });
        let feed = gpui_tokio::Tokio::spawn(cx, async move {
            run_feed(feed_client, sink, feed_catch_up, timings).await;
        });
        self._tasks.push(cx.spawn(async move |_, _| {
            let _ = socket.await;
        }));
        self._tasks.push(cx.spawn(async move |_, _| {
            let _ = feed.await;
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            while let Some(event) = wire.next().await {
                if this
                    .update(cx, |session, cx| session.apply(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this
                    .update(cx, |session, cx| {
                        let signal = session.health.tick(now_ms());
                        session.signal(signal, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
        self.load_roster(client, cx);
    }

    /// Names from the last run, before the network says anything. The list
    /// is readable at once and offline, which is the whole point of the
    /// mirror; the roster fetch behind it only corrects what changed.
    fn seed_from_mirror(&mut self) {
        let Some(mirror) = self.mirror.clone() else {
            return;
        };
        let workspace = self.model.workspace().0.clone();
        let users = mirror.users(&workspace);
        if !users.is_empty() {
            self.model.add_users(users);
        }
        let conversations = mirror.conversations(&workspace);
        if !conversations.is_empty() {
            self.model.add_conversations(conversations);
        }
        if let Some(id) = mirror.self_id(&workspace) {
            self.model.set_self(id);
        }
        // Before the units are read, because a channel the reader opted
        // into is one whose ordinary traffic raises units: read the opt-in
        // after and a restart would forget every card it earned.
        self.model.set_watched(mirror.watched(&workspace));
        self.model.set_reacted_with(mirror.reacted_with(&workspace));
        seed_read_cursors(&mut self.model, &mirror);
        // The units as the last run left them: one range scan, one row per
        // unit, no messages read. A mirror written before rho kept them has
        // to work them out from history the once, and says so afterwards so
        // that no later start pays it again.
        match mirror.units_derived(&workspace) {
            true => restore_units(&mut self.model, &mirror),
            false => self.rebuild_units_from_mirror(),
        }
    }

    /// Works the units out from the mirror's own history and writes them
    /// down. The one pass over messages rho makes, on a mirror that predates
    /// the units table or when a rebuild is asked for.
    fn rebuild_units_from_mirror(&mut self) {
        let Some(mirror) = self.mirror.clone() else {
            return;
        };
        self.derive_units_from_mirror();
        let workspace = self.model.workspace().0.clone();
        for unit in self.model.tracked() {
            if let Some(facts) = self.model.unit(&unit) {
                mirror.put_unit(&workspace, &unit, facts);
            }
        }
        mirror.set_units_derived(&workspace);
    }

    /// A restart is another source of the same messages, and like every
    /// other source it may only raise facts. The activity feed is a cursor:
    /// a mention it has already passed is never reported again, so a unit
    /// raised only by a live mention would be gone after a restart even
    /// though rho still holds the message. The units are therefore derived
    /// from the mirror's own history: every DM with messages, every channel
    /// with a mention, every followed thread. `note_message` is what decides
    /// which of those a message is, and its `seen` set makes a repeat a
    /// no-op, so this runs again once Slack has said which threads are
    /// followed and the replies in them are classified then rather than as
    /// channel traffic. Nothing here is a request: the mirror is on disk.
    fn derive_units_from_mirror(&mut self) {
        let Some(mirror) = self.mirror.clone() else {
            return;
        };
        derive_units(&mut self.model, &mirror);
    }

    /// Users, conversations, and unread counts: everything the list surface
    /// needs before a single message arrives.
    fn load_roster(&mut self, client: Arc<Client>, cx: &mut Context<Self>) {
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            let users = client.users().await;
            let conversations = client.conversations().await;
            let counts = client.counts().await;
            let emoji = client.custom_emoji().await;
            // Which threads are the user's is Slack's list, asked for once
            // per connect, the way the web client asks for it.
            let followed = client.followed_threads().await;
            let muted = client.muted_channels().await;
            (users, conversations, counts, emoji, followed, muted)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok((users, conversations, counts, emoji, followed, muted)) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                if let Ok(users) = users {
                    if let Some(mirror) = session.mirror.as_ref() {
                        mirror.put_users(&session.model.workspace().0.clone(), &users);
                    }
                    session.model.add_users(users);
                }
                if let Ok(conversations) = conversations {
                    if let Some(mirror) = session.mirror.as_ref() {
                        mirror.put_conversations(
                            &session.model.workspace().0.clone(),
                            &conversations,
                        );
                    }
                    session.model.add_conversations(conversations);
                }
                if let Ok(counts) = counts {
                    session.model.set_counts(counts.conversations);
                }
                if let Ok(emoji) = emoji {
                    session.model.set_custom_emoji(emoji);
                }
                if let Ok(muted) = muted {
                    session.model.set_muted(muted);
                }
                let mut dropped = Vec::new();
                if let Ok(followed) = followed {
                    // A thread the list stops naming was unfollowed
                    // somewhere else, possibly while rho was off; its card
                    // is discarded on the way in rather than dealt again.
                    dropped = session.model.set_followed(
                        followed
                            .into_iter()
                            .map(|thread| (thread.channel, thread.thread_ts, thread.last_read)),
                    );
                }
                // Now that the followed list is in, the replies in those
                // threads are thread units rather than channel traffic.
                session.derive_units_from_mirror();
                // Slack's cursors have landed and been reconciled with the
                // ones rho already had; the winner of each is what the next
                // start should begin from.
                session.record_cursors();
                // A DM that arrived while rho was off is in the counts and
                // nowhere else: the feed never carries one. Raised here,
                // once the conversations are known, because whether a
                // channel is a DM is a fact about the roster.
                let raised = session.model.unread_dms(now_ms());
                for change in &raised {
                    if let Change::Raised(key) | Change::Updated(key) = change {
                        session.prefetch_ping(&key.clone(), cx);
                        session.ensure_thread_loaded(&key.clone(), cx);
                    }
                }
                // Anything raised before the roster landed was named
                // "#a conversation"; now it has a name, so say so again.
                let fresh = raised
                    .iter()
                    .filter_map(|change| match change {
                        Change::Raised(key) => Some(key.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let known = session
                    .model
                    .tracked()
                    .into_iter()
                    .filter(|key| !fresh.contains(key))
                    .map(Change::Updated)
                    .chain(
                        dropped
                            .into_iter()
                            .map(|key| Change::Muted(Unit::thread(&key.channel, &key.thread_ts))),
                    )
                    .chain(raised)
                    .collect();
                session.announce(known, cx);
                cx.notify();
            });
        }));
    }

    fn apply(&mut self, event: Wire, cx: &mut Context<Self>) {
        let now = now_ms();
        match event {
            Wire::Connected(connection) => {
                self.status = Status::Connected;
                if let Some(mirror) = self.mirror.as_ref() {
                    mirror.set_self_id(&self.model.workspace().0, &connection.self_id);
                }
                self.model.set_self(connection.self_id);
                let signal = self.health.connected(now);
                self.signal(signal, cx);
                // What the outage swallowed is not replayed by Slack: the
                // list's counters and the open transcript are both stale by
                // exactly as long as the socket was down.
                if std::mem::replace(&mut self.connected_once, true) {
                    self.refresh_counts(cx);
                }
                self.resync_tail(cx);
                cx.emit(SessionEvent::Connected);
            }
            Wire::Frame(WsEvent::Message(message)) => {
                self.receive(*message, now, cx);
            }
            Wire::Frame(WsEvent::Edited(message)) => {
                self.edit(*message);
            }
            Wire::Frame(WsEvent::Deleted { channel, ts }) => {
                self.delete(&channel, &ts);
            }
            Wire::Frame(WsEvent::Subscribed { channel, thread_ts }) => {
                // Nothing is raised here: following a thread says its next
                // reply is the user's business, not that one has arrived.
                self.model.follow(&channel, &thread_ts);
            }
            Wire::Frame(WsEvent::Unsubscribed { channel, thread_ts }) => {
                // Ignored here or in another client: either way Slack has
                // said the thread is no longer the user's, and the card goes
                // with it.
                let unit = Unit::thread(&channel, &thread_ts);
                if self.model.unfollow(&channel, &thread_ts) {
                    self.announce(vec![Change::Muted(unit)], cx);
                }
            }
            Wire::Frame(WsEvent::Reacted {
                channel,
                ts,
                user,
                name,
                added,
            }) => {
                self.react(&channel, &ts, &user, &name, added);
            }
            Wire::Frame(WsEvent::Marked { channel, ts }) => {
                // Read elsewhere. Only the badge is stale: reading is not a
                // verdict, so every card stays exactly where it was.
                self.note_read(&Source::Conversation(channel), &ts);
                cx.notify();
            }
            Wire::Frame(WsEvent::ThreadMarked {
                channel,
                thread_ts,
                ts,
            }) => {
                // A thread read elsewhere. Its own cursor moves and the
                // conversation around it is left alone: nobody has said
                // anything about the channel.
                let key = self.model.key(&channel, &thread_ts);
                self.note_read(&Source::Thread(key), &ts);
                cx.notify();
            }
            Wire::Frame(_) => {}
            Wire::Disconnected(reason) => {
                let signal = self.health.disconnected(now, &reason);
                self.signal(signal, cx);
                cx.emit(SessionEvent::Disconnected(reason));
            }
            Wire::Feed(items) => {
                let signal = self.health.feed_ok();
                self.signal(signal, cx);
                self.resync_tail(cx);
                let mut changes = Vec::new();
                for item in &items {
                    if let Some(change) = self.model.note_activity(item, now) {
                        changes.push(change);
                    }
                }
                // The feed carries no message body, so a thread it raises is
                // loaded once before anyone opens it: otherwise the card is a
                // blank line under a nameless conversation.
                for change in &changes {
                    if let Change::Raised(key) | Change::Updated(key) = change {
                        self.prefetch_ping(&key.clone(), cx);
                        self.ensure_thread_loaded(&key.clone(), cx);
                    }
                }
                self.announce(changes, cx);
            }
            Wire::FeedFailed(error) => {
                let signal = self.health.feed_failed(&error);
                self.signal(signal, cx);
            }
        }
        cx.notify();
    }

    fn receive(&mut self, message: Message, now: i64, cx: &mut Context<Self>) {
        // The real thing takes the local copy's place, whichever arrives
        // first: Slack echoes a sent message down the socket as well as
        // answering the post with it.
        if message.user.as_ref() == Some(self.model.self_id()) {
            for source in self.sources_for(&message.channel, &message.thread_root()) {
                if let Some(loaded) = self.loaded.get_mut(&source) {
                    loaded.settle_local(&message);
                }
            }
        }
        // The counters move for channel traffic too: the list is the whole
        // workspace, and `note_message` answers only about cards.
        self.learn_names(std::slice::from_ref(&message), cx);
        self.model.note_counts(&message);
        let change = self.model.note_message(&message, now);
        self.route(&message);
        self.announce(change.into_iter().collect(), cx);
    }

    /// Puts an arriving message into every open surface it belongs to: the
    /// thread it was said in, and the channel only when it was said to the
    /// room. A reply never appears in the channel body; what changes there
    /// is the count line under its parent.
    fn route(&mut self, message: &Message) {
        // The mirror follows the socket, so a conversation that was open when
        // the message arrived reads the same after a restart.
        if let Some(mirror) = self.mirror.as_ref() {
            let workspace = self.model.workspace().0.clone();
            let thread = Scope::thread(&workspace, &message.channel, &message.thread_root());
            mirror_live(mirror, &thread, message);
            if message.is_top_level() {
                let conversation = Scope::conversation(&workspace, &message.channel);
                mirror_live(mirror, &conversation, message);
            }
        }
        let conversation = Source::Conversation(message.channel.clone());
        let thread = Source::Thread(ThreadKey {
            workspace: self.model.workspace().clone(),
            channel: message.channel.clone(),
            thread_ts: message.thread_root(),
        });
        if let Some(loaded) = self.loaded.get_mut(&thread) {
            loaded.insert_live(message.clone());
        }
        let Some(loaded) = self.loaded.get_mut(&conversation) else {
            return;
        };
        if message.is_top_level() {
            loaded.insert_live(message.clone());
        }
        if message.thread_ts.is_some() {
            // A reply is not channel content, but the fact that the thread
            // grew is: the count line is how the reader learns it.
            let root = message.thread_root();
            let grown = loaded
                .messages
                .iter()
                .find(|candidate| candidate.ts == root)
                .map(|parent| {
                    let mut parent = parent.clone();
                    parent.reply_count = parent.reply_count.saturating_add(1);
                    parent.latest_reply = Some(message.ts.clone());
                    parent
                });
            if let Some(parent) = grown {
                loaded.replace(parent);
            }
        }
    }

    /// An edit overwrites the message in place, in the mirror and in every
    /// open surface. Nothing else about the conversation moves.
    fn edit(&mut self, message: Message) {
        if let Some(mirror) = self.mirror.as_ref() {
            let workspace = self.model.workspace().0.clone();
            let thread = Scope::thread(&workspace, &message.channel, &message.thread_root());
            mirror.insert_messages(&thread, std::slice::from_ref(&message));
            if message.is_top_level() {
                let conversation = Scope::conversation(&workspace, &message.channel);
                mirror.insert_messages(&conversation, std::slice::from_ref(&message));
            }
        }
        for source in self.sources_for(&message.channel, &message.thread_root()) {
            if let Some(loaded) = self.loaded.get_mut(&source) {
                loaded.replace(message.clone());
            }
        }
    }

    /// A deletion takes the message out of the mirror and out of every open
    /// surface: what the author withdrew is not left on screen.
    fn delete(&mut self, channel: &ChannelId, ts: &Ts) {
        let workspace = self.model.workspace().0.clone();
        let threads = self
            .loaded
            .keys()
            .filter_map(|source| match source {
                Source::Thread(key) if &key.channel == channel => Some(key.thread_ts.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some(mirror) = self.mirror.as_ref() {
            mirror.remove_message(&Scope::conversation(&workspace, channel), ts);
            for thread in &threads {
                mirror.remove_message(&Scope::thread(&workspace, channel, thread), ts);
            }
        }
        let sources = self
            .loaded
            .keys()
            .filter(|source| source.channel() == channel)
            .cloned()
            .collect::<Vec<_>>();
        for source in sources {
            if let Some(loaded) = self.loaded.get_mut(&source) {
                loaded.remove(ts);
            }
        }
    }

    /// A reaction from any client, applied in place to every open surface
    /// holding the message and to the mirror behind them.
    fn react(&mut self, channel: &ChannelId, ts: &Ts, user: &UserId, name: &str, added: bool) {
        let sources = self
            .loaded
            .keys()
            .filter(|source| source.channel() == channel)
            .cloned()
            .collect::<Vec<_>>();
        let mut reacted = None;
        for source in sources {
            if let Some(loaded) = self.loaded.get_mut(&source)
                && loaded.react(ts, user, name, added)
            {
                reacted = loaded.held(ts).cloned();
            }
        }
        let Some(message) = reacted else {
            return;
        };
        if let Some(mirror) = self.mirror.as_ref() {
            let workspace = self.model.workspace().0.clone();
            let thread = Scope::thread(&workspace, channel, &message.thread_root());
            mirror.insert_messages(&thread, std::slice::from_ref(&message));
            if message.is_top_level() {
                let conversation = Scope::conversation(&workspace, channel);
                mirror.insert_messages(&conversation, std::slice::from_ref(&message));
            }
        }
    }

    /// Asks whatever is on screen for anything newer than its last message.
    /// The socket is the fast path, not the reliable one: a socket that dies
    /// without saying so delivers nothing, and this is what makes that case
    /// a minute of lag instead of silence.
    ///
    /// A thread is on screen the same way a conversation is, and an outage
    /// swallows its replies the same way. Both ask a bounded question — what
    /// is newer than the last message held — so the request costs what is
    /// new, not the whole of what is there.
    fn resync_tail(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.focused.clone() else {
            return;
        };
        let Some(loaded) = self.loaded.get(&source) else {
            return;
        };
        if loaded.loading {
            return;
        }
        let Some(since) = loaded.messages.last().map(|last| last.ts.clone()) else {
            return;
        };
        self.fetch(source, None, false, Some(since), cx);
    }

    /// Names for the authors the roster has no name for.
    ///
    /// The roster is asked for once per connect, so anyone who joined since
    /// has no name here, and a page that landed before it did has none
    /// either. `Model::author` reads "someone" for both, and it read
    /// "someone" for the rest of the run: nothing asked Slack who they were.
    /// This is what asks — one `users.info` per unknown id, once, and the
    /// answer goes into the model and the mirror as the fact it is.
    ///
    /// Cost: the ids in what just arrived that are new, which is bounded by
    /// the messages it brought and is nothing at all once the roster covers
    /// them.
    /// Asks Slack what people said, and emits the answer when it comes.
    ///
    /// Nothing is read from the mirror and nothing is written to it: the
    /// mirror holds only what rho has already paged, so answering from it
    /// would answer "what have I read" rather than the question. The answer
    /// is a list of places and is not a fact about the workspace, so it goes
    /// to whoever asked as an event and the model never hears about it.
    ///
    /// A query in flight when a second is asked for is abandoned rather than
    /// raced: `asked` counts queries, and an answer that is not the current
    /// one is dropped, so what the reader is shown is always the last thing
    /// they typed and never an older answer that took longer.
    pub fn search(&mut self, query: &str, page: u32, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.asked += 1;
        let asked = self.asked;
        let query = query.to_owned();
        let task = gpui_tokio::Tokio::spawn(cx, {
            let query = query.clone();
            async move { client.search_messages(&query, page).await }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let answer = task.await;
            let _ = this.update(cx, |session, cx| {
                if session.asked != asked {
                    return;
                }
                let page = match answer {
                    Ok(Ok(page)) => Ok(page),
                    // The refusal rho can name is the one the reader can act
                    // on: Slack answers a session that may not search with
                    // one of these two, and every other failure is a failure.
                    Ok(Err(error)) => match refused_search(&format!("{error}")) {
                        true => Err(SearchRefused::NotAllowed),
                        false => Err(SearchRefused::Failed),
                    },
                    Err(_) => Err(SearchRefused::Failed),
                };
                cx.emit(SessionEvent::Found(Found { query, page }));
            });
        }));
    }

    fn learn_names(&mut self, messages: &[Message], cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let model = &self.model;
        let asked = &mut self.asked_names;
        let unknown = messages
            .iter()
            .filter_map(|message| message.user.clone())
            .filter(|id| !model.knows_user(id) && asked.insert(id.clone()))
            .collect::<Vec<_>>();
        for id in unknown {
            let client = client.clone();
            let task = gpui_tokio::Tokio::spawn(cx, async move { client.user_info(&id).await });
            self._tasks.push(cx.spawn(async move |this, cx| {
                let Ok(Ok(user)) = task.await else {
                    return;
                };
                let _ = this.update(cx, |session, cx| {
                    let workspace = session.model.workspace().0.clone();
                    if let Some(mirror) = session.mirror.as_ref() {
                        mirror.put_users(&workspace, std::slice::from_ref(&user));
                    }
                    session.model.add_users([user]);
                    // The name is a fact the surfaces render from, so this
                    // is the only thing owed them: what carried "someone"
                    // reads it again on the next draw.
                    cx.notify();
                });
            }));
        }
    }

    /// The unread counts again, after an outage. Nothing else in the roster
    /// goes stale off a dropped socket, so nothing else is asked for.
    fn refresh_counts(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let task = gpui_tokio::Tokio::spawn(cx, async move { client.counts().await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok(Ok(counts)) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                session.model.set_counts(counts.conversations);
                cx.notify();
            });
        }));
    }

    /// The open surfaces a message belongs to: its conversation, and the
    /// thread it was said in.
    fn sources_for(&self, channel: &ChannelId, thread_ts: &Ts) -> Vec<Source> {
        vec![
            Source::Conversation(channel.clone()),
            Source::Thread(ThreadKey {
                workspace: self.model.workspace().clone(),
                channel: channel.clone(),
                thread_ts: thread_ts.clone(),
            }),
        ]
    }

    fn announce(&mut self, changes: Vec<Change>, cx: &mut Context<Self>) {
        if changes.is_empty() {
            return;
        }
        self.write_units(&changes);
        cx.emit(SessionEvent::Changed(changes));
    }

    /// Puts the units a change moved on disk, and only those.
    ///
    /// This is what a start reads instead of the history: the facts are
    /// written when they move, so opening rho costs the units rho tracks
    /// rather than the messages it holds. A muted thread's row goes, since
    /// the follow that made it a unit is gone.
    fn write_units(&self, changes: &[Change]) {
        let Some(mirror) = self.mirror.as_ref() else {
            return;
        };
        let workspace = self.model.workspace().0.clone();
        for change in changes {
            match change {
                Change::Raised(unit) | Change::Updated(unit) | Change::Replied(unit) => {
                    if let Some(facts) = self.model.unit(unit) {
                        mirror.put_unit(&workspace, unit, facts);
                    }
                }
                Change::Muted(unit) => mirror.remove_unit(&workspace, unit),
            }
        }
    }

    fn signal(&mut self, signal: Option<Signal>, cx: &mut Context<Self>) {
        if let Some(signal) = signal {
            cx.emit(SessionEvent::Health(signal));
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Whether there is a listing to draw at all, as against a status line.
    pub fn has_rows(&self) -> bool {
        self.model.conversation_count() > 0
    }

    /// Narrows the list to the conversations a typed query reaches. The
    /// model answers it from its word index and logs which rows left and
    /// which arrived.
    pub fn narrow(&mut self, query: &str) {
        self.model.narrow(query);
    }

    /// The conversations a query reaches, in the list's own order, for a
    /// caller showing matches without narrowing: the minibuffer offering
    /// names as the reader types.
    pub fn reached_by(&self, query: &str, most: usize) -> Vec<crate::model::ConversationRow> {
        self.model.reached_by(query, most)
    }

    /// What the reader has typed to narrow the list.
    pub fn query(&self) -> String {
        self.model.query()
    }

    /// How many conversations there are, before a query narrows them.
    pub fn conversation_count(&self) -> usize {
        self.model.conversation_count()
    }

    /// Whether a query stands, so a drawer with nothing to draw can say
    /// that nothing matches rather than talk about the socket.
    pub fn is_narrowed(&self) -> bool {
        self.model.is_narrowed()
    }

    /// Why the narrowed list is empty, when it is, so the drawer can say
    /// which of the two happened.
    pub fn empty_narrowing(&self) -> Option<crate::model::Empty> {
        self.model.empty_narrowing()
    }

    /// What the conversation list has done since the drawer last asked.
    /// `None` when the drawer has to write the listing again.
    pub fn take_row_edits(&mut self) -> Option<Vec<crate::model::RowEdit>> {
        self.model.take_row_edits()
    }

    pub fn forget_row_edits(&mut self) {
        self.model.forget_row_edits();
    }

    /// Where a dealt card lands the reader: the oldest message from someone
    /// else past the cursor their last verdict left. Three mentions in a
    /// channel are one card, and this is the first of the three.
    pub fn oldest_from_other_after(&self, unit: &Unit, cursor: Option<&Ts>) -> Option<Ts> {
        oldest_from_other_after(&self.model, self.mirror.as_deref()?, unit, cursor)
    }

    /// The line a card shows for a unit, rendered from the mirror now
    /// rather than kept from when the message landed. A name the roster
    /// only supplied on the second connection is why: a summary frozen at
    /// ingest says `<@U123>` on every cold start, and this says `@ada`.
    /// Every unit rho is tracking, as a card, in the words the mirror has
    /// now. This is the crate's answer to "what do you know about the units
    /// you track"; the host maps it onto whatever a card is on its own desk
    /// and decides nothing about it here.
    ///
    /// The superset of [`Model::cards`], which is the units currently
    /// asking and is what a dealer wants. A unit whose messages have all
    /// been read is still here, with `attention: None`: it is not a card,
    /// but Find reaches it and a card already on the desk needs its facts to
    /// say it has gone quiet. The cost is one pass over the units, never
    /// over the messages — the facts are already the answer.
    pub fn tracked_cards(&self, now_ms: i64) -> Vec<UnitCard> {
        self.model
            .tracked()
            .into_iter()
            .filter_map(|unit| {
                let mut card = self.model.card(&unit, now_ms)?;
                card.title = self.unit_summary(&unit);
                Some(card)
            })
            .collect()
    }

    pub fn unit_summary(&self, unit: &Unit) -> String {
        match self.mirror.as_deref() {
            Some(mirror) => unit_summary(&self.model, mirror, unit),
            None => String::new(),
        }
    }

    pub fn health_reason(&self) -> Option<&str> {
        self.health.reason()
    }

    pub fn pending_sends(&self) -> usize {
        self.pending_sends
    }

    pub fn rows(&self) -> Vec<ConversationRow> {
        self.model.conversation_rows()
    }

    pub fn loaded(&self, source: &Source) -> Option<&Loaded> {
        self.loaded.get(source)
    }

    /// The label a surface titles itself with: `#design`, `@ada`, or the
    /// conversation a thread hangs under.
    pub fn label(&self, source: &Source) -> String {
        let label = self.model.label(source.channel());
        match source {
            Source::Conversation(_) => label,
            Source::Thread(_) => format!("{label} · thread"),
        }
    }

    /// Loads a conversation's newest page the first time it is entered.
    pub fn open(&mut self, source: &Source, cx: &mut Context<Self>) {
        self.focused = Some(source.clone());
        if self.loaded.contains_key(source) {
            return;
        }
        // The mirror answers first, so the conversation is on screen before
        // the network is asked anything. What it holds also bounds the
        // request: only messages newer than its newest are fetched.
        let cached = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.newest_chunk(&self.scope(source), MIRROR_PAGE))
            .unwrap_or_default();
        let since = refresh_since(&cached);
        let reached_oldest = self
            .mirror
            .as_ref()
            .is_some_and(|mirror| mirror.history_begins(&self.scope(source)));
        self.loaded.insert(
            source.clone(),
            Loaded {
                messages: cached,
                reached_oldest,
                // The mirror's own run is not a change, it is where the
                // surface starts: a revision past zero says "you have seen
                // nothing of this", so the first render is one bulk insert.
                revision: 1,
                ..Loaded::default()
            },
        );
        self.fetch(source.clone(), None, true, since, cx);
    }

    /// Brings the chunk holding `ts` into an open conversation. A deal is
    /// answered on the message it is about, which on a long history is a
    /// different chunk from the newest one. Both come off the mirror, so
    /// this costs no request; the hole between them is recorded rather than
    /// papered over.
    pub fn open_at(&mut self, source: &Source, ts: &Ts, cx: &mut Context<Self>) {
        self.open(source, cx);
        if self.land_on(source, ts, cx) {
            return;
        }
        self.fetch_window_around(source, ts, cx);
    }

    /// Puts the chunk holding `ts` in front of the reader, and says whether
    /// it could. False means the mirror holds nothing there — the message
    /// is real, rho has simply never paged that part of the conversation.
    fn land_on(&mut self, source: &Source, ts: &Ts, cx: &mut Context<Self>) -> bool {
        let scope = self.scope(source);
        let Some(mirror) = self.mirror.clone() else {
            return false;
        };
        let Some(loaded) = self.loaded.get_mut(source) else {
            return false;
        };
        if loaded.messages.iter().any(|held| &held.ts == ts) {
            return true;
        }
        let chunk = mirror.chunk_containing(&scope, ts, MIRROR_PAGE);
        let Some(newest) = chunk.last().map(|message| message.ts.clone()) else {
            return false;
        };
        for message in chunk {
            loaded.insert(message);
        }
        loaded.holes.push(newest);
        // The bottom of the run moved down to this chunk: what sits under it
        // is the chunk's own gap, and the paging cursor from the newest
        // chunk's page would ask about the wrong place entirely.
        loaded.older_cursor = None;
        loaded.reached_oldest =
            mirror.gap_at_or_below(&scope, ts).is_none() && mirror.history_begins(&scope);
        cx.notify();
        true
    }

    /// Fetches the conversation around a message rho holds nothing near, and
    /// lands on it when the window arrives.
    ///
    /// The same two bounded calls `prefetch_ping` makes, and for the same
    /// reason: something outside the mirror — a card, or a search hit the
    /// reader chose — names a place, and the reader is owed the place rather
    /// than the newest messages in the conversation, which is where opening
    /// would otherwise leave them with no sign that it did.
    fn fetch_window_around(&mut self, source: &Source, ts: &Ts, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // A thread is fetched whole by the thread load, so a thread source
        // never needs a window cut out of the middle of it.
        let Source::Conversation(channel) = source.clone() else {
            return;
        };
        let scope = self.scope(source);
        let source = source.clone();
        let ts = ts.clone();
        let task = gpui_tokio::Tokio::spawn(cx, {
            let channel = channel.clone();
            let ts = ts.clone();
            async move {
                client
                    .conversations_history_around(&channel, &ts, PING_WINDOW)
                    .await
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok(Ok(messages)) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                session.mirror_page(&source, &messages, false, false);
                session.learn_names(&messages, cx);
                if let Some(mirror) = session.mirror.as_ref() {
                    // The window is an island: what sits under it is
                    // unknown, and saying so is what stops the surface
                    // showing it as continuous history.
                    mirror_island(mirror, &scope, &messages);
                }
                session.land_on(&source, &ts, cx);
            });
        }));
    }

    /// Fills a hole from below: one page forward from the message it sits
    /// over, which is what scrolling onto the row asks for. One page per
    /// action, the same rule as paging back.
    pub fn load_newer(&mut self, source: &Source, after: Ts, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // A thread is fetched whole, so it never has a hole in the middle.
        let Source::Conversation(channel) = source.clone() else {
            return;
        };
        let Some(loaded) = self.loaded.get_mut(source) else {
            return;
        };
        // The hole under the last message loaded is not recorded: it is the
        // run not having caught up, and which message it sits over changes
        // with every page.
        let tail =
            loaded.behind_live && loaded.messages.last().is_some_and(|last| last.ts == after);
        if loaded.loading || !(tail || loaded.holes.contains(&after)) {
            return;
        }
        loaded.loading = true;
        // The chunk over the hole, which is what the page has to reach for
        // the hole to be closed.
        let above = loaded
            .messages
            .iter()
            .find(|message| message.ts.is_newer_than(&after))
            .map(|message| message.ts.clone());
        let scope = self.scope(source);
        let source = source.clone();
        let task = gpui_tokio::Tokio::spawn(cx, {
            let after = after.clone();
            async move { client.conversations_history_since(&channel, &after).await }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok(page) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                let mut fetched = Vec::new();
                let mut closed = false;
                {
                    let Some(loaded) = session.loaded.get_mut(&source) else {
                        return;
                    };
                    loaded.loading = false;
                    match page {
                        Ok(page) => {
                            loaded.error = None;
                            fetched = page.messages.clone();
                            let newest = fetched.last().map(|message| message.ts.clone());
                            closed = match (&above, &newest) {
                                // The page ran into the chunk above: the two
                                // runs are one now.
                                (Some(above), Some(newest)) => !above.is_newer_than(newest),
                                // Nothing above: the hole ends at the live
                                // end, and a short page is how that is known.
                                (None, _) => !page.has_more,
                                (Some(_), None) => false,
                            };
                            for message in page.messages {
                                loaded.insert(message);
                            }
                            loaded.holes.retain(|hole| hole != &after);
                            match (closed, newest) {
                                // A hole between two chunks moves down to
                                // the page's own end; the tail's is not
                                // recorded at all, it is `behind_live`.
                                (false, Some(newest)) if above.is_some() => {
                                    loaded.holes.push(newest)
                                }
                                (true, _) if above.is_none() => loaded.behind_live = false,
                                _ => {}
                            }
                        }
                        Err(error) => loaded.error = Some(format!("{error:#}")),
                    }
                }
                if let Some(mirror) = session.mirror.as_ref() {
                    mirror.insert_messages(&scope, &fetched);
                    if let (true, Some(above)) = (closed, above) {
                        mirror.clear_gap(&scope, &above);
                    }
                }
                session.learn_names(&fetched, cx);
                cx.notify();
            });
        }));
    }

    /// Pages further back, which is what scrolling to the top asks for.
    /// Fills the history above what is loaded, which is what scrolling near
    /// the top asks for. One page in flight per conversation, so a burst of
    /// scroll events is one request; a conversation whose beginning is known
    /// costs nothing at all.
    pub fn load_older(&mut self, source: &Source, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded.get(source) else {
            return;
        };
        let scope = self.scope(source);
        // The gap under the bottom of what is loaded, which after a deal is
        // the dealt chunk's, not the newest chunk's.
        let oldest = loaded.messages.first().map(|message| message.ts.clone());
        let gap = self.mirror.as_ref().and_then(|mirror| {
            if mirror.history_begins(&scope) {
                return None;
            }
            match &oldest {
                Some(oldest) => mirror.gap_at_or_below(&scope, oldest),
                None => mirror.gap_below(&scope, None).map(|(at, _)| at),
            }
        });
        let Some(request) = older_request(
            loaded.loading,
            loaded.reached_oldest,
            loaded.older_cursor.clone(),
            gap,
        ) else {
            // Nothing to ask for is not the same as nothing older: only a
            // page that came back short says the conversation has a start.
            if let Some(loaded) = self.loaded.get_mut(source)
                && loaded.older_cursor.is_none()
                && self
                    .mirror
                    .as_ref()
                    .is_none_or(|mirror| mirror.history_begins(&scope))
            {
                loaded.reached_oldest = true;
            }
            return;
        };
        match request {
            Older::Cursor(cursor) => self.fetch(source.clone(), Some(cursor), false, None, cx),
            Older::Before(latest) => self.fill_gap(source.clone(), latest, cx),
        }
    }

    /// Pages back from a gap's own cursor, which is how a conversation
    /// restored from the mirror grows upwards: the in-memory paging cursor
    /// died with the last run, the gap record did not.
    fn fill_gap(&mut self, source: Source, latest: Ts, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // Set here rather than by the caller, because here is where it is
        // cleared: a request that is never made must not leave the
        // conversation saying it is loading, since that flag is also the
        // gate on asking again.
        self.mark_loading(&source);
        let request = source.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            match &request {
                Source::Conversation(channel) => {
                    client.conversations_history_before(channel, &latest).await
                }
                Source::Thread(key) => {
                    client
                        .conversations_replies(&key.channel, &key.thread_ts, None)
                        .await
                }
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok(page) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                let mut fetched = Vec::new();
                let mut reached_oldest = false;
                {
                    let loaded = session.loaded.entry(source.clone()).or_default();
                    loaded.loading = false;
                    match page {
                        Ok(page) => {
                            loaded.error = None;
                            reached_oldest = page.older_cursor.is_none();
                            loaded.reached_oldest = reached_oldest;
                            loaded.older_cursor = page.older_cursor;
                            fetched = page.messages.clone();
                            for message in page.messages {
                                loaded.insert(message);
                            }
                        }
                        Err(error) => loaded.error = Some(format!("{error:#}")),
                    }
                }
                session.mirror_page(&source, &fetched, true, reached_oldest);
                session.learn_names(&fetched, cx);
                cx.notify();
            });
        }));
    }

    /// The context a ping needs, fetched once when the feed names it: the
    /// window of the conversation on both sides of the pinging message, so
    /// the card opens from the mirror with no network wait. Two bounded
    /// calls, never a page back, never a second conversation, and never at
    /// all when the mirror already holds the message. This is what the web
    /// client fetches when the notification is clicked; rho does it a moment
    /// earlier.
    fn prefetch_ping(&mut self, unit: &Unit, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // The message that pinged is the unit's newest; a followed thread's
        // window opens around its root, which is where its replies hang.
        let Some(ts) = unit
            .thread
            .clone()
            .or_else(|| Some(self.model.unit(unit)?.newest.clone()))
        else {
            return;
        };
        let scope = Scope::conversation(&self.model.workspace().0, &unit.channel);
        if self
            .mirror
            .as_ref()
            .is_none_or(|mirror| mirror.holds(&scope, &ts))
        {
            return;
        }
        let source = Source::Conversation(unit.channel.clone());
        let channel = unit.channel.clone();
        let scope = scope.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client
                .conversations_history_around(&channel, &ts, PING_WINDOW)
                .await
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok(Ok(messages)) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                // Straight to the mirror: nobody has opened this conversation,
                // so there is no surface to feed and no read marker to move.
                session.mirror_page(&source, &messages, false, false);
                session.learn_names(&messages, cx);
                if let Some(mirror) = session.mirror.as_ref() {
                    mirror_island(mirror, &scope, &messages);
                }
                cx.notify();
            });
        }));
    }

    /// Loads a thread the feed raised but nobody has opened. The feed says
    /// only *that* a thread changed, so without this the card would carry no
    /// summary and the conversation would have no name. Reading it here must
    /// not mark it read: nobody has seen it yet.
    fn ensure_thread_loaded(&mut self, unit: &Unit, cx: &mut Context<Self>) {
        // Only a followed thread has replies of its own to load. A
        // conversation's window came from the ping prefetch, and asking for
        // it again would be a request the web client never makes.
        let Some(root) = unit.thread.clone() else {
            return;
        };
        let source = Source::Thread(self.model.key(&unit.channel, &root));
        if self.loaded.contains_key(&source) {
            return;
        }
        self.loaded.insert(source.clone(), Loaded::default());
        self.fetch(source, None, false, None, cx);
    }

    /// Says a request is in flight for this conversation. Only ever called
    /// once the request is certain to be made.
    fn mark_loading(&mut self, source: &Source) {
        if let Some(loaded) = self.loaded.get_mut(source) {
            loaded.loading = true;
        }
    }

    fn fetch(
        &mut self,
        source: Source,
        cursor: Option<String>,
        mark_read: bool,
        since: Option<Ts>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // See `fill_gap`: the function that clears the flag is the one that
        // sets it, after the client check.
        self.mark_loading(&source);
        let request = source.clone();
        // Kept out of the request future, which takes ownership: the result
        // handler needs to know whether this was a bounded tail fetch.
        let bounded = since.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            match &request {
                Source::Conversation(channel) => match &since {
                    Some(since) => client.conversations_history_since(channel, since).await,
                    None => {
                        client
                            .conversations_history(channel, cursor.as_deref())
                            .await
                    }
                },
                Source::Thread(key) => match &since {
                    Some(since) => {
                        client
                            .conversations_replies_since(&key.channel, &key.thread_ts, since)
                            .await
                    }
                    None => {
                        client
                            .conversations_replies(&key.channel, &key.thread_ts, cursor.as_deref())
                            .await
                    }
                },
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok(page) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                let now = now_ms();
                let mut changes = Vec::new();
                let mut fetched = Vec::new();
                let mut reached_oldest = false;
                {
                    let loaded = session.loaded.entry(source.clone()).or_default();
                    loaded.loading = false;
                    match page {
                        Ok(page) => {
                            loaded.error = None;
                            // A tail fetch says nothing about how far back the
                            // history goes: it asked only for what is new.
                            if bounded.is_none() {
                                reached_oldest = page.older_cursor.is_none();
                                loaded.reached_oldest = reached_oldest;
                                loaded.older_cursor = page.older_cursor;
                            } else {
                                // A catch-up page runs forward from what the
                                // mirror held. More behind it means the run
                                // still stops short of the live end. A
                                // thread is never behind the live end: it is
                                // read from its root, and a bounded ask
                                // reaches the last reply or there was none.
                                loaded.behind_live =
                                    page.has_more && matches!(source, Source::Conversation(_));
                            }
                            fetched = page.messages.clone();
                            for message in page.messages {
                                loaded.insert(message);
                            }
                        }
                        Err(error) => loaded.error = Some(format!("{error:#}")),
                    }
                }
                session.mirror_page(&source, &fetched, bounded.is_none(), reached_oldest);
                session.learn_names(&fetched, cx);
                let messages = session
                    .loaded
                    .get(&source)
                    .map(|loaded| loaded.messages.clone())
                    .unwrap_or_default();
                for message in &messages {
                    // A loaded page is how rho learns that the user answered
                    // a thread from the Slack app: their own reply here is
                    // the same done verdict it would be live.
                    match session.model.note_message(message, now) {
                        Some(change) => changes.push(change),
                        // Already counted from the feed, which carries no
                        // body: this is where the card gets its summary.
                        None => changes.extend(session.model.note_loaded(message)),
                    }
                }
                session.announce(changes, cx);
                if mark_read {
                    session.mark_read(&source, cx);
                }
                cx.notify();
            });
        }));
    }

    /// Where a file's bytes are, once they have been fetched.
    pub fn cached_file(&self, id: &str) -> Option<&std::path::Path> {
        self.cached_files.get(id).map(std::path::PathBuf::as_path)
    }

    /// The file's path in the cache, fetching the bytes first when the cache
    /// does not have them. A host that can show the file itself asks for
    /// this rather than handing the file to the desktop.
    pub fn file_path(
        &mut self,
        file: &crate::types::FileSummary,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<std::path::PathBuf>> {
        let path = match file_cache_path(&self.paths.files, file) {
            Ok(path) => path,
            Err(error) => return gpui::Task::ready(Err(error)),
        };
        if path.exists() {
            self.cached_files.insert(file.id.clone(), path.clone());
            return gpui::Task::ready(Ok(path));
        }
        let Some(client) = self.client.clone() else {
            return gpui::Task::ready(Err(anyhow::anyhow!("not connected")));
        };
        if file.url.is_empty() {
            return gpui::Task::ready(Err(anyhow::anyhow!("the file has no address")));
        }
        self.cached_files.insert(file.id.clone(), path.clone());
        let url = file.url.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            let bytes = client.download(&url).await?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
            anyhow::Ok(path)
        });
        cx.background_spawn(async move {
            match task.await {
                Ok(fetched) => fetched,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            }
        })
    }

    /// Fetches a file into the state cache so a surface can show it. Called
    /// when an image first comes into view, never ahead of time: the reader
    /// asked for a conversation, not for a download queue.
    pub fn cache_file(&mut self, file: &crate::types::FileSummary, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if file.url.is_empty() || self.cached_files.contains_key(&file.id) {
            return;
        }
        // Claimed before the fetch starts, so a second redraw does not queue
        // the same download again.
        let Ok(path) = file_cache_path(&self.paths.files, file) else {
            return;
        };
        self.cached_files.insert(file.id.clone(), path.clone());
        if path.exists() {
            return;
        }
        let file = file.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            let bytes = client.download(&file.url).await?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
            anyhow::Ok(file.id)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let fetched = match task.await {
                Ok(fetched) => fetched,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            let _ = this.update(cx, |_session, cx| {
                match fetched {
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "slack file fetch failed");
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Writes a fetched page into the mirror and records what it says about
    /// the shape of the history: a page that reached the beginning ends the
    /// chain, and one that did not leaves a gap carrying the cursor to fill
    /// it. Nothing here guesses — an unfilled hole is always a record.
    fn mirror_page(
        &self,
        source: &Source,
        messages: &[Message],
        paged: bool,
        reached_oldest: bool,
    ) {
        let Some(mirror) = self.mirror.as_ref() else {
            return;
        };
        let scope = self.scope(source);
        let filled = mirror.oldest_ts(&scope);
        mirror.insert_messages(&scope, messages);
        if !paged {
            return;
        }
        if let Some(filled) = filled {
            // Whatever hole sat at the old bottom has just been paged
            // through.
            mirror.clear_gap(&scope, &filled);
        }
        if reached_oldest {
            mirror.set_history_begins(&scope);
        } else if let Some(oldest) = messages.first().map(|message| message.ts.clone()) {
            mirror.put_gap(&scope, &oldest, &oldest);
        }
    }

    /// The mirror's name for a source: a conversation, or one thread in it.
    fn scope(&self, source: &Source) -> Scope {
        let workspace = self.model.workspace().0.clone();
        match source {
            Source::Conversation(channel) => Scope::conversation(&workspace, channel),
            Source::Thread(key) => Scope::thread(&workspace, &key.channel, &key.thread_ts),
        }
    }

    /// Downloads a file into the state cache and hands it to the desktop.
    /// The bytes are written once and never expire: a Slack file id is
    /// immutable, so a second open is a local read.
    pub fn open_file(&mut self, file: &crate::types::FileSummary, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if file.url.is_empty() {
            return;
        }
        let file = file.clone();
        let files = self.paths.files.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            let path = file_cache_path(&files, &file)?;
            if !path.exists() {
                let bytes = client.download(&file.url).await?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, bytes)?;
            }
            // The desktop decides what opens it; rho is not a viewer.
            std::process::Command::new("xdg-open")
                .arg(&path)
                .spawn()
                .map(|_| ())
                .map_err(anyhow::Error::from)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let opened = match task.await {
                Ok(opened) => opened,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            if let Err(error) = opened {
                tracing::warn!(error = %error, "slack file open failed");
                let _ = this.update(cx, |_, cx| {
                    // Not a health signal. A file rho could not open says
                    // nothing about whether the session is keeping up, and
                    // `Health` is the only thing that may say it is not: it
                    // owns the reason that `feed_ok` clears, so a degraded
                    // state raised from outside it is one nothing can lift.
                    cx.emit(SessionEvent::Notice(format!("slack: {error:#}")));
                });
            }
        }));
    }

    /// Records that the reader is through `ts` here, wherever the evidence
    /// came from: rho's own mark, a frame saying another client marked it,
    /// or the cursor Slack handed over at connect. The model keeps the
    /// newer of what it holds and what arrived, and a cursor that actually
    /// moved is written to the mirror, so the next start knows where the
    /// unread rule goes before the network says a word.
    ///
    /// One place, because a cursor written down in three of the four paths
    /// that move it is the bug this is here to close.
    fn note_read(&mut self, source: &Source, ts: &Ts) -> bool {
        let moved = match source {
            Source::Conversation(channel) => self.model.mark_read(channel, ts),
            Source::Thread(key) => self.model.mark_thread_read(key, ts),
        };
        if moved && let Some(mirror) = self.mirror.as_ref() {
            mirror.set_last_read(&self.scope(source), ts);
        }
        moved
    }

    /// Writes down every cursor the model holds, after Slack has handed
    /// over its own at connect. Once per connect and bounded by the roster,
    /// which is how the cursor for a conversation nobody has opened this
    /// run still survives a restart.
    fn record_cursors(&self) {
        let Some(mirror) = self.mirror.as_ref() else {
            return;
        };
        for channel in self.model.conversations() {
            if let Some(ts) = self.model.last_read(&channel) {
                mirror.set_last_read(&self.scope(&Source::Conversation(channel.clone())), ts);
            }
        }
        for key in self.model.followed() {
            if let Some(ts) = self.model.thread_last_read(&key) {
                mirror.set_last_read(&self.scope(&Source::Thread(key.clone())), ts);
            }
        }
    }

    /// Tells Slack the conversation has been read, so rho does not leave the
    /// phone showing a badge for something the user has already seen.
    ///
    /// A thread is marked as a thread. Slack keeps a cursor inside each
    /// followed thread and one on the conversation around it, and a reply's
    /// timestamp is a real timestamp in its channel — so telling Slack the
    /// channel was read at one would mark every message older than that
    /// reply read in a channel the reader never opened.
    pub fn mark_read(&mut self, source: &Source, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(latest) = self
            .loaded
            .get(source)
            .and_then(|loaded| loaded.messages.last())
            .map(|message| message.ts.clone())
        else {
            return;
        };
        self.note_read(source, &latest);
        cx.notify();
        let source = source.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            match &source {
                Source::Conversation(channel) => client.mark_read(channel, &latest).await,
                Source::Thread(key) => {
                    client
                        .mark_thread_read(&key.channel, &key.thread_ts, &latest)
                        .await
                }
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = task.await {
                tracing::warn!(error = %error, "slack mark-read failed");
            }
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    /// The conversation half of a mute: a thread is unfollowed, and a
    /// conversation has nothing to unfollow, so the read marker at its
    /// newest message is what stops every other client raising it. Unlike
    /// `mark_read` this does not need the conversation on screen: a unit is
    /// muted from a card, which the user has not opened.
    pub fn mark_unit_read(&mut self, unit: &crate::model::Unit, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(latest) = self.model.unit(unit).map(|facts| facts.newest.clone()) else {
            return;
        };
        let channel = unit.channel.clone();
        self.note_read(&Source::Conversation(channel.clone()), &latest);
        cx.notify();
        let task =
            gpui_tokio::Tokio::spawn(cx, async move { client.mark_read(&channel, &latest).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = task.await {
                tracing::warn!(error = %error, "slack mark-read failed");
            }
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    /// Opts the reader into a channel, or out of it: the standing word that
    /// this channel's ordinary traffic is to be handed to them rather than
    /// left in the list with a count.
    ///
    /// Slack has nothing to say about this, so nothing is sent: it is rho's
    /// own fact and it goes straight to rho's own file. Opting in takes
    /// effect on what the mirror already holds, so a channel opted into
    /// this second raises the cards its unread traffic has earned rather
    /// than waiting for the next message to arrive.
    pub fn set_watching(
        &mut self,
        channel: &ChannelId,
        watching: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.model.set_watching(channel, watching) {
            return false;
        }
        if let Some(mirror) = self.mirror.as_ref() {
            mirror.set_watched(&self.model.workspace().0.clone(), channel, watching);
        }
        if watching {
            self.derive_units_from_mirror();
        }
        cx.notify();
        true
    }

    pub fn watches(&self, channel: &ChannelId) -> bool {
        self.model.watches(channel)
    }

    /// Slack's ignore thread: the mute the user just made here, made
    /// everywhere they read Slack. One request, and rho keeps no
    /// subscription state of its own, so the socket's `thread_unsubscribed`
    /// that follows is the confirmation rather than a second source of
    /// truth. A failure is reported and changes nothing local: the mute
    /// already stands.
    pub fn ignore_thread(&mut self, key: &ThreadKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // The thread itself is kept: `shift-u` follows it again, and a card
        // whose words rho had thrown away could not come back.
        self.model.ignore(key);
        cx.notify();
        let (channel, thread_ts) = (key.channel.clone(), key.thread_ts.clone());
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client.ignore_thread(&channel, &thread_ts).await
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let failed = match task.await {
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(error) => Some(format!("{error}")),
                Ok(Ok(())) => None,
            };
            let _ = this.update(cx, |_, cx| {
                if let Some(reason) = failed {
                    tracing::warn!(error = %reason, "slack ignore-thread failed");
                    cx.emit(SessionEvent::Notice(
                        "slack: the thread is still followed in Slack".to_owned(),
                    ));
                }
                cx.notify();
            });
        }));
    }

    /// Undoing a mute: the thread is the user's again, in Slack, because
    /// that is where the mute was made. The card comes back with it.
    pub fn follow_thread(&mut self, key: &ThreadKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.model.follow(&key.channel, &key.thread_ts);
        cx.notify();
        let (channel, thread_ts) = (key.channel.clone(), key.thread_ts.clone());
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client.follow_thread(&channel, &thread_ts).await
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let failed = match task.await {
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(error) => Some(format!("{error}")),
                Ok(Ok(())) => None,
            };
            let _ = this.update(cx, |_, cx| {
                if let Some(reason) = failed {
                    tracing::warn!(error = %reason, "slack follow-thread failed");
                    cx.emit(SessionEvent::Notice(
                        "slack: the thread is still ignored in Slack".to_owned(),
                    ));
                }
                cx.notify();
            });
        }));
    }

    /// Marks the old backlog read: one `conversations.mark` per conversation
    /// in the plan and one `subscriptions.thread.mark` per thread, which is
    /// what a person clicking through the same backlog would send. The plan
    /// is what the count line showed, so nothing newer than the cutoff can
    /// be touched between showing it and acting.
    pub fn mark_read_before(&mut self, plan: crate::model::MarkPlan, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        for (channel, ts) in &plan.conversations {
            self.note_read(&Source::Conversation(channel.clone()), ts);
        }
        for (key, ts) in &plan.threads {
            self.note_read(&Source::Thread(key.clone()), ts);
        }
        cx.notify();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            for (channel, ts) in plan.conversations {
                if let Err(error) = client.mark_read(&channel, &ts).await {
                    tracing::warn!(error = %error, "slack mark-read failed");
                }
            }
            for (key, ts) in plan.threads {
                if let Err(error) = client
                    .mark_thread_read(&key.channel, &key.thread_ts, &ts)
                    .await
                {
                    tracing::warn!(error = %error, "slack thread mark-read failed");
                }
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let _ = task.await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    /// Sends `text` where the surface points: into the thread from a thread
    /// surface, into the conversation otherwise.
    /// Posts the message. The words appear at once, muted, under a local
    /// timestamp; Slack's echo replaces that line with the real thing.
    /// A refusal takes the line back and hands the text to the caller,
    /// which is what keeps it out of the reader's way and in their
    /// composer.
    /// Puts the reader's own emoji on a message, or takes theirs off if it
    /// is already there. One key means one state, not two.
    ///
    /// `name` is the shortcode without colons, the form Slack's API takes
    /// and the wire carries back. The change is made here before the
    /// request goes, so the line under the point answers the key at once,
    /// and put back if Slack refuses. Slack's own echo of it is then a
    /// no-op: `Loaded::react` ignores a user already in the list, so the
    /// count never counts the reader twice.
    ///
    /// Cost: the reactions on that one message and the two surfaces that
    /// can hold it, never a pass over the conversation.
    pub fn toggle_reaction(
        &mut self,
        source: &Source,
        ts: &Ts,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            cx.emit(SessionEvent::Notice(
                "slack: not connected, the reaction was not sent".to_owned(),
            ));
            return;
        };
        let user = self.model.self_id().clone();
        let adding = !self.reacted_by_reader(source, ts, name);
        let channel = source.channel().clone();
        self.react(&channel, ts, &user, name, adding);
        if adding {
            self.note_reaction_used(name);
        }
        cx.notify();

        let sent_name = name.to_owned();
        let sent_ts = ts.clone();
        let sent_channel = channel.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            match adding {
                true => {
                    client
                        .add_reaction(&sent_channel, &sent_ts, &sent_name)
                        .await
                }
                false => {
                    client
                        .remove_reaction(&sent_channel, &sent_ts, &sent_name)
                        .await
                }
            }
        });
        let name = name.to_owned();
        let ts = ts.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let sent = match task.await {
                Ok(sent) => sent,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            let Err(error) = sent else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                // Refused: the line goes back to what Slack still holds,
                // rather than showing a reaction that is not there.
                tracing::warn!(error = %error, "slack reaction failed");
                session.react(&channel, &ts, &user, &name, !adding);
                cx.emit(SessionEvent::Notice(format!("slack: {error:#}")));
                cx.notify();
            });
        }));
    }

    /// Remembers what the reader reacted with, here and on disk, so the
    /// menu opens on the emoji they actually use — after a restart too.
    /// One short list written back, and only when it moved.
    fn note_reaction_used(&mut self, name: &str) {
        if !self.model.note_reaction_used(name) {
            return;
        }
        if let Some(mirror) = self.mirror.as_ref() {
            mirror.set_reacted_with(&self.model.workspace().0.clone(), self.model.reacted_with());
        }
    }

    /// The emoji the reader reaches for, most recent first.
    pub fn reacted_with(&self) -> &[String] {
        self.model.reacted_with()
    }

    /// Whether the reader's own emoji is already on this message. Asked of
    /// the surface that holds it, so it costs that message's reactions.
    pub fn reacted_by_reader(&self, source: &Source, ts: &Ts, name: &str) -> bool {
        let user = self.model.self_id();
        self.loaded
            .get(source)
            .and_then(|loaded| loaded.messages.iter().find(|held| &held.ts == ts))
            .map(|message| {
                message
                    .reactions
                    .iter()
                    .any(|reaction| reaction.name == name && reaction.users.contains(user))
            })
            .unwrap_or(false)
    }

    pub fn send(
        &mut self,
        source: &Source,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(client) = self.client.clone() else {
            return Task::ready(Err(anyhow::anyhow!("slack is not connected")));
        };
        if text.trim().is_empty() {
            return Task::ready(Ok(()));
        }
        // What the reader typed is what they read: `@ada` on screen, and
        // `<@U1>` on the wire, which is the only form that makes the
        // mention count for Ada. The echo comes back in the same form, so
        // the local line and Slack's reply still match on text.
        let text = self.model.encode(&text);
        self.pending_sends += 1;
        let channel = source.channel().clone();
        let thread_ts = source.thread_ts().cloned();
        let source = source.clone();
        let body = text.clone();
        let local = self.hold_local(&source, &text, cx);
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client
                .post_message(&channel, thread_ts.as_ref(), &body)
                .await
        });
        cx.spawn(async move |this, cx| {
            let sent = match task.await {
                Ok(sent) => sent,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            let outcome = match &sent {
                Ok(_) => Ok(()),
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            };
            let _ = this.update(cx, |session, cx| {
                session.pending_sends = session.pending_sends.saturating_sub(1);
                match sent {
                    Ok(ts) => session.accept_own(&source, ts, text, cx),
                    Err(error) => {
                        tracing::warn!(error = %error, "slack send failed");
                        let error = format!("{error:#}");
                        // The same two surfaces `hold_local` put it in: a
                        // reply's root is the thread, a top-level message's
                        // is the message itself.
                        let root = source.thread_ts().cloned().unwrap_or_else(|| local.clone());
                        for source in session.sources_for(source.channel(), &root) {
                            if let Some(loaded) = session.loaded.get_mut(&source) {
                                loaded.drop_local(&local);
                            }
                        }
                        // Said once, where the user reads what rho has to
                        // tell them: a send that did not happen is not a
                        // detail of the conversation surface.
                        cx.emit(SessionEvent::Notice(format!("slack: {error}")));
                    }
                }
                cx.notify();
            });
            outcome
        })
    }

    /// Puts the message on screen before Slack has seen it, in every open
    /// surface it belongs to. The mirror is left alone: nothing goes on
    /// disk that the server has not confirmed.
    fn hold_local(&mut self, source: &Source, text: &str, cx: &mut Context<Self>) -> Ts {
        let ts = self.local_ts(source);
        let message = Message {
            ts: ts.clone(),
            thread_ts: source.thread_ts().cloned(),
            channel: source.channel().clone(),
            user: Some(self.model.self_id().clone()),
            bot_name: None,
            blocks: Vec::new(),
            text: text.to_owned(),
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 0,
            latest_reply: None,
            edited: false,
            reactions: Vec::new(),
        };
        for source in self.sources_for(source.channel(), &message.thread_root()) {
            if let Some(loaded) = self.loaded.get_mut(&source) {
                loaded.hold_local(message.clone());
            }
        }
        cx.notify();
        ts
    }

    /// A timestamp for a message Slack has not numbered yet. The transcript
    /// sorts on the number, so it has to be newer than anything held or the
    /// line would appear in the middle of the conversation.
    fn local_ts(&self, source: &Source) -> Ts {
        let now = now_ms() as f64 / 1000.0;
        let newest = self
            .loaded
            .get(source)
            .and_then(|loaded| loaded.messages.last())
            .map(|last| last.ts.epoch_seconds())
            .unwrap_or_default();
        Ts(format!("{:.6}", now.max(newest + 0.000_001)))
    }

    /// Sends a picture with the message. The bytes go up first, then Slack
    /// posts the message with the file on it, and what appears is that
    /// message coming back like any other: never the local bytes.
    pub fn send_file(
        &mut self,
        source: &Source,
        name: String,
        bytes: Vec<u8>,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(client) = self.client.clone() else {
            return Task::ready(Err(anyhow::anyhow!("slack is not connected")));
        };
        let channel = source.channel().clone();
        let thread_ts = source.thread_ts().cloned();
        // A caption is a message like any other: a name in it has to reach
        // the person named.
        let text = self.model.encode(&text);
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client
                .upload_file(&channel, thread_ts.as_ref(), &name, bytes, &text)
                .await
        });
        cx.spawn(async move |this, cx| {
            let sent = match task.await {
                Ok(sent) => sent,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            if let Err(error) = &sent {
                tracing::warn!(error = %error, "slack file send failed");
                let message = format!("{error:#}");
                let _ = this.update(cx, |_, cx| {
                    // The same word a refused send gets. The surface has
                    // already put the picture and the caption back on the
                    // chip; this is what says why they came back.
                    cx.emit(SessionEvent::Notice(format!("slack: {message}")));
                    cx.notify();
                });
            }
            sent
        })
    }

    /// Rewrites a message the reader already sent. What appears is Slack's
    /// own `message_changed` coming back down the socket, so the screen
    /// shows the edit that landed rather than the one that was asked for.
    /// The answer says whether it went, the same as `send`, so the surface
    /// can put a refused rewrite back in the reader's hands rather than
    /// drop it.
    pub fn edit_message(
        &mut self,
        source: &Source,
        ts: Ts,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(client) = self.client.clone() else {
            cx.emit(SessionEvent::Notice(
                "slack: not connected, the rewrite was not sent".to_owned(),
            ));
            return Task::ready(Err(anyhow::anyhow!("slack is not connected")));
        };
        // An empty rewrite is not a delete, and Slack would refuse it. The
        // surface never sends one; saying no here is what makes that true
        // of every caller rather than of the one.
        if text.trim().is_empty() {
            return Task::ready(Err(anyhow::anyhow!("an empty rewrite is not a delete")));
        }
        // The same rule as `send`, and for the same reason: `<@U1>` is the
        // only form that makes the mention count for Ada. A rewrite is how a
        // reader adds the name they forgot, so a rewrite that skipped this
        // was the one way to type `@ada` in rho and have nobody told.
        let text = self.model.encode(&text);
        let channel = source.channel().clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client.update_message(&channel, &ts, &text).await
        });
        cx.spawn(async move |this, cx| {
            let sent = match task.await {
                Ok(sent) => sent,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            let error = match sent {
                Ok(()) => {
                    let _ = this.update(cx, |_, cx| cx.notify());
                    return Ok(());
                }
                Err(error) => format!("{error:#}"),
            };
            tracing::warn!(error = %error, "slack edit failed");
            let _ = this.update(cx, |_, cx| {
                // Said once, where the user reads what rho has to tell them,
                // the same as a send that did not happen.
                cx.emit(SessionEvent::Notice(format!("slack: {error}")));
                cx.notify();
            });
            Err(anyhow::anyhow!("{error}"))
        })
    }

    /// The message Slack accepted, shown at once. Slack echoes it back over
    /// the socket a moment later; the model deduplicates on the timestamp,
    /// so the echo changes nothing.
    fn accept_own(&mut self, source: &Source, ts: Ts, text: String, cx: &mut Context<Self>) {
        let message = Message {
            ts,
            thread_ts: source.thread_ts().cloned(),
            channel: source.channel().clone(),
            user: Some(self.model.self_id().clone()),
            bot_name: None,
            blocks: Vec::new(),
            text,
            attachments: Vec::new(),
            files: Vec::new(),
            subtype: None,
            reply_count: 0,
            latest_reply: None,
            edited: false,
            reactions: Vec::new(),
        };
        let key = self.model.key(&message.channel, &message.thread_root());
        self.receive(message, now_ms(), cx);
        cx.emit(SessionEvent::Replied(key));
    }

    /// Polls the feed now rather than at the next interval.
    pub fn poll_now(&self) {
        self.catch_up.notify_one();
    }
}

/// Where a downloaded file lives: the state cache, keyed on Slack's file id
/// so two files with the same name never collide.
/// What filling the history above the loaded run costs, if anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Older {
    /// Slack's own paging cursor, from a page fetched this run.
    Cursor(String),
    /// A gap record's cursor: the timestamp to page back from. This is what
    /// a conversation restored from the mirror has instead.
    Before(Ts),
}

/// Whether a scroll near the top should ask for anything. One page in flight
/// at a time, and nothing at all once the beginning of the conversation is
/// known: the answer is already on disk.
pub fn older_request(
    loading: bool,
    reached_oldest: bool,
    cursor: Option<String>,
    gap: Option<Ts>,
) -> Option<Older> {
    if loading || reached_oldest {
        return None;
    }
    match (cursor, gap) {
        (Some(cursor), _) => Some(Older::Cursor(cursor)),
        (None, Some(latest)) => Some(Older::Before(latest)),
        (None, None) => None,
    }
}

/// Whether what Slack said is "this session may not search". Its own
/// function so the two names Slack uses for it are in one place and can be
/// asserted without a network.
fn refused_search(said: &str) -> bool {
    said.contains("missing_scope") || said.contains("not_allowed_token_type")
}

/// Puts a message the socket brought into the mirror. A message landing in
/// a scope the mirror holds nothing for is an island: there is no telling
/// what sits under it, so it gets a gap of its own and the reader is told
/// as much until a page fills it.
fn mirror_live(mirror: &Mirror, scope: &Scope, message: &Message) {
    let empty = mirror.newest_chunk(scope, 1).is_empty();
    mirror.insert_messages(scope, std::slice::from_ref(message));
    if empty && !mirror.history_begins(scope) {
        mirror.put_gap(scope, &message.ts, &message.ts);
    }
}

/// Records what a ping's window does not know. The window is an island:
/// it was fetched around one message, so nothing is known below its oldest,
/// and without the record the surface would take those twenty messages for
/// the whole conversation and refuse to page back from them.
fn mirror_island(mirror: &Mirror, scope: &Scope, messages: &[Message]) {
    // Taken before the write, or the island would find itself.
    let above = messages
        .last()
        .and_then(|newest| mirror.next_newer(scope, &newest.ts));
    mirror.insert_messages(scope, messages);
    if let Some(oldest) = messages.first()
        && !mirror.history_begins(scope)
    {
        mirror.put_gap(scope, &oldest.ts, &oldest.ts);
    }
    // Whatever was already held over the island is a run the island never
    // met. Joining them would draw a hole as history, so the run above
    // starts a chunk of its own. If the two turn out to be neighbours after
    // all, the first page forward closes the record.
    if let Some(above) = above {
        mirror.put_gap(scope, &above, &above);
    }
}

/// What the newest-page request on open is bounded by. A mirror holding a
/// real run only needs what came after it; a mirror holding a handful of
/// messages the socket dropped in while the conversation was closed is an
/// island with nothing under it, so opening it asks for the newest page
/// outright. Either way it is one request.
fn refresh_since(cached: &[Message]) -> Option<Ts> {
    if cached.len() < MIRROR_MIN {
        return None;
    }
    cached.last().map(|message| message.ts.clone())
}

/// Below this a mirrored run is not worth reading on its own, so opening
/// the conversation fetches the newest page instead of only what is newer.
const MIRROR_MIN: usize = 20;

/// How much of a mirrored conversation is shown before the network answers.
/// One screenful and then some: enough to read, cheap to decode.
const MIRROR_PAGE: usize = 50;

/// How much of a conversation a ping brings with it, on each side of the
/// pinged message. Wide enough to read the exchange it sits in, narrow
/// enough to be two ordinary requests.
const PING_WINDOW: usize = 20;

/// How far back a dealt card looks for the oldest message it owes an
/// answer to. A unit with more unhandled mentions than this is a backlog,
/// not a card, and the reader is better served landing inside the window
/// than paging the whole history to find its floor.
const LANDING_WINDOW: usize = 200;

/// How far back a restart looks in one conversation for a message that
/// still concerns the user. A unit is at most one card, so what matters is
/// finding the newest such message rather than every one of them, and a
/// window keeps the cost of a start flat in a workspace with a long
/// history.
const STARTUP_WINDOW: usize = 200;

/// Where the reader had read to in each conversation, before Slack has said
/// anything.
///
/// The unread rule is drawn from this cursor, so without it a conversation
/// opened during the first seconds of a start — or opened at all while
/// offline — shows no rule and reads as though none of it had ever been
/// seen. Slack's own cursor overtakes this the moment `client.counts`
/// answers and cannot walk it backwards, so the mirror is a head start and
/// never a second opinion.
///
/// Conversations only: which threads are followed is Slack's list, and it
/// arrives carrying each thread's own cursor, so there is nothing here for
/// a thread to be seeded from.
pub fn seed_read_cursors(model: &mut Model, mirror: &Mirror) {
    let workspace = model.workspace().0.clone();
    for channel in model.conversations() {
        if let Some(ts) = mirror.last_read(&Scope::conversation(&workspace, &channel)) {
            model.mark_read(&channel, &ts);
        }
    }
}

/// The units the last run wrote down, installed as they stand.
///
/// Free, like [`derive_units`], so the start can be proven without standing
/// a session up: the thing being proven is that this reads units and never
/// messages, and a test of it should not need a socket.
pub fn restore_units(model: &mut Model, mirror: &Mirror) {
    let workspace = model.workspace().0.clone();
    for (unit, facts) in mirror.units(&workspace) {
        model.restore_unit(unit, facts);
    }
}

/// The units the mirror's own history implies, raised into the model.
/// Every conversation the mirror knows and every followed thread is walked,
/// and the model decides which messages are the user's business.
///
/// **This is the explicit rebuild, and nothing on the start path calls it.**
/// It reads every message in the mirror, which is O(messages) and is the
/// cost the units table exists to avoid; a start reads [`restore_units`]
/// instead, one row per unit. The one time a start may reach this is a
/// mirror written before the units table existed, which pays it once and
/// then records that it has. Calling it at boot would put the cost back.
/// A replayed message is evidence about when it was said, not about now.
/// `note_message` takes the time a unit was first seen, and the mirror is
/// handing over history: the time to hand it is the message's own, or every
/// card derived here reads as having waited no time at all.
pub fn derive_units(model: &mut Model, mirror: &Mirror) {
    let workspace = model.workspace().0.clone();
    let mut scopes = mirror
        .conversations(&workspace)
        .into_iter()
        .map(|conversation| Scope::conversation(&workspace, &conversation.id))
        .collect::<Vec<_>>();
    scopes.extend(
        model
            .followed()
            .iter()
            .map(|key| Scope::thread(&workspace, &key.channel, &key.thread_ts)),
    );
    for scope in scopes {
        for message in mirror.newest_chunk(&scope, STARTUP_WINDOW) {
            let said_at = message.ts.millis();
            model.note_message(&message, said_at);
        }
    }
}

fn unit_scope(model: &Model, unit: &Unit) -> Scope {
    let workspace = &model.workspace().0;
    match &unit.thread {
        Some(root) => Scope::thread(workspace, &unit.channel, root),
        None => Scope::conversation(workspace, &unit.channel),
    }
}

fn oldest_from_other_after(
    model: &Model,
    mirror: &Mirror,
    unit: &Unit,
    cursor: Option<&Ts>,
) -> Option<Ts> {
    let self_id = model.self_id().clone();
    mirror
        .newest_chunk(&unit_scope(model, unit), LANDING_WINDOW)
        .into_iter()
        .filter(|message| message.user.as_ref() != Some(&self_id))
        .filter(|message| model.concerns_you(message, unit))
        .filter(|message| cursor.is_none_or(|cursor| message.ts.is_newer_than(cursor)))
        .map(|message| message.ts)
        .min_by(|left, right| {
            left.epoch_seconds()
                .partial_cmp(&right.epoch_seconds())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .or_else(|| model.unit(unit).map(|facts| facts.newest.clone()))
}

/// The words a unit is known by: the first line of its newest message, as
/// the mirror holds it now. Public because it is what a card's title is,
/// and a host measuring what a desk rebuild costs has to be able to reach
/// the half of it that reads the mirror.
pub fn unit_summary(model: &Model, mirror: &Mirror, unit: &Unit) -> String {
    let scope = unit_scope(model, unit);
    let message = model
        .unit(unit)
        .map(|facts| facts.newest.clone())
        .and_then(|ts| mirror.chunk_containing(&scope, &ts, 1).pop())
        .or_else(|| mirror.newest_chunk(&scope, 1).pop());
    message
        .map(|message| model.render(&message))
        .as_deref()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The mirror lives beside rho's other state. A machine without a state
/// directory simply runs without one: the client still works, it just has
/// nothing to show before the first response.
/// Opens the mirror the caller named, or goes without one.
///
/// Which file that is comes from `config::Paths` and never from the OS:
/// a library that resolves the user's state directory hands every test
/// and every rig the user's live Slack data, which is what this used to
/// do.
fn open_mirror(path: &std::path::Path) -> Option<Arc<Mirror>> {
    match Mirror::open(path) {
        Ok(mirror) => Some(Arc::new(mirror)),
        Err(error) => {
            tracing::warn!(error = %error, "slack mirror unavailable");
            None
        }
    }
}

fn file_cache_path(
    files: &std::path::Path,
    file: &crate::types::FileSummary,
) -> anyhow::Result<std::path::PathBuf> {
    let name = file
        .title
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("file");
    Ok(files.join(format!("{}-{name}", file.id)))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

/// A convenience for hosts: the poll that the transport would have run, for
/// a catch-up the caller wants to await.
pub async fn catch_up_poll(
    client: &Client,
    newest: Option<&Ts>,
) -> anyhow::Result<Vec<crate::api::ActivityItem>> {
    poll_feed(client, newest).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message(ts: &str, text: &str) -> Message {
        crate::api::parse_message(
            &json!({"ts": ts, "user": "U1", "text": text}),
            &ChannelId("C1".into()),
        )
        .unwrap()
    }

    fn mentioning(ts: &str, user: &str, text: &str) -> Message {
        crate::api::parse_message(
            &json!({"ts": ts, "user": user, "text": text}),
            &ChannelId("C1".into()),
        )
        .unwrap()
    }

    fn seeded() -> (tempfile::TempDir, Mirror, Model) {
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(dir.path().join("slack.redb")).unwrap();
        let mut model = Model::new(crate::config::WorkspaceName("T1".into()));
        model.set_self(UserId("ME".into()));
        model.add_conversations([crate::types::Conversation {
            id: ChannelId("C1".into()),
            kind: crate::types::ConversationKind::Channel,
            name: "design".into(),
            user: None,
            members: Vec::new(),
        }]);
        (dir, mirror, model)
    }

    /// The start that does not read history. The units the last run wrote
    /// are the answer already; walking the messages again to reach the same
    /// answer is the cost the rule forbids, and on a real mirror it is the
    /// difference between a start you notice and one you do not.
    ///
    /// Proven by leaving the mirror's history empty: nothing here could be
    /// derived, so a unit that comes back came back off its own row.
    #[test]
    fn a_start_reads_the_units_the_last_run_wrote_and_never_its_messages() {
        let (_dir, mirror, mut model) = seeded();
        let unit = Unit::conversation(&ChannelId("C1".into()));
        mirror.put_unit(
            "T1",
            &unit,
            &crate::model::UnitFacts {
                reason: crate::types::Reason::Mention,
                newest: Ts("300.0".into()),
                newest_from_other: Some(Ts("300.0".into())),
                newest_from_you: false,
                first_seen_ms: 1_000,
            },
        );
        assert!(
            mirror
                .newest_chunk(&Scope::conversation("T1", &ChannelId("C1".into())), 10)
                .is_empty(),
            "the history is empty, so nothing below can have been derived"
        );

        restore_units(&mut model, &mirror);
        assert_eq!(model.tracked(), vec![unit.clone()]);
        assert_eq!(model.unit(&unit).unwrap().first_seen_ms, 1_000);
        assert_eq!(
            model.attention(&unit),
            Some(crate::model::Attention::Mentioned),
            "and it asks, off its own row, with no message read"
        );

        // The feed replaying what the row already holds does not raise it
        // twice: the row's timestamps are marked seen when it is installed.
        assert_eq!(
            model.note_activity(
                &crate::api::ActivityItem {
                    channel: ChannelId("C1".into()),
                    ts: Ts("300.0".into()),
                    thread_ts: None,
                    kind: crate::api::ActivityKind::Mention,
                    unread: true,
                },
                0
            ),
            None
        );
    }

    /// The opt-in is the reader's standing word, so it outlives the session
    /// that made it. It is rho's own fact and lives in rho's own file.
    #[test]
    fn the_opt_in_comes_back_off_the_file_after_a_restart() {
        let (_dir, mirror, mut model) = seeded();
        let design = ChannelId("C1".into());
        assert!(model.set_watching(&design, true));
        mirror.set_watched("T1", &design, true);

        let mut next = Model::new(crate::config::WorkspaceName("T1".into()));
        next.set_watched(mirror.watched("T1"));
        assert!(next.watches(&design));

        mirror.set_watched("T1", &design, false);
        let mut after = Model::new(crate::config::WorkspaceName("T1".into()));
        after.set_watched(mirror.watched("T1"));
        assert!(!after.watches(&design));
    }

    /// A restart is another source of the same messages and may only raise
    /// facts. The activity feed is a cursor: the mention it has already
    /// passed is never reported again, so without this the card would be
    /// gone the next morning. The mirror still holds the message, and the
    /// unit comes back from there: one card, whatever else is in the
    /// channel, and none at all for a channel nobody was mentioned in.
    #[test]
    fn a_mention_the_feed_has_passed_is_still_a_card_after_a_restart() {
        let (_dir, mirror, mut model) = seeded();
        let quiet = crate::types::Conversation {
            id: ChannelId("C2".into()),
            kind: crate::types::ConversationKind::Channel,
            name: "random".into(),
            user: None,
            members: Vec::new(),
        };
        let direct = crate::types::Conversation {
            id: ChannelId("D1".into()),
            kind: crate::types::ConversationKind::DirectMessage,
            user: Some(UserId("U1".into())),
            name: "D1".into(),
            members: Vec::new(),
        };
        model.add_conversations([quiet.clone(), direct.clone()]);
        mirror.put_conversations(
            "T1",
            &[
                crate::types::Conversation {
                    id: ChannelId("C1".into()),
                    kind: crate::types::ConversationKind::Channel,
                    name: "design".into(),
                    user: None,
                    members: Vec::new(),
                },
                quiet,
                direct,
            ],
        );
        // What the last run left on disk: a mention with traffic on both
        // sides of it, a channel with only traffic, and a direct message.
        mirror.insert_messages(
            &Scope::conversation("T1", &ChannelId("C1".into())),
            &[
                message("100.0", "morning"),
                mentioning("200.0", "U1", "<@ME> can you look?"),
                message("300.0", "unrelated"),
            ],
        );
        mirror.insert_messages(
            &Scope::conversation("T1", &ChannelId("C2".into())),
            &[message("150.0", "nobody is talking to you")],
        );
        mirror.insert_messages(
            &Scope::conversation("T1", &ChannelId("D1".into())),
            &[crate::api::parse_message(
                &json!({"ts": "250.0", "user": "U1", "text": "lunch?"}),
                &ChannelId("D1".into()),
            )
            .unwrap()],
        );

        derive_units(&mut model, &mirror);
        assert_eq!(
            model.tracked(),
            vec![
                Unit::conversation(&ChannelId("C1".into())),
                Unit::conversation(&ChannelId("D1".into())),
            ],
            "the mentioned channel and the direct message, and nothing for the quiet one"
        );
        let mentioned = model
            .unit(&Unit::conversation(&ChannelId("C1".into())))
            .unwrap();
        assert_eq!(mentioned.reason, crate::types::Reason::Mention);
        assert_eq!(
            mentioned.newest,
            Ts("200.0".into()),
            "the traffic after the mention is not about the user, so the card waits from the mention"
        );

        // A second pass is what the roster's followed list triggers, and it
        // is a no-op on everything already derived.
        let before = model.card(&Unit::conversation(&ChannelId("C1".into())), 0);
        derive_units(&mut model, &mirror);
        assert_eq!(
            model.card(&Unit::conversation(&ChannelId("C1".into())), 0),
            before
        );
    }

    /// A card derived from the mirror has waited since the message was said,
    /// not since rho worked out that it had one. Derive is not a once-ever
    /// pass: it runs again on every connect, once the followed list is in,
    /// and again when the reader opts a channel into being handed over — so
    /// a card that took its wait from the derive would read zero days on a
    /// mention that has been sitting there since Friday.
    #[test]
    fn a_card_derived_from_the_mirror_waits_from_the_message_and_not_the_derive() {
        const DAY: i64 = 86_400_000;
        let (_dir, mirror, mut model) = seeded();
        // The scopes derive walks are the mirror's own conversation list.
        mirror.put_conversations(
            "T1",
            &[crate::types::Conversation {
                id: ChannelId("C1".into()),
                kind: crate::types::ConversationKind::Channel,
                name: "design".into(),
                user: None,
                members: Vec::new(),
            }],
        );
        let now = 100 * DAY;
        let said_at = now - 3 * DAY;
        mirror.insert_messages(
            &Scope::conversation("T1", &ChannelId("C1".into())),
            &[mentioning(
                &format!("{}.0", said_at / 1000),
                "U1",
                "<@ME> can you look?",
            )],
        );
        let unit = Unit::conversation(&ChannelId("C1".into()));

        derive_units(&mut model, &mirror);
        let card = model.card(&unit, now).unwrap();
        assert!(
            (card.wait_days - 3.0).abs() < 0.01,
            "three days, not {}",
            card.wait_days
        );

        // The second connect derives again over the same history. It must
        // not restart the clock either: `record` keeps the first_seen it
        // already has, and the first one it had is now the right one.
        derive_units(&mut model, &mirror);
        let card = model.card(&unit, now).unwrap();
        assert!(
            (card.wait_days - 3.0).abs() < 0.01,
            "still three days on the second connect, not {}",
            card.wait_days
        );
    }

    /// The card's words are rendered when the card is drawn, never kept
    /// from when the message landed. This is the cold start: the mention
    /// arrives before the roster does, and the card still reads `@ada`
    /// rather than the id Slack sent.
    #[test]
    fn a_cards_words_are_rendered_now_and_not_when_the_message_landed() {
        let (_dir, mirror, mut model) = seeded();
        let scope = Scope::conversation("T1", &ChannelId("C1".into()));
        let mention = mentioning("100.0", "U1", "<@U9> can you look?");
        mirror.insert_messages(&scope, std::slice::from_ref(&mention));
        model.note_message(&mentioning("100.0", "U1", "hey <@ME> look"), 0);
        let unit = Unit::conversation(&ChannelId("C1".into()));

        assert_eq!(
            unit_summary(&model, &mirror, &unit),
            "@someone can you look?",
            "with no roster there is no name to put on it"
        );
        model.add_users([crate::types::User {
            id: UserId("U9".into()),
            name: "ada".into(),
            handle: "ada".into(),
        }]);
        assert_eq!(
            unit_summary(&model, &mirror, &unit),
            "@ada can you look?",
            "the same message, re-rendered with what is known now"
        );
    }

    /// A channel with three unhandled mentions is one card, and the reader
    /// is put on the first of them rather than the last: the cursor says
    /// what has been dealt with, and everything past it is still theirs.
    #[test]
    fn a_dealt_card_lands_on_the_oldest_message_past_the_cursor() {
        let (_dir, mirror, mut model) = seeded();
        let scope = Scope::conversation("T1", &ChannelId("C1".into()));
        for ts in ["100.0", "200.0", "300.0"] {
            let message = mentioning(ts, "U1", "hey <@ME> look");
            mirror.insert_messages(&scope, std::slice::from_ref(&message));
            model.note_message(&message, 0);
        }
        mirror.insert_messages(&scope, &[mentioning("250.0", "ME", "on it")]);
        let unit = Unit::conversation(&ChannelId("C1".into()));

        assert_eq!(
            oldest_from_other_after(&model, &mirror, &unit, None),
            Some(Ts("100.0".into())),
            "nothing handled yet, so the card opens on the first mention"
        );
        assert_eq!(
            oldest_from_other_after(&model, &mirror, &unit, Some(&Ts("100.0".into()))),
            Some(Ts("200.0".into())),
            "past the cursor, and the user's own message is never the landing"
        );
    }

    #[test]
    fn a_handful_of_live_messages_is_not_a_run_to_page_from() {
        let island: Vec<Message> = (0..3).map(|i| message(&format!("{i}.0"), "live")).collect();
        assert_eq!(refresh_since(&island), None, "an island bounds nothing");
        let run: Vec<Message> = (0..MIRROR_MIN)
            .map(|i| message(&format!("{i}.0"), "held"))
            .collect();
        assert_eq!(
            refresh_since(&run),
            run.last().map(|message| message.ts.clone()),
            "a real run is only topped up"
        );
    }

    #[test]
    fn a_pings_window_leaves_a_gap_under_itself() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(dir.path().join("slack.redb")).unwrap();
        let scope = Scope::conversation("T1", &ChannelId("C1".into()));
        let window: Vec<Message> = (10..30)
            .map(|i| message(&format!("{i}.0"), "around the ping"))
            .collect();
        mirror_island(&mirror, &scope, &window);
        let gap = mirror.gap_below(&scope, None);
        assert_eq!(
            gap.as_ref().map(|(_, gap)| gap.page_before.clone()),
            Some(Ts("10.0".into())),
            "the window says nothing about what came before it"
        );
        assert_eq!(
            mirror.newest_chunk(&scope, 50).len(),
            window.len(),
            "the run stops at the gap rather than joining the page above it"
        );
        assert!(
            matches!(
                older_request(false, false, None, gap.map(|(at, _)| at)),
                Some(Older::Before(ts)) if ts == Ts("10.0".into())
            ),
            "so scrolling to the top has something to ask for"
        );
    }

    #[test]
    fn a_window_written_under_a_run_does_not_join_it() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(dir.path().join("slack.redb")).unwrap();
        let scope = Scope::conversation("T1", &ChannelId("C1".into()));
        let tail: Vec<Message> = (400..420)
            .map(|i| message(&format!("{i}.0"), "the newest chunk"))
            .collect();
        mirror.insert_messages(&scope, &tail);
        let window: Vec<Message> = (100..120)
            .map(|i| message(&format!("{i}.0"), "around the ping"))
            .collect();
        mirror_island(&mirror, &scope, &window);

        assert_eq!(
            mirror.newest_chunk(&scope, 100).len(),
            tail.len(),
            "the newest chunk stops where the run above the window starts"
        );
        let chunk = mirror.chunk_containing(&scope, &Ts("110.0".into()), 100);
        assert_eq!(
            chunk.first().map(|message| message.ts.clone()),
            Some(Ts("100.0".into())),
            "and a deal on the window opens on the window"
        );
        assert_eq!(
            chunk.last().map(|message| message.ts.clone()),
            Some(Ts("119.0".into())),
            "which stops before the hole over it"
        );
    }

    #[test]
    fn a_message_arriving_into_nothing_leaves_a_gap_under_itself() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(dir.path().join("slack.redb")).unwrap();
        let scope = Scope::conversation("T1", &ChannelId("C1".into()));
        mirror_live(&mirror, &scope, &message("2.0", "live"));
        assert!(
            mirror.gap_below(&scope, None).is_some(),
            "nothing is known under a message the socket dropped in"
        );
        mirror_live(&mirror, &scope, &message("3.0", "and another"));
        assert_eq!(
            mirror
                .gap_below(&scope, None)
                .map(|(_, gap)| gap.page_before),
            Some(Ts("2.0".into())),
            "the next message continues the island, it does not re-cut it"
        );
    }

    #[test]
    fn a_message_held_twice_is_held_once() {
        let mut loaded = Loaded::default();
        loaded.insert(message("2.0", "second"));
        loaded.insert(message("1.0", "first"));
        loaded.insert(message("2.0", "second again"));
        assert_eq!(
            loaded
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "the socket and a page deliver the same message"
        );
        assert_eq!(
            loaded.updates_since(0),
            Some(vec![
                Update::Inserted(Ts("2.0".into())),
                Update::Inserted(Ts("1.0".into())),
            ]),
            "a message already held is not a change"
        );
    }

    #[test]
    fn the_change_log_names_exactly_what_moved() {
        let mut loaded = Loaded::default();
        loaded.insert(message("1.0", "first"));
        loaded.insert(message("2.0", "second"));
        let seen = loaded.revision();
        loaded.replace(message("1.0", "first, fixed"));
        loaded.remove(&Ts("2.0".into()));
        assert_eq!(
            loaded.updates_since(seen),
            Some(vec![
                Update::Replaced(Ts("1.0".into())),
                Update::Removed(Ts("2.0".into())),
            ]),
            "a surface rewrites only the two messages that changed"
        );
        assert_eq!(
            loaded.updates_since(loaded.revision()),
            Some(Vec::new()),
            "a surface that is up to date has nothing to do"
        );
        assert!(
            !loaded.replace(message("1.0", "first, fixed")),
            "an identical message is not a change"
        );
    }

    #[test]
    fn a_conversation_seeded_from_the_mirror_renders_in_one_go() {
        let loaded = Loaded {
            messages: vec![message("1.0", "from disk")],
            revision: 1,
            ..Loaded::default()
        };
        assert_eq!(
            loaded.updates_since(0),
            None,
            "what the mirror held is rendered as one insert, not as changes"
        );
    }

    #[test]
    fn a_surface_too_far_behind_is_told_to_rebuild() {
        let mut loaded = Loaded::default();
        for index in 0..LOG_LIMIT + 2 {
            loaded.insert(message(&format!("{index}.0"), "message"));
        }
        assert_eq!(loaded.updates_since(0), None, "the log no longer reaches");
        assert!(loaded.updates_since(loaded.revision() - 1).is_some());
    }

    #[test]
    fn a_reaction_lands_on_the_held_message_and_leaves_with_the_last_reader() {
        let mut loaded = Loaded::default();
        loaded.insert(message("1.0", "hello"));
        let ts = Ts("1.0".into());
        let you = UserId("ME".into());
        let them = UserId("U9".into());
        assert!(loaded.react(&ts, &you, "eyes", true));
        assert!(
            !loaded.react(&ts, &you, "eyes", true),
            "the same reader twice is still one reaction"
        );
        assert!(loaded.react(&ts, &them, "eyes", true));
        assert_eq!(loaded.messages[0].reactions[0].count, 2);
        assert!(loaded.react(&ts, &them, "eyes", false));
        assert!(loaded.react(&ts, &you, "eyes", false));
        assert!(
            loaded.messages[0].reactions.is_empty(),
            "the last one off takes the row with it"
        );
        assert!(
            !loaded.react(&Ts("9.0".into()), &you, "eyes", true),
            "a reaction on a message not held changes nothing"
        );
        // The surface rewrites that one line rather than the transcript.
        assert_eq!(
            loaded.updates_since(1),
            Some(vec![
                Update::Replaced(ts.clone()),
                Update::Replaced(ts.clone()),
                Update::Replaced(ts.clone()),
                Update::Replaced(ts),
            ])
        );
    }

    #[test]
    fn a_sent_message_shows_muted_until_the_echo_takes_its_place() {
        let mut loaded = Loaded::default();
        loaded.insert(message("100.0", "earlier"));
        let local = Ts("100.000001".into());
        loaded.hold_local(Message {
            ts: local.clone(),
            ..message("100.000001", "on its way")
        });
        assert!(loaded.is_pending(&local));
        assert_eq!(loaded.messages.last().unwrap().text, "on its way");

        // Slack's echo carries its own timestamp, so the words are what
        // match the two up.
        loaded.settle_local(&message("101.0", "on its way"));
        assert!(!loaded.is_pending(&local));
        assert_eq!(loaded.messages.len(), 1, "the local copy is gone");
        // The real one arrives by the ordinary route right after.
        loaded.insert(message("101.0", "on its way"));
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn a_refused_message_leaves_no_line_behind() {
        let mut loaded = Loaded::default();
        let local = Ts("100.000001".into());
        loaded.hold_local(Message {
            ts: local.clone(),
            ..message("100.000001", "never sent")
        });
        loaded.drop_local(&local);
        assert!(!loaded.is_pending(&local));
        assert!(loaded.messages.is_empty());
        // An echo of something else must not take a local copy with it.
        loaded.hold_local(Message {
            ts: local.clone(),
            ..message("100.000001", "mine")
        });
        loaded.settle_local(&message("101.0", "somebody else's"));
        assert!(loaded.is_pending(&local));
    }

    /// Where the unread rule goes after a restart. Slack's cursor is the
    /// truth for reading, but it arrives with `client.counts`, and until it
    /// does the reader is looking at a conversation with no rule in it —
    /// every message reading as unseen, including the ones they answered
    /// last night. The cursor the last run wrote down is what covers that
    /// gap, and it is the only thing that covers it at all when the machine
    /// is offline.
    #[test]
    fn the_read_cursor_survives_a_restart_and_slack_still_overtakes_it() {
        let (_dir, mirror, mut model) = seeded();
        mirror.put_conversations(
            "T1",
            &[crate::types::Conversation {
                id: ChannelId("C1".into()),
                kind: crate::types::ConversationKind::Channel,
                name: "design".into(),
                user: None,
                members: Vec::new(),
            }],
        );
        // What the last run left: the reader was through 500.0.
        mirror.set_last_read(
            &Scope::conversation("T1", &ChannelId("C1".into())),
            &Ts("500.0".into()),
        );

        // The restart, before a single request has been answered.
        seed_read_cursors(&mut model, &mirror);
        assert_eq!(
            model.last_read(&ChannelId("C1".into())),
            Some(&Ts("500.0".into())),
            "the rule is in the right place before the network says a word"
        );

        // Slack answers, and it has been read further somewhere else since.
        model.set_counts([crate::api::ConversationCount {
            channel: ChannelId("C1".into()),
            has_unreads: true,
            mention_count: 1,
            unread_count: 1,
            latest: Some(Ts("900.0".into())),
            last_read: Some(Ts("700.0".into())),
        }]);
        assert_eq!(
            model.last_read(&ChannelId("C1".into())),
            Some(&Ts("700.0".into())),
            "Slack's cursor is the truth and overtakes the mirror's"
        );

        // And an answer prepared before rho's own mark reached the server
        // cannot walk the rule back over what the reader has been through.
        model.set_counts([crate::api::ConversationCount {
            channel: ChannelId("C1".into()),
            has_unreads: true,
            mention_count: 1,
            unread_count: 1,
            latest: Some(Ts("900.0".into())),
            last_read: Some(Ts("600.0".into())),
        }]);
        assert_eq!(
            model.last_read(&ChannelId("C1".into())),
            Some(&Ts("700.0".into())),
            "a stale answer does not move the rule backwards"
        );
    }

    /// A thread's cursor is not its channel's. A reply's timestamp is a real
    /// timestamp in the channel it hangs in, so folding the two together
    /// marks every older message in that channel read on the strength of
    /// the reader opening one thread — a channel they never looked at,
    /// silently emptied.
    #[test]
    fn reading_a_thread_says_nothing_about_the_channel_around_it() {
        let (_dir, _mirror, mut model) = seeded();
        model.set_counts([crate::api::ConversationCount {
            channel: ChannelId("C1".into()),
            has_unreads: true,
            mention_count: 2,
            unread_count: 2,
            latest: Some(Ts("900.0".into())),
            last_read: Some(Ts("100.0".into())),
        }]);
        let key = model.key(&ChannelId("C1".into()), &Ts("400.0".into()));

        assert!(model.mark_thread_read(&key, &Ts("800.0".into())));
        assert_eq!(model.thread_last_read(&key), Some(&Ts("800.0".into())));
        assert_eq!(
            model.last_read(&ChannelId("C1".into())),
            Some(&Ts("100.0".into())),
            "the channel's own cursor did not move"
        );
        assert!(
            model.conversation_rows()[0].unread,
            "and the channel is still unread"
        );

        // The thread's cursor rises like every other fact here: a frame that
        // overtakes rho's own mark cannot pull the rule back.
        assert!(!model.mark_thread_read(&key, &Ts("700.0".into())));
        assert_eq!(model.thread_last_read(&key), Some(&Ts("800.0".into())));
    }

    /// A mark that does not reach the newest message leaves the badge
    /// standing. `mark read before` marks at a cutoff, and a conversation
    /// with something newer than the cutoff has genuinely not been read: a
    /// badge cleared here is a message the reader never learns about.
    #[test]
    fn a_mark_short_of_the_newest_message_leaves_the_badge_standing() {
        let (_dir, _mirror, mut model) = seeded();
        model.set_counts([crate::api::ConversationCount {
            channel: ChannelId("C1".into()),
            has_unreads: true,
            mention_count: 3,
            unread_count: 3,
            latest: Some(Ts("900.0".into())),
            last_read: None,
        }]);

        assert!(model.mark_read(&ChannelId("C1".into()), &Ts("500.0".into())));
        assert!(
            model.conversation_rows()[0].unread,
            "there is still something above the cutoff"
        );

        assert!(model.mark_read(&ChannelId("C1".into()), &Ts("900.0".into())));
        assert!(!model.conversation_rows()[0].unread);
        assert_eq!(model.conversation_rows()[0].mention_count, 0);
    }

    /// A workspace whose client never built asks for nothing, and must not
    /// leave the conversation saying it is loading — the flag is also the
    /// gate on asking again, so a stuck one is a conversation that will
    /// never load history for as long as it exists.
    #[gpui::test]
    fn a_scroll_with_no_client_leaves_the_conversation_free_to_ask_again(
        cx: &mut gpui::TestAppContext,
    ) {
        let state = tempfile::tempdir().expect("a state directory of this test's own");
        let credentials = Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
        let session = cx.new(|_| {
            Session::without_client(
                credentials,
                Paths::under(state.path()),
                "the client never built".to_owned(),
            )
        });
        let source = Source::Conversation(ChannelId("C1".into()));

        session.update(cx, |session, cx| {
            session.loaded.insert(
                source.clone(),
                Loaded {
                    older_cursor: Some("page-2".to_owned()),
                    ..Loaded::default()
                },
            );
            session.load_older(&source, cx);
        });

        let (loading, reached_oldest, cursor) = session.read_with(cx, |session, _| {
            let loaded = &session.loaded[&source];
            (
                loaded.loading,
                loaded.reached_oldest,
                loaded.older_cursor.clone(),
            )
        });
        assert!(!loading, "no request was made, so nothing is in flight");
        assert!(
            older_request(loading, reached_oldest, cursor, None).is_some(),
            "and the next scroll is still allowed to ask"
        );
    }

    /// The two names Slack uses for "this session may not search", so the
    /// one failure the reader can act on is told apart from the rest
    /// without a network.
    #[test]
    fn a_session_that_may_not_search_is_told_apart_from_a_search_that_failed() {
        assert!(refused_search("search.messages failed: missing_scope"));
        assert!(refused_search(
            "search.messages failed: not_allowed_token_type"
        ));
        assert!(!refused_search("search.messages failed: fatal_error"));
        assert!(!refused_search("error sending request"));
    }
}
