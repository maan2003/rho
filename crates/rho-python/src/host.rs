//! What the notebook exposes of its host: typed host functions, managed
//! commands and the conversation transcript.
//!
//! A host function is ordinary Rust: its arguments are deserialized straight
//! from the Python call and its result is built straight into Python objects
//! (with `pythonize`). The synchronous part runs on the notebook thread before
//! Python continues, so a host can register work that later calls in the same
//! cell rely on; the returned future then runs on the host's Tokio runtime
//! and resolves an asyncio future.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pyo3::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::CellId;

/// The asynchronous half of a host call.
pub type HostFuture<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'static>>;

/// A host result on its way into Python.
pub(crate) trait IntoPython: Send {
    fn into_python(self: Box<Self>, py: Python<'_>) -> PyResult<Py<PyAny>>;
}

struct Serialized<T>(T);

impl<T: Serialize + Send> IntoPython for Serialized<T> {
    fn into_python(self: Box<Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(pythonize::pythonize(py, &self.0)?.unbind())
    }
}

pub(crate) type Pending = HostFuture<Box<dyn IntoPython>>;

pub(crate) fn pending<T: Serialize + Send + 'static>(future: HostFuture<T>) -> Pending {
    Box::pin(async move {
        future
            .await
            .map(|value| Box::new(Serialized(value)) as Box<dyn IntoPython>)
    })
}

type Start = dyn Fn(CellId, &Bound<'_, PyAny>) -> Result<Pending, String> + Send + Sync;

/// A host function callable from notebook Python.
#[derive(Clone)]
pub struct Function {
    pub(crate) path: &'static str,
    pub(crate) positional: &'static [&'static str],
    pub(crate) detached: bool,
    pub(crate) start: Arc<Start>,
}

impl Function {
    /// `path` places the function in the notebook: `"papercut"` is a global
    /// and `"agents.message"` is `message` in the importable `agents` module.
    /// `positional` names the leading parameters that may also be passed
    /// positionally; every other parameter is keyword-only. `A` receives the
    /// arguments as a map of parameter names, so serde attributes supply
    /// defaults and reject unknown names.
    ///
    /// `start` runs on the notebook thread when Python calls the function.
    /// An error raises `RuntimeError` in the caller at once; otherwise the
    /// returned future resolves the awaitable the call returns.
    pub fn new<A, R, F>(path: &'static str, positional: &'static [&'static str], start: F) -> Self
    where
        A: DeserializeOwned,
        R: Serialize + Send + 'static,
        F: Fn(CellId, A) -> Result<HostFuture<R>, String> + Send + Sync + 'static,
    {
        Self {
            path,
            positional,
            detached: false,
            start: Arc::new(move |cell, arguments| {
                let arguments = pythonize::depythonize(arguments)
                    .map_err(|error| format!("{path}(): {error}"))?;
                start(cell, arguments).map(pending)
            }),
        }
    }

    /// The call returns `None` rather than an awaitable. Its outcome is the
    /// host's to report.
    pub fn detached(mut self) -> Self {
        self.detached = true;
        self
    }

    pub fn path(&self) -> &'static str {
        self.path
    }
}

/// How a managed command ended, as seen by awaiting its handle.
#[derive(Clone, Debug, Serialize)]
pub struct CommandExit {
    pub id: u64,
    pub exit_code: Option<i32>,
}

/// Managed commands behind the notebook's `command()` and `Command`.
pub trait Commands: Send + Sync + 'static {
    /// Start a command. The handle is usable as soon as this returns; the
    /// future completes when the command ends.
    fn start(
        &self,
        cell: CellId,
        cmd: String,
        workdir: Option<String>,
        max_tokens: usize,
    ) -> Result<(u64, HostFuture<CommandExit>), String>;
    /// The live command a report's session ID refers to.
    fn find(&self, session_id: u64) -> Result<u64, String>;
    fn wait(&self, cell: CellId, id: u64) -> Result<HostFuture<CommandExit>, String>;
    fn write_stdin(&self, cell: CellId, id: u64, chars: String) -> Result<HostFuture<()>, String>;
    fn more_output(&self, cell: CellId, id: u64, max_tokens: usize) -> Result<HostFuture<()>, String>;
    fn cancel(&self, cell: CellId, id: u64) -> Result<HostFuture<()>, String>;
}

/// One transcript entry as the notebook's `transcript` shows it.
#[derive(Clone, Debug, Default)]
pub struct HistoryItem {
    pub kind: &'static str,
    pub role: Option<&'static str>,
    pub sender: Option<String>,
    pub text: Option<String>,
    pub content: Vec<HistoryContent>,
    pub name: Option<String>,
    pub call_id: Option<String>,
    pub summary: Vec<String>,
    pub images: Vec<HistoryImage>,
    pub provider: Option<HistoryProviderData>,
    pub status: Option<&'static str>,
    pub phase: Option<&'static str>,
    pub tool_type: Option<&'static str>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub at: Option<i64>,
    pub retain_from: Option<u64>,
    pub call_ids: Vec<String>,
    pub response_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct HistoryContent {
    pub kind: &'static str,
    pub text: Option<String>,
    pub media_type: Option<String>,
    pub data: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct HistoryImage {
    pub media_type: String,
    pub data: Vec<u8>,
    pub detail: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct HistoryProviderData {
    pub tag: String,
    pub data: Vec<u8>,
}

/// Lazy, execution-scoped data behind the notebook's `transcript` sequence.
/// Implementations own snapshots and produce one item at a time.
pub trait History: Send + Sync + 'static {
    fn len(&self, cell: CellId) -> Result<usize, String>;
    fn get(&self, cell: CellId, index: usize) -> Result<HistoryItem, String>;
}

pub(crate) struct EmptyHistory;

impl History for EmptyHistory {
    fn len(&self, _cell: CellId) -> Result<usize, String> {
        Ok(0)
    }
    fn get(&self, _cell: CellId, _index: usize) -> Result<HistoryItem, String> {
        Err("transcript index out of range".into())
    }
}

/// Everything the notebook calls back into.
pub struct Host {
    pub functions: Vec<Function>,
    pub commands: Option<Arc<dyn Commands>>,
    pub history: Arc<dyn History>,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            functions: Vec::new(),
            commands: None,
            history: Arc::new(EmptyHistory),
        }
    }
}
