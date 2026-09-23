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

pub use python::{
    HostFunction, PythonCell, PythonExec, PythonNotebook, PythonStreamProgress, ToolCx,
};
use rho_core::{ToolExecutionContext, ToolOutput, ToolOutputStatus};
use rho_web_search::{WebRequest, WebSearchTools};
pub use tool::{CellFacts, JobEnd, JobFacts, PythonCheckin, ReplyState, SourceFacts, SourceWaker};

/// `web.run(**request)`: the results arrive with the cell's report, and the
/// call also returns them.
pub fn web_run(tools: WebSearchTools) -> HostFunction {
    HostFunction::new("web.run", &[], move |cx: ToolCx, request: WebRequest| {
        let tools = tools.clone();
        async move {
            let output = tools
                .run(request, ToolExecutionContext::default())
                .await?;
            cx.report(&output);
            Ok(output)
        }
    })
}

pub(crate) fn output(text: impl Into<String>, status: ToolOutputStatus) -> ToolOutput {
    ToolOutput {
        output: Arc::new(text.into()),
        full_output: None,
        images: Arc::new(Vec::new()),
        status,
    }
}
