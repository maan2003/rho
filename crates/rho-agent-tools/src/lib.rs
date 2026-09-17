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
use rho_core::{ToolCall, ToolExecutionContext, ToolOutput, ToolOutputStatus};
use rho_web_search::WebSearchTools;
pub use tool::{CellFacts, JobEnd, JobFacts, PythonCheckin, ReplyState, SourceFacts, SourceWaker};

/// An async Rust callback registered by name in a Python notebook.
/// Python owns the callable signatures and documentation; this trait only
/// dispatches calls.
pub trait HostFunction: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput>;
}

impl HostFunction for WebSearchTools {
    fn name(&self) -> &'static str {
        rho_web_search::WEB_SEARCH_TOOL_NAME
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
