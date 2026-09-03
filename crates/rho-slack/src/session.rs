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

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{Context, EventEmitter, Task};
use tokio::sync::Notify;

use crate::api::Client;
use crate::config::Credentials;
use crate::events::WsEvent;
use crate::health::{Health, Signal};
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
            _tasks: Vec::new(),
        };
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

    /// Users, conversations, and unread counts: everything the list surface
    /// needs before a single message arrives.
    fn load_roster(&mut self, client: Arc<Client>, cx: &mut Context<Self>) {
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            let users = client.users().await;
            let conversations = client.conversations().await;
            let counts = client.counts().await;
            (users, conversations, counts)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Ok((users, conversations, counts)) = task.await else {
                return;
            };
            let _ = this.update(cx, |session, cx| {
                if let Ok(users) = users {
                    session.model.add_users(users);
                }
                if let Ok(conversations) = conversations {
                    session.model.add_conversations(conversations);
                }
                if let Ok(counts) = counts {
                    session.model.set_counts(counts.conversations);
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
    /// conversation, and the thread when it is a reply.
    fn route(&mut self, message: &Message) {
        let sources = [
            Source::Conversation(message.channel.clone()),
            Source::Thread(ThreadKey {
                workspace: self.model.workspace().clone(),
                channel: message.channel.clone(),
                thread_ts: message.thread_root(),
            }),
        ];
        for source in sources {
            // A thread reply belongs in its thread, and in the channel only
            // when Slack also broadcast it there.
            if matches!(&source, Source::Conversation(_)) && message.thread_ts.is_some() {
                continue;
            }
            if let Some(loaded) = self.loaded.get_mut(&source) {
                insert_message(&mut loaded.messages, message.clone());
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
        self.loaded.insert(
            source.clone(),
            Loaded {
                loading: true,
                ..Loaded::default()
            },
        );
        self.fetch(source.clone(), None, true, cx);
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
        loaded.loading = true;
        self.fetch(source.clone(), cursor, false, cx);
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
        self.fetch(source, None, false, cx);
    }

    fn fetch(
        &mut self,
        source: Source,
        cursor: Option<String>,
        mark_read: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let request = source.clone();
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            match &request {
                Source::Conversation(channel) => {
                    client
                        .conversations_history(channel, cursor.as_deref())
                        .await
                }
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
                {
                    let loaded = session.loaded.entry(source.clone()).or_default();
                    loaded.loading = false;
                    match page {
                        Ok(page) => {
                            loaded.error = None;
                            loaded.reached_oldest = page.older_cursor.is_none();
                            loaded.older_cursor = page.older_cursor;
                            for message in page.messages {
                                insert_message(&mut loaded.messages, message);
                            }
                        }
                        Err(error) => loaded.error = Some(format!("{error:#}")),
                    }
                }
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
