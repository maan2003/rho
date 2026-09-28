//! Code-first policy shared by the native provider and Claude Code drivers.
//! Transport correlation and conversation ownership stay in the drivers.

pub(crate) mod mailroom;
pub(crate) mod python_preview;
pub(crate) mod tools;
pub(crate) mod wake;

use rho_agent_types::UnixMs;
use rho_notebook::{CellHandle, Notebook, SourceFacts};

use crate::worker::native::notebook::{CellSide, NotebookSide};

/// The model's attention, independent of how a provider delivers its replies.
/// A response is not a task ending, and a report is not a task finishing.
#[derive(Default)]
pub(crate) struct Progress {
    pub last_response: Option<UnixMs>,
    pub told_returned: bool,
    pub prose: u32,
    /// The model called `end_turn()`: until it is woken, no check-in.
    pub ended: bool,
}

impl Progress {
    pub const MAX_PROSE: u32 = 3;

    pub fn facts(&self, notebook: Option<&Notebook>, latest: Option<&CellHandle>) -> wake::Facts {
        let sources = notebook.map(Notebook::facts).unwrap_or_default();
        let wait = notebook
            .map(Notebook::checkin)
            .unwrap_or(wake::DEFAULT_CHECKIN);
        self.source_facts(&sources, wait, latest.map(CellHandle::facts))
    }

    pub fn process_facts(
        &self,
        notebook: Option<&NotebookSide>,
        latest: Option<&CellSide>,
    ) -> anyhow::Result<wake::Facts> {
        let sources = notebook
            .map(NotebookSide::facts)
            .transpose()?
            .unwrap_or_default();
        let wait = notebook
            .map(NotebookSide::checkin)
            .transpose()?
            .unwrap_or(wake::DEFAULT_CHECKIN);
        Ok(self.source_facts(&sources, wait, latest.map(CellSide::facts).transpose()?))
    }

    fn source_facts(
        &self,
        sources: &[SourceFacts],
        wait: std::time::Duration,
        latest: Option<SourceFacts>,
    ) -> wake::Facts {
        wake::Facts {
            finished: latest
                .and_then(|facts| facts.finished)
                .filter(|end| !end.failed && !self.told_returned)
                .map(|end| end.at),
            notified: sources
                .iter()
                .filter_map(|facts| facts.notified_at.into_iter().chain(facts.paged_at).min())
                .min(),
            failure: sources
                .iter()
                .filter(|facts| !facts.delivered && facts.finished.is_some_and(|end| end.failed))
                .filter_map(|facts| facts.finished.map(|end| end.at))
                .min(),
            checkin: self
                .last_response
                .filter(|_| !self.ended)
                .map(|at| at + wait),
            response_finished: self.last_response,
            prose: self.prose > 0,
            ..Default::default()
        }
    }
}
