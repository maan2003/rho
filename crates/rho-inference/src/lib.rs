//! Inference provider integrations for rho.

mod accounts;
mod antigravity;
pub mod auth_cli;
pub mod config;
mod inference;
mod responses;
mod session;

pub use accounts::{
    InferenceQuotaPoint, InferenceQuotaSeries, InferenceQuotaSummary, InferenceState,
};
pub use auth_cli::{AuthArgs, run_auth_cli};
pub use inference::Inference;
pub use responses::{InferenceAuth, OpenAiResponsesProviderData, PromptCacheKey, ResolvedOAuth};
pub use session::InferenceSession;

/// Installs the TLS crypto provider if nothing has yet. Any HTTP client built
/// here needs one; a host that has not installed its own can call this first.
pub fn ensure_crypto_provider() {
    antigravity::ensure_crypto_provider();
}
