//! One host's chat session: the host's log to whoever folds it, and what
//! the user did in the chat to the host, on call streams of their own.
//!
//! The host opens the session on every connection
//! ([`rho_agent_hosts::HostStream`]); what is said on it and where its
//! frames go are this crate's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_agent_hosts::{Dialer, HostId, HostStream};
use rho_agent_types::{ChatRecord, ChatSeq, EndpointId};
use rho_rpc::protocol::{Call, read_frame, write_frame, write_open};

use crate::protocol::{ClientFrame, Open, ServerFrame};

/// What reaches the fold from one host.
#[derive(Debug)]
pub enum ChatEvent {
    /// The session opened: whose log the host carries and how far it
    /// goes. `fresh` says the copy held here was not of this log and was
    /// dropped: the records that follow start from the beginning.
    Hello {
        host: HostId,
        /// Whose log: the host's iroh identity.
        endpoint: EndpointId,
        head: ChatSeq,
        fresh: bool,
    },
    /// A run of the log, oldest first.
    Records {
        host: HostId,
        records: Vec<ChatRecord>,
    },
    /// The session ended; it opens again with the connection.
    Closed { host: HostId },
}

/// Where this client stands in each log, by the identity of the host that
/// keeps it: what a session is asked to send from. Shared between the
/// sessions, which read it on their hello, and whoever folds the records,
/// who moves it. A log is named by its seed and not by the connection, so
/// a host attached under a new name goes on from where its copy stands.
#[derive(Clone, Default)]
pub struct Cursors(Arc<Mutex<HashMap<EndpointId, ChatSeq>>>);

impl Cursors {
    pub fn new(held: impl IntoIterator<Item = (EndpointId, ChatSeq)>) -> Self {
        Self(Arc::new(Mutex::new(held.into_iter().collect())))
    }

    pub fn get(&self, host: EndpointId) -> ChatSeq {
        self.0
            .lock()
            .unwrap()
            .get(&host)
            .copied()
            .unwrap_or(ChatSeq::ZERO)
    }

    /// The newest record held of a log; a session asks for everything
    /// after it.
    pub fn advance(&self, host: EndpointId, to: ChatSeq) {
        let mut held = self.0.lock().unwrap();
        let at = held.entry(host).or_insert(ChatSeq::ZERO);
        *at = (*at).max(to);
    }

    /// Back to nothing: the host's log is not the one the copy holds.
    pub fn reset(&self, host: EndpointId) {
        self.0.lock().unwrap().remove(&host);
    }
}

/// One host's chat session, as the host keeps it across reconnects.
pub struct ChatStream {
    host: HostId,
    cursors: Cursors,
    events: futures_mpsc::UnboundedSender<ChatEvent>,
}

impl ChatStream {
    pub fn new(
        host: HostId,
        cursors: Cursors,
        events: futures_mpsc::UnboundedSender<ChatEvent>,
    ) -> Self {
        Self {
            host,
            cursors,
            events,
        }
    }
}

impl HostStream for ChatStream {
    fn name(&self) -> &'static str {
        "chat"
    }

    /// `Hello`, then the log after the cursor and each new record, for as
    /// long as the connection lasts.
    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let cursors = self.cursors.clone();
        let events = self.events.clone();
        Box::pin(async move {
            // Bulk priority: a catch-up must not hold up anything interactive.
            let mut socket = dialer.open(None).await?;
            write_open(&mut socket, &Open::Session).await?;
            let result = async {
                let ServerFrame::Hello {
                    head,
                    host: endpoint,
                    ..
                } = read_frame(&mut socket).await?
                else {
                    anyhow::bail!("the chat session did not open with its hello");
                };
                // A copy that stands past the host's head is of some other
                // log (the host's database was replaced): it starts over.
                let mut since = cursors.get(endpoint);
                let fresh = since > head;
                if fresh {
                    cursors.reset(endpoint);
                    since = ChatSeq::ZERO;
                }
                if events
                    .unbounded_send(ChatEvent::Hello {
                        host,
                        endpoint,
                        head,
                        fresh,
                    })
                    .is_err()
                {
                    return Ok(());
                }
                write_frame(&mut socket, &ClientFrame::Follow { since }).await?;
                loop {
                    let frame = read_frame::<_, ServerFrame>(&mut socket).await?;
                    let event = match frame {
                        ServerFrame::Chat { records } => ChatEvent::Records { host, records },
                        ServerFrame::Hello { .. } => continue,
                    };
                    if events.unbounded_send(event).is_err() {
                        return Ok(());
                    }
                }
            }
            .await;
            let _ = events.unbounded_send(ChatEvent::Closed { host });
            result
        })
    }
}

/// One call to a host's chat, on a stream of its own.
pub async fn call<C: Call>(dialer: &Dialer, call: C) -> anyhow::Result<C::Reply> {
    let mut stream = dialer.open(C::PRIORITY).await?;
    rho_rpc::protocol::call(&mut stream, call).await
}
