use rho_core::{InferenceEvent, InferenceRequest};

use crate::Inference;
use crate::antigravity::AntigravitySession;
use crate::config::{InferenceModel, InferenceProfile};
use crate::responses::{self, PromptCacheKey};

/// One provider-backed inference session.
pub struct InferenceSession {
    inner: SessionImpl,
}

enum SessionImpl {
    Responses(responses::InferenceSession),
    Antigravity(AntigravitySession),
}

impl InferenceSession {
    pub(crate) fn new_responses(
        inference: Inference,
        profile: InferenceProfile,
        model: InferenceModel,
        prompt_cache_key: PromptCacheKey,
    ) -> Self {
        Self {
            inner: SessionImpl::Responses(responses::InferenceSession::new_deep(
                inference,
                profile,
                model,
                prompt_cache_key,
            )),
        }
    }

    pub(crate) fn new_antigravity(
        profile: InferenceProfile,
        prompt_cache_key: PromptCacheKey,
    ) -> Self {
        Self {
            inner: SessionImpl::Antigravity(AntigravitySession::new(profile, prompt_cache_key)),
        }
    }

    pub(crate) fn new_title(inference: Inference, prompt_cache_key: PromptCacheKey) -> Self {
        Self {
            inner: SessionImpl::Responses(responses::InferenceSession::new_title(
                inference,
                prompt_cache_key,
            )),
        }
    }

    pub(crate) fn new_status(inference: Inference, prompt_cache_key: PromptCacheKey) -> Self {
        Self {
            inner: SessionImpl::Responses(responses::InferenceSession::new_status(
                inference,
                prompt_cache_key,
            )),
        }
    }

    pub fn set_deep_config(&mut self, profile: InferenceProfile, model: InferenceModel) -> bool {
        match &mut self.inner {
            SessionImpl::Responses(session) if model != InferenceModel::Gemini35FlashLow => {
                session.set_deep_config(profile, model)
            }
            SessionImpl::Antigravity(session) if model == InferenceModel::Gemini35FlashLow => {
                session.set_profile(profile);
                true
            }
            _ => false,
        }
    }

    pub fn prompt_cache_key(&self) -> PromptCacheKey {
        match &self.inner {
            SessionImpl::Responses(session) => session.prompt_cache_key(),
            SessionImpl::Antigravity(session) => session.prompt_cache_key(),
        }
    }

    pub fn set_prompt_cache_key(&mut self, key: PromptCacheKey) {
        match &mut self.inner {
            SessionImpl::Responses(session) => session.set_prompt_cache_key(key),
            SessionImpl::Antigravity(session) => session.set_prompt_cache_key(key),
        }
    }

    pub fn has_active_request(&self) -> bool {
        match &self.inner {
            SessionImpl::Responses(session) => session.has_active_request(),
            SessionImpl::Antigravity(session) => session.has_active_request(),
        }
    }

    pub fn context_window(&self) -> Option<u64> {
        match &self.inner {
            SessionImpl::Responses(session) => session.context_window(),
            SessionImpl::Antigravity(session) => session.context_window(),
        }
    }

    pub fn auto_compact_token_limit(&self) -> Option<u64> {
        match &self.inner {
            SessionImpl::Responses(session) => session.auto_compact_token_limit(),
            SessionImpl::Antigravity(session) => session.auto_compact_token_limit(),
        }
    }

    pub fn request(&mut self, request: InferenceRequest) {
        match &mut self.inner {
            SessionImpl::Responses(session) => session.request(request),
            SessionImpl::Antigravity(session) => session.request(request),
        }
    }

    pub async fn run(&mut self) -> InferenceEvent {
        match &mut self.inner {
            SessionImpl::Responses(session) => session.run().await,
            SessionImpl::Antigravity(session) => session.run().await,
        }
    }

    pub fn abort(&mut self) {
        match &mut self.inner {
            SessionImpl::Responses(session) => session.abort(),
            SessionImpl::Antigravity(session) => session.abort(),
        }
    }
}

impl std::fmt::Debug for InferenceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceSession")
            .field(
                "provider",
                &match self.inner {
                    SessionImpl::Responses(_) => "chatgpt",
                    SessionImpl::Antigravity(_) => "antigravity",
                },
            )
            .finish_non_exhaustive()
    }
}
