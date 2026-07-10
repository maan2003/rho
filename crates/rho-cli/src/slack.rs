//! `rho slack init`: install Slack tokens into the daemon's RAM-only secret
//! store and start its in-process Slack connection.
//!
//! Tokens are read from stdin (never argv, so they stay out of shell history
//! and /proc cmdlines) and travel to the daemon over the local control
//! socket; the daemon seals them into a memfd and stashes it in the systemd
//! fd store.

use std::io::{BufRead as _, Write as _};

use anyhow::{Context as _, Result, bail};
use rho_ui_proto::{ClientMessage, ServerMessage};

use crate::{SlackArgs, SlackCommand, connect_or_start_daemon, default_socket_path};

pub(crate) async fn run(args: SlackArgs) -> Result<()> {
    match args.command.clone() {
        SlackCommand::Init { dir } => init(args, dir).await,
    }
}

async fn init(args: SlackArgs, dir: camino::Utf8PathBuf) -> Result<()> {
    let coordinator_repo = rho_workspaces::resolve_repo_root(dir.as_std_path())
        .with_context(|| format!("resolving Slack coordinator repo {dir}"))?;
    let bot_token = prompt_token("Bot User OAuth Token (xoxb-...): ", "xoxb-")?;
    let app_token = prompt_token("App-Level Token for Socket Mode (xapp-...): ", "xapp-")?;

    let socket_path = match args.socket_path {
        Some(path) => path,
        None => default_socket_path()?,
    };
    let mut client = connect_or_start_daemon(&socket_path, &args.auth).await?;
    client
        .send(&ClientMessage::PlatformSecretsSet {
            secrets: vec![
                ("SLACK_BOT_TOKEN".to_owned(), bot_token),
                ("SLACK_APP_TOKEN".to_owned(), app_token),
            ],
            coordinator_repo,
        })
        .await?;
    loop {
        match client.recv().await? {
            ServerMessage::PlatformStatus { running, detail } => {
                if running {
                    eprintln!("slack connected: {detail}");
                    return Ok(());
                }
                bail!("slack not connected: {detail}");
            }
            ServerMessage::Error { message } => bail!("daemon error: {message}"),
            _ => continue,
        }
    }
}

fn prompt_token(prompt: &str, expected_prefix: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading token from stdin")?;
    let token = line.trim().to_owned();
    if token.is_empty() {
        bail!("no token entered");
    }
    if !token.starts_with(expected_prefix) {
        eprintln!("warning: token does not start with {expected_prefix}; continuing anyway");
    }
    Ok(token)
}
