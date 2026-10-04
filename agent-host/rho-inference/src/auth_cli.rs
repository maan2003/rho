use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::responses::DEFAULT_CHATGPT_BASE_URL;
use crate::responses::oauth::{
    InferenceAuth, OAuthFile, ResponsesOAuthCredentials, oauth_token_should_refresh,
    openai_codex_auth_url, openai_codex_exchange, parse_redirect_url, read_success_json,
};

const DEFAULT_AUTH_NAME: &str = "default";
const USER_AGENT: &str = "rho-cli";

#[derive(Clone, Subcommand)]
pub enum AuthArgs {
    Add,
    #[command(alias = "ls")]
    List,
    #[command(alias = "delete")]
    Remove {
        #[arg(default_value = DEFAULT_AUTH_NAME)]
        name: String,
    },
    Path {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
    },
    Status {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
    },
    RateLimits {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
    },
    /// List earned usage-reset credits, including grant and expiry timestamps.
    Resets {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
    },
    /// Redeem one usage-reset credit. This changes the selected account's
    /// allowance.
    Reset {
        /// Select the account explicitly; there is no default for redemption.
        #[arg(long)]
        name: String,
        /// Omit to let ChatGPT select the next available credit.
        #[arg(long)]
        credit_id: Option<String>,
        /// Reuse this key when retrying the same reset after an uncertain
        /// result.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Import {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
        #[arg(long = "file")]
        path: Option<PathBuf>,
    },
}

pub fn run_auth_cli(command: AuthArgs) -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    match command {
        AuthArgs::Add => {
            let name = prompt_with_default("Auth namespace", DEFAULT_AUTH_NAME)?;
            let credentials_json = login_openai_codex()?;
            println!("{}", save_json(name.trim(), &credentials_json)?);
            Ok(())
        }
        AuthArgs::List => list(),
        AuthArgs::Remove { name } => {
            let (path, deleted) = delete(name.trim())?;
            if deleted {
                println!("removed {}", path.display());
            } else {
                println!("missing {}", path.display());
            }
            Ok(())
        }
        AuthArgs::Path { name } => {
            println!("{}", file_path(name)?.display());
            Ok(())
        }
        AuthArgs::Status { name } => {
            println!("{}", status_line(name)?);
            Ok(())
        }
        AuthArgs::RateLimits { name } => print_rate_limits(name.trim()),
        AuthArgs::Resets { name } => print_reset_credits(name.trim()),
        AuthArgs::Reset {
            name,
            credit_id,
            idempotency_key,
        } => redeem_reset_credit(
            name.trim(),
            credit_id.as_deref(),
            idempotency_key.as_deref(),
        ),
        AuthArgs::Import { name, path } => {
            let credentials_json = read_credentials_json(path)?;
            println!("{}", save_json(name, &credentials_json)?);
            Ok(())
        }
    }
}

fn file_path(name: impl AsRef<str>) -> io::Result<PathBuf> {
    Ok(OAuthFile::open_default(name)?.path())
}

fn login_openai_codex() -> Result<String> {
    let (auth_url, expected_state, verifier) = openai_codex_auth_url();

    eprintln!();
    eprintln!("Open this URL in your browser:");
    eprintln!();
    eprintln!("{auth_url}");
    eprintln!("\x1b]8;;{auth_url}\x1b\\Or click here.\x1b]8;;\x1b\\");
    eprintln!();
    eprintln!("After logging in, copy the full redirect URL from the browser address bar.");
    eprint!("Redirect URL: ");
    io::stderr().flush()?;

    let mut redirect_input = String::new();
    io::stdin().read_line(&mut redirect_input)?;
    eprintln!("Exchanging code for tokens...");

    let (code, state) = parse_redirect_url(&redirect_input).map_err(io::Error::other)?;
    if state != expected_state {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state mismatch; restart login and use the newest URL",
        )
        .into());
    }
    let credentials = openai_codex_exchange(&code, &verifier).context("exchanging OAuth code")?;
    serde_json::to_string_pretty(&credentials).map_err(Into::into)
}

fn prompt_with_default(prompt: &str, default: &str) -> Result<String> {
    eprint!("{prompt} [{default}]: ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

fn read_credentials_json(path: Option<PathBuf>) -> Result<String> {
    let text = match path {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading OAuth credentials from {}", path.display()))?,
        None => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            text
        }
    };
    serde_json::from_str::<serde_json::Value>(&text).context("parsing OAuth credentials JSON")?;
    Ok(text)
}

fn save_json(name: impl AsRef<str>, credentials_json: &str) -> io::Result<String> {
    let credentials = serde_json::from_str(credentials_json)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let file = OAuthFile::open_default(name)?;
    file.save(&credentials)?;
    Ok(status_line_for(&file.path(), Some(&credentials)))
}

fn status_line(name: impl AsRef<str>) -> io::Result<String> {
    let file = OAuthFile::open_default(name)?;
    Ok(status_line_for(&file.path(), file.load()?.as_ref()))
}

fn delete(name: impl AsRef<str>) -> io::Result<(PathBuf, bool)> {
    let file = OAuthFile::open_default(name)?;
    let path = file.path();
    let deleted = file.delete()?;
    Ok((path, deleted))
}

fn list() -> Result<()> {
    let credentials = list_credentials().context("reading auth credentials directory")?;
    if credentials.is_empty() {
        println!("No auth credentials configured.");
        return Ok(());
    }
    for (name, status) in credentials {
        println!("{name}\tchatgpt\t{status}");
    }
    Ok(())
}

fn list_credentials() -> io::Result<Vec<(String, &'static str)>> {
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
        providers.push((name.to_owned(), status_label(file.load()?.as_ref())));
    }
    providers.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(providers)
}

/// Names of the OAuth credential namespaces available on this machine.
pub(crate) fn auth_namespaces() -> io::Result<Vec<String>> {
    let auth_dir = OAuthFile::default_auth_dir()?;
    let entries = match std::fs::read_dir(&auth_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut namespaces = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                return None;
            }
            path.file_stem()?.to_str().map(str::to_owned)
        })
        .collect::<Vec<_>>();
    namespaces.sort();
    namespaces.dedup();
    Ok(namespaces)
}

#[derive(Clone, Copy)]
enum AuthStatus {
    Missing,
    Invalid,
    RefreshDue,
    Fresh,
}

fn auth_status(credentials: Option<&ResponsesOAuthCredentials>) -> AuthStatus {
    let Some(credentials) = credentials else {
        return AuthStatus::Missing;
    };
    if credentials.access_token.trim().is_empty() {
        AuthStatus::Invalid
    } else if oauth_token_should_refresh(&credentials.access_token, credentials.expires_at_ms) {
        AuthStatus::RefreshDue
    } else {
        AuthStatus::Fresh
    }
}

fn status_label(credentials: Option<&ResponsesOAuthCredentials>) -> &'static str {
    match auth_status(credentials) {
        AuthStatus::Missing => "missing",
        AuthStatus::Invalid => "invalid",
        AuthStatus::RefreshDue => "refresh-due",
        AuthStatus::Fresh => "logged-in",
    }
}

fn status_line_for(path: &Path, credentials: Option<&ResponsesOAuthCredentials>) -> String {
    let Some(credentials) = credentials else {
        return format!("missing path={}", path.display());
    };
    let status = match auth_status(Some(credentials)) {
        AuthStatus::Missing => unreachable!("credentials are present"),
        AuthStatus::Invalid => "invalid",
        AuthStatus::RefreshDue => "refresh_due",
        AuthStatus::Fresh => "fresh",
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

fn print_rate_limits(name: impl AsRef<str>) -> Result<()> {
    let auth = InferenceAuth::named(name)?;
    let resolved = auth.resolve().context("resolving OAuth credentials")?;
    let status = fetch_rate_limit_status(&resolved.bearer_token, resolved.account_id.as_deref())
        .context("fetching ChatGPT rate limits")?;

    let account = resolved.account_id.as_deref().unwrap_or("unknown");
    let available = status
        .rate_limit_reset_credits
        .as_ref()
        .map(|credits| credits.available_count.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("account={account} rate_limit_reset_credits_available={available}");

    let primary = status.rate_limit.as_ref().map(|rate_limit| {
        (
            "codex",
            None,
            rate_limit.primary_window.as_ref(),
            rate_limit.secondary_window.as_ref(),
        )
    });
    for (limit_id, limit_name, primary, secondary) in
        primary
            .into_iter()
            .chain(
                status
                    .additional_rate_limits
                    .iter()
                    .flatten()
                    .filter_map(|limit| {
                        let rate_limit = limit.rate_limit.as_ref()?;
                        Some((
                            limit.metered_feature.as_deref().unwrap_or("unknown"),
                            limit.limit_name.as_deref(),
                            rate_limit.primary_window.as_ref(),
                            rate_limit.secondary_window.as_ref(),
                        ))
                    }),
            )
    {
        print_window(limit_id, limit_name, "primary", primary);
        print_window(limit_id, limit_name, "secondary", secondary);
    }
    Ok(())
}

/// The account-wide weekly Codex allowance reported by ChatGPT.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChatGptUsage {
    pub account_id: Option<String>,
    /// Weekly usage retained for quota history and UI summaries.
    pub used_percent: f64,
    pub reset_at_unix: i64,
    /// Usage of the currently most constrained reported window. The account
    /// router consumes this from the inference runtime's poll.
    pub routing_used_percent: f64,
    pub routing_reset_at_unix: i64,
}

/// Fetches the weekly window for an OAuth namespace. Accounts which do not
/// report a weekly window return `None`.
pub(crate) fn chatgpt_weekly_usage(name: impl AsRef<str>) -> Result<Option<ChatGptUsage>> {
    let auth = InferenceAuth::named(name)?;
    chatgpt_weekly_usage_for_auth(auth)
}

/// Fetches the weekly window for an already selected OAuth namespace.
fn chatgpt_weekly_usage_for_auth(auth: InferenceAuth) -> Result<Option<ChatGptUsage>> {
    let resolved = auth.resolve().context("resolving OAuth credentials")?;
    let status = fetch_rate_limit_status(&resolved.bearer_token, resolved.account_id.as_deref())
        .context("fetching ChatGPT rate limits")?;
    let Some(rate_limit) = status.rate_limit.as_ref() else {
        return Ok(None);
    };
    let Some(weekly) = weekly_window(rate_limit) else {
        return Ok(None);
    };
    let Some(reset_at_unix) = window_reset_at(weekly) else {
        return Ok(None);
    };
    let routing = routing_window(rate_limit).unwrap_or(weekly);
    let routing_reset_at_unix = window_reset_at(routing).unwrap();
    Ok(weekly.used_percent.is_finite().then_some(ChatGptUsage {
        account_id: resolved.account_id,
        used_percent: weekly.used_percent,
        reset_at_unix,
        routing_used_percent: routing.used_percent,
        routing_reset_at_unix,
    }))
}

fn window_reset_at(window: &RateLimitWindow) -> Option<i64> {
    window.reset_at.or_else(|| {
        window
            .reset_after_seconds
            .map(|seconds| now_secs().saturating_add(seconds))
    })
}

fn routing_window(rate_limit: &RateLimitDetails) -> Option<&RateLimitWindow> {
    rate_limit
        .primary_window
        .iter()
        .chain(rate_limit.secondary_window.iter())
        .filter(|window| window.used_percent.is_finite() && window_reset_at(window).is_some())
        .max_by(|left, right| left.used_percent.total_cmp(&right.used_percent))
}

fn weekly_window(rate_limit: &RateLimitDetails) -> Option<&RateLimitWindow> {
    rate_limit
        .primary_window
        .iter()
        .chain(rate_limit.secondary_window.iter())
        .find(|window| {
            window
                .limit_window_seconds
                .is_some_and(|seconds| seconds.abs_diff(7 * 24 * 60 * 60) <= 7 * 24 * 60 * 3)
        })
}

fn fetch_rate_limit_status(
    bearer_token: &str,
    account_id: Option<&str>,
) -> io::Result<RateLimitStatus> {
    let request = chatgpt_request(
        DEFAULT_CHATGPT_BASE_URL,
        bearer_token,
        account_id,
        reqwest::Method::GET,
        "usage",
    );
    read_chatgpt_response(request)
}

// Match Codex's backend-client contract: details are separate from /usage,
// and redemption sends redeem_request_id (not an Idempotency-Key header).
fn chatgpt_request(
    base_url: &str,
    bearer_token: &str,
    account_id: Option<&str>,
    method: reqwest::Method,
    endpoint: &str,
) -> reqwest::blocking::RequestBuilder {
    let url = format!("{base_url}/wham/{endpoint}");
    let mut request = reqwest::blocking::Client::new()
        .request(method, url)
        .bearer_auth(bearer_token)
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(30));
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    request
}

fn read_chatgpt_response<T: serde::de::DeserializeOwned>(
    request: reqwest::blocking::RequestBuilder,
) -> io::Result<T> {
    let response = request.send().map_err(io::Error::other)?;
    let url = response.url().to_string();
    let json = read_success_json(&url, response)?;
    serde_json::from_value(json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn print_reset_credits(name: &str) -> Result<()> {
    let resolved = InferenceAuth::named(name)?
        .resolve()
        .context("resolving OAuth credentials")?;
    let details: ResetCreditsDetails = read_chatgpt_response(chatgpt_request(
        DEFAULT_CHATGPT_BASE_URL,
        &resolved.bearer_token,
        resolved.account_id.as_deref(),
        reqwest::Method::GET,
        "rate-limit-reset-credits",
    ))
    .context("fetching ChatGPT reset credit details")?;
    write_reset_credits(
        &mut io::stdout().lock(),
        name,
        resolved.account_id.as_deref(),
        &details,
    )
}

fn write_reset_credits(
    out: &mut impl Write,
    name: &str,
    account_id: Option<&str>,
    details: &ResetCreditsDetails,
) -> Result<()> {
    writeln!(
        out,
        "namespace={name} account={} rate_limit_reset_credits_available={} listed={}",
        account_id.unwrap_or("unknown"),
        details.available_count,
        details.credits.len()
    )?;
    // The backend may cap the detail list; its length is not the available
    // count.
    for credit in &details.credits {
        let granted: jiff::Timestamp = credit
            .granted_at
            .parse()
            .with_context(|| format!("invalid granted_at for credit {}", credit.id))?;
        let expires = credit
            .expires_at
            .as_deref()
            .map(str::parse::<jiff::Timestamp>)
            .transpose()
            .with_context(|| format!("invalid expires_at for credit {}", credit.id))?;
        writeln!(
            out,
            "credit_id={} type={} status={} granted_at={granted} granted_at_unix={} expires_at={} expires_at_unix={} title={:?} description={:?}",
            credit.id,
            credit.reset_type,
            credit.status,
            granted.as_second(),
            expires
                .map(|t| t.to_string())
                .unwrap_or_else(|| "never".into()),
            expires
                .map(|t| t.as_second().to_string())
                .unwrap_or_else(|| "none".into()),
            credit.title.as_deref().unwrap_or(""),
            credit.description.as_deref().unwrap_or(""),
        )?;
    }
    Ok(())
}

fn redeem_reset_credit(name: &str, credit_id: Option<&str>, key: Option<&str>) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "name must not be empty");
    anyhow::ensure!(
        credit_id.is_none_or(|id| !id.trim().is_empty()),
        "credit-id must not be empty"
    );
    anyhow::ensure!(
        key.is_none_or(|key| !key.trim().is_empty()),
        "idempotency-key must not be empty"
    );
    let resolved = InferenceAuth::named(name)?
        .resolve()
        .context("resolving OAuth credentials")?;
    let key = key
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // Print before sending: a timeout can still mean the backend consumed the
    // credit. Never automatically retry with a new key.
    eprintln!("namespace={name} idempotency_key={key}");
    eprintln!("If the result is uncertain, retry with --idempotency-key {key}");
    let response = consume_reset_credit(
        DEFAULT_CHATGPT_BASE_URL,
        &resolved.bearer_token,
        resolved.account_id.as_deref(),
        &key,
        credit_id,
    )
    .context("redeeming ChatGPT reset credit; retry only with the same idempotency key")?;
    println!(
        "namespace={name} outcome={} windows_reset={} idempotency_key={key}",
        response.code, response.windows_reset
    );
    Ok(())
}

fn consume_reset_credit(
    base_url: &str,
    bearer_token: &str,
    account_id: Option<&str>,
    key: &str,
    credit_id: Option<&str>,
) -> io::Result<ResetCreditResponse> {
    read_chatgpt_response(
        chatgpt_request(
            base_url,
            bearer_token,
            account_id,
            reqwest::Method::POST,
            "rate-limit-reset-credits/consume",
        )
        .json(&ResetCreditRequest {
            redeem_request_id: key,
            credit_id,
        }),
    )
}

#[derive(Debug, Deserialize)]
struct ResetCreditsDetails {
    available_count: i64,
    credits: Vec<ResetCredit>,
}

#[derive(Debug, Deserialize)]
struct ResetCredit {
    id: String,
    reset_type: String,
    status: String,
    granted_at: String,
    expires_at: Option<String>,
    title: Option<String>,
    description: Option<String>,
}

#[derive(serde::Serialize)]
struct ResetCreditRequest<'a> {
    redeem_request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credit_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ResetCreditResponse {
    code: String,
    #[serde(default)]
    windows_reset: i64,
}

fn print_window(
    limit_id: &str,
    limit_name: Option<&str>,
    kind: &str,
    window: Option<&RateLimitWindow>,
) {
    let Some(window) = window else {
        return;
    };
    let window_mins = window
        .limit_window_seconds
        .map(|seconds| seconds / 60)
        .map(|mins| mins.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let resets_at = window_reset_at(window);
    let resets_at_utc = resets_at
        .and_then(|timestamp| jiff::Timestamp::from_second(timestamp).ok())
        .map(|timestamp| timestamp.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let resets_at = resets_at
        .map(|timestamp| timestamp.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let resets_in = window
        .reset_after_seconds
        .map(|seconds| seconds.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let limit_name = limit_name.unwrap_or("-");
    println!(
        "limit={limit_id} name={limit_name} window={kind} used_percent={} window_mins={window_mins} resets_at={resets_at_utc} resets_at_unix={resets_at} resets_in_secs={resets_in}",
        window.used_percent
    );
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[derive(Debug, Deserialize)]
struct RateLimitStatus {
    #[serde(default)]
    rate_limit: Option<RateLimitDetails>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
    #[serde(default)]
    rate_limit_reset_credits: Option<RateLimitResetCredits>,
}

#[derive(Debug, Deserialize)]
struct RateLimitDetails {
    #[serde(default)]
    primary_window: Option<RateLimitWindow>,
    #[serde(default)]
    secondary_window: Option<RateLimitWindow>,
}

#[derive(Debug, Deserialize)]
struct RateLimitWindow {
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
    #[serde(default)]
    reset_after_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimit {
    #[serde(default)]
    limit_name: Option<String>,
    #[serde(default)]
    metered_feature: Option<String>,
    #[serde(default)]
    rate_limit: Option<RateLimitDetails>,
}

#[derive(Debug, Deserialize)]
struct RateLimitResetCredits {
    available_count: i64,
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn reset_credit_listing_preserves_precision_and_distinguishes_no_expiry() {
        let details: ResetCreditsDetails = serde_json::from_value(serde_json::json!({
            "available_count": 5,
            "credits": [
                {
                    "id": "expiring",
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "granted_at": "2026-06-17T05:30:01.123456+05:30",
                    "expires_at": "2026-07-17T02:00:03.654321-04:00",
                    "title": "Full reset",
                    "description": "Weekly + 5 hr"
                },
                {
                    "id": "no-expiry",
                    "reset_type": "future_type",
                    "status": "redeeming",
                    "granted_at": "2026-06-18T00:00:00Z",
                    "expires_at": null
                }
            ],
            "total_earned_count": 8
        }))
        .unwrap();
        let mut out = Vec::new();
        write_reset_credits(&mut out, "second", Some("account-2"), &details).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            concat!(
                "namespace=second account=account-2 rate_limit_reset_credits_available=5 listed=2\n",
                "credit_id=expiring type=codex_rate_limits status=available ",
                "granted_at=2026-06-17T00:00:01.123456Z granted_at_unix=1781654401 ",
                "expires_at=2026-07-17T06:00:03.654321Z expires_at_unix=1784268003 ",
                "title=\"Full reset\" description=\"Weekly + 5 hr\"\n",
                "credit_id=no-expiry type=future_type status=redeeming ",
                "granted_at=2026-06-18T00:00:00Z granted_at_unix=1781740800 ",
                "expires_at=never expires_at_unix=none title=\"\" description=\"\"\n"
            )
        );

        let invalid: ResetCreditsDetails = serde_json::from_value(serde_json::json!({
            "available_count": 1,
            "credits": [{
                "id": "invalid-expiry",
                "reset_type": "codex_rate_limits",
                "status": "available",
                "granted_at": "2026-06-18T00:00:00Z",
                "expires_at": "bad timestamp"
            }]
        }))
        .unwrap();
        let error = write_reset_credits(&mut Vec::new(), "default", None, &invalid).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid expires_at for credit invalid-expiry")
        );
    }

    #[test]
    fn redemption_uses_account_headers_selected_credit_and_retry_key() {
        use std::io::BufRead;

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Exercise selected and backend-chosen credits, every known outcome,
        // and backend failure without touching a real account.
        for (credit_id, status, response_json) in [
            (
                Some("credit-2"),
                200,
                r#"{"code":"reset","windows_reset":2}"#,
            ),
            (
                None,
                200,
                r#"{"code":"nothing_to_reset","windows_reset":0}"#,
            ),
            (None, 200, r#"{"code":"no_credit"}"#),
            (
                Some("credit-2"),
                200,
                r#"{"code":"already_redeemed","windows_reset":0}"#,
            ),
            (None, 500, r#"{"message":"backend failure"}"#),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}/backend-api", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = io::BufReader::new(socket);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    assert!(!line.is_empty(), "request ended before headers");
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                assert!(headers.starts_with(
                    "POST /backend-api/wham/rate-limit-reset-credits/consume HTTP/1.1\r\n"
                ));
                let headers = headers.to_ascii_lowercase();
                assert!(headers.contains("\r\nauthorization: bearer test-token\r\n"));
                assert!(headers.contains("\r\nchatgpt-account-id: account-2\r\n"));
                assert!(headers.contains("\r\nuser-agent: rho-cli\r\n"));
                assert!(headers.contains("\r\ncontent-type: application/json\r\n"));
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let expected = match credit_id {
                    Some(id) => serde_json::json!({
                        "redeem_request_id": "same-attempt-key", "credit_id": id
                    }),
                    None => serde_json::json!({"redeem_request_id": "same-attempt-key"}),
                };
                assert_eq!(body, expected);
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_json}",
                    response_json.len(),
                ).unwrap();
            });
            let result = consume_reset_credit(
                &base_url,
                "test-token",
                Some("account-2"),
                "same-attempt-key",
                credit_id,
            );
            server.join().unwrap();
            if status == 200 {
                let response = result.unwrap();
                let expected: serde_json::Value = serde_json::from_str(response_json).unwrap();
                assert_eq!(response.code, expected["code"].as_str().unwrap());
                assert_eq!(
                    response.windows_reset,
                    expected["windows_reset"].as_i64().unwrap_or(0)
                );
            } else {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains("HTTP 500") && error.contains("backend failure"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn reset_rejects_empty_selectors_before_resolving_credentials() {
        for (name, id, key, expected) in [
            ("", None, None, "name must not be empty"),
            (
                "missing-test-account",
                Some(" "),
                None,
                "credit-id must not be empty",
            ),
            (
                "missing-test-account",
                None,
                Some(" "),
                "idempotency-key must not be empty",
            ),
        ] {
            assert_eq!(
                redeem_reset_credit(name, id, key).unwrap_err().to_string(),
                expected
            );
        }
    }

    #[test]
    fn auth_cli_installs_the_tls_provider() {
        assert!(run_auth_cli(AuthArgs::Status { name: "/".into() }).is_err());
        reqwest::blocking::Client::builder().build().unwrap();
    }

    #[test]
    fn routing_uses_the_most_constrained_cached_window() {
        let window = |used_percent| RateLimitWindow {
            used_percent,
            limit_window_seconds: None,
            reset_after_seconds: None,
            reset_at: Some(123),
        };
        let limits = RateLimitDetails {
            primary_window: Some(window(35.0)),
            secondary_window: Some(window(90.0)),
        };

        assert_eq!(routing_window(&limits).unwrap().used_percent, 90.0);
    }

    #[test]
    fn finds_weekly_window_regardless_of_position() {
        let window = |seconds| RateLimitWindow {
            used_percent: 32.0,
            limit_window_seconds: Some(seconds),
            reset_after_seconds: None,
            reset_at: Some(123),
        };
        let primary = RateLimitDetails {
            primary_window: Some(window(7 * 24 * 60 * 60)),
            secondary_window: Some(window(5 * 60 * 60)),
        };
        assert_eq!(weekly_window(&primary).unwrap().reset_at, Some(123));

        let secondary = RateLimitDetails {
            primary_window: Some(window(5 * 60 * 60)),
            secondary_window: Some(window(7 * 24 * 60 * 60)),
        };
        assert_eq!(weekly_window(&secondary).unwrap().reset_at, Some(123));
    }
}
