//! `rho` alone: a link for the GUI to pair with this machine's agent host.
//! The host that runs here prints it; with none running, `rho` starts one
//! for as long as it runs itself, and Ctrl-C stops both.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use rho_agent_hosts::protocol as host;
use rho_rpc::protocol::client::{self, Client};

#[derive(Clone, Default, clap::Args)]
pub(crate) struct ShareArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
}

pub(crate) async fn run(args: ShareArgs) -> Result<()> {
    let socket = rho_rpc::protocol::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    if let Ok(mut probe) = Client::connect(&socket).await {
        let _ = probe.shutdown().await;
        let link = client::call(&socket, host::IrohPair).await?;
        print_link(&link);
        return Ok(());
    }
    let mut child = start_host(&socket).await?;
    let link = client::call(&socket, host::IrohPair).await?;
    print_link(&link);
    // Whatever stops this process stops the host it started: Ctrl-C, a
    // closed terminal, or a kill.
    use tokio::signal::unix::{SignalKind, signal};
    let (mut hangup, mut terminate) = (
        signal(SignalKind::hangup())?,
        signal(SignalKind::terminate())?,
    );
    tokio::select! {
        status = child.wait() => {
            let status = status?;
            anyhow::ensure!(status.success(), "the agent host exited with {status}");
            return Ok(());
        }
        _ = tokio::signal::ctrl_c() => {}
        _ = hangup.recv() => {}
        _ = terminate.recv() => {}
    }
    if let Some(pid) = child.id() {
        // SAFETY: a signal to our own child, which has not been reaped.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    child.wait().await?;
    Ok(())
}

/// The link alone. On a terminal it is also an OSC 8 hyperlink, since no
/// terminal's own URL detection knows `rho://`.
fn print_link(link: &str) {
    use std::io::IsTerminal as _;
    if std::io::stdout().is_terminal() {
        println!("\x1b]8;;{link}\x1b\\{link}\x1b]8;;\x1b\\");
    } else {
        println!("{link}");
    }
}

/// An agent host that listens over iroh, its output in a log beside its
/// socket so that the terminal stays the link's.
async fn start_host(socket: &Path) -> Result<tokio::process::Child> {
    let program = std::env::current_exe()?.with_file_name("rho-agent-host");
    let log_path = socket.with_file_name("rho-agent-host.log");
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("create {}", log_path.display()))?;
    let mut child = tokio::process::Command::new(&program);
    child
        .arg("--socket-path")
        .arg(socket)
        .arg("--iroh")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // A kill this process cannot see (SIGKILL, a closed terminal that
    // kills the session) still takes the host with it.
    // SAFETY: prctl only, which is async-signal-safe, between fork and exec.
    unsafe {
        child.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child
        .spawn()
        .with_context(|| format!("start {}", program.display()))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match Client::connect(socket).await {
            Ok(mut probe) => {
                let _ = probe.shutdown().await;
                return Ok(child);
            }
            Err(error) => {
                let exited = child.try_wait()?;
                if exited.is_none() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                let how = match exited {
                    Some(status) => format!("the agent host exited with {status}"),
                    None => "the agent host did not come up in 20 s".to_owned(),
                };
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                let tail = log.lines().rev().take(20).collect::<Vec<_>>();
                let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                return Err(
                    error.context(format!("{how}; the end of {}:\n{tail}", log_path.display()))
                );
            }
        }
    }
}
