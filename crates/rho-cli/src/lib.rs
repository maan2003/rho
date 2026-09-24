//! The `rho` command: daemon launcher and utility subcommands.
//!
//! Interactive use lives in rho-gui; this binary hosts the daemon itself
//! plus the terminal-friendly plumbing around it — auth, PR and debug
//! tools.

use std::io;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use rho_agent_host_proto::client::Client as UiClient;
use rho_agent_host_proto::{Answer, Call, agents, client, host};
use rho_daemon::DaemonArgs;
use rho_daemon::debug::DebugArgs;
use rho_inference::{AuthArgs, run_auth_cli};

mod eval;
mod pr;
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
    if let Command::Daemon(mut daemon_args) = args.command {
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
        Command::ClaudeAccount(args) => run_claude_account(args).await,
        Command::Daemon(_) => unreachable!("daemon runs before the shared async runtime"),
        Command::Debug(args) => {
            rho_daemon::debug::run(args).await?;
            Ok(())
        }
        Command::Eval(args) => eval::run(args).await,
        Command::Iroh(args) => run_iroh(args).await,
        Command::Pr(args) => pr::run(args).await,
        Command::RecordVisualization(args) => visualization::run(args).await,
        Command::Wayland(_) => unreachable!("wayland runs before the shared async runtime"),
        Command::ProtocolLog(args) => {
            let mut stdout = io::stdout().lock();
            rho_agent_host_proto::print_protocol_log(&args.path, &mut stdout, describe_frame)?;
            Ok(())
        }
    }
}

/// A protocol log frame, read as the part it belongs to.
fn describe_frame(open: &rho_agent_host_proto::Open, reply: Option<&[u8]>) -> String {
    use rho_agent_host_proto::{Part, describe_as, desk, shell};
    match open.part {
        Part::Agents => describe_as::<agents::Open>(open, reply),
        Part::Desk => describe_as::<desk::Open>(open, reply),
        Part::Host => describe_as::<host::Open>(open, reply),
        Part::Terminal => describe_as::<rho_terminal::protocol::Open>(open, reply),
        Part::Shell => describe_as::<shell::Open>(open, reply),
        Part::Workspace => describe_as::<rho_files::protocol::Open>(open, reply),
    }
}

/// Approves a pending iroh enrollment over the daemon's Unix socket, so
/// trust decisions always come from a local user on the daemon host.
async fn run_iroh(args: IrohArgs) -> Result<()> {
    let socket_path = rho_agent_host_proto::RuntimePaths::resolve(args.socket_path)?
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

/// One call of the daemon, starting the daemon if it is not running. A
/// refusal is an error.
pub(crate) async fn daemon_call<C: Call>(
    socket_path: &std::path::Path,
    call: C,
) -> Result<C::Reply> {
    let mut daemon = connect_or_start_daemon(socket_path).await?;
    daemon.open(&call.open()).await?;
    daemon.recv::<Answer<C::Reply>>().await?.into_result()
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
    ClaudeAccount(ClaudeAccountArgs),
    Daemon(DaemonArgs),
    Debug(DebugArgs),
    /// Run a headless agent evaluation; JSONL output, temporary state, real
    /// provider.
    Eval(eval::EvalArgs),
    Iroh(IrohArgs),
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
    /// Manage the Claude accounts agents run on.
    ClaudeAccount(ClaudeAccountArgs),
    Daemon(DaemonArgs),
    Debug(DebugArgs),
    /// Run a headless agent evaluation; JSONL output, temporary state, real
    /// provider.
    Eval(eval::EvalArgs),
    Iroh(IrohArgs),
    Pr(PrArgs),
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
/// agents run on is the daemon's, so listing and switching go through it.
/// A login names the directory in `CLAUDE_CONFIG_DIR` because there is no
/// view namespace outside an agent; agents get the same directory by mount.
async fn run_claude_account(args: ClaudeAccountArgs) -> Result<()> {
    let socket_path = rho_agent_host_proto::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    let list = match &args.command {
        ClaudeAccountCommand::List => daemon_call(&socket_path, agents::ClaudeAccounts).await?,
        ClaudeAccountCommand::Use { name } => {
            let call = agents::SetClaudeAccount { name: name.clone() };
            daemon_call(&socket_path, call).await?
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
            CliCommand::Daemon(args) => Command::Daemon(args),
            CliCommand::Debug(args) => Command::Debug(args),
            CliCommand::Eval(args) => Command::Eval(args),
            CliCommand::Iroh(args) => Command::Iroh(args),
            CliCommand::Pr(args) => Command::Pr(args),
            CliCommand::RecordVisualization(args) => Command::RecordVisualization(args),
            CliCommand::ProtocolLog(args) => Command::ProtocolLog(args),
            CliCommand::Wayland(args) => Command::Wayland(args),
        };
        Ok(Self { command })
    }
}
