//! OpenAI Responses Lite transport and its ChatGPT account/route policy.

mod accounts;
pub mod auth_cli;
pub use rho_agent::inference::config;
mod credentials;
mod inference;
mod policy;
mod wiring;
pub use credentials::{CredentialSnapshot, CredentialState};
pub use wiring::worker_main;
mod responses;
mod step;
pub mod transcript;

pub use accounts::{
    InferenceQuotaPoint, InferenceQuotaSeries, InferenceQuotaSummary, InferenceState, SelectedAuth,
};
pub use auth_cli::{AuthArgs, run_auth_cli};
pub use inference::{Accounts, Inference, InferenceConfig};
pub(crate) use inference::{PolicyCall, PolicyReply, PolicyRequest};
pub use responses::{
    DialRoute, InferenceAuth, InferenceRouteProbe, PromptCacheKey, QuotaUpdate, ResolvedAuth,
    ResolvedOAuth, RouteSelection,
};

/// Installs the TLS crypto provider if nothing has yet. Any HTTP client built
/// here needs one; a host that has not installed its own can call this first.
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}
