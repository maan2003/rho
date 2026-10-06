use anyhow::bail;
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};

use crate::{NotionArgs, NotionCommand, host_call};

pub(crate) async fn run(args: NotionArgs) -> anyhow::Result<()> {
    match args.command {
        NotionCommand::Init => init(args.socket_path).await,
    }
}

/// Signs this host in to Notion MCP as the user, and names the page agents
/// work under.
async fn init(socket_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    eprintln!("Agents create pages under one root page, and reach only it and the pages under it.");
    let root = crate::github::prompt_token("Root page URL (create an empty page for agents): ")?;
    anyhow::ensure!(
        notion_server::page_id(&root).is_some(),
        "not a Notion page URL or ID"
    );
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
    let socket_path = rho_rpc::protocol::RuntimePaths::resolve(socket_path)?
        .socket()
        .to_owned();
    let call = PlatformSecretsSet {
        secrets: vec![
            (notion_server::CLIENT_ID.to_owned(), grant.client_id),
            (notion_server::REFRESH_TOKEN.to_owned(), grant.refresh_token),
            (notion_server::ROOT_PAGE.to_owned(), root),
        ],
    };
    match host_call(&socket_path, call).await? {
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
