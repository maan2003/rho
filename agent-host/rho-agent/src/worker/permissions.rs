//! What a workset's calls go through, for one run of its process: Python
//! sets it before the first agent starts, or it is `Bypass` from then on
//! ([ARCH-agent-host](../../../specs/ARCH-agent-host.md)).

use std::sync::{Arc, Mutex};

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

    /// The permissions an agent starts under, fixing `Bypass` if Python
    /// set none.
    pub(crate) fn fix(&self) -> Arc<Permissions> {
        self.0
            .lock()
            .expect("poison")
            .get_or_insert_with(|| Arc::new(Permissions::Bypass))
            .clone()
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
        assert_eq!(*cell.fix(), auto());

        let cell = Cell::default();
        assert_eq!(*cell.fix(), Permissions::Bypass);
        assert!(cell.set(auto()).is_err(), "an agent started: too late");
        assert_eq!(*cell.get().unwrap(), Permissions::Bypass);
    }

    #[test]
    fn an_auto_workset_admits_claude_agents_only() {
        assert!(admits(&auto(), true).is_err());
        assert!(admits(&auto(), false).is_ok());
        assert!(admits(&Permissions::Bypass, true).is_ok());
    }
}
