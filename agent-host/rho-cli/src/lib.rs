//! The `rho` command, run on an agent host: tools agents call from their
//! shell (visualizations, the Wayland session, evaluations) and the
//! plumbing around the host (auth, Claude accounts, iroh trust, debug and
//! protocol logs). The host itself is the `rho-agent-host` binary.

use std::io;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use rho_agent_host::debug::DebugArgs;
use rho_agent_hosts::protocol as host;
use rho_agents_client::protocol as agents;
use rho_inference::{AuthArgs, run_auth_cli};
use rho_rpc::protocol::client::Client as UiClient;
use rho_rpc::protocol::{Answer, Call, client};

mod eval;
mod github;
mod notion;
mod slack;
mod visualization;
mod wayland;

#[cfg(test)]
mod tests;

pub fn main() -> Result<()> {
    let args = Args::parse_or_exit(std::env::args().skip(1));
    // Ordinary utilities die quietly on a closed pipe. Evaluations own live
    // agent work: BrokenPipe must unwind through cancellation, not kill us
    // before subprocess destructors run.
    // SAFETY: top of main, single-threaded, before any runtime exists.
    unsafe {
        libc::signal(
            libc::SIGPIPE,
            if matches!(&args.command, Command::Eval(_)) {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            },
        );
    }
    match args.command {
        Command::Auth(auth) => return run_auth_cli(auth),
        Command::Wayland(args) => return wayland::run(args),
        _ => {}
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args.command))
}

async fn run(command: Command) -> Result<()> {
    match command {
        Command::Auth(_) => unreachable!("auth runs before the shared async runtime"),
        Command::ClaudeAccount(args) => run_claude_account(args).await,
        Command::Debug(args) => {
            rho_agent_host::debug::run(args).await?;
            Ok(())
        }
        Command::Eval(args) => eval::run(args).await,
        Command::Iroh(args) => run_iroh(args).await,
        Command::Github(args) => github::run(args).await,
        Command::Slack(args) => slack::run(args).await,
        Command::Notion(args) => notion::run(args).await,
        Command::RecordVisualization(args) => visualization::run(args).await,
        Command::Wayland(_) => unreachable!("wayland runs before the shared async runtime"),
        Command::ProtocolLog(args) => {
            let mut stdout = io::stdout().lock();
            rho_rpc::protocol::print_protocol_log(&args.path, &mut stdout, describe_frame)?;
            Ok(())
        }
    }
}

/// A protocol log frame, read as the protocol it belongs to.
fn describe_frame(open: &rho_rpc::protocol::Open, reply: Option<&[u8]>) -> String {
    use rho_rpc::protocol::{Protocol, describe_as};
    match open.protocol {
        Protocol::Agents => describe_as::<agents::Open>(open, reply),
        Protocol::Desktop => describe_as::<rho_desktop_client::protocol::Open>(open, reply),
        Protocol::Host => describe_as::<host::Open>(open, reply),
        Protocol::Ledger => "retired ledger protocol".to_owned(),
        Protocol::LedgerLog => describe_as::<rho_ledger::protocol::Open>(open, reply),
        Protocol::Shell => describe_as::<rho_shell_view::protocol::Open>(open, reply),
        Protocol::Terminal => describe_as::<rho_terminal::protocol::Open>(open, reply),
        Protocol::Voice => describe_as::<rho_rtc::protocol::Open>(open, reply),
        Protocol::Workspace => describe_as::<rho_files::protocol::Open>(open, reply),
    }
}

/// Approves a pending iroh enrollment over the agent host's Unix socket, so
/// trust decisions always come from a local user on the agent host.
async fn run_iroh(args: IrohArgs) -> Result<()> {
    let socket_path = rho_rpc::protocol::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    let socket = &socket_path;
    match args.command {
        IrohCommand::Approve { code } => {
            let endpoint_id = client::call(socket, host::IrohApprove { code }).await?;
            println!("enrolled iroh client {endpoint_id}");
        }
        IrohCommand::TrustInMemory { endpoint_id } => {
            let call = host::IrohTrustInMemory {
                endpoint_id: endpoint_id.clone(),
            };
            client::call(socket, call).await?;
            println!("enrolled iroh client {endpoint_id}");
        }
        IrohCommand::Revoke { endpoint_id } => {
            let endpoint_id = client::call(socket, host::IrohRevoke { endpoint_id }).await?;
            println!("revoked iroh client {endpoint_id}");
        }
    }
    Ok(())
}

/// One call of the agent host, starting the agent host if it is not running. A
/// refusal is an error.
pub(crate) async fn host_call<C: Call>(socket_path: &std::path::Path, call: C) -> Result<C::Reply> {
    let mut agent_host = connect_or_start_host(socket_path).await?;
    agent_host.open(&call.open()).await?;
    agent_host.recv::<Answer<C::Reply>>().await?.into_result()
}

pub(crate) async fn connect_or_start_host(socket_path: &std::path::Path) -> Result<UiClient> {
    if let Ok(client) = UiClient::connect(socket_path).await {
        return Ok(client);
    }

    // The host is its own binary, installed beside this one.
    let host = std::env::current_exe()?.with_file_name("rho-agent-host");
    std::process::Command::new(host)
        .arg("--socket-path")
        .arg(socket_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match UiClient::connect(socket_path).await {
            Ok(client) => return Ok(client),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

#[derive(Clone)]
struct Args {
    command: Command,
}

#[derive(Clone)]
enum Command {
    Auth(AuthArgs),
    ClaudeAccount(ClaudeAccountArgs),
    Debug(DebugArgs),
    /// Run a headless agent evaluation; JSONL output, temporary state, real
    /// provider.
    Eval(eval::EvalArgs),
    Iroh(IrohArgs),
    Github(GithubArgs),
    Slack(SlackArgs),
    Notion(NotionArgs),
    RecordVisualization(RecordVisualizationArgs),
    ProtocolLog(ProtocolLogArgs),
    Wayland(wayland::WaylandArgs),
}

#[derive(Parser)]
#[command(name = "rho")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Auth {
        #[command(subcommand)]
        command: AuthArgs,
    },
    /// Manage the Claude accounts agents run on.
    ClaudeAccount(ClaudeAccountArgs),
    Debug(DebugArgs),
    /// Run a headless agent evaluation; JSONL output, temporary state, real
    /// provider.
    Eval(eval::EvalArgs),
    Iroh(IrohArgs),
    Github(GithubArgs),
    Slack(SlackArgs),
    Notion(NotionArgs),
    /// Register an immutable SVG visualization read from stdin.
    RecordVisualization(RecordVisualizationArgs),
    ProtocolLog(ProtocolLogArgs),
    /// Run and control applications in an isolated headless Wayland session.
    Wayland(wayland::WaylandArgs),
}

#[derive(Clone, clap::Args)]
pub(crate) struct ClaudeAccountArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: ClaudeAccountCommand,
}

#[derive(Clone, Subcommand)]
pub(crate) enum ClaudeAccountCommand {
    /// List the accounts agents can run on, marking the current one.
    List,
    /// Open Claude against one account so it can be logged in, creating the
    /// account if it is new. Run `/login` in the session that opens.
    Login { name: String },
    /// Put new agents on an account. Running agents keep theirs.
    Use { name: String },
}

/// Accounts are directories, so making one is a local matter; which one
/// agents run on is the agent host's, so listing and switching go through it.
/// A login names the directory in `CLAUDE_CONFIG_DIR` because there is no
/// view namespace outside an agent; agents get the same directory by mount.
async fn run_claude_account(args: ClaudeAccountArgs) -> Result<()> {
    let socket_path = rho_rpc::protocol::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    let list = match &args.command {
        ClaudeAccountCommand::List => host_call(&socket_path, agents::ClaudeAccounts).await?,
        ClaudeAccountCommand::Use { name } => {
            let call = agents::SetClaudeAccount { name: name.clone() };
            host_call(&socket_path, call).await?
        }
        ClaudeAccountCommand::Login { name } => {
            let dir = rho_claude::accounts::ClaudePaths::from_env()?.prepare(name)?;
            eprintln!("rho: opening Claude on account {name} ({dir}); run /login");
            let status = std::process::Command::new("claude")
                .env("CLAUDE_CONFIG_DIR", dir.as_str())
                .status()
                .context("run claude for account login")?;
            anyhow::ensure!(status.success(), "claude exited with {status}");
            return Ok(());
        }
    };
    for name in list.accounts {
        let mark = if name == list.current { "*" } else { " " };
        println!("{mark} {name}");
    }
    Ok(())
}

#[derive(Clone, clap::Args)]
pub(crate) struct IrohArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: IrohCommand,
}

#[derive(Clone, Subcommand)]
pub(crate) enum IrohCommand {
    /// Approve a pending iroh client enrollment by its displayed code.
    Approve { code: String },
    /// Directly trust an endpoint in agent host memory (for use through SSH).
    TrustInMemory { endpoint_id: String },
    /// Revoke a previously enrolled iroh client endpoint.
    Revoke { endpoint_id: String },
}

#[derive(Clone, clap::Args)]
pub(crate) struct GithubArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: GithubCommand,
}

#[derive(Clone, clap::Args)]
pub(crate) struct SlackArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: SlackCommand,
}

#[derive(Clone, Subcommand)]
pub(crate) enum SlackCommand {
    /// Install the host-held Slack tokens: the bot token agents call Slack
    /// with, and the app token its Socket Mode connection uses.
    Init,
    /// Print the manifest of the Slack app each host needs.
    Manifest,
}

#[derive(Clone, clap::Args)]
pub(crate) struct NotionArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: NotionCommand,
}

#[derive(Clone, Subcommand)]
pub(crate) enum NotionCommand {
    /// Sign this host in to Notion MCP as you, in a browser.
    Init,
    /// Name the page agents work under: they reach only it and the pages
    /// under it.
    Root {
        /// The page's URL or ID.
        page: String,
    },
}

#[derive(Clone, clap::Args)]
pub(crate) struct RecordVisualizationArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
}

#[derive(Clone, Subcommand)]
pub(crate) enum GithubCommand {
    /// Install the host-held GitHub token used by Octo.
    Init,
}

#[derive(Clone, clap::Args)]
struct ProtocolLogArgs {
    path: std::path::PathBuf,
}

impl Args {
    fn parse_or_exit(args: impl Iterator<Item = String>) -> Self {
        Self::try_parse(args).unwrap_or_else(|error| error.exit())
    }

    fn try_parse(args: impl Iterator<Item = String>) -> std::result::Result<Self, clap::Error> {
        let cli = Cli::try_parse_from(std::iter::once("rho".to_owned()).chain(args))?;
        let command = match cli.command {
            CliCommand::Auth { command } => Command::Auth(command),
            CliCommand::ClaudeAccount(args) => Command::ClaudeAccount(args),
            CliCommand::Debug(args) => Command::Debug(args),
            CliCommand::Eval(args) => Command::Eval(args),
            CliCommand::Iroh(args) => Command::Iroh(args),
            CliCommand::Github(args) => Command::Github(args),
            CliCommand::Slack(args) => Command::Slack(args),
            CliCommand::Notion(args) => Command::Notion(args),
            CliCommand::RecordVisualization(args) => Command::RecordVisualization(args),
            CliCommand::ProtocolLog(args) => Command::ProtocolLog(args),
            CliCommand::Wayland(args) => Command::Wayland(args),
        };
        Ok(Self { command })
    }
}
