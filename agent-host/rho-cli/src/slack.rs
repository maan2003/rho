use anyhow::bail;
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};

use crate::github::prompt_token;
use crate::{SlackArgs, SlackCommand, host_call};

pub(crate) async fn run(args: SlackArgs) -> anyhow::Result<()> {
    match args.command {
        SlackCommand::Init => init(args.socket_path).await,
    }
}

async fn init(socket_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let token = prompt_token("Slack bot token (xoxb-...): ")?;
    anyhow::ensure!(
        token.starts_with("xoxb-"),
        "a Slack bot token starts with xoxb-"
    );
    let socket_path = rho_rpc::protocol::RuntimePaths::resolve(socket_path)?
        .socket()
        .to_owned();
    let call = PlatformSecretsSet {
        secrets: vec![("SLACK_BOT_TOKEN".to_owned(), token)],
    };
    match host_call(&socket_path, call).await? {
        PlatformStatus {
            running: true,
            detail,
        } => {
            eprintln!("Slack configured: {detail}");
            Ok(())
        }
        PlatformStatus { detail, .. } => bail!(detail),
    }
}
