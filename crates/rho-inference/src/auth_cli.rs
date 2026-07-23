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
    Import {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
        #[arg(long = "file")]
        path: Option<PathBuf>,
    },
}

pub fn run_auth_cli(command: AuthArgs) -> Result<()> {
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChatGptUsage {
    pub used_percent: f64,
    pub reset_at_unix: i64,
}

/// Fetches the weekly window for an OAuth namespace. Accounts which do not
/// report a weekly window return `None`.
pub fn chatgpt_weekly_usage(name: impl AsRef<str>) -> Result<Option<ChatGptUsage>> {
    let auth = InferenceAuth::named(name)?;
    let resolved = auth.resolve().context("resolving OAuth credentials")?;
    let status = fetch_rate_limit_status(&resolved.bearer_token, resolved.account_id.as_deref())
        .context("fetching ChatGPT rate limits")?;
    let Some(window) = status
        .rate_limit
        .as_ref()
        .and_then(|rate_limit| rate_limit.secondary_window.as_ref())
    else {
        return Ok(None);
    };
    if !window.used_percent.is_finite() {
        return Ok(None);
    }
    let reset_at_unix = window.reset_at.or_else(|| {
        window
            .reset_after_seconds
            .map(|seconds| now_secs().saturating_add(seconds))
    });
    Ok(reset_at_unix.map(|reset_at_unix| ChatGptUsage {
        used_percent: window.used_percent,
        reset_at_unix,
    }))
}

fn fetch_rate_limit_status(
    bearer_token: &str,
    account_id: Option<&str>,
) -> io::Result<RateLimitStatus> {
    let url = format!("{DEFAULT_CHATGPT_BASE_URL}/wham/usage");
    let authorization = format!("Bearer {bearer_token}");
    let mut request = reqwest::blocking::Client::new()
        .get(&url)
        .header("Authorization", &authorization)
        .header("User-Agent", USER_AGENT);
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let json = read_success_json(
        &url,
        request
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(|error| io::Error::other(format!("{url}: {error}")))?,
    )?;
    serde_json::from_value(json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
    let resets_at = window.reset_at.or_else(|| {
        window
            .reset_after_seconds
            .map(|seconds| now_secs().saturating_add(seconds))
    });
    let resets_at = resets_at
        .map(|timestamp| timestamp.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let resets_in = window
        .reset_after_seconds
        .map(|seconds| seconds.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let limit_name = limit_name.unwrap_or("-");
    println!(
        "limit={limit_id} name={limit_name} window={kind} used_percent={} window_mins={window_mins} resets_at_unix={resets_at} resets_in_secs={resets_in}",
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
