//! Rho's Claude Code plugin, whose source is `plugin/`: a hooks module that
//! moves the agent's prompt from the first user message into the system
//! prompt, in place of the Claude Code sections that contradict it, and
//! gives the agent its skills back after a compaction.

use anyhow::{Context as _, Result};
use camino::Utf8Path;

/// The environment variable naming the file that lists the agent's loaded
/// skill directories, one per line. The hooks module reads it after a
/// compaction.
pub const SKILLS_ENV: &str = "RHO_SKILLS_LIST";

const FILES: &[(&str, &str)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../plugin/.claude-plugin/plugin.json"),
    ),
    (
        "hooks/hooks.json",
        include_str!("../plugin/hooks/hooks.json"),
    ),
    (
        "hooks/register.ts",
        include_str!("../plugin/hooks/register.ts"),
    ),
];

/// Writes the plugin into `dir`, for [`crate::ClaudeCodeOptions::plugin_dir`].
pub fn write(dir: &Utf8Path) -> Result<()> {
    for (path, contents) in FILES {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("plugin files are in directories"))
            .with_context(|| format!("create {path}"))?;
        std::fs::write(&path, contents).with_context(|| format!("write {path}"))?;
    }
    Ok(())
}
