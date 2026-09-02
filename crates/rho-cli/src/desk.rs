//! Read-only native Desk tree rendering for shells and editor snapshots.
//!
//! The org-looking text emitted here is presentation only. Desk state is
//! edited through the native GUI tree operations; no runtime path parses this
//! output back into state.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rho_ui_proto::{ClientMessage, ServerMessage};

use crate::connect_or_start_daemon;

#[derive(Clone, clap::Args)]
pub(crate) struct DeskArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: DeskCommand,
}

#[derive(Clone, clap::Subcommand)]
enum DeskCommand {
    /// Print the current Desk hierarchy.
    Cat,
    /// Write a read-only rendering of the current Desk hierarchy to FILE.
    Checkout { file: PathBuf },
}

pub(crate) async fn run(args: DeskArgs) -> Result<()> {
    let socket_path = rho_ui_proto::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    let text = fetch_rendered(&socket_path).await?;
    match args.command {
        DeskCommand::Cat => print!("{text}"),
        DeskCommand::Checkout { file } => {
            std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
            println!("wrote the Desk view to {}", file.display());
        }
    }
    Ok(())
}

async fn fetch_rendered(socket_path: &Path) -> Result<String> {
    let mut client = connect_or_start_daemon(socket_path).await?;
    client.send(&ClientMessage::DeskTreeGet).await?;
    loop {
        match client.recv().await? {
            ServerMessage::DeskTreeDocument { snapshot } => {
                let text = rho_desk::render_org(snapshot).map_err(anyhow::Error::msg)?;
                client.shutdown().await?;
                return Ok(text);
            }
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
}
