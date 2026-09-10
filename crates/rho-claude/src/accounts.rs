//! Claude accounts: one directory per account, each a whole
//! `CLAUDE_CONFIG_DIR` layout (credentials, `.claude.json`, settings).
//!
//! An agent never points Claude at its account directory by path. The
//! account is bind-mounted over the config home inside the agent's view
//! namespace, so every account sees the same `~/.claude` and transcripts
//! stay findable at one host path. Claude's default layout keeps
//! `.claude.json` in `$HOME` rather than in the config directory, which no
//! mount over `~/.claude` could redirect, so spawned processes always get
//! `CLAUDE_CONFIG_DIR` set to the mount point: that is what pulls the
//! account file inside the directory Rho controls.

use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// What the accounts directory is called, beside the config home.
const ACCOUNTS_DIR_SUFFIX: &str = "-accounts";

/// The account the host's own configuration is migrated into, so that a
/// machine that has been running Rho keeps its login and its agents keep
/// their transcripts.
pub const DEFAULT_ACCOUNT: &str = "default";

/// The Claude directories one process works in: the config home every agent
/// sees as `~/.claude`, and the accounts tree beside it.
///
/// Resolved once, in a binary's `main`, and passed down from there. Nothing
/// else in this crate reads `$HOME` or `$CLAUDE_CONFIG_DIR`. A library that
/// resolves the home directory itself puts every caller on the user's live
/// configuration whether it meant to be there or not: that is how a daemon
/// pointed at a rig's state directory, but started by hand rather than by
/// `rho-qa rig up`, read the user's own `~/.claude/projects` and rebuilt
/// agent rows from the user's transcripts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudePaths {
    config_home: Utf8PathBuf,
    accounts_root: Utf8PathBuf,
}

impl ClaudePaths {
    /// The user's own, read from the environment: `$CLAUDE_CONFIG_DIR` when
    /// it is set and `~/.claude` otherwise, with accounts in
    /// `~/.claude-accounts`. The only function here that reads the
    /// environment, and a binary's `main` is the only place to call it.
    pub fn from_env() -> Result<Self> {
        let home = Utf8PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
        let config_home = match std::env::var("CLAUDE_CONFIG_DIR") {
            Ok(dir) => Utf8PathBuf::from(dir),
            Err(_) => home.join(".claude"),
        };
        Ok(Self {
            accounts_root: home.join(format!(".claude{ACCOUNTS_DIR_SUFFIX}")),
            config_home,
        })
    }

    /// Rooted at `dir`: the config home is `dir` itself and accounts live
    /// beside it as `<dir>-accounts`. What a rig passes, so a rig's Claude
    /// state is under the rig and nowhere near the user's.
    pub fn at(dir: impl Into<Utf8PathBuf>) -> Self {
        let config_home: Utf8PathBuf = dir.into();
        let name = config_home.file_name().unwrap_or("claude").to_owned();
        let accounts_root = config_home.with_file_name(format!("{name}{ACCOUNTS_DIR_SUFFIX}"));
        Self {
            config_home,
            accounts_root,
        }
    }

    /// The single path an agent's Claude sees as its config directory, and
    /// the value Rho passes as `CLAUDE_CONFIG_DIR`.
    pub fn config_home(&self) -> &Utf8Path {
        &self.config_home
    }

    /// Where the daemon reads transcripts from, since `projects/` is shared
    /// across accounts.
    pub fn projects(&self) -> Utf8PathBuf {
        self.config_home.join("projects")
    }

    /// The account directory for `name`, whether or not it exists yet.
    pub fn account_dir(&self, name: &str) -> Result<Utf8PathBuf> {
        anyhow::ensure!(
            valid_name(name),
            "Claude account name must be 1-64 characters of [A-Za-z0-9._-] \
             and cannot start with a dot: {name:?}"
        );
        Ok(self.accounts_root.join(name))
    }

    /// The accounts that exist now, sorted. Empty when no account directory
    /// has been made: Rho then runs Claude the way it always did, on the
    /// host's own `~/.claude`.
    pub fn list(&self) -> Result<Vec<String>> {
        let root = &self.accounts_root;
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).context(format!("read Claude accounts directory {root}"));
            }
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

    /// Makes the account directory and the paths the namespace mounts land
    /// on. Bind mounts need their targets to exist already, and `projects/`
    /// must be a real directory for the shared transcript tree to cover it.
    pub fn prepare(&self, name: &str) -> Result<Utf8PathBuf> {
        let dir = self.account_dir(name)?;
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

    /// Makes sure the account agents run on exists, so that "every Claude
    /// agent has an account" holds from the first spawn. Which account that
    /// is lives in the daemon's store, not here.
    ///
    /// Nothing is copied out of `~/.claude`: filling [`DEFAULT_ACCOUNT`]
    /// with a login is the person's own move, whether by copying their
    /// configuration in or by `rho claude-account login default`. An empty
    /// account directory makes an agent start unauthenticated, not silently
    /// run as someone else.
    pub fn bootstrap(&self, account: &str) -> Result<()> {
        std::fs::create_dir_all(&self.accounts_root)
            .context("create the Claude accounts directory")?;
        self.prepare(account)?;
        Ok(())
    }
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

#[cfg(test)]
mod paths_tests {
    use super::*;

    /// A rig's paths are the rig's. Nothing here consults `$HOME`, which is
    /// what a daemon started by hand against a rig's state directory used to
    /// fall back to.
    #[test]
    fn paths_rooted_at_a_rig_stay_under_it() {
        let paths = ClaudePaths::at("/rigs/mv/config/claude");
        assert_eq!(paths.config_home(), "/rigs/mv/config/claude");
        assert_eq!(paths.projects(), "/rigs/mv/config/claude/projects");
        assert_eq!(
            paths.account_dir("default").unwrap(),
            "/rigs/mv/config/claude-accounts/default"
        );
    }

    /// The user's layout is the one that was there before: accounts beside
    /// `~/.claude`, not inside it, because the account directory is mounted
    /// over the config home and would cover them.
    #[test]
    fn the_accounts_tree_sits_beside_the_config_home() {
        let paths = ClaudePaths::at("/home/someone/.claude");
        assert_eq!(
            paths.account_dir("work").unwrap(),
            "/home/someone/.claude-accounts/work"
        );
    }
}
