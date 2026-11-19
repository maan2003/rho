use anyhow::bail;
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};

use crate::{NotionArgs, NotionCommand, host_call};

pub(crate) async fn run(args: NotionArgs) -> anyhow::Result<()> {
    match args.command {
        NotionCommand::Init => init(args.runtime_dir).await,
    }
}

/// Signs this host in to Notion MCP as the user.
async fn init(runtime_dir: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let client = notion_server::oauth::http_client();
    let endpoints = notion_server::Endpoints::notion();
    let pending = notion_server::oauth::begin(&client, &endpoints).await?;
    eprintln!(
        "Open this address in a browser and approve access. Agents write as the Notion user you approve as.\n\n{}\n",
        pending.url
    );
    eprintln!(
        "The browser then fails to open {}: that is expected.",
        notion_server::oauth::REDIRECT_URI
    );
    let redirected = crate::github::prompt_token("Paste the address it shows: ")?;
    let grant = notion_server::oauth::finish(&client, &endpoints, pending, &redirected).await?;
    install(
        runtime_dir,
        vec![
            (notion_server::CLIENT_ID.to_owned(), grant.client_id),
            (notion_server::REFRESH_TOKEN.to_owned(), grant.refresh_token),
        ],
    )
    .await
}

async fn install(
    runtime_dir: Option<std::path::PathBuf>,
    secrets: Vec<(String, String)>,
) -> anyhow::Result<()> {
    let paths = rho_rpc::protocol::RuntimePaths::resolve(runtime_dir)?;
    match host_call(&paths, PlatformSecretsSet { secrets }).await? {
        PlatformStatus {
            running: true,
            detail,
        } => {
            eprintln!("Notion configured: {detail}");
            Ok(())
        }
        PlatformStatus { detail, .. } => bail!(detail),
    }
}
