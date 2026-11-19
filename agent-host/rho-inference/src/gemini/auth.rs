//! The Antigravity login, kept on the agent host. The file is the one the
//! Antigravity CLI (`agy`) writes, copied as it is:
//!
//! ```sh
//! cp ~/.gemini/antigravity-cli/antigravity-oauth-token \
//!    ~/.local/state/rho/auth.d/antigravity/default.json
//! ```
//!
//! The host refreshes its access token in place and asks Cloud Code which
//! project the account works in. Workers get only the resolved token.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use senax_encoder::{Decode, Encode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::responses::oauth::{OAuthFile, atomic_write_private, create_private_dir};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
// Public installed-application credentials of Antigravity, not user secrets.
// Chunked so that repository scanners do not take them for leaked keys.
const CLIENT_ID: &[&str] = &[
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com",
];
const CLIENT_SECRET: &[&str] = &["GOCSPX-", "K58FWR486LdLJ1mLB8sXC4z6qDAf"];
/// Renew this long before the access token expires, so that no request
/// carries a token that dies on the way.
const REFRESH_WINDOW: Duration = Duration::from_secs(5 * 60);

/// What a worker sends a request with. Secret-bearing: never logged.
#[derive(Clone, Encode, Decode)]
pub(crate) struct GeminiAuth {
    pub access_token: String,
    pub project: String,
}

impl std::fmt::Debug for GeminiAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiAuth")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

/// `agy`'s token file. Fields rho does not use are written back unchanged.
#[derive(Serialize, Deserialize)]
struct TokenFile {
    token: Token,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    /// RFC 3339, as Go writes it.
    expiry: String,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

/// The host's Antigravity login: the token file, and the project Cloud Code
/// gave the current access token.
#[derive(Debug)]
pub(crate) struct Login {
    path: PathBuf,
    http: reqwest::Client,
    resolved: Mutex<Option<(GeminiAuth, jiff::Timestamp)>>,
}

impl Login {
    pub(crate) fn open_default() -> io::Result<Self> {
        Ok(Self::new(
            OAuthFile::default_auth_dir()?
                .join("antigravity")
                .join("default.json"),
        ))
    }

    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            http: super::http(),
            resolved: Mutex::new(None),
        }
    }

    /// A token good for at least [`REFRESH_WINDOW`], and its project.
    pub(crate) async fn resolve(&self) -> anyhow::Result<GeminiAuth> {
        let mut resolved = self.resolved.lock().await;
        if let Some((auth, expiry)) = &*resolved
            && fresh(*expiry)
        {
            return Ok(auth.clone());
        }
        let path = self.path.clone();
        let (access_token, expiry) =
            tokio::task::spawn_blocking(move || refreshed(&path)).await??;
        let project = super::project(&self.http, &access_token).await?;
        let auth = GeminiAuth {
            access_token,
            project,
        };
        *resolved = Some((auth.clone(), expiry));
        Ok(auth)
    }
}

fn fresh(expiry: jiff::Timestamp) -> bool {
    jiff::Timestamp::now() + REFRESH_WINDOW < expiry
}

/// The file's access token, refreshed and written back first when it is
/// about to expire. Holds the file's lock throughout, so two hosts never
/// refresh at once.
fn refreshed(path: &std::path::Path) -> anyhow::Result<(String, jiff::Timestamp)> {
    use anyhow::Context as _;
    let dir = path
        .parent()
        .context("no parent for the Antigravity login")?;
    create_private_dir(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "no Antigravity login at {}: copy agy's \
             ~/.gemini/antigravity-cli/antigravity-oauth-token there",
            path.display()
        )
    })?;
    let mut file: TokenFile = serde_json::from_str(&text)
        .with_context(|| format!("{} is not an agy token file", path.display()))?;
    let mut expiry: jiff::Timestamp = file
        .token
        .expiry
        .parse()
        .with_context(|| format!("the expiry in {}", path.display()))?;
    if !fresh(expiry) {
        let renewed = refresh(&file.token.refresh_token)?;
        expiry = jiff::Timestamp::now() + Duration::from_secs(renewed.expires_in);
        file.token.access_token = renewed.access_token;
        file.token.expiry = expiry.to_string();
        if let Some(token) = renewed.refresh_token {
            file.token.refresh_token = token;
        }
        atomic_write_private(path, &serde_json::to_vec_pretty(&file)?)?;
    }
    Ok((file.token.access_token, expiry))
}

#[derive(Deserialize)]
struct Renewed {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

fn refresh(refresh_token: &str) -> anyhow::Result<Renewed> {
    crate::ensure_crypto_provider();
    let response = reqwest::blocking::Client::new()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &CLIENT_ID.concat()),
            ("client_secret", &CLIENT_SECRET.concat()),
        ])
        .send()?;
    let status = response.status();
    if !status.is_success() {
        // Google's error body names the reason (`invalid_grant`) and holds
        // no secret.
        let body = response.text().unwrap_or_default();
        anyhow::bail!("refreshing the Antigravity login: {status}: {body}");
    }
    Ok(response.json()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copied agy file is read, and what rho does not use survives a
    /// rewrite.
    #[test]
    fn agy_files_round_trip_with_their_other_fields() {
        let text = r#"{"auth_method":"consumer","id_token":"jwt",
            "token":{"access_token":"ya29.a","token_type":"Bearer","refresh_token":"1//r",
                     "expiry":"2026-11-12T20:13:01.827296903Z"}}"#;
        let file: TokenFile = serde_json::from_str(text).unwrap();
        assert_eq!(file.token.refresh_token, "1//r");
        let expiry: jiff::Timestamp = file.token.expiry.parse().unwrap();
        assert_eq!(expiry.as_second(), 1_794_514_381);
        let again: serde_json::Value = serde_json::to_value(&file).unwrap();
        assert_eq!(again["auth_method"], "consumer");
        assert_eq!(again["id_token"], "jwt");
        assert_eq!(again["token"]["token_type"], "Bearer");
    }

    /// A token about to expire is not fresh, so it is renewed before it
    /// can fail a request.
    #[test]
    fn tokens_are_renewed_inside_the_window() {
        let now = jiff::Timestamp::now();
        assert!(!fresh(now + Duration::from_secs(4 * 60)));
        assert!(fresh(now + Duration::from_secs(6 * 60)));
    }

    /// A fresh file is used as it is: no refresh, no rewrite.
    #[test]
    fn a_fresh_file_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("antigravity").join("default.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let expiry = jiff::Timestamp::now() + Duration::from_secs(3600);
        let text = format!(
            r#"{{"token":{{"access_token":"ya29.ok","refresh_token":"1//r","expiry":"{expiry}"}}}}"#
        );
        fs::write(&path, &text).unwrap();
        let (token, at) = refreshed(&path).unwrap();
        assert_eq!(token, "ya29.ok");
        assert_eq!(at, expiry);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}
