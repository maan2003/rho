//! The account an agent talks to, and the sessions that talk to it.
//!
//! A session is one conversation; some facts belong to the account behind all
//! of them. Quota is the first: the provider mentions it in passing while
//! streaming something else, it is true of every session at once, and nothing
//! downstream should have to carry it from where it is noticed to where it is
//! shown. Holding it here means it can be watched at the source instead.

use std::sync::{Arc, RwLock};

use rho_core::UnixMs;
use tokio::sync::watch;

use crate::config::{InferenceModel, InferenceProfile};
use crate::responses::{InferenceAuth, InferenceSession, PromptCacheKey};

/// The most recent quota the provider mentioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaObservation {
    pub observed_at: UnixMs,
    pub used_percent: u8,
    /// When the window rolls over, if the provider said.
    pub reset_at_unix: Option<i64>,
}

/// One account's inference. Cheap to clone; every clone shares the account's
/// state, so an observation on any session is visible from all of them.
#[derive(Clone, Debug)]
pub struct Inference(Arc<Account>);

#[derive(Debug)]
struct Account {
    auth: RwLock<InferenceAuth>,
    quota: watch::Sender<Option<QuotaObservation>>,
}

impl Inference {
    pub fn new(auth: InferenceAuth) -> Self {
        Self(Arc::new(Account {
            auth: RwLock::new(auth),
            quota: watch::Sender::new(None),
        }))
    }

    /// A session for the main conversation.
    pub fn deep_session(
        &self,
        profile: InferenceProfile,
        model: InferenceModel,
        prompt_cache_key: PromptCacheKey,
    ) -> InferenceSession {
        InferenceSession::new_deep(self.clone(), profile, model, prompt_cache_key)
    }

    /// A session for naming a conversation, which uses a smaller model and no
    /// profile.
    pub fn title_session(&self, prompt_cache_key: PromptCacheKey) -> InferenceSession {
        InferenceSession::new_title(self.clone(), prompt_cache_key)
    }

    /// A small, runtime-local session for deriving display activity from an
    /// agent transcript. It deliberately has no relationship to the agent's
    /// persisted conversation.
    pub fn status_session(&self, prompt_cache_key: PromptCacheKey) -> InferenceSession {
        InferenceSession::new_status(self.clone(), prompt_cache_key)
    }

    /// Watch the account's quota. Yields immediately with whatever is known,
    /// which is `None` until the provider first mentions it.
    pub fn quota(&self) -> watch::Receiver<Option<QuotaObservation>> {
        self.0.quota.subscribe()
    }

    /// The latest observation, for callers with nothing to await on.
    pub fn latest_quota(&self) -> Option<QuotaObservation> {
        *self.0.quota.borrow()
    }

    pub fn auth(&self) -> InferenceAuth {
        self.0.auth.read().unwrap().clone()
    }

    /// Changes the account used by existing and future sessions. An active
    /// request finishes with the credentials it started with; the next
    /// request observes the replacement and reconnects if necessary.
    pub fn set_auth(&self, auth: InferenceAuth) {
        *self.0.auth.write().unwrap() = auth;
        self.0.quota.send_replace(None);
    }

    pub(crate) fn observe_quota(&self, used_percent: u8, reset_at_unix: Option<i64>) {
        self.0.quota.send_replace(Some(QuotaObservation {
            observed_at: UnixMs::now(),
            used_percent,
            reset_at_unix,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Inference {
        Inference::new(InferenceAuth::oauth_file("/nonexistent"))
    }

    #[tokio::test]
    async fn an_observation_reaches_anything_already_watching() {
        let inference = account();
        let mut watcher = inference.quota();
        inference.observe_quota(42, Some(1_783_173_000));

        watcher.changed().await.unwrap();
        let observed = watcher.borrow().expect("an observation");
        assert_eq!(observed.used_percent, 42);
        assert_eq!(observed.reset_at_unix, Some(1_783_173_000));
        assert_eq!(
            inference.latest_quota(),
            Some(observed),
            "and is there for latecomers"
        );
    }

    #[test]
    fn clones_share_one_account_but_separate_accounts_do_not() {
        let inference = account();
        inference.clone().observe_quota(7, None);
        assert!(inference.latest_quota().is_some(), "same account");
        assert!(
            account().latest_quota().is_none(),
            "a second account starts blank"
        );
    }

    #[test]
    fn auth_changes_reach_existing_clones() {
        let inference = account();
        let clone = inference.clone();
        let replacement = InferenceAuth::oauth_file("/replacement");
        inference.set_auth(replacement.clone());
        assert_eq!(clone.auth(), replacement);
    }
}
