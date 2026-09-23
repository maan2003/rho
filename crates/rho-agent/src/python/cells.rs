//! The cells a runtime holds for the model, and what each owes it.
//!
//! A call's first reply answers it; everything a cell says afterwards is an
//! update. The newest cell's check-in paces the model even after the cell
//! itself is forgotten. Both runtimes hold their cells here and differ only
//! in how a reply reaches the provider.
use std::collections::BTreeMap;
use std::sync::Arc;

use rho_core::{ExecCall, ExecId, ToolOutput, UnixMs};
use tokio::sync::Notify;

use crate::boundary::SourceKind;
use crate::python::{PythonCell, PythonExec, PythonNotebook, SourceWaker};

pub(crate) struct Cells {
    wake: Arc<Notify>,
    held: BTreeMap<ExecId, Held>,
    /// The newest cell, kept after it is forgotten.
    latest: Option<Arc<PythonExec>>,
}

pub(crate) struct Held {
    pub(crate) cell: Box<PythonCell>,
    /// The call as the model wrote it, for readers.
    pub(crate) source: String,
    pub(crate) started_at: UnixMs,
    answered: bool,
}

/// One cell's contribution at a boundary.
pub(crate) struct Reply {
    pub(crate) id: ExecId,
    pub(crate) output: ToolOutput,
    /// Answers the call; anything else is an update.
    pub(crate) first: bool,
    pub(crate) started_at: UnixMs,
}

impl Cells {
    /// Cells whose changes wake `wake`.
    pub(crate) fn new(wake: Arc<Notify>) -> Self {
        Self {
            wake,
            held: BTreeMap::new(),
            latest: None,
        }
    }

    /// Run `call` as the newest cell.
    pub(crate) fn exec(&mut self, notebook: &PythonNotebook, call: ExecCall, now: UnixMs) {
        let cell = notebook.exec(call.clone(), self.waker());
        self.hold(call, cell, now);
    }

    /// Start a streamed call as the newest cell; its source arrives later.
    pub(crate) fn start_stream(
        &mut self,
        notebook: &PythonNotebook,
        call: ExecCall,
        now: UnixMs,
    ) -> Arc<PythonExec> {
        let cell = notebook.start_stream(call.id.clone(), self.waker());
        let exec = cell.execution();
        self.hold(call, cell, now);
        exec
    }

    fn waker(&self) -> SourceWaker {
        SourceWaker::new(Arc::clone(&self.wake))
    }

    fn hold(&mut self, call: ExecCall, cell: Box<PythonCell>, now: UnixMs) {
        self.latest = Some(cell.execution());
        self.held.insert(
            call.id,
            Held {
                cell,
                source: call.source,
                started_at: now,
                answered: false,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn latest(&self) -> Option<&Arc<PythonExec>> {
        self.latest.as_ref()
    }

    /// Make a held cell the newest again, or none.
    pub(crate) fn set_latest(&mut self, id: Option<&ExecId>) {
        self.latest = id
            .and_then(|id| self.held.get(id))
            .map(|held| held.cell.execution());
    }

    pub(crate) fn get(&self, id: &ExecId) -> Option<&Held> {
        self.held.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &ExecId) -> Option<&mut Held> {
        self.held.get_mut(id)
    }

    pub(crate) fn remove(&mut self, id: &ExecId) {
        self.held.remove(id);
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&ExecId, &Held)> {
        self.held.iter()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Some call has not had its first reply.
    pub(crate) fn owe_reply(&self) -> bool {
        self.held.values().any(|held| !held.answered)
    }

    pub(crate) fn cancel(&mut self) {
        for held in self.held.values_mut() {
            held.cell.cancel();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.held.clear();
        self.latest = None;
    }

    /// Every cell and job, as the boundary reads them.
    pub(crate) fn sources(&self) -> Vec<SourceKind> {
        let latest = self.latest.as_ref().map(|exec| exec.id());
        let mut sources = Vec::new();
        for (id, held) in &self.held {
            sources.push(SourceKind::Cell {
                facts: held.cell.facts(),
                latest: latest == Some(id),
            });
            sources.extend(
                held.cell
                    .jobs()
                    .into_iter()
                    .map(|facts| SourceKind::Job { facts }),
            );
        }
        // A quiet cell may be forgotten, but its check-in still paces this
        // model turn.
        if let Some(exec) = &self.latest
            && !self.held.contains_key(exec.id())
        {
            sources.push(SourceKind::Cell {
                facts: exec.facts(),
                latest: true,
            });
        }
        sources
    }

    /// What every cell has to say: the newest first, then the rest in the
    /// order they ran. A call not yet answered is answered even with nothing,
    /// because a provider rejects a request that leaves a call unanswered.
    /// Leased until [`Cells::acknowledge`].
    pub(crate) fn drain(&mut self) -> Vec<Reply> {
        let latest = self.latest.as_ref().map(|exec| exec.id().clone());
        let mut held = self.held.iter_mut().collect::<Vec<_>>();
        held.sort_by_key(|(id, held)| (Some(*id) != latest.as_ref(), held.cell.sequence()));
        held.into_iter()
            .filter_map(|(id, held)| {
                let output = if held.answered {
                    held.cell.more_output()?
                } else {
                    held.cell.first_output()
                };
                Some(Reply {
                    id: id.clone(),
                    output,
                    first: !held.answered,
                    started_at: held.started_at,
                })
            })
            .collect()
    }

    /// The recipient owns what was drained: forget cells with nothing left.
    pub(crate) fn acknowledge(&mut self) {
        for held in self.held.values_mut() {
            if held.cell.acknowledge_output() {
                held.answered = true;
            }
        }
        self.held.retain(|_, held| !held.cell.done());
    }
}
