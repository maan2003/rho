//! `rho octo init`: install a GitHub token into the daemon's RAM-only
//! platform secret store for the embedded Octo server.

use std::io::{BufRead as _, Write as _};

use anyhow::{Context as _, Result, bail};
use rho_ui_proto::{ClientMessage, ServerMessage};

use crate::{OctoArgs, OctoCommand, connect_or_start_daemon, default_socket_path};

pub(crate) async fn run(args: OctoArgs) -> Result<()> {
    match args.command {
        OctoCommand::Init => init(args).await,
    }
}

async fn init(args: OctoArgs) -> Result<()> {
    let token = prompt_token("GitHub token (ghp_/github_pat_/...): ")?;
    let socket_path = match args.socket_path {
        Some(path) => path,
        None => default_socket_path()?,
    };
    let mut client = connect_or_start_daemon(&socket_path, &args.auth).await?;
    client
        .send(&ClientMessage::PlatformSecretsSet {
            secrets: vec![("GITHUB_TOKEN".to_owned(), token)],
            coordinator_repo: None,
        })
        .await?;
    loop {
        match client.recv().await? {
            ServerMessage::PlatformStatus { running, detail } => {
                if running {
                    eprintln!("octo configured: {detail}");
                    return Ok(());
                }
                bail!("octo not configured: {detail}");
            }
            ServerMessage::Error { message } => bail!("daemon error: {message}"),
            _ => continue,
        }
    }
}

fn prompt_token(prompt: &str) -> Result<String> {
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
    Ok(token)
}
