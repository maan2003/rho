//! The agent owns observations and decisions, not the interpreter. Tests may
//! embed the notebook; a workset worker talks to its checkpointable process.
use std::sync::Arc;
use std::time::Duration;

use rho_notebook::process::Client;
use rho_notebook::{CellHandle, Notebook, Report, SourceFacts};

pub(crate) enum NotebookSide {
    Local(Notebook),
    Process(Arc<Client>),
}

pub(crate) enum CellSide {
    Local(CellHandle),
    Process { client: Arc<Client>, id: u64 },
}

impl NotebookSide {
    pub(crate) fn run(&self, code: String) -> anyhow::Result<CellSide> {
        Ok(match self {
            Self::Local(notebook) => CellSide::Local(notebook.run(code)),
            Self::Process(client) => CellSide::Process {
                id: client.run(code).map_err(anyhow::Error::msg)?,
                client: Arc::clone(client),
            },
        })
    }

    pub(crate) fn stream(&self) -> anyhow::Result<CellSide> {
        Ok(match self {
            Self::Local(notebook) => CellSide::Local(notebook.stream()),
            Self::Process(client) => CellSide::Process {
                id: client.stream().map_err(anyhow::Error::msg)?,
                client: Arc::clone(client),
            },
        })
    }

    pub(crate) fn facts(&self) -> anyhow::Result<Vec<SourceFacts>> {
        match self {
            Self::Local(notebook) => Ok(notebook.facts()),
            Self::Process(client) => client.facts().map_err(anyhow::Error::msg),
        }
    }

    pub(crate) fn report(&self) -> anyhow::Result<Option<Report>> {
        match self {
            Self::Local(notebook) => Ok(notebook.report()),
            Self::Process(client) => client.report().map_err(anyhow::Error::msg),
        }
    }

    pub(crate) fn checkin(&self) -> anyhow::Result<Duration> {
        match self {
            Self::Local(notebook) => Ok(notebook.checkin()),
            Self::Process(client) => client.checkin().map_err(anyhow::Error::msg),
        }
    }

    pub(crate) fn reset_checkin(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(notebook) => notebook.reset_checkin(),
            Self::Process(client) => client.reset_checkin().map_err(anyhow::Error::msg)?,
        }
        Ok(())
    }

    pub(crate) fn cancel(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(notebook) => notebook.cancel(),
            Self::Process(client) => client.cancel_all().map_err(anyhow::Error::msg)?,
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(notebook) => notebook.shutdown().await.map_err(anyhow::Error::msg),
            Self::Process(client) => client.shutdown().map_err(anyhow::Error::msg),
        }
    }
}

impl CellSide {
    pub(crate) fn source_id(&self) -> u64 {
        match self {
            Self::Local(cell) => cell.source_id(),
            Self::Process { id, .. } => *id,
        }
    }

    pub(crate) fn facts(&self) -> anyhow::Result<SourceFacts> {
        match self {
            Self::Local(cell) => Ok(cell.facts()),
            Self::Process { client, id } => client.cell_facts(*id).map_err(anyhow::Error::msg),
        }
    }

    pub(crate) fn feed(&self, code: String, eof: bool) -> anyhow::Result<()> {
        match self {
            Self::Local(cell) => cell.feed(code, eof).map_err(anyhow::Error::msg),
            Self::Process { client, id } => client.feed(*id, code, eof).map_err(anyhow::Error::msg),
        }
    }

    pub(crate) fn stop(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(cell) => cell.stop(),
            Self::Process { client, id } => client.stop(*id).map_err(anyhow::Error::msg)?,
        }
        Ok(())
    }

    pub(crate) fn interrupt(&self) -> anyhow::Result<Option<usize>> {
        match self {
            Self::Local(cell) => Ok(cell.interrupt()),
            Self::Process { client, id } => client.interrupt(*id).map_err(anyhow::Error::msg),
        }
    }
}
