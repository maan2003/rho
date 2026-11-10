//! What a workset's calls go through, for one run of its process: Python
//! sets it before the first agent starts, or it is `Bypass` from then on
//! ([ARCH-agent-host](../../../specs/ARCH-agent-host.md)).

use std::sync::{Arc, Mutex};

use rho_agent_types::Props;

/// The workset prop that names the policy `init.py` applies. A workset that
/// has one never falls back to `Bypass`: if nothing set its permissions,
/// for example because `init.py` failed, no agent starts.
pub(crate) const PROFILE: &str = "profile";

/// What Claude Code makes of an agent's calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Permissions {
    /// Every call runs.
    Bypass,
    /// Claude Code's auto mode reviews each call. `environment` is the
    /// review's `autoMode.environment`, or `None` for Claude Code's own.
    /// Only Claude agents run, since nothing reviews a native agent's calls.
    Auto { environment: Option<Vec<String>> },
}

/// The workset's permissions: open until set or until its first agent,
/// then fixed for the run.
#[derive(Default)]
pub(crate) struct Cell(Mutex<Option<Arc<Permissions>>>);

impl Cell {
    /// Python's choice, before any agent starts and once.
    pub(crate) fn set(&self, permissions: Permissions) -> anyhow::Result<()> {
        let mut fixed = self.0.lock().expect("poison");
        anyhow::ensure!(
            fixed.is_none(),
            "the workset's permissions are set once, before its first agent starts"
        );
        *fixed = Some(Arc::new(permissions));
        Ok(())
    }

    /// The permissions an agent starts under, fixing them if Python set
    /// none.
    pub(crate) fn fix(&self, props: &Props) -> anyhow::Result<Arc<Permissions>> {
        let mut fixed = self.0.lock().expect("poison");
        if let Some(permissions) = fixed.as_ref() {
            return Ok(permissions.clone());
        }
        if let Some(profile) = props.get(PROFILE) {
            anyhow::bail!(
                "the workset names profile {profile:?}, but nothing set its permissions; \
                 its policy, such as init.py, must call rho.workset.set_permissions"
            );
        }
        Ok(fixed.insert(Arc::new(Permissions::Bypass)).clone())
    }

    /// The permissions, once fixed.
    pub(crate) fn get(&self) -> Option<Arc<Permissions>> {
        self.0.lock().expect("poison").clone()
    }
}

/// The process's: one workset per process.
pub(crate) static PERMISSIONS: std::sync::LazyLock<Cell> = std::sync::LazyLock::new(Cell::default);

/// Refuses an agent the permissions do not run.
pub(crate) fn admits(permissions: &Permissions, native: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        !(native && matches!(permissions, Permissions::Auto { .. })),
        "an auto workset holds Claude agents only"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> Permissions {
        Permissions::Auto {
            environment: Some(vec!["Trusted: x".into()]),
        }
    }

    /// Python's choice holds for the run, and nothing changes it after.
    #[test]
    fn permissions_are_set_once_and_fixed_by_the_first_agent() {
        let cell = Cell::default();
        cell.set(auto()).unwrap();
        assert!(cell.set(Permissions::Bypass).is_err());
        assert_eq!(*cell.fix(&Props::new()).unwrap(), auto());

        let cell = Cell::default();
        assert_eq!(*cell.fix(&Props::new()).unwrap(), Permissions::Bypass);
        assert!(cell.set(auto()).is_err(), "an agent started: too late");
        assert_eq!(*cell.get().unwrap(), Permissions::Bypass);
    }

    /// A workset with a profile does not fall back to bypass when its
    /// policy failed to set anything, and stays open for a later set.
    #[test]
    fn a_workset_with_a_profile_needs_its_permissions_set() {
        let cell = Cell::default();
        let props = Props::from([(PROFILE.to_owned(), "example".to_owned())]);
        assert!(cell.fix(&props).is_err());
        assert!(cell.get().is_none());
        cell.set(auto()).unwrap();
        assert_eq!(*cell.fix(&props).unwrap(), auto());
    }

    #[test]
    fn an_auto_workset_admits_claude_agents_only() {
        assert!(admits(&auto(), true).is_err());
        assert!(admits(&auto(), false).is_ok());
        assert!(admits(&Permissions::Bypass, true).is_ok());
    }
}
