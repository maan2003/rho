//! Inference provider integrations for rho.

mod accounts;
pub mod auth_cli;
pub mod config;
mod credentials;
pub mod exec;
mod inference;
pub use credentials::{CredentialSnapshot, CredentialState};
mod responses;
pub mod types;

pub use accounts::{
    InferenceQuotaPoint, InferenceQuotaSeries, InferenceQuotaSummary, InferenceState, SelectedAuth,
};
pub use auth_cli::{AuthArgs, run_auth_cli};
pub use inference::{Inference, InferenceConfig, InferenceHost};
pub use responses::{
    DialRoute, InferenceAuth, InferenceRouteProbe, InferenceSession, OpenAiResponsesProviderData,
    PromptCacheKey, QuotaUpdate, ResolvedAuth, ResolvedOAuth, RouteSelection,
};

/// Installs the TLS crypto provider if nothing has yet. Any HTTP client built
/// here needs one; a host that has not installed its own can call this first.
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}
