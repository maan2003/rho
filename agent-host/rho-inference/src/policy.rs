//! One inference-policy client per workset. Policy pushes and RPC replies use
//! the same workset FIFO, so failover acknowledgments fence replacement.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rho_agent::inference::{PolicyClient, PolicySender};
use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    Accounts, Inference, PolicyCall, PolicyReply as Reply, PolicyRequest as Request, RouteSelection,
};

pub(crate) const MAX_REQUESTS: usize = 32;

#[derive(Encode, Decode)]
pub(crate) enum Message {
    Request { id: u64, body: Request },
    Reply { id: u64, body: Reply },
    Route(RouteSelection),
    Credentials(crate::CredentialSnapshot),
}

async fn send(sender: &PolicySender, message: Message) -> anyhow::Result<()> {
    sender(senax_encoder::encode(&message)?.to_vec()).await
}

#[derive(Default)]
struct Replies {
    closed: bool,
    calls: HashMap<u64, (oneshot::Sender<Reply>, tokio::sync::OwnedSemaphorePermit)>,
}

struct Pending {
    id: u64,
    published: bool,
    replies: Arc<Mutex<Replies>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        // Once published, even an abandoned request owns its credit until the
        // agent host replies. Caller cancellation must not let bursts bypass admission.
        if !self.published {
            self.replies.lock().expect("poison").calls.remove(&self.id);
        }
    }
}

pub(crate) struct Host {
    sender: PolicySender,
    next: AtomicU64,
    admission: Arc<tokio::sync::Semaphore>,
    replies: Arc<Mutex<Replies>>,
    routes: watch::Sender<RouteSelection>,
    credentials: watch::Sender<crate::CredentialSnapshot>,
    closed: watch::Sender<bool>,
}
impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorksetInferencePolicy")
            .finish_non_exhaustive()
    }
}
impl Host {
    pub(crate) fn new(sender: PolicySender) -> Arc<Self> {
        Arc::new(Self {
            sender,
            next: AtomicU64::new(1),
            admission: Arc::new(tokio::sync::Semaphore::new(MAX_REQUESTS)),
            replies: Arc::default(),
            routes: watch::Sender::new(RouteSelection::default()),
            credentials: watch::Sender::new(crate::CredentialSnapshot {
                revision: 0,
                state: crate::CredentialState::Pending,
            }),
            closed: watch::Sender::new(false),
        })
    }

    pub(crate) fn inference(
        &self,
        config: crate::InferenceConfig,
    ) -> (Inference, mpsc::Receiver<PolicyCall>) {
        let (calls, receiver) = mpsc::channel(MAX_REQUESTS);
        (
            Inference::from_worker(
                calls,
                self.credentials.subscribe(),
                self.routes.subscribe(),
                self.closed.subscribe(),
                config,
            ),
            receiver,
        )
    }

    // Called by the workset reader: install state/replies synchronously, never
    // await a service or an agent. No second inbox or reader task is needed.
    fn receive_message(&self, message: Message) -> anyhow::Result<()> {
        match message {
            Message::Reply { id, body } => {
                if let Some((reply, _credit)) =
                    self.replies.lock().expect("poison").calls.remove(&id)
                {
                    let _ = reply.send(body);
                }
            }
            Message::Credentials(snapshot) => {
                self.credentials.send_if_modified(|current| {
                    if snapshot.revision <= current.revision {
                        return false;
                    }
                    *current = snapshot;
                    true
                });
            }
            Message::Route(route) => {
                self.routes.send_if_modified(|current| {
                    if route.revision() <= current.revision() {
                        return false;
                    }
                    *current = route;
                    true
                });
            }
            Message::Request { .. } => anyhow::bail!("unexpected inference policy request"),
        }
        Ok(())
    }

    fn disconnected(&self) {
        let mut pending = self.replies.lock().expect("poison");
        pending.closed = true;
        self.admission.close();
        pending.calls.clear();
        self.closed.send_replace(true);
    }

    pub(crate) async fn request(&self, body: Request) -> anyhow::Result<Reply> {
        let credit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("agent connection closed"))?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        {
            let mut pending = self.replies.lock().expect("poison");
            anyhow::ensure!(!pending.closed, "agent connection closed");
            pending.calls.insert(id, (reply, credit));
        }
        let mut pending = Pending {
            id,
            published: false,
            replies: self.replies.clone(),
        };
        send(&self.sender, Message::Request { id, body }).await?;
        // Transport send has no suspension point after enqueueing, so a
        // cancelled send cannot publish a request while releasing its credit.
        pending.published = true;
        match response
            .await
            .map_err(|_| anyhow::anyhow!("agent connection closed"))?
        {
            Reply::Error(error) => Err(anyhow::anyhow!("{error}")),
            reply => Ok(reply),
        }
    }

    pub(crate) async fn closed(&self) {
        let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
    }
}

impl PolicyClient for Host {
    fn receive(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut remaining = bytes;
        let message = senax_encoder::decode(&mut remaining)
            .map_err(|_| anyhow::anyhow!("invalid inference policy message"))?;
        anyhow::ensure!(
            remaining.is_empty(),
            "trailing inference policy message data"
        );
        self.receive_message(message)
    }

    fn disconnect(&self) {
        self.disconnected();
    }
}

async fn call(inference: &Accounts, body: Request, sender: &PolicySender) -> anyhow::Result<Reply> {
    Ok(match body {
        Request::ResolveAuth(auth) => Reply::Auth(inference.resolve_auth(auth).await?),
        Request::SelectAccount => Reply::Account(inference.select().await?),
        Request::RateLimited(selected) => {
            let changed = inference.mark_rate_limited(&selected).await;
            // Publish the replacement before acknowledging the retry.
            let snapshot = inference.credential_snapshot().await?;
            send(sender, Message::Credentials(snapshot)).await?;
            Reply::RateLimited(changed)
        }
        Request::Quota { selected, quota } => {
            inference.observe_quota(&selected, quota).await;
            Reply::Done
        }
        Request::RouteFailed { route, selected } => {
            inference
                .report_connect_failure(route, selected.as_ref())
                .await;
            let route = inference.route_updates().borrow().clone();
            send(sender, Message::Route(route)).await?;
            Reply::Done
        }
    })
}

pub(crate) async fn serve(
    inference: Accounts,
    sender: PolicySender,
    mut incoming: mpsc::Receiver<(Vec<u8>, Arc<()>)>,
) -> anyhow::Result<()> {
    let mut routes = inference.route_updates();
    let mut credentials = inference.credential_updates();
    let mut calls = tokio::task::JoinSet::new();
    tokio::select! {
        result = async {
            loop {
                tokio::select! {
                    biased;
                    Some(result) = calls.join_next(), if !calls.is_empty() => { result??; }
                    message = incoming.recv() => {
                        let (bytes, inflight) = message.ok_or_else(|| anyhow::anyhow!("workset policy connection closed"))?;
                        let mut remaining = bytes.as_slice();
                        let message = senax_encoder::decode(&mut remaining)
                            .map_err(|_| anyhow::anyhow!("invalid inference policy request"))?;
                        anyhow::ensure!(remaining.is_empty(), "trailing inference policy request data");
                        let Message::Request { id, body } = message else {
                            anyhow::bail!("unexpected workset policy message");
                        };
                        if calls.len() >= MAX_REQUESTS {
                            send(&sender, Message::Reply { id, body: Reply::Error("too many pending policy requests".into()) }).await?;
                            continue;
                        }
                        let inference = inference.clone();
                        let sender = sender.clone();
                        calls.spawn(async move {
                            let result = call(&inference, body, &sender).await;
                            let body = result.unwrap_or_else(|error| Reply::Error(error.to_string()));
                            send(&sender, Message::Reply { id, body }).await?;
                            drop(inflight);
                            Ok::<(), anyhow::Error>(())
                        });
                    }
                }
            }
            #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
        } => result,
        result = async {
            loop {
                let route = routes.borrow_and_update().clone();
                send(&sender, Message::Route(route)).await?;
                routes.changed().await?;
            }
            #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
        } => result,
        result = async {
            loop {
                let snapshot = credentials.borrow_and_update().clone();
                send(&sender, Message::Credentials(snapshot)).await?;
                credentials.changed().await?;
            }
            #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
        } => result,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::SelectedAuth;

    struct Server {
        policy: Arc<Host>,
        incoming: mpsc::Receiver<Vec<u8>>,
    }
    impl Server {
        async fn read_policy(&mut self) -> anyhow::Result<Message> {
            let bytes = self
                .incoming
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("policy disconnected"))?;
            let mut remaining = bytes.as_slice();
            Ok(senax_encoder::decode(&mut remaining)?)
        }
        async fn write_policy(&self, message: &Message) -> anyhow::Result<()> {
            self.policy.receive(&senax_encoder::encode(message)?)
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.policy.disconnect();
        }
    }
    fn pair() -> (Arc<Host>, Server) {
        let (sender, incoming) = mpsc::channel(MAX_REQUESTS * 2);
        let sender: PolicySender = Arc::new(move |bytes| {
            let sender = sender.clone();
            Box::pin(async move {
                sender
                    .send(bytes)
                    .await
                    .map_err(|_| anyhow::anyhow!("policy disconnected"))
            })
        });
        let policy = Host::new(sender);
        (policy.clone(), Server { policy, incoming })
    }

    fn worker_inference(host: &Arc<Host>) -> Inference {
        let (inference, mut calls) = host.inference(
            crate::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        );
        let host = host.clone();
        tokio::spawn(async move {
            while let Some(call) = calls.recv().await {
                let host = host.clone();
                tokio::spawn(async move {
                    let _ = call.reply.send(host.request(call.body).await);
                });
            }
        });
        inference
    }

    #[tokio::test]
    async fn cancelled_published_requests_keep_credit_until_reply_and_disconnect_wakes_waiters() {
        let (client, mut server) = pair();
        let host = client;
        let mut first = tokio::task::JoinSet::new();
        for _ in 0..MAX_REQUESTS {
            let host = host.clone();
            first.spawn(async move { host.request(Request::SelectAccount).await });
        }
        let mut ids = Vec::new();
        for _ in 0..MAX_REQUESTS {
            let Message::Request { id, .. } = server.read_policy().await.unwrap() else {
                panic!("request");
            };
            ids.push(id);
        }
        let mut waiting = tokio::task::JoinSet::new();
        for _ in 0..MAX_REQUESTS {
            let host = host.clone();
            waiting.spawn(async move { host.request(Request::SelectAccount).await });
        }
        first.abort_all();
        while first.join_next().await.is_some() {}
        assert_eq!(host.replies.lock().unwrap().calls.len(), MAX_REQUESTS);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), server.read_policy())
                .await
                .is_err()
        );

        // A late reply, not caller cancellation, allows exactly one new request.
        server
            .write_policy(&Message::Reply {
                id: ids[0],
                body: Reply::Done,
            })
            .await
            .unwrap();
        assert!(matches!(
            server.read_policy().await.unwrap(),
            Message::Request { .. }
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), server.read_policy())
                .await
                .is_err()
        );
        assert_eq!(host.replies.lock().unwrap().calls.len(), MAX_REQUESTS);

        drop(server);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(result) = waiting.join_next().await {
                assert!(result.unwrap().is_err());
            }
        })
        .await
        .unwrap();
        assert!(host.replies.lock().unwrap().calls.is_empty());
    }

    #[tokio::test]
    async fn account_selection_waits_for_push_without_an_rpc() {
        let (client, mut server) = pair();
        let host = client;
        let inference = worker_inference(&host);
        let request = tokio::spawn(async move { inference.select_resolved().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), server.read_policy())
                .await
                .is_err()
        );
        server
            .write_policy(&Message::Credentials(crate::CredentialSnapshot {
                revision: 1,
                state: crate::CredentialState::Unavailable {
                    selected: None,
                    error: "no credentials".into(),
                },
            }))
            .await
            .unwrap();
        assert_eq!(
            request.await.unwrap().unwrap_err().to_string(),
            "no credentials"
        );
    }

    #[tokio::test]
    async fn rate_limit_ack_installs_replacement_and_reconnect_resolves_pinned_auth() {
        let (client, mut server) = pair();
        let host = client;
        let auth_a = crate::InferenceAuth::named("fixture-a").unwrap();
        let auth_b = crate::InferenceAuth::named("fixture-b").unwrap();
        // SelectedAuth is opaque outside inference; construct a wire fixture.
        #[derive(Encode)]
        struct SelectedWire {
            auth: crate::InferenceAuth,
            namespace: Option<String>,
            account_id: Option<String>,
        }
        let selected = |auth: &crate::InferenceAuth| {
            let bytes = senax_encoder::encode(&SelectedWire {
                auth: auth.clone(),
                namespace: None,
                account_id: None,
            })
            .unwrap();
            senax_encoder::decode::<SelectedAuth>(&mut bytes.as_ref()).unwrap()
        };
        let a = selected(&auth_a);
        let b = selected(&auth_b);
        let resolved = |token: &str| crate::ResolvedAuth {
            bearer_token: token.into(),
            account_id: None,
            client_secret: [0; 32],
        };
        let snapshot = |revision, selected, token: &str| {
            Message::Credentials(crate::CredentialSnapshot {
                revision,
                state: crate::CredentialState::Ready {
                    selected,
                    auth: resolved(token),
                    refresh_at: u64::MAX,
                },
            })
        };
        server
            .write_policy(&snapshot(1, a.clone(), "a"))
            .await
            .unwrap();
        let inference = worker_inference(&host);
        assert_eq!(
            inference.select_resolved().await.unwrap().1.bearer_token,
            "a"
        );
        let first_agent = worker_inference(&host);
        let second_agent = worker_inference(&host);
        assert_eq!(
            first_agent.select_resolved().await.unwrap().1.bearer_token,
            "a"
        );
        drop(first_agent);
        let limit = tokio::spawn({
            let host = host.clone();
            let a = a.clone();
            async move { worker_inference(&host).mark_rate_limited(&a).await }
        });
        let Message::Request {
            id,
            body: Request::RateLimited(_),
        } = server.read_policy().await.unwrap()
        else {
            panic!()
        };
        server
            .write_policy(&snapshot(3, b.clone(), "b"))
            .await
            .unwrap();
        server
            .write_policy(&snapshot(2, a, "stale-a"))
            .await
            .unwrap();
        server
            .write_policy(&Message::Reply {
                id,
                body: Reply::RateLimited(true),
            })
            .await
            .unwrap();
        assert!(limit.await.unwrap());
        assert_eq!(
            inference.select_resolved().await.unwrap().1.bearer_token,
            "b"
        );
        assert_eq!(
            second_agent.select_resolved().await.unwrap().1.bearer_token,
            "b"
        );

        let reconnect = tokio::spawn({
            let host = host.clone();
            let auth_a = auth_a.clone();
            async move { worker_inference(&host).resolve_auth(auth_a).await }
        });
        let Message::Request {
            id,
            body: Request::ResolveAuth(auth),
        } = server.read_policy().await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            auth, auth_a,
            "reconnect switched to the newly selected account"
        );
        server
            .write_policy(&Message::Reply {
                id,
                body: Reply::Auth(resolved("refreshed-a")),
            })
            .await
            .unwrap();
        assert_eq!(
            reconnect.await.unwrap().unwrap().bearer_token,
            "refreshed-a"
        );
        assert_eq!(
            inference.select_resolved().await.unwrap().1.bearer_token,
            "b"
        );
    }

    #[tokio::test]
    async fn credential_push_revision_and_reply_fence_prevent_stale_replacement() {
        let (client, mut server) = pair();
        let host = client;
        for (revision, error) in [(8, "replacement"), (3, "obsolete")] {
            server
                .write_policy(&Message::Credentials(crate::CredentialSnapshot {
                    revision,
                    state: crate::CredentialState::Unavailable {
                        selected: None,
                        error: error.into(),
                    },
                }))
                .await
                .unwrap();
        }
        // Use a normal correlated reply to fence receipt of both pushes.
        let call = tokio::spawn({
            let host = host.clone();
            async move { worker_inference(&host).select().await }
        });
        let Message::Request {
            id,
            body: Request::SelectAccount,
        } = server.read_policy().await.unwrap()
        else {
            panic!()
        };
        server
            .write_policy(&Message::Reply {
                id,
                body: Reply::Error("fenced".into()),
            })
            .await
            .unwrap();
        assert_eq!(call.await.unwrap().unwrap_err().to_string(), "fenced");
        let inference = worker_inference(&host);
        for _ in 0..3 {
            assert_eq!(
                inference.select_resolved().await.unwrap_err().to_string(),
                "replacement"
            );
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), server.read_policy())
                .await
                .is_err()
        );
        drop(server);
        host.closed().await;
        assert_eq!(
            inference.select_resolved().await.unwrap_err().to_string(),
            "agent connection closed"
        );
    }
}
