//! Claude accounts: one directory per account, each a whole
//! `CLAUDE_CONFIG_DIR` layout (credentials, `.claude.json`, settings).
//!
//! An agent never points Claude at its account directory by path. The
//! account is bind-mounted over [`config_home`] inside the agent's view
//! namespace, so every account sees the same `~/.claude` and transcripts
//! stay findable at one host path. Claude's default layout keeps
//! `.claude.json` in `$HOME` rather than in the config directory, which no
//! mount over `~/.claude` could redirect, so spawned processes always get
//! `CLAUDE_CONFIG_DIR` set to the mount point: that is what pulls the
//! account file inside the directory Rho controls.

use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// Where account directories live, relative to `$HOME`.
const ACCOUNTS_DIR_NAME: &str = ".claude-accounts";

/// The account the host's own configuration is migrated into, so that a
/// machine that has been running Rho keeps its login and its agents keep
/// their transcripts.
pub const DEFAULT_ACCOUNT: &str = "default";

/// The single path an agent's Claude sees as its config directory, and the
/// value Rho passes as `CLAUDE_CONFIG_DIR`. Also where the daemon reads
/// transcripts from, since `projects/` is shared across accounts.
pub fn config_home() -> Result<Utf8PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Ok(Utf8PathBuf::from(dir));
    }
    Ok(home()?.join(".claude"))
}

/// The account directory for `name`, whether or not it exists yet.
pub fn account_dir(name: &str) -> Result<Utf8PathBuf> {
    anyhow::ensure!(
        valid_name(name),
        "Claude account name must be 1-64 characters of [A-Za-z0-9._-] \
         and cannot start with a dot: {name:?}"
    );
    Ok(accounts_root()?.join(name))
}

/// The accounts that exist now, sorted. Empty when no account directory has
/// been made: Rho then runs Claude the way it always did, on the host's own
/// `~/.claude`.
pub fn list() -> Result<Vec<String>> {
    let root = accounts_root()?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context(format!("read Claude accounts directory {root}")),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.context("read Claude account entry")?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if valid_name(&name) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Rho's own MCP server, under the name Claude records it by.
const MCP_SERVER_NAME: &str = "rho";

/// Registers Rho's MCP server in the account, if it is not there already.
///
/// Claude keeps MCP registrations in `.claude.json`, the same file that
/// carries the login, so the registration is per account: an account made by
/// `rho claude-account login` would otherwise start its agents without their
/// Rho tools. Written by rename and only when missing, since Claude writes
/// that file too.
fn ensure_mcp_server(dir: &Utf8Path) -> Result<()> {
    let path = dir.join(".claude.json");
    let mut config: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse Claude account configuration {path}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error).with_context(|| format!("read {path}")),
    };
    let servers = config
        .as_object_mut()
        .with_context(|| format!("Claude account configuration {path} is not an object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .with_context(|| format!("mcpServers in {path} is not an object"))?;
    if servers.contains_key(MCP_SERVER_NAME) {
        return Ok(());
    }
    servers.insert(
        MCP_SERVER_NAME.to_owned(),
        serde_json::json!({
            "type": "stdio",
            "command": "rho",
            "args": ["mcp-agent-tools"],
            "env": {},
        }),
    );
    let staged = dir.join(".claude.json.rho-staged");
    std::fs::write(&staged, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("stage {staged}"))?;
    std::fs::rename(&staged, &path).with_context(|| format!("write {path}"))?;
    Ok(())
}

/// Makes the account directory and the paths the namespace mounts land on.
/// Bind mounts need their targets to exist already, and `projects/` must be
/// a real directory for the shared transcript tree to cover it.
pub fn prepare(name: &str) -> Result<Utf8PathBuf> {
    let dir = account_dir(name)?;
    std::fs::create_dir_all(dir.join("projects"))
        .with_context(|| format!("create Claude account directory {dir}"))?;
    let prompt = dir.join("CLAUDE.md");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&prompt)
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create Claude prompt mount target {prompt}"));
        }
    }
    ensure_mcp_server(&dir)?;
    Ok(dir)
}

/// Makes sure the account agents run on exists, so that "every Claude agent
/// has an account" holds from the first spawn. Which account that is lives
/// in the daemon's store, not here.
///
/// Nothing is copied out of `~/.claude`: filling [`DEFAULT_ACCOUNT`] with a
/// login is the person's own move, whether by copying their configuration
/// in or by `rho claude-account login default`. An empty account directory
/// makes an agent start unauthenticated, not silently run as someone else.
pub fn bootstrap(account: &str) -> Result<()> {
    std::fs::create_dir_all(accounts_root()?).context("create the Claude accounts directory")?;
    prepare(account)?;
    Ok(())
}

fn accounts_root() -> Result<Utf8PathBuf> {
    Ok(home()?.join(ACCOUNTS_DIR_NAME))
}

fn home() -> Result<Utf8PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(Utf8PathBuf::from(home))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn names_that_could_escape_the_accounts_directory_are_rejected() {
        assert!(valid_name("work"));
        assert!(valid_name("personal-2"));
        assert!(!valid_name(".."));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(""));
    }
}
