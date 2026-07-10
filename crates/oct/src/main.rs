use anyhow::Result;
use clap::{Parser, Subcommand};

mod ci;
mod octo_client;
mod pr;
mod repo;

#[derive(Parser)]
#[command(name = "oct")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Ci {
        #[command(subcommand)]
        command: CiCommands,
    },
    Pr {
        #[command(subcommand)]
        command: PrCommands,
    },
}

#[derive(Subcommand)]
enum CiCommands {
    Status(ci::StatusArgs),
    Wait(ci::WaitArgs),
    Logs(ci::LogsArgs),
    Rerun(ci::RerunArgs),
}

#[derive(Subcommand)]
enum PrCommands {
    Create(pr::CreateArgs),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Ci { command } => match command {
            CiCommands::Status(args) => ci::status(args).await,
            CiCommands::Wait(args) => ci::wait(args).await,
            CiCommands::Logs(args) => ci::logs(args).await,
            CiCommands::Rerun(args) => ci::rerun(args).await,
        },
        Commands::Pr { command } => match command {
            PrCommands::Create(args) => pr::create(args).await,
        },
    }
}
