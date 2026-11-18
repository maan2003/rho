//! The OAuth grant the server calls Google with: `rho google init` builds
//! the authorization URL for an OAuth client of the Desktop app type, the
//! user approves it in a browser signed in as the account agents act as,
//! and the server refreshes access tokens from the grant's refresh token.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng as _;
use reqwest::{Client, Url};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// Where the browser lands after approval. Google lets a Desktop app client
/// redirect to any loopback port. Nothing listens there: the user copies
/// the address it shows back into `rho google init`.
pub const REDIRECT_URI: &str = "http://127.0.0.1:8977/callback";

/// What agents may reach, all read-only: Drive files, Meet transcripts
/// among them; Meet's conference records; and Calendar events. The Sheets
/// and Docs APIs accept `drive.readonly`, so they need no scope of their
/// own, only their API enabled.
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/drive.readonly",
    "https://www.googleapis.com/auth/meetings.space.readonly",
    "https://www.googleapis.com/auth/calendar.readonly",
];

/// Google's OAuth endpoints, and where API calls go.
#[derive(Clone)]
pub struct Endpoints {
    pub authorize: Url,
    pub token: Url,
    /// Where a call to `host` goes: `https://<host>/`, or in tests one
    /// server for every host.
    pub api: Api,
}

#[derive(Clone)]
pub enum Api {
    Google,
    At(Url),
}

impl Endpoints {
    pub fn google() -> Self {
        Self {
            authorize: Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
                .expect("static URL"),
            token: Url::parse("https://oauth2.googleapis.com/token").expect("static URL"),
            api: Api::Google,
        }
    }

    pub fn at(base: &Url) -> Self {
        let join = |path| base.join(path).expect("static path");
        Self {
            authorize: join("authorize"),
            token: join("token"),
            api: Api::At(base.clone()),
        }
    }

    /// The base URL for a host already checked to be Google's.
    pub(crate) fn api_base(&self, host: &str) -> Result<Url> {
        match &self.api {
            Api::Google => Url::parse(&format!("https://{host}/")).context("not a host name"),
            Api::At(base) => Ok(base.clone()),
        }
    }
}

pub fn http_client() -> Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static Google HTTP client configuration is valid")
}

/// The OAuth client `rho google init` signs in with.
#[derive(Clone)]
pub struct OAuthClient {
    pub id: String,
    pub secret: String,
}

/// An authorization the user has yet to approve.
pub struct Pending {
    /// The page the user approves the grant on.
    pub url: Url,
    verifier: String,
    state: String,
}

pub struct Tokens {
    pub access_token: String,
    /// How long the access token lasts.
    pub expires_in: Duration,
}

/// The authorization URL, with PKCE. `access_type=offline` and
/// `prompt=consent` make Google grant a refresh token even when the account
/// approved this client before.
pub fn begin(endpoints: &Endpoints, client_id: &str) -> Pending {
    let verifier = random();
    let state = random();
    let mut url = endpoints.authorize.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Pending {
        url,
        verifier,
        state,
    }
}

/// Exchanges the code in `redirected`, the address the browser landed on,
/// for the grant's refresh token.
pub async fn finish(
    http: &Client,
    endpoints: &Endpoints,
    client: &OAuthClient,
    pending: Pending,
    redirected: &str,
) -> Result<String> {
    let redirected = Url::parse(redirected.trim()).context("not an address")?;
    let query = |name| {
        redirected
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if let Some(error) = query("error") {
        bail!("Google refused the authorization: {error}");
    }
    if query("state").as_deref() != Some(pending.state.as_str()) {
        bail!("the address is from another authorization: run `rho google init` again");
    }
    let code = query("code").context("the address has no authorization code")?;
    let tokens = check(
        http.post(endpoints.token.clone())
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("client_id", &client.id),
                ("client_secret", &client.secret),
                ("code_verifier", &pending.verifier),
            ])
            .send()
            .await,
        "exchanging the authorization code",
    )
    .await?;
    granted(&tokens)?;
    Ok(tokens["refresh_token"]
        .as_str()
        .context("Google granted no refresh token")?
        .to_owned())
}

/// Fails unless the grant has every scope: the consent screen lets the user
/// leave some unticked.
fn granted(tokens: &Value) -> Result<()> {
    let granted: Vec<&str> = tokens["scope"]
        .as_str()
        .unwrap_or_default()
        .split(' ')
        .collect();
    let missing: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|scope| !granted.contains(scope))
        .collect();
    if !missing.is_empty() {
        bail!(
            "the grant lacks {}: approve every access, or enable its API in the client's project",
            missing.join(", ")
        );
    }
    Ok(())
}

pub async fn refresh(
    http: &Client,
    endpoints: &Endpoints,
    client: &OAuthClient,
    refresh_token: &str,
) -> Result<Tokens> {
    let tokens = check(
        http.post(endpoints.token.clone())
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &client.id),
                ("client_secret", &client.secret),
            ])
            .send()
            .await,
        "refreshing the Google access token",
    )
    .await?;
    Ok(Tokens {
        access_token: tokens["access_token"]
            .as_str()
            .context("Google returned no access token")?
            .to_owned(),
        expires_in: Duration::from_secs(
            tokens["expires_in"]
                .as_u64()
                .context("Google returned no expiry")?,
        ),
    })
}

async fn check(response: reqwest::Result<reqwest::Response>, doing: &str) -> Result<Value> {
    let response = response.with_context(|| doing.to_owned())?;
    let status = response.status();
    let body = response.text().await.with_context(|| doing.to_owned())?;
    if !status.is_success() {
        bail!("{doing}: Google returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("{doing}: reading the reply"))
}

fn random() -> String {
    let mut bytes = [0; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("system entropy");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE's S256 challenge (RFC 7636).
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn challenge_is_rfc_7636s_s256() {
        // The example in RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_grant_with_an_unticked_scope_is_refused() {
        let all = SCOPES.join(" ");
        assert!(granted(&json!({ "scope": all })).is_ok());
        let error = granted(&json!({ "scope": SCOPES[0] })).unwrap_err();
        assert!(error.to_string().contains(SCOPES[1]), "{error}");
    }

    #[test]
    fn the_authorization_url_asks_for_a_refresh_token_and_every_scope() {
        let pending = begin(&Endpoints::google(), "client.apps.googleusercontent.com");
        let query: std::collections::HashMap<String, String> =
            pending.url.query_pairs().into_owned().collect();
        assert_eq!(query["access_type"], "offline");
        assert_eq!(query["prompt"], "consent");
        assert_eq!(query["scope"], SCOPES.join(" "));
        assert_eq!(query["code_challenge"], challenge(&pending.verifier));
        assert_eq!(query["state"], pending.state);
    }
}
