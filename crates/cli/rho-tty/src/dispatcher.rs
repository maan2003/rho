//! Where gpui's tasks run: main-thread runnables go through the same
//! channel as terminal input, so the main loop wakes for either; background
//! runnables go to a worker pool, and delayed ones to a timer thread.

use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use gpui::{
    PlatformDispatcher, Priority, PriorityQueueReceiver, PriorityQueueSender, RunnableVariant,
};

/// What wakes the main loop.
pub(crate) enum MainMsg {
    Run(RunnableVariant),
    Input(crossterm::event::Event),
    InputClosed,
    /// Only wakes the loop, to look for idleness again.
    Wake,
}

pub(crate) struct TtyDispatcher {
    main: Sender<MainMsg>,
    background: PriorityQueueSender<RunnableVariant>,
    timers: Sender<Timer>,
    main_thread: ThreadId,
    /// Background runnables queued or running: work whose result the main
    /// loop has yet to hear of.
    busy: Arc<AtomicUsize>,
}

struct Timer {
    at: Instant,
    seq: u64,
    runnable: RunnableVariant,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Timer {
    // A max-heap: the earliest deadline must compare greatest.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.at, other.seq).cmp(&(self.at, self.seq))
    }
}

impl TtyDispatcher {
    pub(crate) fn new(main: Sender<MainMsg>) -> Self {
        let (background, receiver) = PriorityQueueReceiver::new();
        let busy = Arc::new(AtomicUsize::new(0));
        let workers = thread::available_parallelism().map_or(2, |n| n.get().max(2));
        for i in 0..workers {
            let receiver: PriorityQueueReceiver<RunnableVariant> = receiver.clone();
            let busy = busy.clone();
            thread::Builder::new()
                .name(format!("Worker-{i}"))
                .spawn(move || {
                    for runnable in receiver.iter() {
                        runnable.run();
                        busy.fetch_sub(1, Ordering::SeqCst);
                    }
                })
                .expect("spawn worker");
        }
        let (timers, timer_rx) = mpsc::channel::<Timer>();
        thread::Builder::new()
            .name("Timer".into())
            .spawn(move || {
                let mut heap = BinaryHeap::<Timer>::new();
                loop {
                    let now = Instant::now();
                    while heap.peek().is_some_and(|t| t.at <= now) {
                        heap.pop().unwrap().runnable.run();
                    }
                    let next = match heap.peek() {
                        Some(t) => timer_rx.recv_timeout(t.at.saturating_duration_since(now)),
                        None => timer_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match next {
                        Ok(timer) => heap.push(timer),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            // Dropping a scheduled runnable would panic its
                            // awaiter.
                            std::mem::forget(heap);
                            return;
                        }
                    }
                }
            })
            .expect("spawn timer");
        Self {
            main,
            background,
            timers,
            main_thread: thread::current().id(),
            busy,
        }
    }

    /// Whether background work is queued or running.
    pub(crate) fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }
}

impl PlatformDispatcher for TtyDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        self.busy.fetch_add(1, Ordering::SeqCst);
        if let Err(runnable) = self.background.send(priority, runnable) {
            self.busy.fetch_sub(1, Ordering::SeqCst);
            std::mem::forget(runnable);
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        if let Err(error) = self.main.send(MainMsg::Run(runnable)) {
            // The main loop is gone; the runnable may hold a !Send future
            // that must not drop on this thread.
            std::mem::forget(error);
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let timer = Timer {
            at: Instant::now() + duration,
            seq,
            runnable,
        };
        if let Err(error) = self.timers.send(timer) {
            std::mem::forget(error);
        }
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        thread::spawn(f);
    }
}
