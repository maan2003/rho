//! Registered Slack workspaces and the credentials that reach them.
//!
//! Rho signs in as the person, with the same `xoxc` token and `d` cookie the
//! desktop client holds, so the store is as sensitive as a password file:
//! it is written owner-only, in the client state directory, and never leaves
//! the client.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The human name of a registered workspace ("acme"). This is the only Slack
/// identity that is ever shown, so it is what the user types.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceName(pub String);

impl std::fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub workspace: WorkspaceName,
    /// The web client's `xoxc-…` token.
    pub token: String,
    /// The value of the `d` cookie that authenticates that token.
    pub cookie: String,
}

impl Credentials {
    /// Accepts what the user can actually copy out of a browser: the token as
    /// shown, and the cookie either bare or as the `d=…;` pair from the
    /// cookie editor. Rejecting a stray `d=` prefix would be a puzzle, not a
    /// safeguard.
    pub fn parse(workspace: &str, token: &str, cookie: &str) -> anyhow::Result<Self> {
        let workspace = workspace.trim();
        anyhow::ensure!(!workspace.is_empty(), "workspace name is empty");
        let token = token.trim();
        anyhow::ensure!(
            token.starts_with("xoxc-"),
            "expected an xoxc token from the web client"
        );
        let cookie = cookie.trim().trim_end_matches(';').trim();
        let cookie = cookie.strip_prefix("d=").unwrap_or(cookie).trim();
        anyhow::ensure!(!cookie.is_empty(), "the d cookie is empty");
        Ok(Self {
            workspace: WorkspaceName(workspace.to_owned()),
            token: token.to_owned(),
            cookie: cookie.to_owned(),
        })
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    workspaces: BTreeMap<String, StoredCredentials>,
}

#[derive(Deserialize, Serialize)]
struct StoredCredentials {
    token: String,
    cookie: String,
}

/// Every registered workspace, ordered by name so the prompt and the surface
/// list them the same way.
pub struct CredentialStore {
    path: PathBuf,
    stored: Stored,
}

impl CredentialStore {
    /// The client state directory, or wherever `RHO_SLACK_CREDENTIALS` points.
    /// The override exists so an isolated run (QA, a second profile) cannot
    /// touch the real workspaces.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        if let Some(path) =
            std::env::var_os("RHO_SLACK_CREDENTIALS").filter(|path| !path.is_empty())
        {
            return Ok(PathBuf::from(path));
        }
        let base = dirs::state_dir().context("state directory not available")?;
        Ok(base.join("rho/slack-credentials.json"))
    }

    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(Self::default_path()?)
    }

    /// Reads the store, treating "not there yet" as an empty store: the first
    /// registration creates the file.
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let stored = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        Ok(Self { path, stored })
    }

    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceName> + '_ {
        self.stored
            .workspaces
            .keys()
            .map(|name| WorkspaceName(name.clone()))
    }

    pub fn get(&self, workspace: &WorkspaceName) -> Option<Credentials> {
        let stored = self.stored.workspaces.get(&workspace.0)?;
        Some(Credentials {
            workspace: workspace.clone(),
            token: stored.token.clone(),
            cookie: stored.cookie.clone(),
        })
    }

    pub fn all(&self) -> Vec<Credentials> {
        self.workspaces()
            .filter_map(|workspace| self.get(&workspace))
            .collect()
    }

    /// Registers or replaces one workspace. Re-registering is how a rotated
    /// token is entered, so it overwrites rather than erroring.
    pub fn register(&mut self, credentials: Credentials) -> anyhow::Result<()> {
        self.stored.workspaces.insert(
            credentials.workspace.0,
            StoredCredentials {
                token: credentials.token,
                cookie: credentials.cookie,
            },
        );
        self.save()
    }

    pub fn forget(&mut self, workspace: &WorkspaceName) -> anyhow::Result<bool> {
        if self.stored.workspaces.remove(&workspace.0).is_none() {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            restrict(parent, 0o700)?;
        }
        let contents = serde_json::to_string_pretty(&self.stored)?;
        // Write through a temporary file in the same directory so a crash
        // mid-write cannot leave a half-parsed credential file behind, and
        // narrow its permissions before any secret reaches it.
        let temporary = self.path.with_extension("json.new");
        std::fs::write(&temporary, "")
            .with_context(|| format!("creating {}", temporary.display()))?;
        restrict(&temporary, 0o600)?;
        std::fs::write(&temporary, contents)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        restrict(&self.path, 0o600)
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_pasted_cookie_pair_and_rejects_a_bot_token() {
        let credentials = Credentials::parse("acme", " xoxc-123 ", " d=abc; ").unwrap();
        assert_eq!(credentials.workspace, WorkspaceName("acme".into()));
        assert_eq!(credentials.token, "xoxc-123");
        assert_eq!(credentials.cookie, "abc");

        let error = Credentials::parse("acme", "xoxb-123", "abc").unwrap_err();
        assert!(error.to_string().contains("xoxc"));
        assert!(
            Credentials::parse("acme", "xoxc-1", "  ")
                .unwrap_err()
                .to_string()
                .contains("cookie")
        );
        assert!(
            Credentials::parse(" ", "xoxc-1", "abc")
                .unwrap_err()
                .to_string()
                .contains("workspace")
        );
    }

    #[test]
    fn several_workspaces_persist_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state/slack-credentials.json");
        let mut store = CredentialStore::open(&path).unwrap();
        store
            .register(Credentials::parse("acme", "xoxc-a", "cookie-a").unwrap())
            .unwrap();
        store
            .register(Credentials::parse("borg", "xoxc-b", "cookie-b").unwrap())
            .unwrap();

        let reopened = CredentialStore::open(&path).unwrap();
        assert_eq!(
            reopened.workspaces().collect::<Vec<_>>(),
            vec![WorkspaceName("acme".into()), WorkspaceName("borg".into())]
        );
        assert_eq!(
            reopened.get(&WorkspaceName("borg".into())).unwrap().token,
            "xoxc-b"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials must be owner-only");
            let parent = std::fs::metadata(path.parent().unwrap()).unwrap();
            assert_eq!(parent.permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn re_registering_replaces_a_rotated_token() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("slack-credentials.json");
        let mut store = CredentialStore::open(&path).unwrap();
        store
            .register(Credentials::parse("acme", "xoxc-old", "cookie").unwrap())
            .unwrap();
        store
            .register(Credentials::parse("acme", "xoxc-new", "cookie").unwrap())
            .unwrap();
        assert_eq!(store.workspaces().count(), 1);
        assert_eq!(
            store.get(&WorkspaceName("acme".into())).unwrap().token,
            "xoxc-new"
        );
        assert!(store.forget(&WorkspaceName("acme".into())).unwrap());
        assert!(!store.forget(&WorkspaceName("acme".into())).unwrap());
        assert!(CredentialStore::open(&path).unwrap().all().is_empty());
    }
}
