//! A private, full-duplex Unix channel. The reader only routes messages;
//! it never waits for a service request to finish.
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rho_core::{ExecId, UnixMs};
use rho_inference::{DialRoute, InferenceHost, QuotaUpdate, RouteSelection, SelectedAuth};
use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot, watch};

use crate::AgentEvent;
use crate::db::{
    AgentEventPos, AgentHead, AgentRole, AgentUsageBucket, ClaudeRewind, SessionBinding, TurnEdge,
};

pub(super) const VERSION: u32 = 2;

#[derive(Encode, Decode)]
pub(super) struct Bootstrap {
    pub cwd: camino::Utf8PathBuf,
}

#[derive(Encode, Decode)]
pub(super) enum Control {
    Retire,
    User {
        content: Vec<rho_core::ContentPart>,
        delivery: rho_core::MessageDelivery,
    },
    Mail {
        sender: rho_core::AgentId,
        label: String,
        body: String,
        delivery: rho_core::MessageDelivery,
    },
    NoticeCarried,
    TellTail,
    Compact,
    Cancel,
    Retry,
    Effort(rho_claude::Effort),
    Role(crate::db::AgentRole),
    CacheKey,
    Rewind(u32),
}

#[derive(Encode, Decode)]
pub(super) enum Request<'a> {
    Name(String),
    Team,
    SharedTool(rho_core::ToolCall),
    Usage(AgentUsageBucket),
    Completed(String),
    Failed(String),
    Settled,
    Head,
    History,
    Append(AgentEvent<'a>),
    ExecAdmitted(ExecId),
    Profile {
        role: AgentRole,
        binding: SessionBinding,
    },
    CacheKey(rho_inference::PromptCacheKey),
    Rewind {
        at: UnixMs,
        to: AgentEventPos,
    },
    ClaudeRewind {
        at: UnixMs,
        to: Option<AgentEventPos>,
        rewind: Option<ClaudeRewind>,
    },
    CompleteClaudeRewind(uuid::Uuid),
    ClaudeAccount,
    ClaudePendingOutput,
    UsageTotal,
    Turn {
        at: UnixMs,
        edge: TurnEdge,
    },
    SelectAccount,
    ResolveAuth(rho_inference::InferenceAuth),
    RateLimited(SelectedAuth),
    Quota {
        selected: SelectedAuth,
        quota: QuotaUpdate,
    },
    RouteFailed {
        route: DialRoute,
        selected: Option<SelectedAuth>,
    },
}

#[derive(Encode, Decode)]
pub(super) enum Reply {
    Team(Option<crate::multi_agent_tools::Team>),
    Tool(rho_core::ToolOutput),
    Head(AgentHead),
    History {
        next: AgentEventPos,
        rows: Vec<(AgentEventPos, AgentEvent<'static>)>,
    },
    Position(AgentEventPos),
    Admitted(bool),
    ClaudeAccount(String),
    ClaudePendingOutput(Option<crate::ClaudeOutputBatch>),
    Usage(AgentUsageBucket),
    Error(String),
    Account(SelectedAuth),
    Auth(rho_inference::ResolvedAuth),
    RateLimited(bool),
    Done,
}

#[derive(Encode, Decode)]
pub(super) enum Message<'a> {
    Stop,
    Stopped {
        error: Option<String>,
    },
    Bootstrap(Bootstrap),
    Ready {
        status: crate::AgentStatus,
    },
    Control {
        id: u64,
        body: Control,
    },
    Controlled {
        id: u64,
        error: Option<String>,
    },
    Named(AgentHead),
    Status {
        status: crate::AgentStatus,
        queue: Option<Vec<rho_ui_proto::mirror::QueuedItem>>,
        reset: bool,
    },
    HistoryBatch {
        id: u64,
        rows: Vec<(AgentEventPos, AgentEvent<'static>)>,
    },
    Request {
        id: u64,
        body: Request<'a>,
    },
    Reply {
        id: u64,
        body: Reply,
    },
    Route(RouteSelection),
}

pub(super) fn decode(bytes: &[u8]) -> io::Result<Message<'static>> {
    let mut remaining = bytes;
    let message = senax_encoder::decode(&mut remaining)
        .map_err(|_| io::Error::other("invalid agent message"))?;
    if !remaining.is_empty() {
        return Err(io::Error::other("trailing agent message data"));
    }
    Ok(message)
}

pub(super) fn encode(message: &Message<'_>) -> io::Result<bytes::Bytes> {
    let mut bytes = bytes::BytesMut::new();
    senax_encoder::encode_to(message, &mut bytes)
        .map_err(|_| io::Error::other("invalid agent message"))?;
    Ok(bytes.freeze())
}

#[derive(Default)]
struct Replies {
    closed: bool,
    calls: HashMap<u64, Waiting>,
}
struct Waiting {
    reply: oneshot::Sender<Reply>,
    history: Vec<(AgentEventPos, AgentEvent<'static>)>,
}

type Waiters = Arc<Mutex<Replies>>;

struct Pending {
    id: u64,
    waiters: Waiters,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.waiters.lock().expect("poison").calls.remove(&self.id);
    }
}

/// A coalesced observation, not another runtime-state owner. The loop updates
/// its existing status slot; the writer snapshots only when it can send.
#[derive(Default)]
struct Publication {
    queue: Mutex<Option<Vec<rho_ui_proto::mirror::QueuedItem>>>,
    status: Mutex<std::sync::Weak<std::sync::RwLock<crate::AgentStatus>>>,
    changed: tokio::sync::Notify,
    full: std::sync::atomic::AtomicBool,
}

/// Worker-side services. There is no database or account manager behind this
/// handle. Dropping the last handle closes the socket and its reader task.
pub(crate) struct Host {
    outgoing: mpsc::Sender<bytes::Bytes>,
    controls: Mutex<Option<mpsc::UnboundedReceiver<(u64, Control)>>>,
    waiters: Waiters,
    next: Arc<AtomicU64>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    routes: watch::Receiver<RouteSelection>,
    closed: watch::Receiver<bool>,
    names: watch::Receiver<Option<AgentHead>>,
    publication: Arc<Publication>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHost")
            .field("closed", &*self.closed.borrow())
            .finish_non_exhaustive()
    }
}

/// A failed persistence operation has unknown commit status. Runtimes must
/// return this to their supervisor, never handle it as a provider retry.
#[derive(Debug)]
pub(crate) struct StoreError(anyhow::Error);
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for StoreError {}

impl Host {
    pub(super) fn connect(
        writer: super::transport::Sender,
        port: super::transport::Port,
        mut incoming: mpsc::Receiver<bytes::Bytes>,
        next: Arc<AtomicU64>,
    ) -> Arc<Self> {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let (outgoing, mut queue) = mpsc::channel::<bytes::Bytes>(32);
        let waiters: Waiters = Arc::default();
        let pending = waiters.clone();
        let publication = Arc::new(Publication::default());
        let published = publication.clone();
        let (routes, route_updates) = watch::channel(RouteSelection::default());
        let (closed, connection_closed) = watch::channel(false);
        let (names, name_updates) = watch::channel(None);
        tokio::spawn(async move {
            let receive = async {
                loop {
                    let message = decode(&incoming.recv().await.ok_or(io::ErrorKind::BrokenPipe)?)?;
                    match message {
                        Message::Named(head) => {
                            names.send_replace(Some(head));
                        }
                        Message::Reply { id, body } => {
                            if let Some(waiter) = pending.lock().expect("poison").calls.remove(&id)
                            {
                                let body = match body {
                                    Reply::History { next, mut rows } => {
                                        let mut history = waiter.history;
                                        history.append(&mut rows);
                                        Reply::History {
                                            next,
                                            rows: history,
                                        }
                                    }
                                    reply => reply,
                                };
                                let _ = waiter.reply.send(body);
                            }
                        }
                        Message::Route(route) => {
                            routes.send_if_modified(|current| {
                                if route.revision() <= current.revision() {
                                    return false;
                                }
                                *current = route;
                                true
                            });
                        }
                        Message::HistoryBatch { id, mut rows } => {
                            if let Some(waiter) = pending.lock().expect("poison").calls.get_mut(&id)
                            {
                                waiter.history.append(&mut rows);
                            }
                        }
                        Message::Control { id, body } => {
                            control_tx
                                .send((id, body))
                                .map_err(|_| io::Error::other("agent controller stopped"))?;
                        }
                        Message::Stop => return Ok(()),
                        Message::Stopped { .. }
                        | Message::Request { .. }
                        | Message::Status { .. }
                        | Message::Bootstrap(_)
                        | Message::Ready { .. }
                        | Message::Controlled { .. } => {
                            return Err::<(), _>(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "unexpected worker request",
                            ));
                        }
                    }
                }
            };
            let send = async {
                loop {
                    tokio::select! {
                        biased;
                        message = queue.recv() => {
                            let Some(message) = message else { return Ok::<(), io::Error>(()); };
                            writer.send(port, message).await?;
                        }
                        _ = published.changed.notified() => {
                            let status = published.status.lock().expect("poison").upgrade()
                                .map(|status| status.read().expect("poison").clone());
                            if let Some(status) = status {
                                let reset = published.full.swap(false, Ordering::Relaxed);
                                let queue = published.queue.lock().expect("poison").clone();
                                writer.send(port, encode(&Message::Status { status, queue, reset })?).await?;
                            }
                        }
                    }
                }
            };
            tokio::select! {
                _ = stopped => {}
                _ = receive => {}
                _ = send => {}
            }
            // All outstanding calls fail before the worker's owner is told to
            // stop. The executor stays alive to perform normal job cleanup.
            {
                let mut pending = pending.lock().expect("poison");
                pending.closed = true;
                pending.calls.clear();
            }
            closed.send_replace(true);
        });
        Arc::new(Self {
            controls: Mutex::new(Some(control_rx)),
            outgoing,
            waiters,
            next,
            stop: Mutex::new(Some(stop)),
            routes: route_updates,
            closed: connection_closed,
            names: name_updates,
            publication,
        })
    }

    pub(super) fn controls(&self) -> mpsc::UnboundedReceiver<(u64, Control)> {
        self.controls
            .lock()
            .expect("poison")
            .take()
            .expect("one worker controller")
    }

    pub(super) async fn send(&self, message: Message<'_>) -> anyhow::Result<()> {
        self.outgoing
            .send(encode(&message)?)
            .await
            .map_err(|_| anyhow::anyhow!("agent connection closed"))
    }

    pub(super) async fn shutdown(&self) {
        if let Some(stop) = self.stop.lock().expect("poison").take() {
            let _ = stop.send(());
        }
        self.closed().await;
    }

    pub(crate) async fn team(&self) -> anyhow::Result<Option<crate::multi_agent_tools::Team>> {
        match self.request(Request::Team).await? {
            Reply::Team(team) => Ok(team),
            _ => anyhow::bail!("unexpected team reply"),
        }
    }
    pub(crate) async fn shared_tool(&self, call: rho_core::ToolCall) -> rho_core::ToolOutput {
        match self.request(Request::SharedTool(call)).await {
            Ok(Reply::Tool(output)) => output,
            result => rho_core::ToolOutput {
                output: Arc::new(match result {
                    Err(error) => error.to_string(),
                    _ => "unexpected shared service reply".into(),
                }),
                full_output: None,
                images: Arc::new(Vec::new()),
                status: rho_core::ToolOutputStatus::Error,
            },
        }
    }

    pub(crate) fn observe(&self, status: &Arc<std::sync::RwLock<crate::AgentStatus>>) {
        *self.publication.status.lock().expect("poison") = Arc::downgrade(status);
        self.tell_tail();
    }

    pub(crate) fn publish_queue(&self, queue: Vec<rho_ui_proto::mirror::QueuedItem>) {
        *self.publication.queue.lock().expect("poison") = Some(queue);
    }

    pub(crate) fn published(&self) {
        self.publication.changed.notify_one();
    }

    pub(crate) fn tell_tail(&self) {
        self.publication.full.store(true, Ordering::Relaxed);
        self.published();
    }

    pub(crate) fn names(&self) -> watch::Receiver<Option<AgentHead>> {
        self.names.clone()
    }

    pub(crate) async fn name(&self, input: &str) -> Result<AgentHead, StoreError> {
        match self
            .request(Request::Name(input.to_owned()))
            .await
            .map_err(StoreError)?
        {
            Reply::Head(head) => Ok(head),
            _ => Err(StoreError(anyhow::anyhow!("unexpected naming reply"))),
        }
    }

    pub(crate) async fn closed(&self) {
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    async fn request(&self, body: Request<'_>) -> anyhow::Result<Reply> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        {
            let mut waiters = self.waiters.lock().expect("poison");
            anyhow::ensure!(!waiters.closed, "agent service connection closed");
            waiters.calls.insert(
                id,
                Waiting {
                    reply,
                    history: Vec::new(),
                },
            );
        }
        let _pending = Pending {
            id,
            waiters: self.waiters.clone(),
        };
        self.outgoing
            .send(encode(&Message::Request { id, body })?)
            .await
            .map_err(|_| anyhow::anyhow!("agent service connection closed"))?;
        match response
            .await
            .map_err(|_| anyhow::anyhow!("agent service connection closed"))?
        {
            Reply::Error(error) => Err(anyhow::Error::msg(error)),
            reply => Ok(reply),
        }
    }
    pub(crate) async fn head(&self) -> Result<AgentHead, StoreError> {
        match self.request(Request::Head).await.map_err(StoreError)? {
            Reply::Head(head) => Ok(head),
            _ => Err(StoreError(anyhow::anyhow!("unexpected head reply"))),
        }
    }
    pub(crate) async fn history(
        &self,
    ) -> Result<(AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>), StoreError> {
        match self.request(Request::History).await.map_err(StoreError)? {
            Reply::History { next, rows } => Ok((next, rows)),
            _ => Err(StoreError(anyhow::anyhow!("unexpected history reply"))),
        }
    }
    pub(crate) async fn append(&self, event: AgentEvent<'_>) -> Result<AgentEventPos, StoreError> {
        match self
            .request(Request::Append(event))
            .await
            .map_err(StoreError)?
        {
            Reply::Position(position) => Ok(position),
            _ => Err(StoreError(anyhow::anyhow!("unexpected append reply"))),
        }
    }
    pub(crate) async fn exec_was_admitted(&self, id: ExecId) -> Result<bool, StoreError> {
        match self
            .request(Request::ExecAdmitted(id))
            .await
            .map_err(StoreError)?
        {
            Reply::Admitted(admitted) => Ok(admitted),
            _ => Err(StoreError(anyhow::anyhow!("unexpected admission reply"))),
        }
    }

    pub(crate) async fn record_usage(&self, usage: AgentUsageBucket) -> Result<(), StoreError> {
        self.change(Request::Usage(usage)).await
    }
    pub(crate) async fn completed(&self, answer: String) -> Result<(), StoreError> {
        self.change(Request::Completed(answer)).await
    }
    pub(crate) async fn failed(&self, error: String) -> Result<(), StoreError> {
        self.change(Request::Failed(error)).await
    }
    pub(crate) async fn settled(&self) -> Result<(), StoreError> {
        self.change(Request::Settled).await
    }

    pub(crate) async fn usage_total(&self) -> Result<AgentUsageBucket, StoreError> {
        match self
            .request(Request::UsageTotal)
            .await
            .map_err(StoreError)?
        {
            Reply::Usage(usage) => Ok(usage),
            _ => Err(StoreError(anyhow::anyhow!("unexpected usage reply"))),
        }
    }

    pub(crate) async fn claude_pending_output(
        &self,
    ) -> Result<Option<crate::ClaudeOutputBatch>, StoreError> {
        match self
            .request(Request::ClaudePendingOutput)
            .await
            .map_err(StoreError)?
        {
            Reply::ClaudePendingOutput(batch) => Ok(batch),
            _ => Err(StoreError(anyhow::anyhow!(
                "unexpected Claude output reply"
            ))),
        }
    }
    pub(crate) async fn claude_account(&self) -> Result<String, StoreError> {
        match self
            .request(Request::ClaudeAccount)
            .await
            .map_err(StoreError)?
        {
            Reply::ClaudeAccount(account) => Ok(account),
            _ => Err(StoreError(anyhow::anyhow!(
                "unexpected Claude account reply"
            ))),
        }
    }
    pub(crate) async fn claude_rewind(
        &self,
        at: UnixMs,
        to: Option<AgentEventPos>,
        rewind: Option<ClaudeRewind>,
    ) -> Result<(), StoreError> {
        self.change(Request::ClaudeRewind { at, to, rewind }).await
    }
    pub(crate) async fn complete_claude_rewind(
        &self,
        session: uuid::Uuid,
    ) -> Result<(), StoreError> {
        self.change(Request::CompleteClaudeRewind(session)).await
    }

    pub(crate) async fn profile(
        &self,
        role: AgentRole,
        binding: SessionBinding,
    ) -> Result<(), StoreError> {
        self.change(Request::Profile { role, binding }).await
    }
    pub(crate) async fn cache_key(
        &self,
        key: rho_inference::PromptCacheKey,
    ) -> Result<(), StoreError> {
        self.change(Request::CacheKey(key)).await
    }
    pub(crate) async fn rewind(&self, at: UnixMs, to: AgentEventPos) -> Result<(), StoreError> {
        self.change(Request::Rewind { at, to }).await
    }
    pub(crate) async fn turn(&self, at: UnixMs, edge: TurnEdge) -> Result<(), StoreError> {
        self.change(Request::Turn { at, edge }).await
    }
    async fn change(&self, request: Request<'_>) -> Result<(), StoreError> {
        match self.request(request).await.map_err(StoreError)? {
            Reply::Done => Ok(()),
            _ => Err(StoreError(anyhow::anyhow!("unexpected mutation reply"))),
        }
    }
}

impl InferenceHost for Host {
    fn select(&self) -> BoxFuture<'_, anyhow::Result<SelectedAuth>> {
        Box::pin(async {
            match self.request(Request::SelectAccount).await? {
                Reply::Account(selected) => Ok(selected),
                _ => anyhow::bail!("unexpected account service reply"),
            }
        })
    }

    fn resolve_auth(
        &self,
        auth: rho_inference::InferenceAuth,
    ) -> BoxFuture<'_, anyhow::Result<rho_inference::ResolvedAuth>> {
        Box::pin(async move {
            match self.request(Request::ResolveAuth(auth)).await? {
                Reply::Auth(auth) => Ok(auth),
                _ => anyhow::bail!("unexpected credential service reply"),
            }
        })
    }

    fn mark_rate_limited(&self, selected: SelectedAuth) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            matches!(
                self.request(Request::RateLimited(selected)).await,
                Ok(Reply::RateLimited(true))
            )
        })
    }

    fn observe_quota(&self, selected: SelectedAuth, quota: QuotaUpdate) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.request(Request::Quota { selected, quota }).await;
        })
    }

    fn route_updates(&self) -> watch::Receiver<RouteSelection> {
        self.routes.clone()
    }

    fn report_connect_failure(
        &self,
        route: DialRoute,
        selected: Option<SelectedAuth>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.request(Request::RouteFailed { route, selected }).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replies_are_routed_independently_and_eof_fails_pending_requests() {
        let (client, mut server) = crate::worker::testing::pair();
        let host = client.host();
        let one = tokio::spawn({
            let host = host.clone();
            async move { host.select().await }
        });
        let Message::Request { id: first, .. } = server.read().await.unwrap() else {
            panic!()
        };
        let two = tokio::spawn({
            let host = host.clone();
            async move { host.select().await }
        });
        let Message::Request { id: second, .. } = server.read().await.unwrap() else {
            panic!()
        };
        assert_ne!(first, second);
        server
            .write(&Message::Reply {
                id: second,
                body: Reply::Error("second reply".into()),
            })
            .await
            .unwrap();
        assert_eq!(two.await.unwrap().unwrap_err().to_string(), "second reply");
        assert!(!one.is_finished());
        drop(server);
        host.closed().await;
        assert!(
            one.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("connection closed")
        );
        assert!(
            host.select()
                .await
                .unwrap_err()
                .to_string()
                .contains("connection closed")
        );
        assert!(host.waiters.lock().unwrap().calls.is_empty());
    }

    #[tokio::test]
    async fn cancelled_call_releases_its_waiter_and_late_reply_is_ignored() {
        let (client, mut server) = crate::worker::testing::pair();
        let host = client.host();
        let task = tokio::spawn({
            let host = host.clone();
            async move { host.select().await }
        });
        let Message::Request { id, .. } = server.read().await.unwrap() else {
            panic!()
        };
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(host.waiters.lock().unwrap().calls.is_empty());
        server
            .write(&Message::Reply {
                id,
                body: Reply::Error("late".into()),
            })
            .await
            .unwrap();
        let next = tokio::spawn({
            let host = host.clone();
            async move { host.select().await }
        });
        let Message::Request { id, .. } = server.read().await.unwrap() else {
            panic!()
        };
        server
            .write(&Message::Reply {
                id,
                body: Reply::Error("current".into()),
            })
            .await
            .unwrap();
        assert_eq!(next.await.unwrap().unwrap_err().to_string(), "current");
    }

    #[tokio::test]
    async fn large_canonical_append_crosses_frames_without_partial_delivery() {
        let (client, mut server) = crate::worker::testing::pair();
        let count = 64 * 1024 * 1024 + 137;
        let sending = tokio::spawn(async move {
            client
                .write(&Message::Request {
                    id: 19,
                    body: Request::Append(AgentEvent::Native(
                        crate::native::NativeEvent::RequestStarted {
                            input: vec![rho_core::ContextBlock::UserMessage {
                                sender: crate::MessageSender::User,
                                content: vec![rho_core::ContentPart::Text {
                                    text: "a".repeat(count),
                                }],
                            }],
                            context: None,
                            wake: None,
                            at: rho_core::UnixMs(1),
                        },
                    )),
                })
                .await
                .unwrap();
            client
        });
        let Message::Request {
            id: 19,
            body:
                Request::Append(AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                    input,
                    ..
                })),
        } = server.read().await.unwrap()
        else {
            panic!("wrong logical message")
        };
        let rho_core::ContextBlock::UserMessage { content, .. } = &input[0] else {
            panic!()
        };
        let rho_core::ContentPart::Text { text } = &content[0] else {
            panic!()
        };
        assert_eq!(text.len(), count);
        assert!(text.bytes().all(|byte| byte == b'a'));
        sending.await.unwrap();
    }
}
