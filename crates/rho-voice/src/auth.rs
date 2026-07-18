//! OAuth credentials for xAI Grok realtime voice.
//!
//! Voice intentionally couples to the Grok CLI login for now: first use copies
//! the access/refresh token from `~/.grok/auth.json` into rho's own auth file,
//! then rho refreshes that token itself. There is no API-key fallback.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const XAI_OAUTH_ISSUER: &str = "https://auth.x.ai";
const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_CLI_AUTH_KEY: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";
const LEGACY_GROK_CLI_AUTH_KEY: &str = "https://accounts.x.ai/sign-in";
pub(crate) const DEFAULT_AUTH_NAME: &str = "grok-voice";
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);
const REFRESH_EXPIRY_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceAuth {
    kind: VoiceAuthKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum VoiceAuthKind {
    OAuthFile(OAuthFile),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VoiceOAuthCredentials {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) refresh_token: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) grok_client_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedAuth {
    pub(crate) bearer_token: String,
    pub(crate) grok_client_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OAuthFile {
    path: PathBuf,
}

impl VoiceAuth {
    /// OAuth auth backed by rho's default local copy of the Grok CLI login.
    pub fn grok_cli() -> io::Result<Self> {
        Ok(Self::oauth_file(
            OAuthFile::open_default(DEFAULT_AUTH_NAME)?.path(),
        ))
    }

    pub(crate) fn oauth_file(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: VoiceAuthKind::OAuthFile(OAuthFile::new(path)),
        }
    }

    pub(crate) fn resolve(&self) -> io::Result<ResolvedAuth> {
        self.resolve_with_refresh_and_import(xai_oauth_refresh, import_grok_cli_credentials)
    }

    pub(crate) fn resolve_with_refresh_and_import(
        &self,
        refresh: impl FnMut(&str) -> io::Result<VoiceOAuthCredentials>,
        import: impl FnMut() -> io::Result<VoiceOAuthCredentials>,
    ) -> io::Result<ResolvedAuth> {
        match &self.kind {
            VoiceAuthKind::OAuthFile(file) => file.resolve_with_refresh_and_import(refresh, import),
        }
    }
}

impl VoiceOAuthCredentials {
    pub(crate) fn resolved(&self) -> io::Result<ResolvedAuth> {
        if self.access_token.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Grok OAuth credentials are missing an access token",
            ));
        }
        Ok(ResolvedAuth {
            bearer_token: self.access_token.clone(),
            grok_client_version: self
                .grok_client_version
                .as_ref()
                .filter(|version| !version.trim().is_empty())
                .cloned(),
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

    pub(crate) fn load(&self) -> io::Result<Option<VoiceOAuthCredentials>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save(&self, credentials: &VoiceOAuthCredentials) -> io::Result<()> {
        self.with_lock(|| self.write(credentials))
    }

    pub(crate) fn delete(&self) -> io::Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn resolve_with_refresh_and_import(
        &self,
        mut refresh: impl FnMut(&str) -> io::Result<VoiceOAuthCredentials>,
        mut import: impl FnMut() -> io::Result<VoiceOAuthCredentials>,
    ) -> io::Result<ResolvedAuth> {
        self.with_lock(|| {
            let current = match self.load()? {
                Some(credentials) => credentials,
                None => {
                    let imported = import()?;
                    self.write(&imported)?;
                    imported
                }
            };
            if !oauth_token_should_refresh(&current.access_token, current.expires_at_ms)
                || current.refresh_token.trim().is_empty()
            {
                return current.resolved();
            }

            let mut refreshed = refresh(&current.refresh_token)?;
            if refreshed.refresh_token.trim().is_empty() {
                refreshed.refresh_token = current.refresh_token;
            }
            if refreshed.grok_client_version.is_none() {
                refreshed.grok_client_version = current.grok_client_version;
            }
            self.write(&refreshed)?;
            refreshed.resolved()
        })
    }

    fn write(&self, credentials: &VoiceOAuthCredentials) -> io::Result<()> {
        let dir = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no parent for OAuth auth path")
        })?;
        create_private_dir(dir)?;
        let json = serde_json::to_string_pretty(credentials)?;
        atomic_write_private(&self.path, json.as_bytes())
    }

    /// Runs `f` while holding an exclusive cross-process lock on the auth file,
    /// so concurrent imports, refreshes, and writes stay serialized.
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

pub(crate) fn import_grok_cli_credentials() -> io::Result<VoiceOAuthCredentials> {
    let auth_path = grok_home_file("auth.json")?;
    let text = fs::read_to_string(&auth_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Grok CLI OAuth credentials not found at {}; run `grok login` first",
                    auth_path.display()
                ),
            )
        } else {
            error
        }
    })?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let entry = json
        .get(GROK_CLI_AUTH_KEY)
        .or_else(|| json.get(LEGACY_GROK_CLI_AUTH_KEY))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Grok CLI auth file has no xAI OAuth entry for {XAI_OAUTH_ISSUER}; run `grok login --oauth`"
                ),
            )
        })?;
    let access_token = entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Grok CLI auth entry is missing an access token; run `grok login`",
            )
        })?
        .to_owned();
    let refresh_token = entry
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expires_at_ms = jwt_expires_at_ms(&access_token)
        .unwrap_or_else(|| now_ms().saturating_add(duration_millis_u64(DEFAULT_TOKEN_LIFETIME)));

    Ok(VoiceOAuthCredentials {
        access_token,
        refresh_token,
        expires_at_ms,
        grok_client_version: read_grok_client_version().ok().flatten(),
    })
}

fn read_grok_client_version() -> io::Result<Option<String>> {
    let version_path = grok_home_file("version.json")?;
    let text = match fs::read_to_string(version_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let json: Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(json
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| !version.trim().is_empty())
        .map(str::to_owned))
}

fn grok_home_file(file_name: &str) -> io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory")
    })?;
    Ok(home.join(".grok").join(file_name))
}

fn xai_oauth_refresh(refresh_token: &str) -> io::Result<VoiceOAuthCredentials> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}",
        refresh_token = urlencoding(refresh_token),
        client_id = XAI_CLIENT_ID,
    );
    let json = post_form(XAI_TOKEN_URL, &body)?;
    parse_xai_token_response(&json)
}

fn parse_xai_token_response(json: &Value) -> io::Result<VoiceOAuthCredentials> {
    let access_token = json["access_token"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing access_token"))?
        .to_owned();
    let refresh_token = json["refresh_token"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let expires_in = json["expires_in"]
        .as_u64()
        .unwrap_or(DEFAULT_TOKEN_LIFETIME.as_secs());
    let expires_at_ms = jwt_expires_at_ms(&access_token)
        .unwrap_or_else(|| now_ms().saturating_add(expires_in.saturating_mul(1000)));

    Ok(VoiceOAuthCredentials {
        access_token,
        refresh_token,
        expires_at_ms,
        grok_client_version: None,
    })
}

pub(crate) fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
    if access_token.trim().is_empty() {
        return true;
    }
    let now_ms = now_ms();
    let expires_at_ms = jwt_expires_at_ms(access_token).unwrap_or(expires_at_ms);
    if let Some(issued_at_ms) = jwt_issued_at_ms(access_token) {
        let lifetime_ms = expires_at_ms.saturating_sub(issued_at_ms);
        let refresh_at_ms = issued_at_ms.saturating_add(lifetime_ms / 2);
        if refresh_at_ms <= now_ms {
            return true;
        }
    }
    expires_at_ms <= now_ms.saturating_add(duration_millis_u64(REFRESH_EXPIRY_WINDOW))
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

fn read_success_json(url: &str, resp: reqwest::blocking::Response) -> io::Result<Value> {
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

fn jwt_issued_at_ms(jwt: &str) -> Option<u64> {
    jwt_claims(jwt)?
        .get("iat")?
        .as_u64()
        .map(|secs| secs.saturating_mul(1000))
}

fn jwt_expires_at_ms(jwt: &str) -> Option<u64> {
    jwt_claims(jwt)?
        .get("exp")?
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

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn jwt_with_claims(claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode("{}");
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn oauth_file_imports_grok_cli_credentials_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let file = OAuthFile::open_at(temp.path(), "grok-voice").unwrap();
        let token = jwt_with_claims(json!({
            "iat": now_ms() / 1000,
            "exp": now_ms() / 1000 + 3600,
        }));
        let auth = VoiceAuth::oauth_file(file.path());

        let resolved = auth
            .resolve_with_refresh_and_import(
                |_| panic!("fresh imported token should not refresh"),
                || {
                    Ok(VoiceOAuthCredentials {
                        access_token: token.clone(),
                        refresh_token: "refresh".to_owned(),
                        expires_at_ms: jwt_expires_at_ms(&token).unwrap(),
                        grok_client_version: Some("0.2.86".to_owned()),
                    })
                },
            )
            .unwrap();

        assert_eq!(resolved.bearer_token, token);
        assert_eq!(resolved.grok_client_version.as_deref(), Some("0.2.86"));
        assert_eq!(file.load().unwrap().unwrap().refresh_token, "refresh");
    }

    #[test]
    fn oauth_file_refreshes_expired_credentials_and_persists_them() {
        let temp = tempfile::tempdir().unwrap();
        let file = OAuthFile::open_at(temp.path(), "grok-voice").unwrap();
        file.save(&VoiceOAuthCredentials {
            access_token: "old".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at_ms: 1,
            grok_client_version: Some("0.2.86".to_owned()),
        })
        .unwrap();
        let auth = VoiceAuth::oauth_file(file.path());

        let resolved = auth
            .resolve_with_refresh_and_import(
                |refresh_token| {
                    assert_eq!(refresh_token, "refresh");
                    Ok(VoiceOAuthCredentials {
                        access_token: "new".to_owned(),
                        refresh_token: "new-refresh".to_owned(),
                        expires_at_ms: u64::MAX,
                        grok_client_version: None,
                    })
                },
                || panic!("credentials already exist"),
            )
            .unwrap();

        assert_eq!(resolved.bearer_token, "new");
        assert_eq!(resolved.grok_client_version.as_deref(), Some("0.2.86"));
        assert_eq!(file.load().unwrap().unwrap().access_token, "new");
    }

    #[test]
    fn refresh_policy_uses_jwt_expiry_over_stale_file_expiry() {
        let token = jwt_with_claims(json!({
            "iat": now_ms() / 1000,
            "exp": now_ms() / 1000 + 3600,
        }));

        assert!(!oauth_token_should_refresh(&token, 1));
    }

    #[test]
    fn refresh_policy_uses_jwt_half_life() {
        let token = jwt_with_claims(json!({
            "iat": now_ms().saturating_sub(duration_millis_u64(Duration::from_secs(120))) / 1000,
            "exp": now_ms().saturating_add(duration_millis_u64(Duration::from_secs(120))) / 1000,
        }));

        assert!(oauth_token_should_refresh(&token, u64::MAX));
    }

    #[test]
    fn token_response_keeps_refresh_token_optional() {
        let token = jwt_with_claims(json!({"exp": now_ms() / 1000 + 3600}));
        let parsed = parse_xai_token_response(&json!({
            "access_token": token,
            "expires_in": 3600,
        }))
        .unwrap();

        assert_eq!(parsed.refresh_token, "");
        assert!(parsed.expires_at_ms > now_ms());
    }

    #[test]
    fn oauth_file_rejects_unsafe_names() {
        for name in ["", ".hidden", "-leading", "has/slash", "has space"] {
            assert!(OAuthFile::open_at("/tmp", name).is_err());
        }
    }
}
