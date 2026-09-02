//! The `rho` command: daemon launcher and utility subcommands.
//!
//! Interactive use lives in rho-gui; this binary hosts the daemon itself
//! plus the terminal-friendly plumbing around it — auth, land, PR and debug
//! tools.

use std::io;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rho_daemon::DaemonArgs;
use rho_daemon::debug::DebugArgs;
use rho_inference::{AuthArgs, run_auth_cli};
use rho_ui_proto::client::Client as UiClient;

mod desk;
mod land;
mod mcp_agent_tools;
mod pr;
mod visualization;
mod wayland;

#[cfg(test)]
mod tests;

pub fn main() -> Result<()> {
    // Dying quietly on a closed pipe is the correct
    // CLI behavior; Rust's default ignore turns it into a print panic.
    // SAFETY: top of main, single-threaded, resetting to default handling.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args = Args::parse_or_exit(std::env::args().skip(1));
    if let Command::Land(land) = &args.command
        && let Some(socket_path) = &land.socket_path
    {
        let socket_path = rho_ui_proto::RuntimePaths::new(Some(socket_path))?
            .socket()
            .to_owned();
        // SAFETY: no runtime or other thread exists; jj may invoke
        // git-remote-octo, which must follow this CLI's daemon.
        unsafe {
            std::env::set_var(rho_ui_proto::RuntimePaths::SOCKET_ENV, socket_path);
        }
    }
    if let Command::Daemon(mut daemon_args) = args.command {
        // SAFETY: top of main, before the runtime — no threads exist yet and
        // nothing has captured pre-namespace state.
        unsafe { rho_daemon::init_daemon_namespace() }.expect("set up daemon namespace");
        let profiler = rho_daemon::DaemonProfiler::start(&mut daemon_args)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let result = runtime.block_on(rho_daemon::run(daemon_args));
        drop(runtime);
        return profiler.finish(result);
    }
    if let Command::Wayland(args) = args.command {
        return wayland::run(args);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args.command))
}

async fn run(command: Command) -> Result<()> {
    match command {
        Command::Auth(auth) => {
            run_auth_cli(auth)?;
            Ok(())
        }
        Command::Daemon(_) => unreachable!("daemon runs before the shared async runtime"),
        Command::Debug(args) => {
            rho_daemon::debug::run(args).await?;
            Ok(())
        }
        Command::Desk(args) => desk::run(args).await,
        Command::Iroh(args) => run_iroh(args).await,
        Command::Land(args) => land::run(args).await,
        Command::McpAgentTools(args) => mcp_agent_tools::run(args).await,
        Command::Pr(args) => pr::run(args).await,
        Command::RecordVisualization(args) => visualization::run(args).await,
        Command::Wayland(_) => unreachable!("wayland runs before the shared async runtime"),
        Command::ProtocolLog(args) => {
            let mut stdout = io::stdout().lock();
            rho_ui_proto::print_protocol_log(&args.path, &mut stdout)?;
            Ok(())
        }
    }
}

/// Approves a pending iroh enrollment over the daemon's Unix socket, so
/// trust decisions always come from a local user on the daemon host.
async fn run_iroh(args: IrohArgs) -> Result<()> {
    let request = match args.command {
        IrohCommand::Approve { code } => rho_ui_proto::ClientMessage::IrohApprove { code },
        IrohCommand::TrustInMemory { endpoint_id } => {
            rho_ui_proto::ClientMessage::IrohTrustInMemory { endpoint_id }
        }
        IrohCommand::Revoke { endpoint_id } => {
            rho_ui_proto::ClientMessage::IrohRevoke { endpoint_id }
        }
    };
    let socket_path = rho_ui_proto::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    let mut client = UiClient::connect(&socket_path).await?;
    client.send(&request).await?;
    loop {
        match client.recv().await? {
            rho_ui_proto::ServerMessage::IrohApproved { endpoint_id } => {
                println!("enrolled iroh client {endpoint_id}");
                return Ok(());
            }
            rho_ui_proto::ServerMessage::IrohRevoked { endpoint_id } => {
                println!("revoked iroh client {endpoint_id}");
                return Ok(());
            }
            rho_ui_proto::ServerMessage::Error { message } => anyhow::bail!("{message}"),
            // The daemon greets every connection with Ready and may stream
            // other broadcasts; only the approve outcome matters here.
            _ => {}
        }
    }
}

/// Resolves an agent handle — role-prefixed (`eng-j2qk`) or a raw encoded id
/// as found in `$RHO_AGENT_ID` — against the daemon's id domain.
pub(crate) fn resolve_agent_id(
    text: &str,
    machine_seed: u64,
    agent_counter: u64,
) -> Result<rho_ui_proto::AgentId> {
    let text = text.trim();
    let raw = text.split_once('-').map_or(text, |(_, raw)| raw);
    let domain = rho_ui_proto::AgentIdDomain(machine_seed);
    match rho_ui_proto::AgentId::from_prefix(raw, agent_counter + 1, &domain)? {
        prefix_id::PrefixResolution::Unique(agent_id)
        | prefix_id::PrefixResolution::Ambiguous {
            first: agent_id, ..
        } => Ok(agent_id),
        prefix_id::PrefixResolution::NotFound => anyhow::bail!("no agent with id {text}"),
    }
}

pub(crate) async fn connect_or_start_daemon(socket_path: &std::path::Path) -> Result<UiClient> {
    if let Ok(client) = UiClient::connect(socket_path).await {
        return Ok(client);
    }

    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("daemon");
    command
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
    Daemon(DaemonArgs),
    Debug(DebugArgs),
    Desk(desk::DeskArgs),
    Iroh(IrohArgs),
    Land(LandArgs),
    McpAgentTools(McpAgentToolsArgs),
    Pr(PrArgs),
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
    Daemon(DaemonArgs),
    Debug(DebugArgs),
    /// Read and edit the Desk document through checkout/apply files.
    Desk(desk::DeskArgs),
    Iroh(IrohArgs),
    Land(LandArgs),
    McpAgentTools(McpAgentToolsArgs),
    Pr(PrArgs),
    /// Register an immutable SVG visualization read from stdin.
    RecordVisualization(RecordVisualizationArgs),
    ProtocolLog(ProtocolLogArgs),
    /// Run and control applications in an isolated headless Wayland session.
    Wayland(wayland::WaylandArgs),
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
    /// Directly trust an endpoint in daemon memory (for use through SSH).
    TrustInMemory { endpoint_id: String },
    /// Revoke a previously enrolled iroh client endpoint.
    Revoke { endpoint_id: String },
}

#[derive(Clone, clap::Args)]
pub(crate) struct PrArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: PrCliCommand,
}

#[derive(Clone, clap::Args)]
pub(crate) struct RecordVisualizationArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
}

#[derive(Clone, Subcommand)]
pub(crate) enum PrCliCommand {
    /// Install the GitHub token used for PR, Actions, and constrained Git
    /// operations.
    Init,
    /// Create a draft pull request.
    Create {
        #[arg(short = 'H', long)]
        head: String,
        #[arg(short = 'B', long)]
        base: Option<String>,
        #[arg(short = 't', long)]
        title: String,
        #[arg(short = 'b', long)]
        body: String,
    },
    /// Fetch the current PR, CI, and review snapshot.
    Status { url: String },
    /// Edit a pull request's title, description, or base branch.
    Edit {
        url: String,
        #[arg(short = 'B', long)]
        base: Option<String>,
        #[arg(short = 't', long)]
        title: Option<String>,
        #[arg(short = 'b', long, alias = "description")]
        body: Option<String>,
    },
    /// Add a PR comment or reply to an inline review comment.
    Comment {
        url: String,
        /// Numeric GitHub inline review-comment ID from `rho pr comments`.
        #[arg(long)]
        reply_comment: Option<u64>,
        #[arg(short = 'b', long)]
        body: String,
    },
    /// List PR comments and their replyable GitHub IDs.
    Comments { url: String },
    /// Show CI checks for a pull request, optionally until they complete.
    Checks {
        url: String,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 10)]
        interval: u64,
    },
    /// Rerun failed jobs in a GitHub Actions workflow run.
    Rerun { url: String, run_id: u64 },
    /// Download and extract logs for a GitHub Actions workflow run.
    Logs { url: String, run_id: u64 },
}

#[derive(Clone, clap::Args)]
pub(crate) struct McpAgentToolsArgs {
    #[arg(long = "agent-id")]
    agent_id: Option<String>,
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
}

#[derive(Clone, clap::Args)]
pub(crate) struct LandArgs {
    /// Checkout path to land from (defaults to the current directory).
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
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
            CliCommand::Daemon(args) => Command::Daemon(args),
            CliCommand::Debug(args) => Command::Debug(args),
            CliCommand::Desk(args) => Command::Desk(args),
            CliCommand::Iroh(args) => Command::Iroh(args),
            CliCommand::Land(args) => Command::Land(args),
            CliCommand::McpAgentTools(args) => Command::McpAgentTools(args),
            CliCommand::Pr(args) => Command::Pr(args),
            CliCommand::RecordVisualization(args) => Command::RecordVisualization(args),
            CliCommand::ProtocolLog(args) => Command::ProtocolLog(args),
            CliCommand::Wayland(args) => Command::Wayland(args),
        };
        Ok(Self { command })
    }
}
