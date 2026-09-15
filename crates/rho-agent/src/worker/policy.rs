//! One inference-policy client per workset. Policy pushes and RPC replies use
//! the same workset FIFO, so failover acknowledgments fence replacement.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rho_inference::{
    DialRoute, Inference, InferenceHost, QuotaUpdate, RouteSelection, SelectedAuth,
};
use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot, watch};

use super::{transport, workset};

#[derive(Encode, Decode)]
pub(super) enum Request {
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
    Account(SelectedAuth),
    Auth(rho_inference::ResolvedAuth),
    RateLimited(bool),
    Done,
    Error(String),
}

#[derive(Encode, Decode)]
pub(super) enum Message {
    Request { id: u64, body: Request },
    Reply { id: u64, body: Reply },
    Route(RouteSelection),
    Credentials(rho_inference::CredentialSnapshot),
}

async fn send(sender: &transport::Sender, message: Message) -> anyhow::Result<()> {
    sender
        .send(
            transport::Port::Workset,
            workset::encode(&workset::Message::Policy(message))?,
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

pub(super) struct Host {
    sender: transport::Sender,
    next: Arc<AtomicU64>,
    replies: Arc<Mutex<Replies>>,
    routes: watch::Sender<RouteSelection>,
    credentials: watch::Sender<Option<rho_inference::CredentialSnapshot>>,
    closed: watch::Sender<bool>,
}
impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorksetInferenceHost")
            .finish_non_exhaustive()
    }
}
impl Host {
    pub(super) fn new(sender: transport::Sender, next: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            sender,
            next,
            replies: Arc::default(),
            routes: watch::Sender::new(RouteSelection::default()),
            credentials: watch::Sender::new(None),
            closed: watch::Sender::new(false),
        })
    }

    // Called by the workset reader: install state/replies synchronously, never
    // await a service or an agent. No second inbox or reader task is needed.
    pub(super) fn receive(&self, message: Message) -> anyhow::Result<()> {
        match message {
            Message::Reply { id, body } => {
                if let Some(reply) = self.replies.lock().expect("poison").calls.remove(&id) {
                    let _ = reply.send(body);
                }
            }
            Message::Credentials(snapshot) => {
                self.credentials.send_if_modified(|current| {
                    if current
                        .as_ref()
                        .is_some_and(|old| snapshot.revision <= old.revision)
                    {
                        return false;
                    }
                    *current = Some(snapshot);
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

    pub(super) fn disconnect(&self) {
        let mut pending = self.replies.lock().expect("poison");
        pending.closed = true;
        pending.calls.clear();
        self.closed.send_replace(true);
    }

    async fn request(&self, body: Request) -> anyhow::Result<Reply> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        {
            let mut pending = self.replies.lock().expect("poison");
            anyhow::ensure!(!pending.closed, "agent connection closed");
            pending.calls.insert(id, reply);
        }
        let _pending = Pending {
            id,
            replies: self.replies.clone(),
        };
        send(&self.sender, Message::Request { id, body }).await?;
        match response
            .await
            .map_err(|_| anyhow::anyhow!("agent connection closed"))?
        {
            Reply::Error(error) => Err(anyhow::anyhow!("{error}")),
            reply => Ok(reply),
        }
    }

    #[cfg(test)]
    pub(super) async fn closed(&self) {
        let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
    }
}

impl InferenceHost for Host {
    fn select_resolved(
        &self,
    ) -> BoxFuture<'_, anyhow::Result<(SelectedAuth, rho_inference::ResolvedAuth)>> {
        Box::pin(async {
            let mut credentials = self.credentials.subscribe();
            let mut closed = self.closed.subscribe();
            loop {
                anyhow::ensure!(!*closed.borrow_and_update(), "agent connection closed");
                if let Some(result) = credentials
                    .borrow_and_update()
                    .as_ref()
                    .and_then(|snapshot| snapshot.current())
                {
                    return result;
                }
                tokio::select! {
                    biased;
                    _ = closed.changed() => anyhow::bail!("agent connection closed"),
                    change = credentials.changed() => change?,
                }
            }
        })
    }

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
        self.routes.subscribe()
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

async fn call(
    inference: &Inference,
    body: Request,
    sender: &transport::Sender,
) -> anyhow::Result<Reply> {
    Ok(match body {
        Request::ResolveAuth(auth) => Reply::Auth(inference.resolve_auth(auth).await?),
        Request::SelectAccount => Reply::Account(inference.select().await?),
        Request::RateLimited(selected) => {
            let changed = inference.mark_rate_limited(&selected).await;
            // Install the replacement before the retry can select again.
            // Older background pushes are rejected by snapshot revision.
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
            // The caller observes the demotion before its acknowledgement.
            // Revision checking also rejects older watch notifications.
            send(sender, Message::Route(route)).await?;
            Reply::Done
        }
    })
}

pub(super) async fn serve(
    inference: Inference,
    sender: transport::Sender,
    mut incoming: mpsc::Receiver<Message>,
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
                        let message = message.ok_or_else(|| anyhow::anyhow!("workset policy connection closed"))?;
                        let Message::Request { id, body } = message else {
                            anyhow::bail!("unexpected workset policy message");
                        };
                        if calls.len() >= 32 {
                            send(&sender, Message::Reply { id, body: Reply::Error("too many pending policy requests".into()) }).await?;
                            continue;
                        }
                        let inference = inference.clone();
                        let sender = sender.clone();
                        calls.spawn(async move {
                            let result = async {
                                call(&inference, body, &sender).await
                            }.await;
                            let body = result.unwrap_or_else(|error| Reply::Error(error.to_string()));
                            send(&sender, Message::Reply { id, body }).await
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
    #[tokio::test]
    async fn account_selection_waits_for_push_without_an_rpc() {
        let (client, mut server) = crate::worker::testing::pair();
        let host = client.policy();
        let request = tokio::spawn(async move { host.select_resolved().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), server.read_policy())
                .await
                .is_err()
        );
        server
            .write_policy(&Message::Credentials(rho_inference::CredentialSnapshot {
                revision: 1,
                state: rho_inference::CredentialState::Unavailable {
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
        let (client, mut server) = crate::worker::testing::pair();
        let host = client.policy();
        let auth_a = rho_inference::InferenceAuth::named("fixture-a").unwrap();
        let auth_b = rho_inference::InferenceAuth::named("fixture-b").unwrap();
        // SelectedAuth is opaque outside inference; construct a wire fixture.
        #[derive(Encode)]
        struct SelectedWire {
            auth: rho_inference::InferenceAuth,
            namespace: Option<String>,
            account_id: Option<String>,
        }
        let selected = |auth: &rho_inference::InferenceAuth| {
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
        let resolved = |token: &str| rho_inference::ResolvedAuth {
            bearer_token: token.into(),
            account_id: None,
            client_secret: [0; 32],
        };
        let snapshot = |revision, selected, token: &str| {
            Message::Credentials(rho_inference::CredentialSnapshot {
                revision,
                state: rho_inference::CredentialState::Ready {
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
        assert_eq!(host.select_resolved().await.unwrap().1.bearer_token, "a");
        let config =
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap();
        let first_agent = Inference::from_host(host.clone(), config.clone());
        let second_agent = Inference::from_host(host.clone(), config);
        assert_eq!(
            first_agent.select_resolved().await.unwrap().1.bearer_token,
            "a"
        );
        drop(first_agent);
        let limit = tokio::spawn({
            let host = host.clone();
            let a = a.clone();
            async move { host.mark_rate_limited(a).await }
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
        assert_eq!(host.select_resolved().await.unwrap().1.bearer_token, "b");
        assert_eq!(
            second_agent.select_resolved().await.unwrap().1.bearer_token,
            "b"
        );

        let reconnect = tokio::spawn({
            let host = host.clone();
            let auth_a = auth_a.clone();
            async move { host.resolve_auth(auth_a).await }
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
        assert_eq!(host.select_resolved().await.unwrap().1.bearer_token, "b");
    }

    #[tokio::test]
    async fn credential_push_revision_and_reply_fence_prevent_stale_replacement() {
        let (client, mut server) = crate::worker::testing::pair();
        let host = client.policy();
        for (revision, error) in [(8, "replacement"), (3, "obsolete")] {
            server
                .write_policy(&Message::Credentials(rho_inference::CredentialSnapshot {
                    revision,
                    state: rho_inference::CredentialState::Unavailable {
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
            async move { host.select().await }
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
        for _ in 0..3 {
            assert_eq!(
                host.select_resolved().await.unwrap_err().to_string(),
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
            host.select_resolved().await.unwrap_err().to_string(),
            "agent connection closed"
        );
    }
}
