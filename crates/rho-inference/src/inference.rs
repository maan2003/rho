//! The daemon-wide inference runtime and the sessions created from it.

use std::sync::Arc;

use rho_core::UnixMs;
use rho_db::RhoDb;
use tokio::sync::watch;

use crate::accounts::{self, AccountManager, InferenceQuotaSeries, InferenceState, SelectedAuth};
use crate::config::{InferenceModel, InferenceProfile};
use crate::responses::{InferenceAuth, PromptCacheKey, QuotaUpdate};
use crate::session::InferenceSession;

/// Provider account policy, quota, persistence, and session creation. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct Inference(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    accounts: Option<Arc<AccountManager>>,
    responses_base_url: Arc<str>,
    #[cfg(test)]
    fixed_auth: Option<InferenceAuth>,
}

impl Inference {
    /// Applies provider-owned database migrations without starting runtime
    /// work or accessing credentials.
    pub async fn migrate(db: &RhoDb) -> anyhow::Result<()> {
        accounts::init(db).await
    }

    /// Opens the daemon-owned inference runtime and starts its fallback quota
    /// poller.
    pub async fn new(db: RhoDb) -> anyhow::Result<Self> {
        Self::new_with_config(db, InferenceConfig::default()).await
    }

    /// Opens inference with explicit provider transport configuration. This is
    /// used by isolated QA rigs; the production default remains ChatGPT.
    pub async fn new_with_config(db: RhoDb, config: InferenceConfig) -> anyhow::Result<Self> {
        Self::migrate(&db).await?;
        let accounts = Arc::new(AccountManager::open(db).await);
        // The quota endpoint is a first-party ChatGPT-only API. An explicit
        // Responses endpoint (for example the full-stack QA server) must not
        // leak a side request to the production provider.
        if &*config.responses_base_url == crate::responses::DEFAULT_CHATGPT_BASE_URL {
            accounts.spawn_poller();
        }
        let inference = Self(Arc::new(Inner {
            accounts: Some(accounts),
            responses_base_url: config.responses_base_url,
            #[cfg(test)]
            fixed_auth: None,
        }));
        Ok(inference)
    }

    #[cfg(test)]
    pub(crate) fn for_test(auth: InferenceAuth) -> Self {
        Self(Arc::new(Inner {
            accounts: None,
            responses_base_url: crate::responses::DEFAULT_CHATGPT_BASE_URL.into(),
            fixed_auth: Some(auth),
        }))
    }

    pub(crate) fn responses_base_url(&self) -> &str {
        &self.0.responses_base_url
    }

    pub fn deep_session(
        &self,
        profile: InferenceProfile,
        model: InferenceModel,
        prompt_cache_key: PromptCacheKey,
    ) -> InferenceSession {
        match model {
            InferenceModel::Gemini37FlashLow => {
                InferenceSession::new_antigravity(profile, prompt_cache_key)
            }
            _ => InferenceSession::new_responses(self.clone(), profile, model, prompt_cache_key),
        }
    }

    pub fn title_session(&self, prompt_cache_key: PromptCacheKey) -> InferenceSession {
        InferenceSession::new_title(self.clone(), prompt_cache_key)
    }

    pub fn status_session(&self, prompt_cache_key: PromptCacheKey) -> InferenceSession {
        InferenceSession::new_status(self.clone(), prompt_cache_key)
    }

    /// Returns the account decision already made by the account manager.
    pub async fn auth(&self) -> anyhow::Result<InferenceAuth> {
        Ok(self.select().await?.auth)
    }

    pub(crate) async fn select(&self) -> anyhow::Result<SelectedAuth> {
        if let Some(accounts) = &self.0.accounts {
            accounts.select().await
        } else {
            #[cfg(test)]
            if let Some(auth) = &self.0.fixed_auth {
                return Ok(SelectedAuth {
                    auth: auth.clone(),
                    namespace: None,
                    account_id: None,
                });
            }
            anyhow::bail!("inference account manager is unavailable")
        }
    }

    pub(crate) async fn mark_rate_limited(&self, selected: &SelectedAuth) -> bool {
        let Some(accounts) = &self.0.accounts else {
            return false;
        };
        accounts.mark_rate_limited(selected).await
    }

    pub(crate) async fn observe_quota(&self, selected: &SelectedAuth, quota: QuotaUpdate) {
        if let Some(accounts) = &self.0.accounts {
            accounts.observe_quota(selected, quota).await;
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

    fn accounts(&self) -> &AccountManager {
        self.0.accounts.as_ref().expect("inference account manager")
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
