//! In-process Python notebook on CPython.
//!
//! The process embeds one CPython interpreter. Each notebook gets its own
//! globals, its own thread with private cwd state, and its own asyncio event
//! loop; modules and interpreter-wide state are shared. A cell's work is
//! found through a context variable that asyncio already propagates into
//! every task and callback, and that threads inherit: the cell has finished
//! when its code has returned and nothing it started is still live.
//! Rust talks to the notebook thread through one inbox; Python objects never
//! leave it.
//!
//! Host functions are typed Rust functions; Python arguments and results
//! convert directly to and from serde types. This is ordinary, unsandboxed
//! Python. The thread has private cwd state and is initialized in the agent's
//! workspace view before Python starts; other process-global operations
//! retain their normal in-process semantics.

mod host;
mod interpreter;
mod notebook;

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use host::{
    CommandExit, Commands, Function, History, HistoryContent, HistoryImage, HistoryItem,
    HistoryProviderData, Host, HostFuture,
};

pub type CellId = u64;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum Input {
    Execute { cell: CellId, source: String },
    BeginStream { cell: CellId },
    StreamFeed { cell: CellId, source: String, eof: bool },
    StreamPermit { cell: CellId, end: usize },
    StreamStop { cell: CellId },
    Cancel { cell: CellId },
    Shutdown,
}

#[derive(Debug)]
pub enum Event {
    Started { cell: CellId },
    UnitReady { cell: CellId, end: usize },
    UnitSettled { cell: CellId, end: usize, error: Option<String> },
    Returned { cell: CellId, error: Option<String> },
    Text { cell: CellId, text: String, max_tokens: usize, important: bool },
    MaxWait { cell: CellId, seconds: u64 },
    SuppressToolWakeups { cell: CellId },
    Finished { cell: CellId, error: Option<String> },
    Stopped { error: Option<String> },
}

/// An execution's Rust-owned state. Events arrive synchronously, on the
/// notebook thread or on a thread the cell started, with the interpreter
/// lock held; implementations must not block or wait for async work.
pub trait Execution: Send + Sync {
    fn event(&self, event: Event);
}

/// The cells the notebook knows about, shared with every thread.
#[derive(Default)]
pub(crate) struct Registry {
    cells: Mutex<HashMap<CellId, Arc<dyn Execution>>>,
    stopped: AtomicBool,
}

impl Registry {
    /// Deliver an event to its execution. `Finished` retires the cell.
    pub(crate) fn emit(&self, event: Event) {
        let cell = match &event {
            Event::Started { cell }
            | Event::UnitReady { cell, .. }
            | Event::UnitSettled { cell, .. }
            | Event::Returned { cell, .. }
            | Event::Text { cell, .. }
            | Event::MaxWait { cell, .. }
            | Event::SuppressToolWakeups { cell }
            | Event::Finished { cell, .. } => *cell,
            Event::Stopped { .. } => return,
        };
        let execution = {
            let mut cells = self.cells.lock().unwrap();
            if matches!(event, Event::Finished { .. }) {
                cells.remove(&cell)
            } else {
                cells.get(&cell).cloned()
            }
        };
        if let Some(execution) = execution {
            execution.event(event);
        }
    }

    /// The runtime ended: every remaining execution learns why.
    pub(crate) fn stop(&self, error: Option<String>) {
        self.stopped.store(true, Ordering::Release);
        let cells = std::mem::take(&mut *self.cells.lock().unwrap());
        for execution in cells.into_values() {
            execution.event(Event::Stopped {
                error: error.clone(),
            });
        }
    }
}

/// What other threads post to the notebook thread.
pub(crate) enum Message {
    Input(Input),
    /// A host call finished.
    Done(u64, Result<Box<dyn host::IntoPython>, String>),
}

/// The notebook thread's queue. Posting never needs the interpreter lock:
/// an eventfd the event loop watches says the queue is non-empty.
pub(crate) struct Inbox {
    queue: Mutex<VecDeque<Message>>,
    wake: OwnedFd,
}

impl Inbox {
    fn new() -> Result<Arc<Self>, String> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(format!("notebook eventfd: {}", std::io::Error::last_os_error()));
        }
        Ok(Arc::new(Self {
            queue: Mutex::default(),
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
        }))
    }

    pub(crate) fn post(&self, message: Message) {
        self.queue.lock().unwrap().push_back(message);
        let one = 1u64;
        unsafe { libc::write(self.wake.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    pub(crate) fn fd(&self) -> i32 {
        self.wake.as_raw_fd()
    }

    /// Everything posted so far, in order.
    pub(crate) fn take(&self) -> VecDeque<Message> {
        let mut count = 0u64;
        unsafe { libc::read(self.wake.as_raw_fd(), (&raw mut count).cast(), 8) };
        std::mem::take(&mut *self.queue.lock().unwrap())
    }
}

/// A cloneable sender, not an owner of the interpreter's lifetime.
///
/// Inputs enter one FIFO that the notebook thread drains in order. Admission
/// is unbounded so synchronous Python cannot block delivery of the
/// completions needed to make progress.
#[derive(Clone)]
pub struct Sender {
    inbox: Arc<Inbox>,
    registry: Arc<Registry>,
}

impl Sender {
    fn admit(&self, cell: CellId, execution: Arc<dyn Execution>, input: Input) -> Result<(), String> {
        self.registry.cells.lock().unwrap().insert(cell, execution);
        if let Err(error) = self.send(input) {
            self.registry.cells.lock().unwrap().remove(&cell);
            return Err(error);
        }
        Ok(())
    }

    /// Publish the execution before making its code runnable.
    pub fn execute(
        &self,
        cell: CellId,
        source: String,
        execution: Arc<dyn Execution>,
    ) -> Result<(), String> {
        self.admit(cell, execution, Input::Execute { cell, source })
    }

    /// Start a cell that waits for source and per-unit admission.
    pub fn stream(&self, cell: CellId, execution: Arc<dyn Execution>) -> Result<(), String> {
        self.admit(cell, execution, Input::BeginStream { cell })
    }

    /// Queue an input behind whatever is already queued. Fails only when the
    /// input is too large or the runtime is gone.
    pub fn send(&self, input: Input) -> Result<(), String> {
        let size = match &input {
            Input::Execute { source, .. } | Input::StreamFeed { source, .. } => source.len(),
            _ => 0,
        };
        if size > MAX_MESSAGE_BYTES {
            return Err("Python input exceeds 1 MiB".into());
        }
        if self.registry.stopped.load(Ordering::Acquire) {
            return Err("Python runtime disconnected".into());
        }
        self.inbox.post(Message::Input(input));
        Ok(())
    }

    /// The same queue as [`Sender::send`], for callers that already await.
    pub async fn send_async(&self, input: Input) -> Result<(), String> {
        self.send(input)
    }

    /// Cancel the cell's tasks and callbacks. Synchronous Python keeps
    /// running until it next yields to the event loop.
    pub fn cancel(&self, cell: CellId) {
        let _ = self.send(Input::Cancel { cell });
    }
}

pub struct Session {
    sender: Sender,
}

impl Session {
    /// `setup` runs once on the dedicated thread after unsharing its cwd
    /// state. It may enter the agent's mount namespace and set the initial
    /// directory. Host futures run on `runtime`.
    pub fn new(
        setup: impl FnOnce() -> Result<(), String> + Send + 'static,
        host: Host,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, String> {
        let inbox = Inbox::new()?;
        let registry = Arc::new(Registry::default());
        notebook::spawn(
            Arc::clone(&inbox),
            Arc::clone(&registry),
            Box::new(setup),
            host,
            runtime,
        )?;
        Ok(Self {
            sender: Sender { inbox, registry },
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Never join in-process code on an agent/Tokio thread: the loop stops
        // when it next runs, and synchronous Python may still block it.
        self.sender.inbox.post(Message::Input(Input::Shutdown));
    }
}

#[cfg(test)]
mod tests;
