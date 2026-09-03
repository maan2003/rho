//! The live Slack session: one workspace's socket, poll, model, and the
//! conversations the surfaces are reading.
//!
//! Every surface observes this entity and re-reads what it needs, so two
//! panes on the same conversation cannot disagree. The session owns no UI
//! and no storage: it emits [`SessionEvent`], and the host decides what a
//! raised thread means for its inbox, its journal, and its lamp.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AppContext as _, Context, EventEmitter, Task};
use tokio::sync::Notify;

use crate::api::Client;
use crate::config::Credentials;
use crate::events::WsEvent;
use crate::health::{Health, Signal};
use crate::mirror::{Mirror, Scope};
use crate::model::{Change, ConversationRow, Model};
use crate::socket::{Timings, Wire, poll_feed, run_feed, run_socket};
use crate::types::{ChannelId, Message, ThreadKey, Ts};

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
    /// Files already fetched into the state cache, by Slack file id. An
    /// image is shown from here, so a redraw never refetches.
    cached_files: HashMap<String, std::path::PathBuf>,
    /// What rho already knows, on disk. Surfaces render from here before the
    /// network answers, and a refresh asks only for what it does not hold.
    mirror: Option<Arc<Mirror>>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    pub fn new(credentials: Credentials, cx: &mut Context<Self>) -> Self {
        match Client::new(credentials.clone()) {
            Ok(client) => Self::with_client(Arc::new(client), cx),
            Err(error) => Self {
                client: None,
                model: Model::new(credentials.workspace),
                status: Status::Failed(format!("{error:#}")),
                health: Health::default(),
                loaded: HashMap::new(),
                catch_up: Arc::new(Notify::new()),
                pending_sends: 0,
                cached_files: HashMap::new(),
                mirror: open_mirror(),
                _tasks: Vec::new(),
            },
        }
    }

    pub fn with_client(client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        let mut session = Self {
            model: Model::new(client.workspace().clone()),
            client: Some(client.clone()),
            status: Status::Connecting,
            health: Health::default(),
            loaded: HashMap::new(),
            catch_up: Arc::new(Notify::new()),
            pending_sends: 0,
            cached_files: HashMap::new(),
            mirror: open_mirror(),
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
            (users, conversations, counts, emoji, followed)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok((users, conversations, counts, emoji, followed)) = task.await else {
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
                let mut dropped = Vec::new();
                if let Ok(followed) = followed {
                    // A thread the list stops naming was unfollowed
                    // somewhere else, possibly while rho was off; its card
                    // is discarded on the way in rather than dealt again.
                    dropped = session.model.set_followed(
                        followed
                            .into_iter()
                            .map(|thread| (thread.channel, thread.thread_ts)),
                    );
                }
                // Anything raised before the roster landed was named
                // "#a conversation"; now it has a name, so say so again.
                let known = session
                    .model
                    .tracked()
                    .into_iter()
                    .map(Change::Updated)
                    .chain(dropped.into_iter().map(Change::Discarded))
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
                let key = self.model.key(&channel, &thread_ts);
                if self.model.unfollow(&channel, &thread_ts) {
                    self.announce(vec![Change::Discarded(key)], cx);
                }
            }
            Wire::Frame(WsEvent::Marked { channel, ts }) => {
                // Read elsewhere. Only the badge is stale: reading is not a
                // verdict, so every card stays exactly where it was.
                self.model.mark_read(&channel, &ts);
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
        if !changes.is_empty() {
            cx.emit(SessionEvent::Changed(changes));
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
                loading: true,
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
        let scope = self.scope(source);
        let Some(mirror) = self.mirror.clone() else {
            return;
        };
        let Some(loaded) = self.loaded.get_mut(source) else {
            return;
        };
        if loaded.messages.iter().any(|held| &held.ts == ts) {
            return;
        }
        let chunk = mirror.chunk_containing(&scope, ts, MIRROR_PAGE);
        let Some(newest) = chunk.last().map(|message| message.ts.clone()) else {
            return;
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
        if let Some(loaded) = self.loaded.get_mut(source) {
            loaded.loading = true;
        }
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
    fn prefetch_ping(&mut self, key: &ThreadKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let scope = Scope::conversation(&self.model.workspace().0, &key.channel);
        if self
            .mirror
            .as_ref()
            .is_none_or(|mirror| mirror.holds(&scope, &key.thread_ts))
        {
            return;
        }
        let source = Source::Conversation(key.channel.clone());
        let channel = key.channel.clone();
        let ts = key.thread_ts.clone();
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
    fn ensure_thread_loaded(&mut self, key: &ThreadKey, cx: &mut Context<Self>) {
        let source = Source::Thread(key.clone());
        if self.loaded.contains_key(&source) {
            return;
        }
        self.loaded.insert(
            source.clone(),
            Loaded {
                loading: true,
                ..Loaded::default()
            },
        );
        self.fetch(source, None, false, None, cx);
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
                Source::Thread(key) => {
                    client
                        .conversations_replies(&key.channel, &key.thread_ts, cursor.as_deref())
                        .await
                }
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
                                // thread comes whole or not at all, so there
                                // is nothing to say about it.
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
        let path = match file_cache_path(file) {
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
        let Ok(path) = file_cache_path(file) else {
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
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            let path = file_cache_path(&file)?;
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
                let _ = this.update(cx, |session, cx| {
                    session.signal(Some(Signal::Degraded(format!("slack: {error:#}"))), cx);
                });
            }
        }));
    }

    /// Tells Slack the conversation has been read, so rho does not leave the
    /// phone showing a badge for something the user has already seen.
    pub fn mark_read(&mut self, source: &Source, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let channel = source.channel().clone();
        let Some(latest) = self
            .loaded
            .get(source)
            .and_then(|loaded| loaded.messages.last())
            .map(|message| message.ts.clone())
        else {
            return;
        };
        self.model.mark_read(&channel, &latest);
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

    /// Slack's ignore thread: the discard the user just made here, made
    /// everywhere they read Slack. One request, and rho keeps no
    /// subscription state of its own, so the socket's `thread_unsubscribed`
    /// that follows is the confirmation rather than a second source of
    /// truth. A failure is reported and changes nothing local: the discard
    /// already stands.
    pub fn ignore_thread(&mut self, key: &ThreadKey, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.model.unfollow(&key.channel, &key.thread_ts);
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
            self.model.mark_read(channel, ts);
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
    pub fn send(&mut self, source: &Source, text: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        self.pending_sends += 1;
        let channel = source.channel().clone();
        let thread_ts = source.thread_ts().cloned();
        let source = source.clone();
        let body = text.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            client
                .post_message(&channel, thread_ts.as_ref(), &body)
                .await
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let sent = match task.await {
                Ok(sent) => sent,
                Err(error) => Err(anyhow::anyhow!("{error}")),
            };
            let _ = this.update(cx, |session, cx| {
                session.pending_sends = session.pending_sends.saturating_sub(1);
                match sent {
                    Ok(ts) => session.accept_own(&source, ts, text, cx),
                    Err(error) => {
                        tracing::warn!(error = %error, "slack send failed");
                        if let Some(loaded) = session.loaded.get_mut(&source) {
                            loaded.error = Some(format!("{error:#}"));
                        }
                    }
                }
                cx.notify();
            });
        }));
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

/// The mirror lives beside rho's other state. A machine without a state
/// directory simply runs without one: the client still works, it just has
/// nothing to show before the first response.
fn open_mirror() -> Option<Arc<Mirror>> {
    let path = dirs::state_dir()?.join("rho").join("slack.redb");
    match Mirror::open(&path) {
        Ok(mirror) => Some(Arc::new(mirror)),
        Err(error) => {
            tracing::warn!(error = %error, "slack mirror unavailable");
            None
        }
    }
}

fn file_cache_path(file: &crate::types::FileSummary) -> anyhow::Result<std::path::PathBuf> {
    let base = dirs::state_dir().context("state directory not available")?;
    let name = file
        .title
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("file");
    Ok(base
        .join("rho/slack-files")
        .join(format!("{}-{name}", file.id)))
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
}
