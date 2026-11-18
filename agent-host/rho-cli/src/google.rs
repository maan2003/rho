use anyhow::bail;
use google_server::oauth;
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};

use crate::{GoogleArgs, GoogleCommand, host_call};

pub(crate) async fn run(args: GoogleArgs) -> anyhow::Result<()> {
    match args.command {
        GoogleCommand::Init { client_id } => init(args.runtime_dir, client_id).await,
    }
}

/// Signs this host in to Google as the account agents act as.
async fn init(runtime_dir: Option<std::path::PathBuf>, client_id: String) -> anyhow::Result<()> {
    let secret = crate::github::prompt_token("The OAuth client's secret: ")?;
    let client = oauth::OAuthClient {
        id: client_id,
        secret,
    };
    let endpoints = google_server::Endpoints::google();
    let pending = oauth::begin(&endpoints, &client.id);
    eprintln!(
        "Open this address in a browser signed in as the Google account agents act as, and allow \
         every access it asks for. Agents read what that account can read.\n\n{}\n",
        pending.url
    );
    eprintln!(
        "The browser then fails to open {}: that is expected.",
        oauth::REDIRECT_URI
    );
    let redirected = crate::github::prompt_token("Paste the address it shows: ")?;
    let refresh_token = oauth::finish(
        &oauth::http_client(),
        &endpoints,
        &client,
        pending,
        &redirected,
    )
    .await?;
    let paths = rho_rpc::protocol::RuntimePaths::resolve(runtime_dir)?;
    let secrets = vec![
        (google_server::CLIENT_ID.to_owned(), client.id),
        (google_server::CLIENT_SECRET.to_owned(), client.secret),
        (google_server::REFRESH_TOKEN.to_owned(), refresh_token),
    ];
    match host_call(&paths, PlatformSecretsSet { secrets }).await? {
        PlatformStatus {
            running: true,
            detail,
        } => {
            eprintln!("Google configured: {detail}");
            Ok(())
        }
        PlatformStatus { detail, .. } => bail!(detail),
    }
}
