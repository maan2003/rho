use std::io::Write as _;

use anyhow::{Context as _, bail};
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};

use crate::{GithubArgs, GithubCommand, host_call};

pub(crate) async fn run(args: GithubArgs) -> anyhow::Result<()> {
    match args.command {
        GithubCommand::Init => init(args.socket_path).await,
    }
}

async fn init(socket_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let token = prompt_token("GitHub token (ghp_/github_pat_/...): ")?;
    let socket_path = rho_rpc::protocol::RuntimePaths::resolve(socket_path)?
        .socket()
        .to_owned();
    let call = PlatformSecretsSet {
        secrets: vec![("GITHUB_TOKEN".to_owned(), token)],
    };
    match host_call(&socket_path, call).await? {
        PlatformStatus {
            running: true,
            detail,
        } => {
            eprintln!("GitHub configured: {detail}");
            Ok(())
        }
        PlatformStatus { detail, .. } => bail!(detail),
    }
}

pub(crate) fn prompt_token(prompt: &str) -> anyhow::Result<String> {
    prompt_optional(prompt)?.context("no token entered")
}

/// A token, or `None` when the user just pressed Enter.
pub(crate) fn prompt_optional(prompt: &str) -> anyhow::Result<Option<String>> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut token = String::new();
    std::io::stdin().read_line(&mut token)?;
    let token = token.trim();
    Ok((!token.is_empty()).then(|| token.to_owned()))
}
