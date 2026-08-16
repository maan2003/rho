//! Per-cell state shared between the session handle (any thread) and the JS
//! runtime thread. Everything here must be `Send + Sync`; the JS thread and
//! session-side observers communicate exclusively through this state plus
//! `Notify` wakeups.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rho_core::{ImageContent, ToolCallId, ToolExecutionContext};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const MAX_NESTED_IMAGES: usize = 20;
const MAX_NESTED_IMAGE_BYTES: usize = 32 * 1024 * 1024;

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
    pub(crate) tool_context: ToolExecutionContext,
    output: Mutex<CellBuffer>,
    image_state: Mutex<CellImageState>,
    next_image: AtomicU32,
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
    items: Vec<CellOutput>,
    consumed: usize,
}

#[derive(Default)]
struct CellImageState {
    retained: HashMap<u32, ImageContent>,
    total_count: usize,
    total_bytes: usize,
}

#[derive(Clone)]
pub(crate) enum CellOutput {
    Text(String),
    Image(ImageContent),
}

impl CellShared {
    pub(crate) fn new(
        id: u32,
        exec_call_id: ToolCallId,
        tool_context: ToolExecutionContext,
    ) -> Self {
        Self {
            id,
            exec_call_id,
            tool_context,
            output: Mutex::new(CellBuffer::default()),
            image_state: Mutex::new(CellImageState::default()),
            next_image: AtomicU32::new(1),
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
            output.items.push(CellOutput::Text(item));
        }
        self.notify.notify_waiters();
    }

    /// Returns output items appended since the previous drain.
    pub(crate) fn push_image(&self, image: ImageContent) {
        {
            let mut output = self.output.lock().unwrap();
            output.items.push(CellOutput::Image(image));
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn retain_nested_images(
        &self,
        images: Vec<ImageContent>,
    ) -> Result<Vec<u32>, String> {
        let mut state = self.image_state.lock().unwrap();
        if state.total_count.saturating_add(images.len()) > MAX_NESTED_IMAGES {
            return Err(format!(
                "code-mode cell image limit exceeded (maximum {MAX_NESTED_IMAGES})"
            ));
        }
        let added_bytes = images.iter().map(|image| image.data.len()).sum::<usize>();
        if state.total_bytes.saturating_add(added_bytes) > MAX_NESTED_IMAGE_BYTES {
            return Err("code-mode cell images exceed the 32 MiB limit".to_owned());
        }
        state.total_count += images.len();
        state.total_bytes += added_bytes;
        Ok(images
            .into_iter()
            .map(|image| {
                let id = self.next_image.fetch_add(1, Ordering::Relaxed);
                state.retained.insert(id, image);
                id
            })
            .collect())
    }

    pub(crate) fn append_nested_image(&self, id: u32) -> bool {
        let image = self.image_state.lock().unwrap().retained.remove(&id);
        if let Some(image) = image {
            self.push_image(image);
            true
        } else {
            false
        }
    }

    /// Returns output items appended since the previous drain.
    pub(crate) fn drain_new_output(&self) -> Vec<CellOutput> {
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
