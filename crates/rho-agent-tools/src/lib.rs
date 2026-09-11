//! What a tool is to the agent loop, and the one real tool in that shape.
//!
//! The Python notebook is a [`ToolSession`]: it holds its output until the
//! core asks, reports what its cells and jobs are doing, and is answered
//! whenever the next request goes out. Nothing here waits for a deadline of
//! its own — a command that takes a minute costs one request, one that takes
//! an hour costs the same one, and the model names any pace it wants from
//! inside a cell.

mod python;
#[cfg(test)]
mod tests;
mod tool;

use std::sync::Arc;

use futures::future::BoxFuture;
pub use python::{
    ExecReturn, PythonExec, PythonStreamProgress, PythonTool, python_instructions,
    python_instructions_for,
};
use rho_core::{ToolCall, ToolExecutionContext, ToolOutput, ToolOutputStatus, ToolSpec, UnixMs};
use rho_web_search::WebSearchTools;
pub use tool::{
    CellFacts, JobEnd, JobFacts, PythonCheckin, SourceFacts, SourceWaker, Tool, ToolSession,
};

/// A tool whose whole answer is one future, reachable from a cell as a host
/// function.
pub trait FutureTool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;
    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput>;
}

impl FutureTool for WebSearchTools {
    fn spec(&self) -> ToolSpec {
        WebSearchTools::spec(self)
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        WebSearchTools::call(self, call, ToolExecutionContext::default())
    }
}

/// A cell that is over before it is asked anything: the result is its one
/// answer, and it has nothing to add. What a cell becomes when the notebook
/// could not even start it.
pub(crate) struct Finished {
    cell: u64,
    output: ToolOutput,
    at: UnixMs,
}

impl Finished {
    pub(crate) fn error(cell: u64, text: impl Into<String>) -> Self {
        Self {
            cell,
            output: output(text.into(), ToolOutputStatus::Error),
            at: UnixMs::now(),
        }
    }
}

impl ToolSession for Finished {
    fn sources(&self) -> Vec<(u64, SourceFacts)> {
        vec![(
            u64::MAX,
            SourceFacts::Cell(CellFacts {
                cell: self.cell,
                started: true,
                returned: Some(self.at),
                failed: true,
                output_since: Some(self.at),
                notified_at: None,
                checkin: None,
                foreground_cell: 0,
            }),
        )]
    }

    fn done(&self) -> bool {
        true
    }

    fn first_output(&mut self) -> ToolOutput {
        self.output.clone()
    }

    fn more_output(&mut self) -> Option<ToolOutput> {
        None
    }

    fn cancel(&mut self) {}
}

pub(crate) fn output(text: impl Into<String>, status: ToolOutputStatus) -> ToolOutput {
    ToolOutput {
        output: Arc::new(text.into()),
        full_output: None,
        images: Arc::new(Vec::new()),
        status,
    }
}
