//! Concrete Python notebook, independent jobs, and leased output contributions.
//!
//! Native and Claude runtimes share these mechanisms and source facts, not an
//! abstract tool-dispatch runtime. Host functions are callable only inside
//! cells; no notebook source chooses a scheduling deadline or starts inference.

mod python;
#[cfg(test)]
mod tests;
mod tool;

use std::sync::Arc;

use futures::future::BoxFuture;
pub use python::{PythonCell, PythonExec, PythonNotebook, PythonStreamProgress};
use rho_core::{ToolCall, ToolExecutionContext, ToolOutput, ToolOutputStatus, ToolSpec};
use rho_web_search::WebSearchTools;
pub use tool::{CellFacts, JobEnd, JobFacts, PythonCheckin, ReplyState, SourceFacts, SourceWaker};

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

pub(crate) fn output(text: impl Into<String>, status: ToolOutputStatus) -> ToolOutput {
    ToolOutput {
        output: Arc::new(text.into()),
        full_output: None,
        images: Arc::new(Vec::new()),
        status,
    }
}
