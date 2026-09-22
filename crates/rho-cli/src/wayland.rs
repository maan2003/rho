//! Compatibility entry point; the desktop program owns the driver.
use std::ffi::OsString;
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Clone, clap::Args)]
#[command(disable_help_flag = true)]
pub struct WaylandArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}
pub fn run(args: WaylandArgs) -> Result<()> {
    anyhow::ensure!(cfg!(target_os = "linux"), "agent desktops require Linux");
    let program = concat!(env!("RHO_AGENT_BASE"), "/bin/rho-agent-desktop");
    let status = Command::new(program)
        .arg("wayland")
        .args(args.args)
        .status()
        .context("start bundled rho-agent-desktop")?;
    anyhow::ensure!(status.success(), "rho-agent-desktop exited with {status}");
    Ok(())
}
