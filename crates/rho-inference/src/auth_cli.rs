use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::responses::oauth::{
    OAuthFile, ResponsesOAuthCredentials, oauth_token_should_refresh, openai_codex_auth_url,
    openai_codex_exchange, parse_redirect_url,
};

const DEFAULT_AUTH_NAME: &str = "default";

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
