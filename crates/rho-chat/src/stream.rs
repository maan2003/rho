//! One host's chat session: the host's log to whoever folds it, and what
//! the user did in the chat to the host, on call streams of their own.
//!
//! The host opens the session on every connection
//! ([`rho_agent_hosts::HostStream`]); what is said on it and where its
//! frames go are this crate's.

use std::sync::{Arc, Mutex};

use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_agent_hosts::{Dialer, HostId, HostStream};
use rho_agent_types::{ChatRecord, ChatSeq};
use rho_rpc::protocol::{Call, read_frame, write_frame, write_open};

use crate::protocol::{ClientFrame, Open, ServerFrame};

/// What reaches the fold from one host.
#[derive(Debug)]
pub enum ChatEvent {
    /// The session opened: the newest event on the host.
    Hello { host: HostId, head: ChatSeq },
    /// A run of the log, oldest first.
    Records {
        host: HostId,
        records: Vec<ChatRecord>,
    },
    /// The session ended; it opens again with the connection.
    Closed { host: HostId },
}

/// Where this client stands in the host's log: what each new session is
/// asked to send from. Moved by whoever folds the records.
#[derive(Clone, Default)]
pub struct Cursor(Arc<Mutex<ChatSeq>>);

impl Cursor {
    pub fn new(since: ChatSeq) -> Self {
        Self(Arc::new(Mutex::new(since)))
    }

    pub fn get(&self) -> ChatSeq {
        *self.0.lock().unwrap()
    }

    /// The newest record held; a session asks for everything after it.
    pub fn advance(&self, to: ChatSeq) {
        let mut held = self.0.lock().unwrap();
        *held = (*held).max(to);
    }

    /// Back to nothing: the host's log is not the one the copy holds.
    pub fn reset(&self) {
        *self.0.lock().unwrap() = ChatSeq::ZERO;
    }
}

/// One host's chat session, as the host keeps it across reconnects.
pub struct ChatStream {
    host: HostId,
    cursor: Cursor,
    events: futures_mpsc::UnboundedSender<ChatEvent>,
}

impl ChatStream {
    pub fn new(
        host: HostId,
        cursor: Cursor,
        events: futures_mpsc::UnboundedSender<ChatEvent>,
    ) -> Self {
        Self {
            host,
            cursor,
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
        let cursor = self.cursor.clone();
        let events = self.events.clone();
        Box::pin(async move {
            // Bulk priority: a catch-up must not hold up anything interactive.
            let mut socket = dialer.open(None).await?;
            write_open(&mut socket, &Open::Session).await?;
            let result = async {
                let ServerFrame::Hello { head } = read_frame(&mut socket).await? else {
                    anyhow::bail!("the chat session did not open with its hello");
                };
                if events
                    .unbounded_send(ChatEvent::Hello { host, head })
                    .is_err()
                {
                    return Ok(());
                }
                // The fold may have reset the cursor on hearing the hello:
                // the host's log is not the one the copy holds.
                let since = cursor.get();
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
