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
    let program =
        std::env::var_os("RHO_AGENT_DESKTOP").unwrap_or_else(|| "rho-agent-desktop".into());
    let status = Command::new(program)
        .arg("wayland")
        .args(args.args)
        .status()
        .context(
            "start rho-agent-desktop; install the desktop companion or set RHO_AGENT_DESKTOP",
        )?;
    anyhow::ensure!(status.success(), "rho-agent-desktop exited with {status}");
    Ok(())
}
