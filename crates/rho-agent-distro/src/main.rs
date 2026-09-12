use std::path::PathBuf;

use anyhow::Context as _;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let root = arguments
        .next()
        .map(PathBuf::from)
        .context("usage: rho-agent-distro IMAGE_ROOT [EXTRA_NIX_PACKAGE ...]")?;
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let mut packages = rho_agent_distro::resolve_programs(rho_agent_distro::PROGRAMS, &path)?;
    packages.extend(arguments.map(PathBuf::from));
    rho_agent_distro::build_image(&root, &packages)?;
    Ok(())
}
