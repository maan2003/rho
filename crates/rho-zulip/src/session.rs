//! The live client: credentials, the event queue, and the message stores
//! the surfaces read.
//!
//! Every view observes this entity and re-reads what it needs on notify,
//! so two panes over the same conversation stay identical without either
//! one owning the data.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{Context, Task};

use crate::api::{Anchor, Client};
use crate::config::Credentials;
use crate::events::SessionEvent;
use crate::model::Model;
use crate::types::Message;
use crate::{Destination, Narrow};

/// How many messages a conversation loads on entry. A topic is usually
/// shorter than this, so the common case is one request and done.
const PAGE: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// No credentials yet, or the first register is in flight.
    Connecting,
    Connected,
    /// Terminal for the session: bad credentials, no zuliprc, or a server
    /// that kept refusing. The inbox shows this instead of an empty list.
    Failed(String),
}

/// One conversation's loaded messages, oldest first.
#[derive(Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub loading: bool,
    /// Whether the oldest message in the narrow has been reached, so the
    /// surface can stop offering to load more.
    pub found_oldest: bool,
    pub error: Option<String>,
}

pub struct Session {
    client: Option<Arc<Client>>,
    model: Model,
    status: Status,
    conversations: HashMap<Narrow, Conversation>,
    /// Sends that have not been acknowledged, shown as pending so a slow
    /// server never looks like a dropped message.
    pending_sends: usize,
    _event_task: Option<Task<()>>,
}

impl Session {
    /// Starts a session from `~/.zuliprc`. Missing or malformed
    /// credentials are a failed session rather than an error to the host:
    /// the surface explains itself, and nothing else in the GUI cares.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut session = Self {
            client: None,
            model: Model::default(),
            status: Status::Connecting,
            conversations: HashMap::new(),
            pending_sends: 0,
            _event_task: None,
        };
        match Credentials::discover().and_then(Client::new) {
            Ok(client) => session.connect(Arc::new(client), cx),
            Err(error) => session.status = Status::Failed(format!("{error:#}")),
        }
        session
    }

    /// Starts a session against an already-built client, for tests and for
    /// hosts that source credentials elsewhere.
    pub fn with_client(client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        let mut session = Self {
            client: None,
            model: Model::default(),
            status: Status::Connecting,
            conversations: HashMap::new(),
            pending_sends: 0,
            _event_task: None,
        };
        session.connect(client, cx);
        session
    }

    fn connect(&mut self, client: Arc<Client>, cx: &mut Context<Self>) {
        let (sink, mut events) = mpsc::unbounded();
        let loop_client = client.clone();
        self.client = Some(client);
        // The queue loop is a plain future: the host decides which runtime
        // carries it by driving this entity's tasks.
        self._event_task = Some(cx.spawn(async move |this, cx| {
            let pump = crate::events::run(loop_client, sink);
            let drain = async {
                while let Some(event) = events.next().await {
                    if this
                        .update(cx, |session, cx| session.apply(event, cx))
                        .is_err()
                    {
                        return;
                    }
                }
            };
            futures::future::join(pump, drain).await;
        }));
    }

    fn apply(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::Connected(register) => {
                self.status = Status::Connected;
                self.model.apply_register(*register);
            }
            SessionEvent::Events(events) => {
                for event in &events {
                    let change = self.model.apply_event(event);
                    if let Some((narrow, message_id)) = change.message {
                        self.route_message(&narrow, message_id, event);
                    }
                }
            }
            // A dropped connection is chrome, not a failure: the loop is
            // still retrying, and the last-known listing stays readable.
            SessionEvent::Disconnected(reason) => {
                tracing::warn!(reason, "zulip event queue disconnected");
            }
        }
        cx.notify();
    }

    /// Appends a newly arrived message to the conversation if it is loaded.
    /// A conversation that was never opened stays unloaded; its unread
    /// count in the inbox is what the user sees until they enter it.
    fn route_message(&mut self, narrow: &Narrow, message_id: u64, event: &crate::types::Event) {
        let crate::types::Event::Message { message, .. } = event else {
            return;
        };
        for (key, conversation) in self.conversations.iter_mut() {
            let belongs = key == narrow
                || key.accepts(message.stream_id, &message.topic)
                || (matches!(key, Narrow::Mentions) && message.mentions_you());
            if belongs
                && !conversation
                    .messages
                    .iter()
                    .any(|held| held.id == message_id)
            {
                conversation.messages.push((**message).clone());
            }
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn pending_sends(&self) -> usize {
        self.pending_sends
    }

    pub fn conversation(&self, narrow: &Narrow) -> Option<&Conversation> {
        self.conversations.get(narrow)
    }

    /// Ensures a conversation is loaded, fetching its most recent page the
    /// first time it is entered. Re-entering a loaded conversation is free,
    /// which is what makes the `n`-key reading loop fast.
    pub fn open(&mut self, narrow: &Narrow, cx: &mut Context<Self>) {
        if self.conversations.contains_key(narrow) {
            return;
        }
        self.conversations.insert(
            narrow.clone(),
            Conversation {
                loading: true,
                ..Conversation::default()
            },
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let narrow = narrow.clone();
        cx.spawn(async move |this, cx| {
            let page = client
                .messages(&narrow, Anchor::Newest, PAGE, 0)
                .await
                .map(|page| (page.messages, page.found_oldest));
            let _ = this.update(cx, |session, cx| {
                let entry = session.conversations.entry(narrow).or_default();
                entry.loading = false;
                match page {
                    Ok((messages, found_oldest)) => {
                        // Server order is oldest-first; anything that
                        // arrived over the queue while the page was in
                        // flight is appended after it.
                        let arrived = std::mem::take(&mut entry.messages);
                        entry.messages = messages;
                        for message in arrived {
                            if !entry.messages.iter().any(|held| held.id == message.id) {
                                entry.messages.push(message);
                            }
                        }
                        entry.found_oldest = found_oldest;
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Loads the page before the oldest message held for `narrow`.
    pub fn load_older(&mut self, narrow: &Narrow, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversations.get_mut(narrow) else {
            return;
        };
        if conversation.loading || conversation.found_oldest {
            return;
        }
        let Some(oldest) = conversation.messages.first().map(|message| message.id) else {
            return;
        };
        conversation.loading = true;
        let Some(client) = self.client.clone() else {
            return;
        };
        let narrow = narrow.clone();
        cx.spawn(async move |this, cx| {
            let page = client.messages(&narrow, Anchor::Id(oldest), PAGE, 0).await;
            let _ = this.update(cx, |session, cx| {
                let entry = session.conversations.entry(narrow).or_default();
                entry.loading = false;
                match page {
                    Ok(page) => {
                        entry.found_oldest = page.found_oldest;
                        let held = entry.messages.clone();
                        entry.messages = page.messages;
                        for message in held {
                            if !entry.messages.iter().any(|older| older.id == message.id) {
                                entry.messages.push(message);
                            }
                        }
                    }
                    Err(error) => entry.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sends `content` to `destination`. The message appears when the
    /// server echoes it back over the event queue, so a send never
    /// invents a message that the server did not accept.
    pub fn send(&mut self, destination: Destination, content: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.pending_sends += 1;
        cx.spawn(async move |this, cx| {
            let sent = client.send(&destination, &content).await;
            let _ = this.update(cx, |session, cx| {
                session.pending_sends = session.pending_sends.saturating_sub(1);
                if let Err(error) = sent {
                    tracing::warn!(error = %error, "zulip send failed");
                    session.status = Status::Connected;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Marks every unread message in `narrow` read, the way leaving a Gnus
    /// summary buffer marks its articles read. Counts drop immediately;
    /// the server's own flag event confirms them.
    pub fn mark_read(&mut self, narrow: &Narrow, cx: &mut Context<Self>) {
        let ids = self.model.unread_in(narrow);
        if ids.is_empty() {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let marked = client.mark_read(&ids).await;
            if let Err(error) = marked {
                tracing::warn!(error = %error, "zulip mark-read failed");
            }
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// The next unread conversation after `current`, for the reading loop.
    pub fn next_unread(&self, current: Option<&Narrow>) -> Option<Narrow> {
        self.model.next_unread(current)
    }
}
