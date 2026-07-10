//! Per-cell state shared between the session handle (any thread) and the JS
//! runtime thread. Everything here must be `Send + Sync`; the JS thread and
//! session-side observers communicate exclusively through this state plus
//! `Notify` wakeups.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rho_core::ToolCallId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellStatus {
    Running,
    /// The evaluation settled: `error` carries the script failure text.
    Completed {
        error: Option<String>,
    },
    Terminated,
}

pub(crate) struct CellShared {
    pub(crate) id: u32,
    /// The `exec` call that started this cell. `notify(...)` updates are
    /// attributed to this call for the cell's whole lifetime, across later
    /// `wait` calls (matching Codex).
    pub(crate) exec_call_id: ToolCallId,
    output: Mutex<CellBuffer>,
    status: Mutex<CellStatus>,
    /// Wakes observers on new output, yield requests, and status changes.
    pub(crate) notify: Notify,
    /// Cancels this cell's pending ops (tool calls, timers). Triggered on
    /// terminate and when the cell reaches a terminal status.
    pub(crate) cancel: CancellationToken,
    pub(crate) yield_requested: AtomicBool,
    pub(crate) exit_requested: AtomicBool,
    pub(crate) terminate_requested: AtomicBool,
}

#[derive(Default)]
struct CellBuffer {
    items: Vec<String>,
    consumed: usize,
}

impl CellShared {
    pub(crate) fn new(id: u32, exec_call_id: ToolCallId) -> Self {
        Self {
            id,
            exec_call_id,
            output: Mutex::new(CellBuffer::default()),
            status: Mutex::new(CellStatus::Running),
            notify: Notify::new(),
            cancel: CancellationToken::new(),
            yield_requested: AtomicBool::new(false),
            exit_requested: AtomicBool::new(false),
            terminate_requested: AtomicBool::new(false),
        }
    }

    pub(crate) fn push_output(&self, item: String) {
        {
            let mut output = self.output.lock().unwrap();
            output.items.push(item);
        }
        self.notify.notify_waiters();
    }

    /// Returns output items appended since the previous drain.
    pub(crate) fn drain_new_output(&self) -> Vec<String> {
        let mut output = self.output.lock().unwrap();
        let new = output.items[output.consumed..].to_vec();
        output.consumed = output.items.len();
        new
    }

    pub(crate) fn status(&self) -> CellStatus {
        self.status.lock().unwrap().clone()
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self.status(), CellStatus::Running)
    }

    /// Transitions to a terminal status. Only the first transition wins, so a
    /// force-terminated zombie cell keeps its `Terminated` status even if the
    /// runtime later reports how the evaluation actually settled.
    pub(crate) fn finish(&self, status: CellStatus) {
        {
            let mut current = self.status.lock().unwrap();
            if *current != CellStatus::Running {
                return;
            }
            *current = status;
        }
        self.cancel.cancel();
        self.notify.notify_waiters();
    }

    pub(crate) fn take_yield_request(&self) -> bool {
        self.yield_requested.swap(false, Ordering::AcqRel)
    }
}
