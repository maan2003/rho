use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
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
pub struct ResponsesAuth {
    kind: ResponsesAuthKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResponsesAuthKind {
    None,
    OAuthFile { path: PathBuf },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesOAuthCredentials {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAuth {
    pub bearer_token: String,
    pub account_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct OAuthFile {
    path: PathBuf,
}

impl ResponsesAuth {
    pub(crate) fn none() -> Self {
        Self {
            kind: ResponsesAuthKind::None,
        }
    }

    pub fn oauth_file(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: ResponsesAuthKind::OAuthFile { path: path.into() },
        }
    }

    pub fn oauth_file_named(name: impl AsRef<str>) -> io::Result<Self> {
        Ok(Self::oauth_file(OAuthFile::open_default(name)?.path()))
    }

    pub fn oauth_file_path(&self) -> Option<&Path> {
        match &self.kind {
            ResponsesAuthKind::OAuthFile { path } => Some(path),
            ResponsesAuthKind::None => None,
        }
    }

    pub fn resolve(&self) -> io::Result<Option<ResolvedAuth>> {
        self.resolve_with_refresh(openai_codex_refresh)
    }

    pub fn resolve_with_refresh(
        &self,
        refresh: impl FnMut(&str) -> io::Result<ResponsesOAuthCredentials>,
    ) -> io::Result<Option<ResolvedAuth>> {
        match &self.kind {
            ResponsesAuthKind::None => Ok(None),
            ResponsesAuthKind::OAuthFile { path } => {
                OAuthFile::new(path).resolve_with_refresh(refresh)
            }
        }
    }
}

impl ResponsesOAuthCredentials {
    pub fn from_access_token(access_token: impl Into<String>) -> Self {
        let access_token = access_token.into();
        let account_id = openai_account_id_from_jwt(&access_token);
        Self {
            access_token,
            account_id,
            ..Default::default()
        }
    }

    pub fn resolved(&self) -> Option<ResolvedAuth> {
        if self.access_token.trim().is_empty() {
            return None;
        }
        Some(ResolvedAuth {
            bearer_token: self.access_token.clone(),
            account_id: self
                .account_id
                .as_ref()
                .filter(|account_id| !account_id.trim().is_empty())
                .cloned(),
        })
    }
}

impl OAuthFile {
    pub fn default_auth_dir() -> io::Result<PathBuf> {
        let state_dir = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "cannot determine state directory")
            })?
            .join("rho");
        Ok(state_dir.join("auth.d"))
    }

    pub fn open_default(name: impl AsRef<str>) -> io::Result<Self> {
        Self::open_at(Self::default_auth_dir()?, name)
    }

    pub fn open_in(state_dir: impl Into<PathBuf>, name: impl AsRef<str>) -> io::Result<Self> {
        Self::open_at(state_dir.into().join("auth.d"), name)
    }

    pub fn open_at(auth_dir: impl Into<PathBuf>, name: impl AsRef<str>) -> io::Result<Self> {
        let name = name.as_ref();
        validate_auth_file_name(name)?;
        Ok(Self::new(auth_dir.into().join(format!("{name}.json"))))
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    pub fn load(&self) -> io::Result<Option<ResponsesOAuthCredentials>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, credentials: &ResponsesOAuthCredentials) -> io::Result<()> {
        self.with_lock(|locked| locked.save(credentials))
    }

    pub fn delete(&self) -> io::Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn resolve_with_refresh(
        &self,
        refresh: impl FnMut(&str) -> io::Result<ResponsesOAuthCredentials>,
    ) -> io::Result<Option<ResolvedAuth>> {
        self.with_lock(|locked| locked.resolve_with_refresh(refresh))
    }

    fn with_lock<R>(&self, f: impl FnOnce(&LockedOAuthFile<'_>) -> io::Result<R>) -> io::Result<R> {
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
        let locked = LockedOAuthFile {
            oauth_file: self,
            lock_file,
        };
        let result = f(&locked);
        let unlock_result = locked.lock_file.unlock();
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

struct LockedOAuthFile<'a> {
    oauth_file: &'a OAuthFile,
    lock_file: File,
}

impl LockedOAuthFile<'_> {
    fn load(&self) -> io::Result<Option<ResponsesOAuthCredentials>> {
        self.oauth_file.load()
    }

    fn save(&self, credentials: &ResponsesOAuthCredentials) -> io::Result<()> {
        let path = &self.oauth_file.path;
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no parent for OAuth auth path")
        })?;
        create_private_dir(dir)?;
        let json = serde_json::to_string_pretty(credentials)?;
        atomic_write_private(path, json.as_bytes())
    }

    fn resolve_with_refresh(
        &self,
        mut refresh: impl FnMut(&str) -> io::Result<ResponsesOAuthCredentials>,
    ) -> io::Result<Option<ResolvedAuth>> {
        let Some(current) = self.load()? else {
            return Ok(None);
        };
        if !oauth_token_should_refresh(&current.access_token, current.expires_at_ms)
            || current.refresh_token.trim().is_empty()
        {
            return Ok(current.resolved());
        }

        let mut refreshed = refresh(&current.refresh_token)?;
        if refreshed.account_id.is_none() {
            refreshed.account_id = current.account_id;
        }
        self.save(&refreshed)?;
        Ok(refreshed.resolved())
    }
}

pub fn openai_codex_auth_url() -> (String, String, String) {
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

pub fn parse_redirect_url(input: &str) -> Result<(String, String), String> {
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

pub fn openai_codex_exchange(code: &str, verifier: &str) -> io::Result<ResponsesOAuthCredentials> {
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

pub fn openai_codex_refresh(refresh_token: &str) -> io::Result<ResponsesOAuthCredentials> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}",
        refresh_token = urlencoding(refresh_token),
        client_id = OPENAI_CLIENT_ID,
    );
    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

pub fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
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
    })
}

fn post_form(url: &str, body: &str) -> io::Result<Value> {
    let resp = ureq::post(url)
        .content_type("application/x-www-form-urlencoded")
        .send(body)
        .map_err(|error| io::Error::other(format!("{url}: {error}")))?;
    read_success_json(url, resp)
}

fn read_success_json(url: &str, mut resp: ureq::http::Response<ureq::Body>) -> io::Result<Value> {
    let status = resp.status();
    if !status.is_success() {
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let detail = format_error_body(&content_type, &body);
        let message = if detail.is_empty() {
            format!("{url}: HTTP {} (empty response body)", status.as_u16())
        } else {
            format!("{url}: HTTP {}: {detail}", status.as_u16())
        };
        return Err(io::Error::other(message));
    }

    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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

#[cfg(test)]
mod tests;
