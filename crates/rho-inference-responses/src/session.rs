use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::oauth::{
    OAuthFile, ResponsesAuth, oauth_token_should_refresh, openai_codex_auth_url,
    openai_codex_exchange, parse_redirect_url,
};
use crate::ws::WebSocketPool;
use crate::{DEFAULT_CHATGPT_BASE_URL, DEFAULT_MODEL, ResponsesCompaction};

#[derive(Clone)]
pub struct InferenceService {
    pub(super) websocket_pool: Arc<Mutex<WebSocketPool>>,
    pub(crate) base_url: String,
    pub(crate) auth: ResponsesAuth,
    pub(crate) compaction: Option<ResponsesCompaction>,
    pub(crate) model: String,
    pub(crate) prompt_cache_key: Option<String>,
}

impl InferenceService {
    pub const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            websocket_pool: Arc::new(Mutex::new(WebSocketPool::new())),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth: ResponsesAuth::none(),
            compaction: None,
            model: model.into(),
            prompt_cache_key: None,
        }
    }

    pub(crate) fn chatgpt_codex_with_auth(model: impl Into<String>, auth: ResponsesAuth) -> Self {
        let mut session = Self::new(model);
        session.auth = auth;
        session
    }

    pub fn chatgpt_codex_with_auth_file(
        model: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self::chatgpt_codex_with_auth(model, ResponsesAuth::oauth_file(path))
    }

    pub fn chatgpt_codex_with_auth_file_named(
        model: impl Into<String>,
        name: impl AsRef<str>,
    ) -> io::Result<Self> {
        Ok(Self::chatgpt_codex_with_auth(
            model,
            ResponsesAuth::oauth_file_named(name)?,
        ))
    }

    pub fn chatgpt_codex_auth_file_path(name: impl AsRef<str>) -> io::Result<PathBuf> {
        Ok(OAuthFile::open_default(name)?.path())
    }

    pub fn chatgpt_codex_auth_login_url() -> (String, String, String) {
        openai_codex_auth_url()
    }

    pub fn chatgpt_codex_exchange_redirect_url(
        redirect_url: &str,
        expected_state: &str,
        verifier: &str,
    ) -> io::Result<String> {
        let (code, state) =
            parse_redirect_url(redirect_url).map_err(|error| io::Error::other(error))?;
        if state != expected_state {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state mismatch; restart login and use the newest URL",
            ));
        }
        let credentials = openai_codex_exchange(&code, verifier)?;
        serde_json::to_string_pretty(&credentials).map_err(io::Error::other)
    }

    pub fn chatgpt_codex_auth_save_json(
        name: impl AsRef<str>,
        credentials_json: &str,
    ) -> io::Result<String> {
        let credentials = serde_json::from_str(credentials_json)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let file = OAuthFile::open_default(name)?;
        file.save(&credentials)?;
        Ok(chatgpt_codex_auth_status_line_for(
            &file.path(),
            Some(&credentials),
        ))
    }

    pub fn chatgpt_codex_auth_status_line(name: impl AsRef<str>) -> io::Result<String> {
        let file = OAuthFile::open_default(name)?;
        Ok(chatgpt_codex_auth_status_line_for(
            &file.path(),
            file.load()?.as_ref(),
        ))
    }

    pub fn chatgpt_codex_auth_delete(name: impl AsRef<str>) -> io::Result<(PathBuf, bool)> {
        let file = OAuthFile::open_default(name)?;
        let path = file.path();
        let deleted = file.delete()?;
        Ok((path, deleted))
    }

    pub fn chatgpt_codex_auth_list() -> io::Result<Vec<(String, &'static str)>> {
        let auth_dir = OAuthFile::default_auth_dir()?;
        let entries = match std::fs::read_dir(&auth_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        let mut providers = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let file = OAuthFile::open_at(&auth_dir, name)?;
            providers.push((
                name.to_owned(),
                chatgpt_codex_auth_status_label(file.load()?.as_ref()),
            ));
        }
        providers.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(providers)
    }

    pub fn with_compaction(mut self) -> Self {
        self.compaction = Some(ResponsesCompaction::default());
        self
    }

    pub fn with_compaction_threshold(mut self, compact_threshold: u64) -> Self {
        self.compaction = Some(ResponsesCompaction {
            compact_threshold: Some(compact_threshold),
        });
        self
    }

    pub fn with_prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }

    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }
}

fn chatgpt_codex_auth_status_label(
    credentials: Option<&crate::oauth::ResponsesOAuthCredentials>,
) -> &'static str {
    let Some(credentials) = credentials else {
        return "missing";
    };
    if credentials.access_token.trim().is_empty() {
        "invalid"
    } else if oauth_token_should_refresh(&credentials.access_token, credentials.expires_at_ms) {
        "refresh-due"
    } else {
        "logged-in"
    }
}

fn chatgpt_codex_auth_status_line_for(
    path: &Path,
    credentials: Option<&crate::oauth::ResponsesOAuthCredentials>,
) -> String {
    let Some(credentials) = credentials else {
        return format!("missing path={}", path.display());
    };
    let status = if credentials.access_token.trim().is_empty() {
        "invalid"
    } else if oauth_token_should_refresh(&credentials.access_token, credentials.expires_at_ms) {
        "refresh_due"
    } else {
        "fresh"
    };
    let account = credentials.account_id.as_deref().unwrap_or("unknown");
    let refresh = if credentials.refresh_token.trim().is_empty() {
        "no"
    } else {
        "yes"
    };
    format!(
        "present path={} status={} account={} refresh_token={} expires_at_ms={}",
        path.display(),
        status,
        account,
        refresh,
        credentials.expires_at_ms
    )
}

impl std::fmt::Debug for InferenceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceService")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("compaction", &self.compaction)
            .field("model", &self.model)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
