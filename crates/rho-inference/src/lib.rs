//! Inference provider integrations for rho.

mod accounts;
pub mod auth_cli;
pub mod config;
pub mod exec;
mod inference;
mod responses;

pub use accounts::{
    InferenceQuotaPoint, InferenceQuotaSeries, InferenceQuotaSummary, InferenceState,
};
pub use auth_cli::{AuthArgs, run_auth_cli};
pub use inference::{Inference, InferenceConfig};
pub use responses::{
    InferenceAuth, InferenceRouteProbe, InferenceSession, OpenAiResponsesProviderData,
    PromptCacheKey, ResolvedOAuth,
};

/// Installs the TLS crypto provider if nothing has yet. Any HTTP client built
/// here needs one; a host that has not installed its own can call this first.
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}
