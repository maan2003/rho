//! The daemon-wide inference runtime and the sessions created from it.

use std::sync::Arc;

use rho_core::UnixMs;
use rho_db::RhoDb;
use tokio::sync::watch;

use crate::config::{InferenceModel, InferenceProfile};
use crate::accounts::{self, AccountManager, InferenceQuotaSeries, InferenceState, SelectedAuth};
use crate::responses::{InferenceAuth, InferenceSession, PromptCacheKey, QuotaUpdate};

/// Provider account policy, quota, persistence, and session creation. Cheap to
/// clone.
#[derive(Clone, Debug)]
pub struct Inference(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    accounts: Option<Arc<AccountManager>>,
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
        Self::migrate(&db).await?;
        let accounts = Arc::new(AccountManager::open(db).await);
        accounts.spawn_poller();
        let inference = Self(Arc::new(Inner {
            accounts: Some(accounts),
            #[cfg(test)]
            fixed_auth: None,
        }));
        Ok(inference)
    }

    #[cfg(test)]
    pub(crate) fn for_test(auth: InferenceAuth) -> Self {
        Self(Arc::new(Inner {
            accounts: None,
            fixed_auth: Some(auth),
        }))
    }

    pub fn deep_session(
        &self,
        profile: InferenceProfile,
        model: InferenceModel,
        prompt_cache_key: PromptCacheKey,
    ) -> InferenceSession {
        InferenceSession::new_deep(self.clone(), profile, model, prompt_cache_key)
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
        self.0
            .accounts
            .as_ref()
            .expect("inference account manager")
    }
}
