//! The OAuth grant the server signs in to Notion MCP with: `rho notion init`
//! registers a client and the user approves it in a browser; the server
//! refreshes access tokens from the grant's refresh token.

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng as _;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// Where the browser lands after approval. Nothing listens there: the user
/// copies the address it shows back into `rho notion init`.
pub const REDIRECT_URI: &str = "http://localhost:8976/callback";

/// Notion MCP's endpoints, from its OAuth metadata
/// (`/.well-known/oauth-authorization-server`).
#[derive(Clone)]
pub struct Endpoints {
    pub mcp: Url,
    pub register: Url,
    pub authorize: Url,
    pub token: Url,
}

impl Endpoints {
    pub fn notion() -> Self {
        Self::at(&Url::parse("https://mcp.notion.com/").expect("static URL"))
    }

    pub fn at(base: &Url) -> Self {
        let join = |path| base.join(path).expect("static path");
        Self {
            mcp: join("mcp"),
            register: join("register"),
            authorize: join("authorize"),
            token: join("token"),
        }
    }
}

pub fn http_client() -> Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static Notion HTTP client configuration is valid")
}

/// An authorization the user has yet to approve.
pub struct Pending {
    /// The page the user approves the grant on.
    pub url: Url,
    client_id: String,
    verifier: String,
    state: String,
}

/// What the host keeps: the server refreshes access tokens with it.
pub struct Grant {
    pub client_id: String,
    pub refresh_token: String,
}

pub struct Tokens {
    pub access_token: String,
    /// Set when Notion rotates the refresh token.
    pub refresh_token: Option<String>,
}

/// Registers a client (RFC 7591) and builds its authorization URL with PKCE.
pub async fn begin(client: &Client, endpoints: &Endpoints) -> Result<Pending> {
    let registration: Value = check(
        client
            .post(endpoints.register.clone())
            .json(&json!({
                "client_name": "rho agent host",
                "redirect_uris": [REDIRECT_URI],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            }))
            .send()
            .await,
        "registering a Notion MCP client",
    )
    .await?;
    let client_id = registration["client_id"]
        .as_str()
        .context("Notion MCP registered no client_id")?
        .to_owned();
    let verifier = random();
    let state = random();
    let mut url = endpoints.authorize.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("resource", endpoints.mcp.as_str());
    Ok(Pending {
        url,
        client_id,
        verifier,
        state,
    })
}

/// Exchanges the code in `redirected`, the address the browser landed on.
pub async fn finish(
    client: &Client,
    endpoints: &Endpoints,
    pending: Pending,
    redirected: &str,
) -> Result<Grant> {
    let redirected = Url::parse(redirected.trim()).context("not an address")?;
    let query = |name| {
        redirected
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if let Some(error) = query("error") {
        bail!("Notion refused the authorization: {error}");
    }
    if query("state").as_deref() != Some(pending.state.as_str()) {
        bail!("the address is from another authorization: run `rho notion init` again");
    }
    let code = query("code").context("the address has no authorization code")?;
    let tokens: Value = check(
        client
            .post(endpoints.token.clone())
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("client_id", &pending.client_id),
                ("code_verifier", &pending.verifier),
                ("resource", endpoints.mcp.as_str()),
            ])
            .send()
            .await,
        "exchanging the authorization code",
    )
    .await?;
    Ok(Grant {
        client_id: pending.client_id,
        refresh_token: tokens["refresh_token"]
            .as_str()
            .context("Notion MCP granted no refresh token")?
            .to_owned(),
    })
}

pub async fn refresh(
    client: &Client,
    endpoints: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let tokens: Value = check(
        client
            .post(endpoints.token.clone())
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
                ("resource", endpoints.mcp.as_str()),
            ])
            .send()
            .await,
        "refreshing the Notion MCP access token",
    )
    .await?;
    Ok(Tokens {
        access_token: tokens["access_token"]
            .as_str()
            .context("Notion MCP returned no access token")?
            .to_owned(),
        refresh_token: tokens["refresh_token"].as_str().map(str::to_owned),
    })
}

async fn check(response: reqwest::Result<reqwest::Response>, doing: &str) -> Result<Value> {
    let response = response.with_context(|| doing.to_owned())?;
    let status = response.status();
    let body = response.text().await.with_context(|| doing.to_owned())?;
    if !status.is_success() {
        bail!("{doing}: Notion MCP returned {status}: {body}");
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
    #[test]
    fn challenge_is_rfc_7636s_s256() {
        // The example in RFC 7636, appendix B.
        assert_eq!(
            super::challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
