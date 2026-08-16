mod auth;
mod session;
mod wire;

pub(crate) use auth::{AntigravityAuthFile, create_credentials};
pub(crate) use session::AntigravitySession;

fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}
