//! The workset's dev shell cache client and its daemon-side service. The
//! daemon owns the entries and their GC roots (`rho-devshell-daemon`); the
//! workset checks, evaluates and pins in its own namespace (`rho-devshell`).
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rho_devshell_daemon::Candidate;
use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot};

use super::{transport, workset};

#[derive(Encode, Decode)]
pub(super) enum Request {
    Lookup(String),
    Used(u64),
    Store {
        key: String,
        env_store_path: String,
        data: Vec<u8>,
    },
    Forget(u64),
}

#[derive(Encode, Decode)]
pub(super) enum Reply {
    Candidates(Vec<Candidate>),
    Rooted(bool),
    Stored(u64),
    Done,
    Error(String),
}

#[derive(Encode, Decode)]
pub(super) enum Message {
    Request { id: u64, body: Request },
    Reply { id: u64, body: Reply },
}

async fn send(sender: &transport::Sender, message: Message) -> anyhow::Result<()> {
    sender
        .send(
            transport::Port::Workset,
            workset::encode(&workset::Message::Devshell(message))?,
        )
        .await?;
    Ok(())
}

#[derive(Default)]
struct Replies {
    closed: bool,
    calls: HashMap<u64, oneshot::Sender<Reply>>,
}

struct Pending {
    id: u64,
    replies: Arc<Mutex<Replies>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.replies.lock().expect("poison").calls.remove(&self.id);
    }
}

/// The worker's end: a [`rho_devshell::Cache`] over the workset connection.
pub(super) struct Host {
    sender: transport::Sender,
    next: Arc<AtomicU64>,
    replies: Arc<Mutex<Replies>>,
}

impl Host {
    pub(super) fn new(sender: transport::Sender, next: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            sender,
            next,
            replies: Arc::default(),
        })
    }

    // Called by the workset reader; never awaits.
    pub(super) fn receive(&self, message: Message) -> anyhow::Result<()> {
        let Message::Reply { id, body } = message else {
            anyhow::bail!("unexpected dev shell request");
        };
        if let Some(reply) = self.replies.lock().expect("poison").calls.remove(&id) {
            let _ = reply.send(body);
        }
        Ok(())
    }

    pub(super) fn disconnect(&self) {
        let mut replies = self.replies.lock().expect("poison");
        replies.closed = true;
        replies.calls.clear();
    }

    async fn request(self: Arc<Self>, body: Request) -> anyhow::Result<Reply> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        {
            let mut replies = self.replies.lock().expect("poison");
            anyhow::ensure!(!replies.closed, "daemon connection closed");
            replies.calls.insert(id, reply);
        }
        let _pending = Pending {
            id,
            replies: self.replies.clone(),
        };
        send(&self.sender, Message::Request { id, body }).await?;
        match response
            .await
            .map_err(|_| anyhow::anyhow!("daemon connection closed"))?
        {
            Reply::Error(error) => Err(anyhow::anyhow!("{error}")),
            reply => Ok(reply),
        }
    }
}

/// Shares one [`Host`] as the process's cache.
pub(super) struct Cache(pub Arc<Host>);

impl rho_devshell::Cache for Cache {
    fn lookup(&self, key: String) -> BoxFuture<'static, anyhow::Result<Vec<rho_devshell::Candidate>>> {
        let host = self.0.clone();
        Box::pin(async move {
            match host.request(Request::Lookup(key)).await? {
                Reply::Candidates(candidates) => Ok(candidates
                    .into_iter()
                    .map(|candidate| rho_devshell::Candidate {
                        id: candidate.id,
                        env_store_path: candidate.env_store_path,
                        data: candidate.data,
                    })
                    .collect()),
                _ => anyhow::bail!("unexpected dev shell cache reply"),
            }
        })
    }

    fn used(&self, id: u64) -> BoxFuture<'static, anyhow::Result<bool>> {
        let host = self.0.clone();
        Box::pin(async move {
            match host.request(Request::Used(id)).await? {
                Reply::Rooted(rooted) => Ok(rooted),
                _ => anyhow::bail!("unexpected dev shell cache reply"),
            }
        })
    }

    fn store(
        &self,
        key: String,
        env_store_path: String,
        data: Vec<u8>,
    ) -> BoxFuture<'static, anyhow::Result<u64>> {
        let host = self.0.clone();
        Box::pin(async move {
            match host
                .request(Request::Store {
                    key,
                    env_store_path,
                    data,
                })
                .await?
            {
                Reply::Stored(id) => Ok(id),
                _ => anyhow::bail!("unexpected dev shell cache reply"),
            }
        })
    }

    fn forget(&self, id: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.0.clone();
        Box::pin(async move {
            match host.request(Request::Forget(id)).await? {
                Reply::Done => Ok(()),
                _ => anyhow::bail!("unexpected dev shell cache reply"),
            }
        })
    }
}

/// The daemon's end for one workset connection. The store serializes its
/// operations; requests are answered in order.
pub(super) async fn serve(
    store: Arc<rho_devshell_daemon::Store>,
    sender: transport::Sender,
    mut incoming: mpsc::Receiver<Message>,
) -> anyhow::Result<()> {
    while let Some(message) = incoming.recv().await {
        let Message::Request { id, body } = message else {
            anyhow::bail!("unexpected dev shell message");
        };
        let result = match body {
            Request::Lookup(key) => store.lookup(&key).await.map(Reply::Candidates),
            Request::Used(id) => store.used(id).await.map(Reply::Rooted),
            Request::Store {
                key,
                env_store_path,
                data,
            } => store.store(key, env_store_path, data).await.map(Reply::Stored),
            Request::Forget(id) => store.forget(id).await.map(|()| Reply::Done),
        };
        let body = result.unwrap_or_else(|error| Reply::Error(format!("{error:#}")));
        send(&sender, Message::Reply { id, body }).await?;
    }
    anyhow::bail!("workset dev shell connection closed")
}
