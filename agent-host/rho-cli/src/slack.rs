use anyhow::bail;
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};

use crate::github::prompt_token;
use crate::{SlackArgs, SlackCommand, host_call};

pub(crate) async fn run(args: SlackArgs) -> anyhow::Result<()> {
    match args.command {
        SlackCommand::Init => init(args.socket_path).await,
        SlackCommand::Manifest => {
            print!("{MANIFEST}");
            Ok(())
        }
    }
}

/// The Slack app manifest, with the steps to install it in its comments.
const MANIFEST: &str = include_str!("../../slack-server/manifest.yaml");

async fn init(socket_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    eprintln!(
        "This host needs its own Slack app: `rho slack manifest` prints it, with the steps to install it."
    );
    let bot = prompt_token("Slack bot token (xoxb-...): ")?;
    anyhow::ensure!(
        bot.starts_with("xoxb-"),
        "a Slack bot token starts with xoxb-"
    );
    let app = prompt_token("Slack app-level token with connections:write (xapp-...): ")?;
    anyhow::ensure!(
        app.starts_with("xapp-"),
        "a Slack app-level token starts with xapp-"
    );
    let socket_path = rho_rpc::protocol::RuntimePaths::resolve(socket_path)?
        .socket()
        .to_owned();
    let call = PlatformSecretsSet {
        secrets: vec![
            ("SLACK_BOT_TOKEN".to_owned(), bot),
            ("SLACK_APP_TOKEN".to_owned(), app),
        ],
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
