use std::io::{self, IsTerminal as _, Read as _};
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail, ensure};
use google_server::{GOOGLE_AUTHORIZED_USER, authorized_user_json};
use rho_agent_hosts::protocol::{PlatformSecretsSet, PlatformStatus};
use rho_rpc::protocol::RuntimePaths;

#[derive(Clone, clap::Args)]
pub(crate) struct GoogleArgs {
    #[arg(long = "runtime-dir")]
    runtime_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: GoogleCommand,
}

#[derive(Clone, clap::Subcommand)]
enum GoogleCommand {
    /// Install the host-held Google Drive and Meet credential from
    /// authorized_user JSON on stdin.
    Init,
}

pub(crate) async fn run(args: GoogleArgs) -> Result<()> {
    match args.command {
        GoogleCommand::Init => {
            let credential = read_credential()?;
            let paths = RuntimePaths::resolve(args.runtime_dir)?;
            let call = PlatformSecretsSet {
                secrets: vec![(GOOGLE_AUTHORIZED_USER.to_owned(), credential)],
            };
            match crate::host_call(&paths, call).await? {
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
    }
}

fn read_credential() -> Result<String> {
    let mut stdin = io::stdin().lock();
    ensure!(
        !stdin.is_terminal(),
        "redirect an authorized_user JSON file into `rho google init`; do not paste it into the terminal"
    );
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .context("read Google authorized_user JSON from stdin")?;
    authorized_user_json(&input)
}
