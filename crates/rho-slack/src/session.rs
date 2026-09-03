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
use gpui::{Context, EventEmitter, Task};
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
    Health(Signal),
}

#[derive(Default)]
pub struct Loaded {
    /// Oldest first, the order the surface renders.
    pub messages: Vec<Message>,
    pub loading: bool,
    pub reached_oldest: bool,
    older_cursor: Option<String>,
    pub error: Option<String>,
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
            (users, conversations, counts, emoji)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok((users, conversations, counts, emoji)) = task.await else {
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
                // Anything raised before the roster landed was named
                // "#a conversation"; now it has a name, so say so again.
                let known = session
                    .model
                    .tracked()
                    .into_iter()
                    .map(Change::Updated)
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
            Wire::Frame(WsEvent::Marked { channel, ts }) => {
                // Read elsewhere: Slack's own read cursor is the verdict, so
                // the obligation is discharged here too.
                let changes = self.model.mark_read(&channel, &ts);
                self.announce(changes, cx);
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
            mirror.insert_messages(&thread, std::slice::from_ref(message));
            if message.is_top_level() {
                let conversation = Scope::conversation(&workspace, &message.channel);
                mirror.insert_messages(&conversation, std::slice::from_ref(message));
            }
        }
        let conversation = Source::Conversation(message.channel.clone());
        let thread = Source::Thread(ThreadKey {
            workspace: self.model.workspace().clone(),
            channel: message.channel.clone(),
            thread_ts: message.thread_root(),
        });
        if let Some(loaded) = self.loaded.get_mut(&thread) {
            insert_message(&mut loaded.messages, message.clone());
        }
        let Some(loaded) = self.loaded.get_mut(&conversation) else {
            return;
        };
        if message.is_top_level() {
            insert_message(&mut loaded.messages, message.clone());
        }
        if message.thread_ts.is_some() {
            // A reply is not channel content, but the fact that the thread
            // grew is: the count line is how the reader learns it.
            let root = message.thread_root();
            if let Some(parent) = loaded
                .messages
                .iter_mut()
                .find(|candidate| candidate.ts == root)
            {
                parent.reply_count = parent.reply_count.saturating_add(1);
                parent.latest_reply = Some(message.ts.clone());
            }
        }
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
        let since = cached.last().map(|message| message.ts.clone());
        self.loaded.insert(
            source.clone(),
            Loaded {
                loading: true,
                messages: cached,
                ..Loaded::default()
            },
        );
        self.fetch(source.clone(), None, true, since, cx);
    }

    /// Pages further back, which is what scrolling to the top asks for.
    pub fn load_older(&mut self, source: &Source, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded.get_mut(source) else {
            return;
        };
        if loaded.loading || loaded.reached_oldest {
            return;
        }
        let cursor = loaded.older_cursor.clone();
        if cursor.is_none() {
            loaded.reached_oldest = true;
            return;
        }
        let _ = loaded;
        // The mirror knows when there is nothing older: the first page ever
        // sent here is a fact, recorded when Slack said `has_more: false`.
        // Asking again would be a request whose answer is already on disk.
        if self
            .mirror
            .as_ref()
            .is_some_and(|mirror| mirror.history_begins(&self.scope(source)))
        {
            if let Some(loaded) = self.loaded.get_mut(source) {
                loaded.reached_oldest = true;
            }
            return;
        }
        if let Some(loaded) = self.loaded.get_mut(source) {
            loaded.loading = true;
        }
        self.fetch(source.clone(), cursor, false, None, cx);
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
                            }
                            fetched = page.messages.clone();
                            for message in page.messages {
                                insert_message(&mut loaded.messages, message);
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
        let changes = self.model.mark_read(&channel, &latest);
        self.announce(changes, cx);
        let task =
            gpui_tokio::Tokio::spawn(cx, async move { client.mark_read(&channel, &latest).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = task.await {
                tracing::warn!(error = %error, "slack mark-read failed");
            }
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

/// Inserts in timestamp order, ignoring one rho already holds. Both sources
/// can deliver the same message, and a page can arrive after the socket.
fn insert_message(messages: &mut Vec<Message>, message: Message) {
    match messages.binary_search_by(|held| {
        held.ts
            .epoch_seconds()
            .total_cmp(&message.ts.epoch_seconds())
    }) {
        Ok(_) => {}
        Err(index) => messages.insert(index, message),
    }
}

/// Where a downloaded file lives: the state cache, keyed on Slack's file id
/// so two files with the same name never collide.
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
    fn a_message_held_twice_is_held_once() {
        let mut messages = Vec::new();
        insert_message(&mut messages, message("2.0", "second"));
        insert_message(&mut messages, message("1.0", "first"));
        insert_message(&mut messages, message("2.0", "second again"));
        assert_eq!(
            messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "the socket and a page deliver the same message"
        );
    }
}
