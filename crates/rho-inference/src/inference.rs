//! The daemon-wide inference runtime and the sessions created from it.

use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use rho_agent_types::UnixMs;
use rho_db::RhoDb;
use tokio::sync::watch;

use crate::InferenceSession;
use crate::accounts::{self, AccountManager, InferenceQuotaSeries, InferenceState, SelectedAuth};
use crate::config::{InferenceModel, InferenceProfile};
use crate::responses::{
    DialRoute, InferenceAuth, PromptCacheKey, QuotaUpdate, RouteSelection, RouteSelector,
};

/// Provider account policy, quota, persistence, and session creation. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct Inference(Arc<Inner>);

/// The session-facing daemon services. A worker receives account decisions and
/// route observations; it never opens the account database or starts pollers.
pub trait InferenceHost: std::fmt::Debug + Send + Sync {
    fn select(&self) -> BoxFuture<'_, anyhow::Result<SelectedAuth>>;
    fn select_resolved(
        &self,
    ) -> BoxFuture<'_, anyhow::Result<(SelectedAuth, crate::ResolvedAuth)>> {
        Box::pin(async {
            let selected = self.select().await?;
            let resolved = self.resolve_auth(selected.auth.clone()).await?;
            Ok((selected, resolved))
        })
    }
    fn resolve_auth(
        &self,
        auth: InferenceAuth,
    ) -> BoxFuture<'_, anyhow::Result<crate::ResolvedAuth>>;
    fn mark_rate_limited(&self, selected: SelectedAuth) -> BoxFuture<'_, bool>;
    fn observe_quota(&self, selected: SelectedAuth, quota: QuotaUpdate) -> BoxFuture<'_, ()>;
    fn route_updates(&self) -> watch::Receiver<RouteSelection>;
    fn report_connect_failure(
        &self,
        route: DialRoute,
        selected: Option<SelectedAuth>,
    ) -> BoxFuture<'_, ()>;
}

#[derive(Debug)]
enum Backend {
    Daemon {
        accounts: Arc<AccountManager>,
        routes: RouteSelector,
    },
    Host(Arc<dyn InferenceHost>),
    #[cfg(test)]
    Fixed {
        auth: InferenceAuth,
        routes: RouteSelector,
    },
}

#[derive(Debug)]
struct Inner {
    backend: Backend,
    responses_base_url: Arc<str>,
    credentials: OnceLock<watch::Receiver<crate::CredentialSnapshot>>,
}

impl Inference {
    /// Applies provider-owned database migrations without starting runtime
    /// work or accessing credentials.
    pub async fn migrate(db: &RhoDb) -> anyhow::Result<()> {
        accounts::init(db).await
    }

    /// Opens the daemon-owned inference runtime and starts its quota and route
    /// pollers.
    pub async fn new(db: RhoDb) -> anyhow::Result<Self> {
        Self::new_with_config(db, InferenceConfig::default()).await
    }

    /// Opens inference with explicit provider transport configuration. This is
    /// used by isolated QA rigs; the production default remains ChatGPT.
    pub async fn new_with_config(db: RhoDb, config: InferenceConfig) -> anyhow::Result<Self> {
        Self::migrate(&db).await?;
        let accounts = Arc::new(AccountManager::open(db.clone()).await);
        // The quota endpoint is a first-party ChatGPT-only API. An explicit
        // Responses endpoint (for example the full-stack QA server) must not
        // leak a side request to the production provider.
        if &*config.responses_base_url == crate::responses::DEFAULT_CHATGPT_BASE_URL {
            accounts.spawn_poller();
        }
        let production_chatgpt =
            &*config.responses_base_url == crate::responses::DEFAULT_CHATGPT_BASE_URL;
        let inference = Self(Arc::new(Inner {
            backend: Backend::Daemon {
                accounts,
                routes: RouteSelector::new(Some(db)),
            },
            responses_base_url: config.responses_base_url,
            credentials: OnceLock::new(),
        }));
        if production_chatgpt {
            inference.spawn_route_prober();
        }
        Ok(inference)
    }

    #[cfg(test)]
    pub(crate) fn for_test(auth: InferenceAuth) -> Self {
        Self(Arc::new(Inner {
            backend: Backend::Fixed {
                auth,
                routes: RouteSelector::new(None),
            },
            responses_base_url: crate::responses::DEFAULT_CHATGPT_BASE_URL.into(),
            credentials: OnceLock::new(),
        }))
    }

    pub fn responses_base_url(&self) -> &str {
        &self.0.responses_base_url
    }

    /// Session transport without daemon-owned account state or background work.
    pub fn from_host(host: Arc<dyn InferenceHost>, config: InferenceConfig) -> Self {
        Self(Arc::new(Inner {
            backend: Backend::Host(host),
            responses_base_url: config.responses_base_url,
            credentials: OnceLock::new(),
        }))
    }

    pub fn route_updates(&self) -> watch::Receiver<RouteSelection> {
        match &self.0.backend {
            Backend::Daemon { routes, .. } => routes.subscribe(),
            Backend::Host(host) => host.route_updates(),
            #[cfg(test)]
            Backend::Fixed { routes, .. } => routes.subscribe(),
        }
    }

    pub(crate) fn route_for_session(
        &self,
        config: &crate::responses::session::ResponsesConfig,
        selected: Option<&SelectedAuth>,
    ) -> DialRoute {
        self.route_updates().borrow().for_session(config, selected)
    }

    pub async fn report_connect_failure(&self, route: DialRoute, selected: Option<&SelectedAuth>) {
        match &self.0.backend {
            Backend::Daemon { routes, .. } => routes.report_connect_failure(route, selected),
            Backend::Host(host) => host.report_connect_failure(route, selected.cloned()).await,
            #[cfg(test)]
            Backend::Fixed { routes, .. } => routes.report_connect_failure(route, selected),
        }
    }

    fn spawn_route_prober(&self) {
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let inference = Inference(inner);
                if let Backend::Daemon { routes, .. } = &inference.0.backend {
                    routes.probe_once(&inference).await;
                }
                drop(inference);
                tokio::time::sleep(RouteSelector::probe_interval()).await;
            }
        });
    }

    pub fn deep_session(
        &self,
        profile: InferenceProfile,
        model: InferenceModel,
        prompt_cache_key: PromptCacheKey,
    ) -> InferenceSession {
        InferenceSession::new_deep(self.clone(), profile, model, prompt_cache_key)
    }

    /// A single text-only exchange. The caller owns its deadline and any retry.
    /// Dropping this future drops the session and cancels its socket task.
    pub async fn text(&self, instructions: Arc<str>, input: String) -> anyhow::Result<String> {
        use rho_agent_types::ContentPart;

        use crate::types::{
            ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem, MessageSender,
            PendingInferenceResponse,
        };
        let mut session = InferenceSession::new_title(self.clone(), PromptCacheKey::generate());
        session.request(InferenceRequest {
            instructions,
            input: vec![Arc::new(ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text { text: input }],
            })],
            agent_id_labels: Default::default(),
        });
        let mut pending = PendingInferenceResponse::default();
        loop {
            match session.run().await {
                InferenceEvent::ContextItem { index, event } => pending.apply(index, event),
                InferenceEvent::Finished { .. } => {
                    let mut text = String::new();
                    for item in pending.finish()? {
                        match item {
                            InferenceResponseItem::AssistantMessage { content, .. } => {
                                text.push_str(&crate::types::text_content(&content));
                            }
                            InferenceResponseItem::ToolCall { .. } => {
                                anyhow::bail!("text completion returned a tool call")
                            }
                            _ => {}
                        }
                    }
                    return Ok(text);
                }
                InferenceEvent::Failed { error }
                | InferenceEvent::TemporaryFailure { error, .. } => anyhow::bail!("{error:#}"),
                _ => {}
            }
        }
    }

    /// Returns the account decision already made by the account manager.
    pub async fn auth(&self) -> anyhow::Result<InferenceAuth> {
        Ok(self.select().await?.auth)
    }

    /// Resolve/refresh credentials in the daemon's filesystem context, even
    /// when the requesting provider transport lives in a pivoted workset.
    pub async fn resolve_auth(&self, auth: InferenceAuth) -> anyhow::Result<crate::ResolvedAuth> {
        if let Backend::Host(host) = &self.0.backend {
            return host.resolve_auth(auth).await;
        }
        Ok(tokio::task::spawn_blocking(move || auth.resolve()).await??)
    }

    /// Private daemon-to-worker credential stream; never a UI/public-state DTO.
    pub fn credential_updates(&self) -> watch::Receiver<crate::CredentialSnapshot> {
        self.0
            .credentials
            .get_or_init(|| crate::credentials::subscribe(self.accounts().selection_updates()))
            .clone()
    }

    /// Fence a policy mutation before acknowledging it to a worker. Selection
    /// changes during resolution are followed rather than publishing stale
    /// auth.
    pub async fn credential_snapshot(&self) -> anyhow::Result<crate::CredentialSnapshot> {
        let mut updates = self.credential_updates();
        loop {
            let wanted = self.accounts().selection_updates().borrow().clone();
            let snapshot = updates.borrow_and_update().clone();
            if snapshot.matches_selection(&wanted) && snapshot.current().is_some() {
                return Ok(snapshot);
            }
            updates.changed().await?;
        }
    }

    pub async fn select_resolved(&self) -> anyhow::Result<(SelectedAuth, crate::ResolvedAuth)> {
        if let Backend::Host(host) = &self.0.backend {
            return host.select_resolved().await;
        }
        let selected = self.select().await?;
        let resolved = self.resolve_auth(selected.auth.clone()).await?;
        Ok((selected, resolved))
    }

    pub async fn select(&self) -> anyhow::Result<SelectedAuth> {
        match &self.0.backend {
            Backend::Daemon { accounts, .. } => accounts.select().await,
            Backend::Host(host) => host.select().await,
            #[cfg(test)]
            Backend::Fixed { auth, .. } => Ok(SelectedAuth {
                auth: auth.clone(),
                namespace: None,
                account_id: None,
            }),
        }
    }

    pub async fn mark_rate_limited(&self, selected: &SelectedAuth) -> bool {
        match &self.0.backend {
            Backend::Daemon { accounts, .. } => accounts.mark_rate_limited(selected).await,
            Backend::Host(host) => host.mark_rate_limited(selected.clone()).await,
            #[cfg(test)]
            Backend::Fixed { .. } => false,
        }
    }

    pub async fn observe_quota(&self, selected: &SelectedAuth, quota: QuotaUpdate) {
        match &self.0.backend {
            Backend::Daemon { accounts, .. } => accounts.observe_quota(selected, quota).await,
            Backend::Host(host) => host.observe_quota(selected.clone(), quota).await,
            #[cfg(test)]
            Backend::Fixed { .. } => {}
        }
    }

    pub async fn set_account_enabled(&self, namespace: &str, enabled: bool) {
        self.accounts().set_enabled(namespace, enabled).await;
    }

    pub fn state(&self) -> InferenceState {
        self.accounts().state()
    }

    pub fn subscribe(&self) -> watch::Receiver<InferenceState> {
        self.accounts().subscribe()
    }

    pub fn quota_history(&self, since: UnixMs) -> Vec<InferenceQuotaSeries> {
        self.accounts().history(since)
    }

    pub fn route_probe_history(&self, since: UnixMs) -> Vec<crate::responses::InferenceRouteProbe> {
        match &self.0.backend {
            Backend::Daemon { routes, .. } => routes.history(since),
            _ => Vec::new(),
        }
    }

    fn accounts(&self) -> &AccountManager {
        let Backend::Daemon { accounts, .. } = &self.0.backend else {
            panic!("account administration belongs to the daemon")
        };
        accounts
    }
}

#[derive(Clone, Debug)]
pub struct InferenceConfig {
    responses_base_url: Arc<str>,
}

impl InferenceConfig {
    pub fn with_responses_base_url(base_url: impl Into<Arc<str>>) -> anyhow::Result<Self> {
        let base_url = base_url.into();
        let parsed = url::Url::parse(&base_url)?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "inference base URL must use http or https"
        );
        anyhow::ensure!(
            parsed.host().is_some(),
            "inference base URL must have a host"
        );
        Ok(Self {
            responses_base_url: base_url,
        })
    }
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            responses_base_url: crate::responses::DEFAULT_CHATGPT_BASE_URL.into(),
        }
    }
}

#[cfg(test)]
mod host_tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct Host {
        selected: SelectedAuth,
        routes: RouteSelector,
        calls: Mutex<Vec<&'static str>>,
    }

    impl InferenceHost for Host {
        fn select(&self) -> BoxFuture<'_, anyhow::Result<SelectedAuth>> {
            Box::pin(async {
                self.calls.lock().unwrap().push("select");
                Ok(self.selected.clone())
            })
        }
        fn resolve_auth(
            &self,
            _auth: InferenceAuth,
        ) -> BoxFuture<'_, anyhow::Result<crate::ResolvedAuth>> {
            Box::pin(async {
                self.calls.lock().unwrap().push("resolve");
                Ok(crate::ResolvedAuth {
                    bearer_token: "test".into(),
                    account_id: None,
                    client_secret: [0; 32],
                })
            })
        }
        fn mark_rate_limited(&self, selected: SelectedAuth) -> BoxFuture<'_, bool> {
            Box::pin(async move {
                assert_eq!(selected, self.selected);
                self.calls.lock().unwrap().push("rate-limit");
                true
            })
        }
        fn observe_quota(&self, selected: SelectedAuth, quota: QuotaUpdate) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                assert_eq!(selected, self.selected);
                assert_eq!(quota.weekly_used_percent, 17);
                self.calls.lock().unwrap().push("quota");
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
                assert_eq!(selected.as_ref(), Some(&self.selected));
                self.calls.lock().unwrap().push("route-failure");
                self.routes.report_connect_failure(route, selected.as_ref());
            })
        }
    }

    #[tokio::test]
    async fn worker_sessions_delegate_policy_without_opening_a_store_or_selecting_early() {
        let host = Arc::new(Host {
            selected: SelectedAuth {
                auth: InferenceAuth::oauth_file("/unused/credentials.json"),
                namespace: Some("account".into()),
                account_id: Some("identity".into()),
            },
            routes: RouteSelector::new(None),
            calls: Mutex::new(Vec::new()),
        });
        let inference = Inference::from_host(
            host.clone(),
            InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        );
        assert!(host.calls.lock().unwrap().is_empty());
        assert_eq!(inference.responses_base_url(), "http://127.0.0.1:1");
        let selected = inference.select().await.unwrap();
        let resolved = inference.resolve_auth(selected.auth.clone()).await.unwrap();
        assert_eq!(
            resolved.bearer_token, "test",
            "worker tried to read the nonexistent OAuth file"
        );
        let quota = QuotaUpdate {
            weekly_used_percent: 17,
            weekly_reset_at_unix: Some(123),
            routing_used_percent: 23,
            routing_reset_at_unix: None,
        };
        let bytes = senax_encoder::encode(&(selected.clone(), quota)).unwrap();
        let decoded: (SelectedAuth, QuotaUpdate) =
            senax_encoder::decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded, (selected.clone(), quota));
        inference.observe_quota(&selected, quota).await;
        assert!(inference.mark_rate_limited(&selected).await);
        inference
            .report_connect_failure(DialRoute::Dns, Some(&selected))
            .await;
        assert_eq!(
            *inference.route_updates().borrow(),
            *host.routes.subscribe().borrow()
        );
        assert_eq!(
            *host.calls.lock().unwrap(),
            vec!["select", "resolve", "quota", "rate-limit", "route-failure"]
        );
    }
}
