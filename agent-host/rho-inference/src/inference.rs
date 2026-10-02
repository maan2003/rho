//! Host-wide account policy and transport construction.

use std::sync::{Arc, OnceLock};

use rho_agent_types::UnixMs;
use rho_db::RhoDb;
use senax_encoder::{Decode, Encode};
use tokio::sync::{mpsc, oneshot, watch};

use crate::accounts::{self, AccountManager, InferenceQuotaSeries, InferenceState, SelectedAuth};
use crate::config::{InferenceModel, InferenceProfile};
use crate::responses::{DialRoute, InferenceAuth, QuotaUpdate, RouteSelection, RouteSelector};

/// Host-owned account administration, persistence, refresh and route probing.
#[derive(Clone, Debug)]
pub struct Accounts(Arc<HostInner>);

#[derive(Debug)]
struct HostInner {
    accounts: Arc<AccountManager>,
    routes: RouteSelector,
    responses_base_url: Arc<str>,
    credentials: OnceLock<watch::Receiver<crate::CredentialSnapshot>>,
    client: OnceLock<Inference>,
    closed: watch::Sender<bool>,
}

impl Drop for HostInner {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}

/// Client of provider policy, usable in the host or a workset. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Inference {
    responses_base_url: Arc<str>,
    calls: mpsc::Sender<PolicyCall>,
    credentials: watch::Receiver<crate::CredentialSnapshot>,
    routes: watch::Receiver<RouteSelection>,
    closed: watch::Receiver<bool>,
}

/// Private workset policy operations. These variants are also the existing IPC
/// request vocabulary; changing them changes the workset wire encoding.
#[derive(Encode, Decode)]
pub(crate) enum PolicyRequest {
    SelectAccount,
    ResolveAuth(InferenceAuth),
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
pub(crate) enum PolicyReply {
    Account(SelectedAuth),
    Auth(crate::ResolvedAuth),
    RateLimited(bool),
    Done,
    Error(String),
}

/// A local request to the workset IPC client, not another transport reader.
pub(crate) struct PolicyCall {
    pub body: PolicyRequest,
    pub reply: oneshot::Sender<anyhow::Result<PolicyReply>>,
}

impl Accounts {
    /// Initializes provider-owned tables or validates their format without
    /// starting runtime work.
    pub async fn init(db: &RhoDb) -> anyhow::Result<()> {
        accounts::init(db).await
    }

    pub async fn new(db: RhoDb) -> anyhow::Result<Self> {
        Self::new_with_config(db, InferenceConfig::default()).await
    }

    pub async fn new_with_config(db: RhoDb, config: InferenceConfig) -> anyhow::Result<Self> {
        Self::init(&db).await?;
        let accounts = Arc::new(AccountManager::open(db.clone()).await);
        let production_chatgpt =
            &*config.responses_base_url == crate::responses::DEFAULT_CHATGPT_BASE_URL;
        // The quota endpoint is first-party ChatGPT-only.
        if production_chatgpt {
            accounts.spawn_poller();
        }
        let host = Self(Arc::new(HostInner {
            accounts,
            routes: RouteSelector::new(Some(db)),
            responses_base_url: config.responses_base_url,
            credentials: OnceLock::new(),
            client: OnceLock::new(),
            closed: watch::Sender::new(false),
        }));
        if production_chatgpt {
            host.spawn_route_prober();
        }
        Ok(host)
    }

    pub fn responses_base_url(&self) -> &str {
        &self.0.responses_base_url
    }

    /// A host-local client uses the same policy request path as a worker.
    pub fn client(&self) -> Inference {
        self.0.client.get_or_init(|| self.open_client()).clone()
    }

    fn open_client(&self) -> Inference {
        let (calls, mut requests) = mpsc::channel::<PolicyCall>(32);
        let host = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            while let Some(call) = requests.recv().await {
                let Some(host) = host.upgrade() else { return };
                let result = Accounts(host).policy_call(call.body).await;
                let _ = call.reply.send(result);
            }
        });
        Inference::from_worker(
            calls,
            self.credential_updates(),
            self.route_updates(),
            self.0.closed.subscribe(),
            InferenceConfig {
                responses_base_url: self.0.responses_base_url.clone(),
            },
        )
    }

    async fn policy_call(&self, body: PolicyRequest) -> anyhow::Result<PolicyReply> {
        Ok(match body {
            PolicyRequest::SelectAccount => PolicyReply::Account(self.select().await?),
            PolicyRequest::ResolveAuth(auth) => PolicyReply::Auth(self.resolve_auth(auth).await?),
            PolicyRequest::RateLimited(selected) => {
                let changed = self.mark_rate_limited(&selected).await;
                self.credential_snapshot().await?;
                PolicyReply::RateLimited(changed)
            }
            PolicyRequest::Quota { selected, quota } => {
                self.observe_quota(&selected, quota).await;
                PolicyReply::Done
            }
            PolicyRequest::RouteFailed { route, selected } => {
                self.report_connect_failure(route, selected.as_ref()).await;
                PolicyReply::Done
            }
        })
    }

    pub fn route_updates(&self) -> watch::Receiver<RouteSelection> {
        self.0.routes.subscribe()
    }

    pub async fn report_connect_failure(&self, route: DialRoute, selected: Option<&SelectedAuth>) {
        self.0.routes.report_connect_failure(route, selected)
    }

    fn spawn_route_prober(&self) {
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { return };
                inner.routes.probe_once(&Accounts(inner.clone())).await;
                drop(inner);
                tokio::time::sleep(RouteSelector::probe_interval()).await;
            }
        });
    }

    pub async fn auth(&self) -> anyhow::Result<InferenceAuth> {
        Ok(self.select().await?.auth)
    }

    pub async fn resolve_auth(&self, auth: InferenceAuth) -> anyhow::Result<crate::ResolvedAuth> {
        Ok(tokio::task::spawn_blocking(move || auth.resolve()).await??)
    }

    pub fn credential_updates(&self) -> watch::Receiver<crate::CredentialSnapshot> {
        self.0
            .credentials
            .get_or_init(|| crate::credentials::subscribe(self.0.accounts.selection_updates()))
            .clone()
    }

    /// Fence a policy mutation against the published selection.
    pub async fn credential_snapshot(&self) -> anyhow::Result<crate::CredentialSnapshot> {
        let mut updates = self.credential_updates();
        loop {
            let wanted = self.0.accounts.selection_updates().borrow().clone();
            let snapshot = updates.borrow_and_update().clone();
            if snapshot.matches_selection(&wanted) && snapshot.current().is_some() {
                return Ok(snapshot);
            }
            updates.changed().await?;
        }
    }

    pub async fn select(&self) -> anyhow::Result<SelectedAuth> {
        self.0.accounts.select().await
    }

    pub async fn mark_rate_limited(&self, selected: &SelectedAuth) -> bool {
        self.0.accounts.mark_rate_limited(selected).await
    }

    pub async fn observe_quota(&self, selected: &SelectedAuth, quota: QuotaUpdate) {
        self.0.accounts.observe_quota(selected, quota).await
    }

    pub async fn set_account_enabled(&self, namespace: &str, enabled: bool) {
        self.0.accounts.set_enabled(namespace, enabled).await;
    }

    pub fn state(&self) -> InferenceState {
        self.0.accounts.state()
    }
    pub fn subscribe(&self) -> watch::Receiver<InferenceState> {
        self.0.accounts.subscribe()
    }
    pub fn quota_history(&self, since: UnixMs) -> Vec<InferenceQuotaSeries> {
        self.0.accounts.history(since)
    }
    pub fn route_probe_history(&self, since: UnixMs) -> Vec<crate::responses::InferenceRouteProbe> {
        self.0.routes.history(since)
    }
}

impl Inference {
    pub fn responses_base_url(&self) -> &str {
        &self.responses_base_url
    }

    /// Worker client: watches are pushed by workset IPC, policy calls share its
    /// FIFO.
    pub(crate) fn from_worker(
        calls: mpsc::Sender<PolicyCall>,
        credentials: watch::Receiver<crate::CredentialSnapshot>,
        routes: watch::Receiver<RouteSelection>,
        closed: watch::Receiver<bool>,
        config: InferenceConfig,
    ) -> Self {
        Self {
            responses_base_url: config.responses_base_url,
            calls,
            credentials,
            routes,
            closed,
        }
    }

    pub fn route_updates(&self) -> watch::Receiver<RouteSelection> {
        self.routes.clone()
    }

    async fn request(&self, body: PolicyRequest) -> anyhow::Result<PolicyReply> {
        let (reply, result) = oneshot::channel();
        self.calls
            .send(PolicyCall { body, reply })
            .await
            .map_err(|_| anyhow::anyhow!("agent connection closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent connection closed"))?
    }

    pub async fn report_connect_failure(&self, route: DialRoute, selected: Option<&SelectedAuth>) {
        let _ = self
            .request(PolicyRequest::RouteFailed {
                route,
                selected: selected.cloned(),
            })
            .await;
    }

    /// An inference session owns its socket; its observations are fenced
    /// through this client.
    pub(crate) fn session(
        &self,
        profile: InferenceProfile,
        model: InferenceModel,
    ) -> crate::step::InferenceSession {
        let (session, mut reports) = crate::step::InferenceSession::new(
            self.responses_base_url.clone(),
            profile,
            model,
            self.route_updates(),
        );
        let policy = self.clone();
        tokio::spawn(async move {
            while let Some(report) = reports.recv().await {
                match report {
                    crate::step::Observation::Quota {
                        selected,
                        quota,
                        done,
                    } => {
                        policy.observe_quota(&selected, quota).await;
                        let _ = done.send(());
                    }
                    crate::step::Observation::RouteFailed {
                        selected,
                        route,
                        done,
                    } => {
                        policy.report_connect_failure(route, Some(&selected)).await;
                        let _ = done.send(());
                    }
                }
            }
        });
        session
    }

    /// A text-only provider exchange. The caller owns retries.
    pub async fn text(&self, instructions: Arc<str>, input: String) -> anyhow::Result<String> {
        let (selected, auth) = self.select_resolved().await?;
        let session = self.session(
            InferenceProfile {
                effort: crate::config::ReasoningEffort::Medium,
                fast_mode: true,
            },
            InferenceModel::Gpt6Luna,
        );
        let mut response = session.text_start(instructions, input, selected.clone(), auth);
        while let Some(event) = response.recv().await {
            match event {
                crate::step::Event::Completed(step) => {
                    anyhow::ensure!(step.call.is_none(), "text completion returned a tool call");
                    return Ok(step.prose);
                }
                crate::step::Event::Failed(error) if error.is::<crate::step::RateLimited>() => {
                    if self.retryable(&error, &selected).await {
                        return Err(crate::step::Retryable(error.to_string()).into());
                    }
                    anyhow::bail!("provider quota exhausted: {error}");
                }
                crate::step::Event::Failed(error) => return Err(error),
                crate::step::Event::NeedsContext => anyhow::bail!("text exchange needs context"),
                crate::step::Event::Call { .. } | crate::step::Event::Code(_) => {}
            }
        }
        anyhow::bail!("provider exchange ended without completion")
    }

    /// A rate-limit observation changes account selection before retry
    /// admission.
    pub async fn retryable(&self, error: &anyhow::Error, selected: &SelectedAuth) -> bool {
        if error.is::<crate::step::RateLimited>() {
            self.mark_rate_limited(selected).await
        } else {
            crate::step::is_retryable(error)
        }
    }

    pub async fn auth(&self) -> anyhow::Result<InferenceAuth> {
        Ok(self.select().await?.auth)
    }

    pub async fn resolve_auth(&self, auth: InferenceAuth) -> anyhow::Result<crate::ResolvedAuth> {
        match self.request(PolicyRequest::ResolveAuth(auth)).await? {
            PolicyReply::Auth(auth) => Ok(auth),
            _ => anyhow::bail!("unexpected credential service reply"),
        }
    }

    pub async fn select_resolved(&self) -> anyhow::Result<(SelectedAuth, crate::ResolvedAuth)> {
        let mut credentials = self.credentials.clone();
        let mut closed = self.closed.clone();
        loop {
            anyhow::ensure!(!*closed.borrow_and_update(), "agent connection closed");
            if let Some(result) = credentials.borrow_and_update().current() {
                return result.map(|(mut selected, resolved)| {
                    selected.account_id = resolved.account_id.clone();
                    (selected, resolved)
                });
            }
            tokio::select! {
                biased;
                _ = closed.changed() => anyhow::bail!("agent connection closed"),
                change = credentials.changed() => change?,
            }
        }
    }

    pub async fn select(&self) -> anyhow::Result<SelectedAuth> {
        match self.request(PolicyRequest::SelectAccount).await? {
            PolicyReply::Account(selected) => Ok(selected),
            _ => anyhow::bail!("unexpected account service reply"),
        }
    }

    pub async fn mark_rate_limited(&self, selected: &SelectedAuth) -> bool {
        matches!(
            self.request(PolicyRequest::RateLimited(selected.clone()))
                .await,
            Ok(PolicyReply::RateLimited(true))
        )
    }

    pub async fn observe_quota(&self, selected: &SelectedAuth, quota: QuotaUpdate) {
        let _ = self
            .request(PolicyRequest::Quota {
                selected: selected.clone(),
                quota,
            })
            .await;
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
    use super::*;
    use crate::CredentialState;

    #[tokio::test]
    async fn host_client_uses_host_policy_without_accessing_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let host = Accounts::new_with_config(
            RhoDb::open(temp.path().join("rho.redb")),
            InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        // Disable discovered accounts before the client opens its credential
        // watch.
        for namespace in host.state().namespaces {
            host.set_account_enabled(&namespace, false).await;
        }
        let client = host.client();
        assert_eq!(
            client.select().await.unwrap_err().to_string(),
            host.select().await.unwrap_err().to_string()
        );
        let error =
            tokio::time::timeout(std::time::Duration::from_secs(3), client.select_resolved())
                .await
                .unwrap()
                .unwrap_err();
        assert_ne!(error.to_string(), "agent connection closed");
    }

    #[tokio::test]
    async fn worker_uses_pushed_credentials_and_policy_calls_without_opening_a_store() {
        let selected = SelectedAuth {
            auth: InferenceAuth::oauth_file("/unused/credentials.json"),
            namespace: Some("account".into()),
            account_id: None,
        };
        let (calls, mut requests) = mpsc::channel(1);
        let (credentials, updates) = watch::channel(crate::CredentialSnapshot {
            revision: 1,
            state: CredentialState::Ready {
                selected: selected.clone(),
                auth: crate::ResolvedAuth {
                    bearer_token: "test".into(),
                    account_id: Some("identity".into()),
                    client_secret: [0; 32],
                },
                refresh_at: u64::MAX,
            },
        });
        let (_routes, routes) = watch::channel(RouteSelection::default());
        let (_closed, closed) = watch::channel(false);
        let inference = Inference::from_worker(
            calls,
            updates,
            routes,
            closed,
            InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        );
        assert_eq!(inference.responses_base_url(), "http://127.0.0.1:1");
        let (selected_with_identity, resolved) = inference.select_resolved().await.unwrap();
        assert_eq!(resolved.bearer_token, "test");
        assert_eq!(
            selected_with_identity.account_id.as_deref(),
            Some("identity")
        );
        let selected_copy = selected.clone();
        let server = tokio::spawn(async move {
            let call = requests.recv().await.unwrap();
            assert!(matches!(call.body, PolicyRequest::ResolveAuth(_)));
            let _ = call.reply.send(Ok(PolicyReply::Auth(crate::ResolvedAuth {
                bearer_token: "refreshed".into(),
                account_id: None,
                client_secret: [0; 32],
            })));
            let call = requests.recv().await.unwrap();
            assert!(matches!(call.body, PolicyRequest::Quota { selected, quota }
                if selected == selected_copy && quota.weekly_used_percent == 17));
            let _ = call.reply.send(Ok(PolicyReply::Done));
            let call = requests.recv().await.unwrap();
            assert!(
                matches!(call.body, PolicyRequest::RateLimited(selected) if selected == selected_copy)
            );
            let _ = call.reply.send(Ok(PolicyReply::RateLimited(true)));
        });
        assert_eq!(
            inference
                .resolve_auth(selected.auth.clone())
                .await
                .unwrap()
                .bearer_token,
            "refreshed"
        );
        inference
            .observe_quota(
                &selected,
                QuotaUpdate {
                    weekly_used_percent: 17,
                    weekly_reset_at_unix: None,
                    routing_used_percent: 23,
                    routing_reset_at_unix: None,
                },
            )
            .await;
        assert!(inference.mark_rate_limited(&selected).await);
        server.await.unwrap();
        drop(credentials);
    }
}
