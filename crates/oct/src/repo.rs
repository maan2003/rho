use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;

static REMOTE_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"github\.com[:/]([^/]+)/([^/.]+?)(?:\.git)?$").unwrap());

pub fn resolve_repo() -> Result<(String, String)> {
    let output = std::process::Command::new("jj")
        .args(["git", "remote", "list"])
        .output()
        .context("failed to run jj")?;

    if !output.status.success() {
        anyhow::bail!("jj git remote list failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let remote_url = stdout
        .lines()
        .find(|l| l.starts_with("origin"))
        .and_then(|l| l.split_whitespace().nth(1))
        .context("origin remote not found")?;

    let caps = REMOTE_URL_RE
        .captures(remote_url)
        .context("could not parse GitHub remote URL")?;
    Ok((caps[1].to_string(), caps[2].to_string()))
}

pub fn resolve_default_base_branch() -> Result<String> {
    let output = std::process::Command::new("jj")
        .args([
            "log",
            "-r",
            "trunk()",
            "--no-graph",
            "-T",
            "bookmarks.first().name()",
        ])
        .output()
        .context("failed to run jj")?;

    if !output.status.success() {
        anyhow::bail!("jj log -r trunk() failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let bookmark = stdout.trim();
    if bookmark.is_empty() {
        anyhow::bail!("no bookmark found for trunk()");
    }

    Ok(bookmark.to_string())
}
