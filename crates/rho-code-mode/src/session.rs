//! Public code-mode session: `exec` starts cells, `wait` resumes or
//! terminates them, and both return model-facing result text with the same
//! status headers and yield semantics as Codex code mode.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use rho_core::{ToolCall, ToolCallId, ToolOutput, ToolOutputStatus};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::cell::{CellShared, CellStatus};
use crate::description::{
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_YIELD_TIME_MS, NestedTool, parse_exec_source,
};
use crate::runtime::{self, Command, HEARTBEAT_STALE, RuntimeHandle};
use crate::truncate::truncate_middle;

/// Executes nested tool calls made by scripts. Implementations run outside
/// the JS thread (typically forwarding to the agent's normal tool path) and
/// must honor cancellation by simply completing; the cell-side future is
/// dropped when a cell is terminated.
pub trait ToolDispatcher: Send + Sync + 'static {
    fn call_tool(&self, call: ToolCall) -> BoxFuture<'static, NestedToolOutput>;

    /// A `notify(...)` progress note from a running script, attributed to the
    /// `exec` call that started the cell. Fire-and-forget.
    fn notify(&self, exec_call_id: ToolCallId, text: String) {
        let _ = (exec_call_id, text);
    }
}

/// A nested tool's JavaScript value. Model-facing tool calls may render the
/// same result as text, but scripts receive this structured value directly.
pub struct NestedToolOutput {
    pub value: JsonValue,
    pub status: ToolOutputStatus,
}

/// Arguments of the `wait` tool.
#[derive(Debug, Deserialize)]
pub struct WaitArgs {
    pub cell_id: String,
    #[serde(default = "default_yield_time_ms")]
    pub yield_time_ms: u64,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub terminate: bool,
}

fn default_yield_time_ms() -> u64 {
    DEFAULT_YIELD_TIME_MS
}

/// One persistent code-mode session (one V8 isolate on its own thread).
/// Dropping the session shuts the runtime down and terminates live cells.
pub struct CodeModeSession {
    cells: Arc<Mutex<HashMap<u32, Arc<CellShared>>>>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    handle: RuntimeHandle,
    next_cell: AtomicU32,
}

impl CodeModeSession {
    /// Spawns the runtime thread. Blocks briefly while V8 initializes.
    pub fn new(
        tools: Vec<NestedTool>,
        dispatcher: Arc<dyn ToolDispatcher>,
    ) -> Result<Self, String> {
        let cells = Arc::new(Mutex::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        runtime::spawn_runtime_thread(tools, dispatcher, Arc::clone(&cells), cmd_rx, ready_tx);
        let handle = ready_rx
            .recv()
            .map_err(|_| "code-mode runtime thread exited during startup".to_string())??;
        Ok(Self {
            cells,
            cmd_tx,
            handle,
            next_cell: AtomicU32::new(1),
        })
    }

    /// Handles one `exec` tool call: raw JavaScript source, optionally with a
    /// first-line `// @exec:` pragma. `call_id` is the exec call's id; the
    /// cell's `notify(...)` updates are attributed to it.
    pub async fn execute(&self, call_id: ToolCallId, input: &str) -> ToolOutput {
        let parsed = match parse_exec_source(input) {
            Ok(parsed) => parsed,
            Err(error) => return error_output(error),
        };
        let cell = Arc::new(CellShared::new(
            self.next_cell.fetch_add(1, Ordering::Relaxed),
            call_id,
        ));
        self.cells
            .lock()
            .unwrap()
            .insert(cell.id, Arc::clone(&cell));
        if self
            .cmd_tx
            .send(Command {
                cell: Arc::clone(&cell),
                source: parsed.code,
            })
            .is_err()
        {
            self.cells.lock().unwrap().remove(&cell.id);
            return error_output("code-mode runtime is not available".to_string());
        }
        let yield_time =
            Duration::from_millis(parsed.yield_time_ms.unwrap_or(DEFAULT_YIELD_TIME_MS));
        self.observe(&cell, yield_time, parsed.max_output_tokens)
            .await
    }

    /// Handles one `wait` tool call.
    pub async fn wait(&self, args: WaitArgs) -> ToolOutput {
        let cell = args
            .cell_id
            .parse::<u32>()
            .ok()
            .and_then(|id| self.cells.lock().unwrap().get(&id).cloned());
        let Some(cell) = cell else {
            return missing_cell_output(&args.cell_id);
        };
        if args.terminate {
            self.terminate_cell(&cell).await;
        }
        self.observe(
            &cell,
            Duration::from_millis(args.yield_time_ms),
            args.max_tokens,
        )
        .await
    }

    /// Ask any currently running cells to yield to their observers.
    ///
    /// This is used by the host agent loop when external input arrives while a
    /// model-facing `wait` call is parked on a cell: the cell keeps running,
    /// but the `wait` tool returns promptly so the queued input can enter the
    /// next model request.
    pub fn request_yield(&self) {
        for cell in self.cells.lock().unwrap().values() {
            if cell.is_running() {
                cell.yield_requested.store(true, Ordering::Release);
                cell.notify.notify_waiters();
            }
        }
    }

    /// Waits for terminal status, new output, an explicit `yield_control()`,
    /// or the yield deadline, then formats the model-facing response.
    async fn observe(
        &self,
        cell: &Arc<CellShared>,
        yield_time: Duration,
        max_tokens: Option<usize>,
    ) -> ToolOutput {
        let call_started = Instant::now();
        let deadline = call_started + yield_time;
        let status = loop {
            let status = cell.status();
            if status != CellStatus::Running {
                break status;
            }
            if cell.take_yield_request() {
                break CellStatus::Running;
            }
            let Some(remaining) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                break CellStatus::Running;
            };
            let _ = tokio::time::timeout(remaining, cell.notify.notified()).await;
        };

        if status != CellStatus::Running {
            self.cells.lock().unwrap().remove(&cell.id);
        }
        format_response(cell, status, call_started.elapsed(), max_tokens)
    }

    /// Escalating per-cell termination: cancel pending ops (rejects the
    /// promises a parked cell awaits), and only if the runtime thread is
    /// wedged in a synchronous JS segment, terminate the running execution.
    async fn terminate_cell(&self, cell: &Arc<CellShared>) {
        cell.terminate_requested.store(true, Ordering::Release);
        cell.cancel.cancel();
        if wait_terminal(cell, Duration::from_millis(500)).await {
            return;
        }
        let heartbeat_age = Duration::from_millis(
            runtime::now_millis().saturating_sub(self.handle.heartbeat.load(Ordering::Acquire)),
        );
        if heartbeat_age >= HEARTBEAT_STALE {
            // The thread is stuck executing synchronous JS. Whichever cell is
            // on-CPU is the one blocking the whole session; V8 unwinds it and
            // the isolate (REPL scope, parked cells) survives.
            self.handle.isolate.terminate_execution();
            if wait_terminal(cell, Duration::from_secs(2)).await {
                return;
            }
        }
        // Parked on a promise we do not control: make it an inert zombie.
        // Ops from it are refused and its output is discarded.
        cell.finish(CellStatus::Terminated);
    }
}

impl Drop for CodeModeSession {
    fn drop(&mut self) {
        let cells = self
            .cells
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for cell in cells {
            cell.terminate_requested.store(true, Ordering::Release);
            cell.cancel.cancel();
        }
        // Closing the command channel ends the runtime loop.
    }
}

async fn wait_terminal(cell: &Arc<CellShared>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cell.status() != CellStatus::Running {
            return true;
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            return false;
        };
        let _ = tokio::time::timeout(remaining, cell.notify.notified()).await;
    }
}

fn format_response(
    cell: &CellShared,
    status: CellStatus,
    wall_time: Duration,
    max_tokens: Option<usize>,
) -> ToolOutput {
    let (status_line, error, output_status) = match &status {
        CellStatus::Running => (
            format!("Script running with cell ID {}", cell.id),
            None,
            ToolOutputStatus::Success,
        ),
        CellStatus::Terminated => (
            "Script terminated".to_string(),
            None,
            ToolOutputStatus::Success,
        ),
        CellStatus::Completed { error: None } => (
            "Script completed".to_string(),
            None,
            ToolOutputStatus::Success,
        ),
        CellStatus::Completed { error: Some(error) } => (
            "Script failed".to_string(),
            Some(error.clone()),
            ToolOutputStatus::Error,
        ),
    };

    let mut body = cell.drain_new_output().join("\n");
    if let Some(error) = error {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("Script error:\n{error}"));
    }
    let body = truncate_middle(&body, max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS));

    let wall_time_seconds = (wall_time.as_secs_f32() * 10.0).round() / 10.0;
    ToolOutput {
        output: Arc::new(format!(
            "{status_line}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n{body}"
        )),
        status: output_status,
    }
}

fn missing_cell_output(cell_id: &str) -> ToolOutput {
    ToolOutput {
        output: Arc::new(format!(
            "Script failed\nWall time 0.0 seconds\nOutput:\nScript error:\nexec cell {cell_id} not found"
        )),
        status: ToolOutputStatus::Error,
    }
}

fn error_output(error: String) -> ToolOutput {
    ToolOutput {
        output: Arc::new(error),
        status: ToolOutputStatus::Error,
    }
}
