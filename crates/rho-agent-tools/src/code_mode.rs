//! Code mode as a source: one `exec` tool, and a running cell is a session
//! that stays attached to the call that started it.
//!
//! Code mode's own `wait(cell_id)` is not offered. The core's `wait` names an
//! interval, and a cell's output reaches the model on its `exec` call whenever
//! the next request goes out, so there is nothing for a cell-scoped wait to
//! do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use rho_code_mode::{
    CellHandle, CellOutcome, CellWatcher, CodeModeSession, NestedTool, NestedToolOutput,
    ToolDispatcher,
};
use rho_core::{
    ToolCall, ToolCallId, ToolExecutionContext, ToolOutput, ToolOutputStatus, ToolSpec, UnixMs,
};
use rho_tool_shell::{BoundedOutput, ShellTools, decode_output_lossy};
use tokio::sync::Notify;

use crate::{Finished, FutureTool, SourceWaker, Tool, ToolHaste, ToolSession};

pub struct CodeModeTool {
    session: Arc<CodeModeSession>,
    spec: ToolSpec,
    links: Arc<Links>,
}

/// What a running cell can push at its session from the JS side: `notify`
/// text, attributed by the exec call's id.
#[derive(Default)]
struct Links(Mutex<HashMap<ToolCallId, Arc<CellLink>>>);

#[derive(Default)]
struct CellLink {
    notes: Mutex<Vec<String>>,
    /// When the cell first said something that stands on its own — a
    /// `notify` or a `yield_control()` — since the last drain.
    soon_since: Mutex<Option<UnixMs>>,
    /// When unsent plain output started accumulating.
    output_since: Mutex<Option<UnixMs>>,
    ended_at: Mutex<Option<UnixMs>>,
    waker: Mutex<Option<SourceWaker>>,
}

impl CellLink {
    fn wake(&self) {
        if let Some(waker) = &*self.waker.lock().unwrap() {
            waker.wake();
        }
    }
}

struct Dispatcher {
    shell: ShellTools,
    others: HashMap<String, Arc<dyn FutureTool>>,
    runtime: tokio::runtime::Handle,
    links: Arc<Links>,
}

impl ToolDispatcher for Dispatcher {
    fn call_tool(
        &self,
        _context: ToolExecutionContext,
        call: ToolCall,
    ) -> BoxFuture<'static, NestedToolOutput> {
        let shell = self.shell.clone();
        let other = self.others.get(call.name.as_str()).cloned();
        // Nested calls run on the agent's runtime, not the JS thread's.
        let task = self.runtime.spawn(async move {
            if let Some(tool) = other {
                return NestedToolOutput::from_tool_output(tool.call(call).await);
            }
            match shell.call_code_mode(call).await {
                Ok(value) => NestedToolOutput {
                    images: Vec::new(),
                    value,
                    status: ToolOutputStatus::Success,
                },
                Err(error) => NestedToolOutput {
                    images: Vec::new(),
                    value: serde_json::Value::String(error.to_string()),
                    status: ToolOutputStatus::Error,
                },
            }
        });
        Box::pin(async move {
            task.await.unwrap_or_else(|_| NestedToolOutput {
                images: Vec::new(),
                value: serde_json::Value::String("nested tool task failed".to_owned()),
                status: ToolOutputStatus::Error,
            })
        })
    }

    fn notify(&self, exec_call_id: ToolCallId, text: String) {
        let link = self.links.0.lock().unwrap().get(&exec_call_id).cloned();
        if let Some(link) = link {
            link.notes.lock().unwrap().push(text);
            link.soon_since
                .lock()
                .unwrap()
                .get_or_insert_with(UnixMs::now);
            link.wake();
        }
    }
}

impl CodeModeTool {
    /// Starts the V8 thread. Must run on a tokio runtime, and blocks briefly.
    pub fn new(shell: ShellTools, others: Vec<Arc<dyn FutureTool>>) -> Result<Self, String> {
        let mut nested: Vec<NestedTool> = shell
            .specs()
            .iter()
            .map(|spec| {
                let tool = NestedTool::from_spec(spec);
                match ShellTools::code_mode_output_schema(spec.name.as_str()) {
                    Some(schema) => tool.with_output_schema(schema),
                    None => tool,
                }
            })
            .collect();
        nested.extend(
            others
                .iter()
                .map(|tool| NestedTool::from_spec(&tool.spec())),
        );
        let links = Arc::new(Links::default());
        let dispatcher = Arc::new(Dispatcher {
            shell,
            others: others
                .into_iter()
                .map(|tool| (tool.spec().name.as_str().to_owned(), tool))
                .collect(),
            runtime: tokio::runtime::Handle::current(),
            links: Arc::clone(&links),
        });
        let spec = rho_code_mode::exec_tool_spec(&nested);
        let session = CodeModeSession::new(nested, dispatcher)?;
        Ok(Self {
            session: Arc::new(session),
            spec,
            links,
        })
    }
}

impl Tool for CodeModeTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession> {
        let max_output_tokens = rho_code_mode::parse_exec_source(&call.arguments)
            .ok()
            .and_then(|parsed| parsed.max_output_tokens);
        let handle = match self.session.start(
            call.id.clone(),
            &call.arguments,
            ToolExecutionContext::default(),
        ) {
            Ok(handle) => handle,
            Err(output) => return Box::new(Finished::new(output)),
        };
        let link = Arc::new(CellLink::default());
        *link.waker.lock().unwrap() = Some(waker.clone());
        self.links
            .0
            .lock()
            .unwrap()
            .insert(call.id.clone(), Arc::clone(&link));
        let cancel = Arc::new(Notify::new());
        tokio::spawn(watch(
            handle.watcher(),
            Arc::clone(&link),
            Arc::clone(&cancel),
            waker,
        ));
        Box::new(CellSession {
            session: Arc::clone(&self.session),
            links: Arc::clone(&self.links),
            call_id: call.id,
            handle,
            link,
            cancel,
            started: Instant::now(),
            max_output_tokens,
            answered: false,
            end_reported: false,
        })
    }
}

/// Wakes the core when the cell changes. The cell's wakeups are not stored,
/// so a short poll backs them up rather than trusting every one to land.
async fn watch(cell: CellWatcher, link: Arc<CellLink>, cancel: Arc<Notify>, waker: SourceWaker) {
    loop {
        if !cell.is_running() {
            link.ended_at
                .lock()
                .unwrap()
                .get_or_insert_with(UnixMs::now);
            waker.wake();
            return;
        }
        tokio::select! {
            biased;
            _ = cancel.notified() => return,
            _ = cell.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        waker.wake();
    }
}

struct CellSession {
    session: Arc<CodeModeSession>,
    links: Arc<Links>,
    call_id: ToolCallId,
    handle: CellHandle,
    link: Arc<CellLink>,
    cancel: Arc<Notify>,
    started: Instant,
    max_output_tokens: Option<usize>,
    answered: bool,
    end_reported: bool,
}

impl CellSession {
    fn render(&mut self, first: bool) -> ToolOutput {
        let outcome = self.handle.outcome();
        let (text, images) = self.handle.take_output();
        let notes = std::mem::take(&mut *self.link.notes.lock().unwrap());
        *self.link.soon_since.lock().unwrap() = None;
        *self.link.output_since.lock().unwrap() = None;
        self.handle.take_yield_request();

        let (status_line, error, status) = match &outcome {
            CellOutcome::Running => (
                format!("Script running with cell ID {}", self.handle.id()),
                None,
                ToolOutputStatus::Success,
            ),
            CellOutcome::Completed => (
                "Script completed".to_owned(),
                None,
                ToolOutputStatus::Success,
            ),
            CellOutcome::Failed(error) => (
                "Script failed".to_owned(),
                Some(error.clone()),
                ToolOutputStatus::Error,
            ),
            CellOutcome::Terminated => (
                "Script terminated".to_owned(),
                None,
                ToolOutputStatus::Success,
            ),
        };
        if outcome != CellOutcome::Running {
            self.end_reported = true;
        }
        let mut body = notes.into_iter().chain(text).collect::<Vec<_>>().join("\n");
        if let Some(error) = error {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str("Script error:\n");
            body.push_str(&error);
        }
        let full_output = Arc::new(body.clone());
        let mut bounded = BoundedOutput::for_tokens(self.max_output_tokens);
        bounded.push(body.as_bytes());
        let body = decode_output_lossy(bounded.into_bytes());
        let wall = if first {
            format!(
                "\nWall time {:.1} seconds",
                self.started.elapsed().as_secs_f64()
            )
        } else {
            String::new()
        };
        ToolOutput {
            output: Arc::new(format!("{status_line}{wall}\nOutput:\n{body}")),
            full_output: Some(full_output),
            images: Arc::new(images),
            status,
        }
    }

    fn has_unsent(&self) -> bool {
        self.handle.has_new_output() || !self.link.notes.lock().unwrap().is_empty()
    }
}

impl ToolSession for CellSession {
    fn sources(&self) -> Vec<(u64, crate::SourceFacts)> {
        let haste = if self.handle.outcome() != CellOutcome::Running {
            ToolHaste::Ended {
                at: self
                    .link
                    .ended_at
                    .lock()
                    .unwrap()
                    .unwrap_or_else(UnixMs::now),
            }
        } else {
            let soon = *self.link.soon_since.lock().unwrap();
            if soon.is_some() || self.handle.yield_requested() {
                ToolHaste::Soon {
                    since: soon.unwrap_or_else(UnixMs::now),
                }
            } else if self.handle.has_new_output() {
                ToolHaste::Eventually {
                    since: *self
                        .link
                        .output_since
                        .lock()
                        .unwrap()
                        .get_or_insert_with(UnixMs::now),
                }
            } else {
                ToolHaste::None
            }
        };
        vec![(0, crate::SourceFacts::Tool(haste))]
    }

    fn done(&self) -> bool {
        self.answered && self.end_reported && !self.has_unsent()
    }

    fn first_output(&mut self) -> ToolOutput {
        self.answered = true;
        self.render(true)
    }

    fn more_output(&mut self) -> Option<ToolOutput> {
        let ended = self.handle.outcome() != CellOutcome::Running && !self.end_reported;
        (ended || self.has_unsent()).then(|| self.render(false))
    }

    fn cancel(&mut self) {
        let session = Arc::clone(&self.session);
        let cell = self.handle.watcher();
        tokio::spawn(async move { session.terminate(&cell).await });
    }
}

impl Drop for CellSession {
    fn drop(&mut self) {
        self.cancel.notify_one();
        self.links.0.lock().unwrap().remove(&self.call_id);
        if self.handle.outcome() == CellOutcome::Running {
            let session = Arc::clone(&self.session);
            let cell = self.handle.watcher();
            tokio::spawn(async move { session.terminate(&cell).await });
        }
    }
}
