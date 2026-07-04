use std::io;
use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;

use crate::auth::{
    DEFAULT_AUTH_NAME, OAuthFile, VoiceOAuthCredentials, import_grok_cli_credentials,
    oauth_token_should_refresh,
};

#[derive(Clone, Subcommand)]
pub enum VoiceArgs {
    /// Copy the current Grok CLI OAuth session from ~/.grok/auth.json.
    Import {
        #[arg(long, default_value = DEFAULT_AUTH_NAME)]
        name: String,
    },
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
}

pub fn run_voice_cli(command: VoiceArgs) -> Result<()> {
    match command {
        VoiceArgs::Import { name } => {
            let credentials =
                import_grok_cli_credentials().context("copying Grok CLI OAuth credentials")?;
            let file = OAuthFile::open_default(name.trim())?;
            file.save(&credentials)?;
            println!("{}", status_line_for(&file.path(), Some(&credentials)));
            Ok(())
        }
        VoiceArgs::List => list(),
        VoiceArgs::Remove { name } => {
            let file = OAuthFile::open_default(name.trim())?;
            let path = file.path();
            if file.delete()? {
                println!("removed {}", path.display());
            } else {
                println!("missing {}", path.display());
            }
            Ok(())
        }
        VoiceArgs::Path { name } => {
            println!("{}", OAuthFile::open_default(name.trim())?.path().display());
            Ok(())
        }
        VoiceArgs::Status { name } => {
            let file = OAuthFile::open_default(name.trim())?;
            println!("{}", status_line_for(&file.path(), file.load()?.as_ref()));
            Ok(())
        }
    }
}

fn list() -> Result<()> {
    let credentials = list_credentials().context("reading voice auth credentials directory")?;
    if credentials.is_empty() {
        println!("No voice auth credentials configured.");
        return Ok(());
    }
    for (name, status) in credentials {
        println!("{name}\tgrok\t{status}");
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

fn auth_status(credentials: Option<&VoiceOAuthCredentials>) -> AuthStatus {
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

fn status_label(credentials: Option<&VoiceOAuthCredentials>) -> &'static str {
    match auth_status(credentials) {
        AuthStatus::Missing => "missing",
        AuthStatus::Invalid => "invalid",
        AuthStatus::RefreshDue => "refresh-due",
        AuthStatus::Fresh => "logged-in",
    }
}

fn status_line_for(path: &Path, credentials: Option<&VoiceOAuthCredentials>) -> String {
    let Some(credentials) = credentials else {
        return format!("missing path={}", path.display());
    };
    let status = match auth_status(Some(credentials)) {
        AuthStatus::Missing => unreachable!("credentials are present"),
        AuthStatus::Invalid => "invalid",
        AuthStatus::RefreshDue => "refresh_due",
        AuthStatus::Fresh => "fresh",
    };
    let refresh = if credentials.refresh_token.trim().is_empty() {
        "no"
    } else {
        "yes"
    };
    let version = credentials
        .grok_client_version
        .as_deref()
        .unwrap_or("unknown");
    format!(
        "present path={} status={} provider=grok refresh_token={} grok_client_version={} expires_at_ms={}",
        path.display(),
        status,
        refresh,
        version,
        credentials.expires_at_ms,
    )
}
