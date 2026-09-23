//! Rho's Python notebook: embedded CPython, and the boundary between the
//! work its cells start and the agent loop that reports it.
//!
//! The process embeds one CPython interpreter. Each notebook gets its own
//! globals, its own thread with private cwd state, and its own asyncio event
//! loop; modules and interpreter-wide state are shared. A cell's work is
//! found through a context variable that asyncio already propagates into
//! every task and callback, and that threads inherit: the cell has finished
//! when its code has returned and nothing it started is still live.
//!
//! Host tools are ordinary PyO3 classes and functions. What they start is an
//! [`operation`] of the running cell: registered before Python continues,
//! run to completion whether or not Python awaits it, and reported with the
//! cell's output, so agents need not await what they start. Native and
//! Claude runtimes share these mechanisms and source facts, not an abstract
//! tool-dispatch runtime; no notebook source chooses a scheduling deadline
//! or starts inference.
//!
//! This is ordinary, unsandboxed Python. The thread has private cwd state
//! and is initialized in the agent's workspace view before Python starts;
//! other process-global operations retain their normal in-process semantics.

mod cell;
mod cells;
mod commands;
mod history;
pub(crate) mod host;
mod interpreter;
mod notebook;
mod runtime;
#[cfg(test)]
mod runtime_tests;
mod source;
#[cfg(test)]
mod tests;
mod tool;

use std::sync::Arc;

pub use cell::{INTERRUPTED, PythonCell, PythonExec};
pub(crate) use cells::Cells;
pub use notebook::{Export, PythonNotebook, PythonStreamProgress, ToolCx, detached, operation};
use rho_core::{ToolOutput, ToolOutputStatus};
pub use tool::{CellFacts, JobEnd, JobFacts, PythonCheckin, SourceWaker};

pub(crate) fn output(text: impl Into<String>, status: ToolOutputStatus) -> ToolOutput {
    ToolOutput {
        output: Arc::new(text.into()),
        full_output: None,
        images: Arc::new(Vec::new()),
        status,
    }
}
