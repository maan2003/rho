#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const REFRESH_EXPIRY_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceAuth {
    kind: InferenceAuthKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InferenceAuthKind {
    OAuthFile(OAuthFile),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponsesOAuthCredentials {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) refresh_token: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account_id: Option<String>,
    pub(crate) client_secret: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedAuth {
    pub(crate) bearer_token: String,
    pub(crate) account_id: Option<String>,
    pub(crate) client_secret: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OAuthFile {
    path: PathBuf,
}

impl InferenceAuth {
    pub fn named(name: impl AsRef<str>) -> io::Result<Self> {
        Self::oauth_file_named(name)
    }

    pub(crate) fn oauth_file(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: InferenceAuthKind::OAuthFile(OAuthFile::new(path)),
        }
    }

    pub(crate) fn oauth_file_named(name: impl AsRef<str>) -> io::Result<Self> {
        Ok(Self::oauth_file(OAuthFile::open_default(name)?.path()))
    }

    pub(crate) fn resolve(&self) -> io::Result<ResolvedAuth> {
        self.resolve_with_refresh(openai_codex_refresh)
    }

    pub(crate) fn resolve_with_refresh(
        &self,
        refresh: impl FnMut(&str) -> io::Result<ResponsesOAuthCredentials>,
    ) -> io::Result<ResolvedAuth> {
        match &self.kind {
            InferenceAuthKind::OAuthFile(file) => file.resolve_with_refresh(refresh),
        }
    }
}

impl ResponsesOAuthCredentials {
    pub(crate) fn resolved(&self) -> io::Result<ResolvedAuth> {
        if self.access_token.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OAuth credentials are missing an access token",
            ));
        }
        Ok(ResolvedAuth {
            bearer_token: self.access_token.clone(),
            account_id: self
                .account_id
                .as_ref()
                .filter(|account_id| !account_id.trim().is_empty())
                .cloned(),
            client_secret: self.client_secret,
        })
    }
}

impl OAuthFile {
    pub(crate) fn default_auth_dir() -> io::Result<PathBuf> {
        let state_dir = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "cannot determine state directory")
            })?
            .join("rho");
        Ok(state_dir.join("auth.d"))
    }

    pub(crate) fn open_default(name: impl AsRef<str>) -> io::Result<Self> {
        Self::open_at(Self::default_auth_dir()?, name)
    }

    pub(crate) fn open_at(auth_dir: impl Into<PathBuf>, name: impl AsRef<str>) -> io::Result<Self> {
        let name = name.as_ref();
        validate_auth_file_name(name)?;
        Ok(Self::new(auth_dir.into().join(format!("{name}.json"))))
    }

    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    pub(crate) fn load(&self) -> io::Result<Option<ResponsesOAuthCredentials>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save(&self, credentials: &ResponsesOAuthCredentials) -> io::Result<()> {
        self.with_lock(|| self.write(credentials))
    }

    pub(crate) fn delete(&self) -> io::Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn resolve_with_refresh(
        &self,
        mut refresh: impl FnMut(&str) -> io::Result<ResponsesOAuthCredentials>,
    ) -> io::Result<ResolvedAuth> {
        self.with_lock(|| {
            let Some(current) = self.load()? else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "OAuth credentials file is missing",
                ));
            };
            if !oauth_token_should_refresh(&current.access_token, current.expires_at_ms)
                || current.refresh_token.trim().is_empty()
            {
                return current.resolved();
            }

            let mut refreshed = refresh(&current.refresh_token)?;
            if refreshed.account_id.is_none() {
                refreshed.account_id = current.account_id;
            }
            refreshed.client_secret = current.client_secret;
            self.write(&refreshed)?;
            refreshed.resolved()
        })
    }

    fn write(&self, credentials: &ResponsesOAuthCredentials) -> io::Result<()> {
        let dir = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no parent for OAuth auth path")
        })?;
        create_private_dir(dir)?;
        let json = serde_json::to_string_pretty(credentials)?;
        atomic_write_private(&self.path, json.as_bytes())
    }

    /// Runs `f` while holding an exclusive cross-process lock on the auth file,
    /// so concurrent reads, refreshes, and writes stay serialized.
    fn with_lock<R>(&self, f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
        let lock_path = self.lock_path();
        let lock_dir = lock_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no parent for OAuth lock path")
        })?;
        create_private_dir(lock_dir)?;
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock()?;
        let result = f();
        let unlock_result = lock_file.unlock();
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

pub(crate) fn openai_codex_auth_url() -> (String, String, String) {
    let verifier = generate_code_verifier();
    let challenge = code_challenge(&verifier);
    let state = generate_state();
    let url = format!(
        "{OPENAI_AUTH_URL}?client_id={client_id}&redirect_uri={redirect}&response_type=code&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&codex_cli_simplified_flow=true&id_token_add_organizations=true",
        client_id = OPENAI_CLIENT_ID,
        redirect = urlencoding(OPENAI_REDIRECT_URI),
        scope = urlencoding("openid profile email offline_access"),
    );
    (url, state, verifier)
}

pub(crate) fn parse_redirect_url(input: &str) -> Result<(String, String), String> {
    let trimmed = input.trim();
    let url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Url::parse(trimmed).map_err(|error| format!("invalid URL: {error}"))?
    } else if trimmed.starts_with('/') || trimmed.starts_with('?') {
        Url::parse(&format!("http://localhost{trimmed}"))
            .map_err(|error| format!("invalid URL fragment: {error}"))?
    } else {
        return Err("expected full URL, or path/query string starting with '/' or '?'".to_owned());
    };

    let params: HashMap<_, _> = url.query_pairs().collect();
    let code = params
        .get("code")
        .ok_or("no 'code' parameter in URL")?
        .to_string();
    let state = params
        .get("state")
        .ok_or("no 'state' parameter in URL")?
        .to_string();

    Ok((code, state))
}

pub(crate) fn openai_codex_exchange(
    code: &str,
    verifier: &str,
) -> io::Result<ResponsesOAuthCredentials> {
    let body = format!(
        "grant_type=authorization_code&code={code}&code_verifier={verifier}&redirect_uri={redirect}&client_id={client_id}",
        code = urlencoding(code),
        verifier = urlencoding(verifier),
        redirect = urlencoding(OPENAI_REDIRECT_URI),
        client_id = OPENAI_CLIENT_ID,
    );
    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

pub(crate) fn openai_codex_refresh(refresh_token: &str) -> io::Result<ResponsesOAuthCredentials> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}",
        refresh_token = urlencoding(refresh_token),
        client_id = OPENAI_CLIENT_ID,
    );
    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

fn generate_client_secret() -> [u8; 32] {
    let mut bytes = [0; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

pub(crate) fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
    let now_ms = now_ms();
    if let Some(issued_at_ms) = jwt_issued_at_ms(access_token) {
        let lifetime_ms = expires_at_ms.saturating_sub(issued_at_ms);
        let refresh_at_ms = issued_at_ms.saturating_add(lifetime_ms / 2);
        if refresh_at_ms <= now_ms {
            return true;
        }
    }
    expires_at_ms <= now_ms.saturating_add(duration_millis_u64(REFRESH_EXPIRY_WINDOW))
}

fn parse_openai_token_response(json: &Value) -> io::Result<ResponsesOAuthCredentials> {
    let access_token = json["access_token"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing access_token"))?
        .to_owned();
    let refresh_token = json["refresh_token"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing refresh_token"))?
        .to_owned();
    let expires_in = json["expires_in"]
        .as_u64()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing expires_in"))?;
    let expires_at_ms = now_ms().saturating_add(expires_in.saturating_mul(1000));
    let account_id = openai_account_id_from_jwt(&access_token);

    Ok(ResponsesOAuthCredentials {
        access_token,
        refresh_token,
        expires_at_ms,
        account_id,
        client_secret: generate_client_secret(),
    })
}

fn post_form(url: &str, body: &str) -> io::Result<Value> {
    let resp = reqwest::blocking::Client::new()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(30))
        .body(body.to_owned())
        .send()
        .map_err(|error| io::Error::other(format!("{url}: {error}")))?;
    read_success_json(url, resp)
}

pub(crate) fn read_success_json(url: &str, resp: reqwest::blocking::Response) -> io::Result<Value> {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let text = resp
        .text()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !status.is_success() {
        let detail = format_error_body(&content_type, &text);
        let message = if detail.is_empty() {
            format!("{url}: HTTP {} (empty response body)", status.as_u16())
        } else {
            format!("{url}: HTTP {}: {detail}", status.as_u16())
        };
        return Err(io::Error::other(message));
    }

    serde_json::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn format_error_body(content_type: &str, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if content_type.contains("json")
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        let error = value.get("error").and_then(Value::as_str);
        let description = value
            .get("error_description")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str);
        match (error, description) {
            (Some(error), Some(description)) => {
                return format!("{error}: {description}");
            }
            (Some(error), None) => return error.to_owned(),
            (None, Some(description)) => return description.to_owned(),
            (None, None) => {}
        }
    }
    trimmed.to_owned()
}

fn atomic_write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no parent for atomic write path")
    })?;
    create_private_dir(dir)?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(tmp_path, path)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_auth_file_name(name: &str) -> io::Result<()> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "auth file name must be non-empty",
        ));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("auth file name '{name}' may not start with '.' or '-'"),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "auth file name '{name}' may only contain ASCII letters, digits, '_', '-', '.'"
            ),
        ));
    }
    Ok(())
}

fn openai_account_id_from_jwt(jwt: &str) -> Option<String> {
    jwt_claims(jwt)?
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account_id| !account_id.trim().is_empty())
        .map(str::to_owned)
}

fn jwt_issued_at_ms(jwt: &str) -> Option<u64> {
    jwt_claims(jwt)?
        .get("iat")?
        .as_u64()
        .map(|secs| secs.saturating_mul(1000))
}

fn jwt_claims(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..64)
        .map(|_| *CHARSET.choose(&mut rng).expect("non-empty charset") as char)
        .collect()
}

fn generate_state() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
