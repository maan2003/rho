use itertools::Itertools;
use scheduler::{Instant, SpawnTime};
use std::{
    cell::LazyCell,
    collections::{HashMap, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

mod actions;
pub use actions::{ActionStatistics, ActionTiming, take_action_stats};
pub(crate) use actions::{save_action_timing, update_running_action};

use serde::{Deserialize, Serialize};

use crate::{SharedString, TasksIncluded, WindowId};

#[cfg(feature = "profiler")]
#[doc(hidden)]
pub fn get_all_timings(included: gpui::TasksIncluded) -> Vec<gpui::ThreadTaskTimings> {
    ThreadTaskTimings::collect(upgraded_thread_timings(), included)
}

#[cfg(feature = "profiler")]
#[doc(hidden)]
pub fn get_current_thread_timings(included: TasksIncluded) -> gpui::ThreadTaskTimings {
    gpui::profiler::get_current_thread_task_timings(included)
}

#[cfg(feature = "profiler")]
#[doc(hidden)]
pub fn take_all_stats(included: TasksIncluded) -> Vec<gpui::ThreadTaskStatistics> {
    ThreadTaskStatistics::collect_and_reset(upgraded_thread_timings(), included)
}

#[cfg(not(feature = "profiler"))]
#[doc(hidden)]
pub fn get_all_timings(_included: gpui::TasksIncluded) -> Vec<gpui::ThreadTaskTimings> {
    Vec::new()
}
#[cfg(not(feature = "profiler"))]
#[doc(hidden)]
pub fn get_current_thread_timings(_included: TasksIncluded) -> gpui::ThreadTaskTimings {
    gpui::ThreadTaskTimings {
        thread_name: None,
        thread_id: std::thread::current().id(),
        timings: Vec::new(),
        stats: TaskStatistics::default(),
        total_pushed: 0,
    }
}
#[cfg(not(feature = "profiler"))]
#[doc(hidden)]
pub fn take_all_stats(_included: TasksIncluded) -> Vec<gpui::ThreadTaskStatistics> {
    Vec::new()
}

#[doc(hidden)]
#[derive(Debug, Copy, Clone)]
pub struct YieldTime(pub Instant);

#[doc(hidden)]
#[derive(Copy, Clone)]
pub struct TaskTiming {
    pub location: &'static core::panic::Location<'static>,
    pub spawned: SpawnTime,
    pub start: Instant,
    pub end: YieldTime,
}

impl std::fmt::Debug for TaskTiming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTiming")
            .field("location", &self.location)
            .field("since_spawned", &self.spawned.0.elapsed())
            .field("last_poll_duration", &self.poll_duration())
            .field("total_runtime", &self.since_spawn())
            .finish()
    }
}

#[doc(hidden)]
#[derive(Debug, Copy, Clone)]
pub struct ActiveTiming {
    pub location: &'static core::panic::Location<'static>,
    pub spawned: SpawnTime,
    pub start: Instant,
}

impl TaskTiming {
    /// A task timing with a duration of zero. Any task will replace this in history.
    pub fn placeholder() -> Self {
        let now = Instant::now();
        Self {
            location: std::panic::Location::caller(),
            spawned: SpawnTime(now),
            start: now,
            end: YieldTime(now),
        }
    }

    #[inline(always)]
    pub fn poll_duration(&self) -> Duration {
        self.end.0 - self.start
    }

    #[inline(always)]
    fn since_spawn(&self) -> Duration {
        self.end.0 - self.spawned.0
    }
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct ThreadTaskTimings {
    pub thread_name: Option<String>,
    pub thread_id: ThreadId,
    pub timings: Vec<TaskTiming>,
    pub stats: TaskStatistics,
    pub total_pushed: u64,
}

impl ThreadTaskTimings {
    /// Convert upgraded per-thread timings into their structured format.
    pub fn collect(
        timings: Vec<(ThreadId, Arc<GuardedTaskTimings>)>,
        included: TasksIncluded,
    ) -> Vec<Self> {
        timings
            .into_iter()
            .map(|(thread_id, timings)| {
                let timings = timings.lock();
                let thread_name = timings.thread_name.clone();
                let total_pushed = timings.total_pushed;
                let completed = &timings.timings;

                let mut vec = Vec::with_capacity(completed.len() + 1); // +1 for running task
                let (s1, s2) = completed.as_slices();
                vec.extend_from_slice(s1);
                vec.extend_from_slice(s2);
                if let TasksIncluded::CompletedAndRunning = included
                    && let Some(running) = timings.running
                {
                    vec.push(TaskTiming {
                        location: running.location,
                        spawned: running.spawned,
                        start: running.start,
                        end: YieldTime(Instant::now()),
                    })
                }

                ThreadTaskTimings {
                    thread_name,
                    thread_id,
                    timings: vec,
                    stats: timings.stats.clone(),
                    total_pushed,
                }
            })
            .collect()
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct ThreadTaskStatistics {
    pub thread_name: Option<String>,
    pub thread_id: ThreadId,
    pub stats: TaskStatistics,
}

impl ThreadTaskStatistics {
    pub fn collect_and_reset(
        timings: Vec<(ThreadId, Arc<GuardedTaskTimings>)>,
        include_running: TasksIncluded,
    ) -> Vec<Self> {
        timings
            .into_iter()
            .map(|(thread_id, timings)| {
                let mut timings = timings.lock();
                let thread_name = timings.thread_name.clone();

                let mut stats = std::mem::take(&mut timings.stats);
                if let TasksIncluded::CompletedAndRunning = include_running
                    && let Some(ActiveTiming {
                        location,
                        spawned,
                        start,
                    }) = timings.running
                {
                    let end = YieldTime(Instant::now());
                    let timing = TaskTiming {
                        location,
                        spawned,
                        start,
                        end,
                    };
                    stats.add_runtime(timing);
                    stats.add_yield_timing(timing);
                }

                Self {
                    thread_name,
                    thread_id,
                    stats,
                }
            })
            .collect()
    }
}

/// Serializable variant of [`core::panic::Location`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedLocation {
    /// Name of the source file
    pub file: SharedString,
    /// Line in the source file
    pub line: u32,
    /// Column in the source file
    pub column: u32,
}

impl From<&core::panic::Location<'static>> for SerializedLocation {
    fn from(value: &core::panic::Location<'static>) -> Self {
        SerializedLocation {
            file: value.file().into(),
            line: value.line(),
            column: value.column(),
        }
    }
}

/// Serializable variant of [`TaskTiming`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedTaskTiming {
    /// Location of the timing
    pub location: SerializedLocation,
    /// Time at which the measurement was reported in nanoseconds
    pub start: u128,
    /// Duration of the measurement in nanoseconds
    pub duration: u128,
}

impl SerializedTaskTiming {
    /// Convert an array of [`TaskTiming`] into their serializable format
    ///
    /// # Params
    ///
    /// `anchor` - [`Instant`] that should be earlier than all timings to use as base anchor
    pub fn convert(anchor: Instant, timings: &[TaskTiming]) -> Vec<SerializedTaskTiming> {
        let serialized = timings
            .iter()
            .map(|timing| {
                let start = timing.start.duration_since(anchor).as_nanos();
                let duration = timing.end.0.duration_since(timing.start).as_nanos();
                SerializedTaskTiming {
                    location: timing.location.into(),
                    start,
                    duration,
                }
            })
            .collect::<Vec<_>>();

        serialized
    }

    /// `anchor` - [`Instant`] that should be earlier than all timings to use as base anchor
    pub fn from(anchor: Instant, timing: TaskTiming) -> SerializedTaskTiming {
        let start = timing.start.duration_since(anchor).as_nanos();
        let duration = timing.end.0.duration_since(timing.start).as_nanos();
        SerializedTaskTiming {
            location: timing.location.into(),
            start,
            duration,
        }
    }
}

/// Serializable variant of [`ThreadTaskTimings`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedThreadTaskTimings {
    /// Thread name
    pub thread_name: Option<String>,
    /// Hash of the thread id
    pub thread_id: u64,
    /// Timing records for this thread
    pub timings: Vec<SerializedTaskTiming>,
}

impl SerializedThreadTaskTimings {
    /// Convert [`ThreadTaskTimings`] into their serializable format
    ///
    /// # Params
    ///
    /// `anchor` - [`Instant`] that should be earlier than all timings to use as base anchor
    pub fn convert(anchor: Instant, timings: ThreadTaskTimings) -> SerializedThreadTaskTimings {
        let serialized_timings = SerializedTaskTiming::convert(anchor, &timings.timings);

        let mut hasher = DefaultHasher::new();
        timings.thread_id.hash(&mut hasher);
        let thread_id = hasher.finish();

        SerializedThreadTaskTimings {
            thread_name: timings.thread_name,
            thread_id,
            timings: serialized_timings,
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct ThreadTimingsDelta {
    /// Hashed thread id
    pub thread_id: u64,
    /// Thread name, if known
    pub thread_name: Option<String>,
    /// New timings since the last call. If the circular buffer wrapped around
    /// since the previous poll, some entries may have been lost.
    pub new_timings: Vec<SerializedTaskTiming>,
}

/// Tracks which timing events have already been seen so that callers can request only unseen events.
#[doc(hidden)]
pub struct ProfilingCollector {
    startup_time: Instant,
    cursors: HashMap<ThreadId, u64>,
}

impl ProfilingCollector {
    pub fn new(startup_time: Instant) -> Self {
        Self {
            startup_time,
            cursors: HashMap::default(),
        }
    }

    pub fn startup_time(&self) -> Instant {
        self.startup_time
    }

    pub fn collect_unseen(
        &mut self,
        all_timings: Vec<ThreadTaskTimings>,
    ) -> Vec<ThreadTimingsDelta> {
        let mut deltas = Vec::with_capacity(all_timings.len());

        for thread in all_timings {
            let mut hasher = DefaultHasher::new();
            thread.thread_id.hash(&mut hasher);
            let hashed_id = hasher.finish();

            let prev_cursor = self.cursors.get(&thread.thread_id).copied().unwrap_or(0);
            let buffer_len = thread.timings.len() as u64;
            let buffer_start = thread.total_pushed.saturating_sub(buffer_len);

            let mut slice = if prev_cursor < buffer_start {
                // Cursor fell behind the buffer — some entries were evicted.
                // Return everything still in the buffer.
                thread.timings.as_slice()
            } else {
                let skip = (prev_cursor - buffer_start) as usize;
                &thread.timings[skip.min(thread.timings.len())..]
            };

            let cursor_advance = thread.total_pushed;
            self.cursors.insert(thread.thread_id, cursor_advance);

            if slice.is_empty() {
                continue;
            }

            let new_timings = SerializedTaskTiming::convert(self.startup_time, slice);

            deltas.push(ThreadTimingsDelta {
                thread_id: hashed_id,
                thread_name: thread.thread_name,
                new_timings,
            });
        }

        deltas
    }

    pub fn reset(&mut self) {
        self.cursors.clear();
    }
}

// Allow 16MiB of task timing entries.
// VecDeque grows by doubling its capacity when full, so keep this a power of 2 to avoid wasting
// memory.
#[cfg(feature = "profiler")]
const MAX_TASK_TIMINGS: usize = (16 * 1024 * 1024) / core::mem::size_of::<TaskTiming>();

#[doc(hidden)]
pub(crate) type TaskTimings = VecDeque<TaskTiming>;

#[doc(hidden)]
pub type GuardedTaskTimings = spin::Mutex<ThreadTimings>;

#[doc(hidden)]
pub struct GlobalThreadTimings {
    pub thread_id: ThreadId,
    pub timings: std::sync::Weak<GuardedTaskTimings>,
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct TaskStatistics {
    pub poll_time_to_beat: Duration,
    pub runtime_to_beat: Duration,
    pub longest_poll_times: [TaskTiming; 5],
    pub longest_runtimes: [TaskTiming; 5],
}

impl std::fmt::Display for TaskStatistics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tasks that blocked the longest before yielding\n")?;
        for timing in self.longest_poll_times {
            f.write_fmt(format_args!(
                "{:<20} - {}:{}\n",
                format!("{:?}", timing.poll_duration()),
                timing.location.file(),
                timing.location.column()
            ))?;
        }
        f.write_str("Tasks that ran the longest\n")?;
        for timing in self.longest_runtimes {
            f.write_fmt(format_args!(
                "{:<20} - {}:{}\n",
                format!("{:?}", timing.since_spawn()),
                timing.location.file(),
                timing.location.column()
            ))?;
        }
        Ok(())
    }
}

impl Default for TaskStatistics {
    fn default() -> Self {
        Self {
            // Do not track polls that are not problematic
            // this keeps more calls on the fast path
            poll_time_to_beat: Duration::from_micros(100),
            runtime_to_beat: Duration::from_micros(100),
            longest_poll_times: [TaskTiming::placeholder(); 5],
            longest_runtimes: [TaskTiming::placeholder(); 5],
        }
    }
}

impl TaskStatistics {
    #[inline(always)]
    fn add_yield_timing(&mut self, task: TaskTiming) {
        let yielded_after = task.poll_duration();
        if yielded_after >= self.poll_time_to_beat {
            std::hint::cold_path(); // most tasks are not the worst, optimize for that
            let to_replace = self
                .longest_poll_times
                .iter()
                .position_min_by_key(|task| task.since_spawn())
                .expect("guarded by the comparison with nth_longest_yield_time");
            self.longest_poll_times[to_replace] = task;

            self.poll_time_to_beat = self
                .longest_poll_times
                .iter()
                .map(|task| task.since_spawn())
                .min()
                .expect("never empty");
        }
    }

    #[inline(always)]
    fn add_runtime(&mut self, task: TaskTiming) {
        let runtime = task.since_spawn();
        if runtime >= self.runtime_to_beat {
            std::hint::cold_path(); // most tasks are not the worst, optimize for that
            let to_replace = self
                .longest_runtimes
                .iter()
                .position_min_by_key(|task| task.since_spawn())
                .expect("guarded by the comparison with nth_longest_yield_time");
            self.longest_runtimes[to_replace] = task;

            self.runtime_to_beat = self
                .longest_runtimes
                .iter()
                .map(|task| task.since_spawn())
                .min()
                .expect("never empty");
        }
    }
}

#[doc(hidden)]
pub static GLOBAL_THREAD_TIMINGS: spin::Mutex<Vec<GlobalThreadTimings>> =
    spin::Mutex::new(Vec::new());

/// Upgrades all live per-thread timing handles, holding the global registry
/// lock only for the duration of the upgrades.
///
/// The upgraded `Arc`s must never be dropped while `GLOBAL_THREAD_TIMINGS` is
/// locked: dropping the last strong reference runs [`ThreadTimings::drop`],
/// which locks `GLOBAL_THREAD_TIMINGS` again and would deadlock the
/// non-reentrant spinlock. A thread exiting concurrently can hand off its last
/// reference to us at any time, so callers of this function process (lock,
/// read, drop) the returned handles only after the global lock is released.
fn upgraded_thread_timings() -> Vec<(ThreadId, Arc<GuardedTaskTimings>)> {
    let global_thread_timings = GLOBAL_THREAD_TIMINGS.lock();
    global_thread_timings
        .iter()
        .filter_map(|t| Some((t.thread_id, t.timings.upgrade()?)))
        .collect()
}

thread_local! {
    #[doc(hidden)]
    pub static THREAD_TIMINGS: LazyCell<Arc<GuardedTaskTimings>> = LazyCell::new(|| {
        let current_thread = std::thread::current();
        let thread_name = current_thread.name();
        let thread_id = current_thread.id();
        let timings = ThreadTimings::new(thread_name.map(|e| e.to_string()), thread_id);
        let timings = Arc::new(spin::Mutex::new(timings));

        {
            let timings = Arc::downgrade(&timings);
            let global_timings = GlobalThreadTimings {
                thread_id: std::thread::current().id(),
                timings,
            };
            GLOBAL_THREAD_TIMINGS.lock().push(global_timings);
        }

        timings
    });
}

#[doc(hidden)]
pub struct ThreadTimings {
    pub thread_name: Option<String>,
    pub thread_id: ThreadId,
    pub timings: TaskTimings,
    pub running: Option<ActiveTiming>,
    pub stats: TaskStatistics,
    pub total_pushed: u64,
}

impl ThreadTimings {
    pub fn new(thread_name: Option<String>, thread_id: ThreadId) -> Self {
        ThreadTimings {
            thread_name,
            thread_id,
            timings: TaskTimings::new(),
            stats: TaskStatistics::default(),
            total_pushed: 0,
            running: None,
        }
    }

    #[cfg(feature = "profiler")]
    pub fn update_running_task(
        &mut self,
        spawned: SpawnTime,
        location: &'static std::panic::Location<'_>,
    ) {
        let start = Instant::now();
        self.running = Some(ActiveTiming {
            spawned,
            location,
            start,
        });
    }
    #[cfg(not(feature = "profiler"))]
    pub fn update_running_task(&mut self, _: SpawnTime, _: &'static std::panic::Location<'_>) {}

    #[cfg(feature = "profiler")]
    pub fn save_task_timing(&mut self, ended: YieldTime) {
        let ActiveTiming {
            location,
            start,
            spawned,
        } = self
            .running
            .take()
            .expect("this function is only ever called after register_task_start");

        let timing = TaskTiming {
            location,
            spawned,
            start,
            end: ended,
        };
        self.stats.add_yield_timing(timing);
        self.stats.add_runtime(timing);

        if trace_enabled() {
            std::hint::cold_path(); // optimize for when the profiling is off
            if self.timings.len() >= MAX_TASK_TIMINGS {
                self.timings.pop_front();
            }
            self.timings.push_back(timing);
            self.total_pushed += 1;
        }
    }
    #[cfg(not(feature = "profiler"))]
    pub fn save_task_timing(&mut self, _: YieldTime) {}

    // Running tasks are included in the reliability trace, which is written
    // whenever the foreground executor makes no progress for > n seconds
    pub fn get_thread_task_timings(&self, includes: TasksIncluded) -> ThreadTaskTimings {
        ThreadTaskTimings {
            thread_name: self.thread_name.clone(),
            thread_id: self.thread_id,
            timings: self
                .timings
                .iter()
                .cloned()
                .chain(
                    self.running
                        .filter(|_| matches!(includes, TasksIncluded::CompletedAndRunning))
                        .map(|running| TaskTiming {
                            spawned: running.spawned,
                            location: running.location,
                            start: running.start,
                            end: YieldTime(Instant::now()),
                        }),
                )
                .collect(),
            stats: self.stats.clone(),
            total_pushed: self.total_pushed,
        }
    }
}

impl Drop for ThreadTimings {
    fn drop(&mut self) {
        let mut thread_timings = GLOBAL_THREAD_TIMINGS.lock();

        let Some((index, _)) = thread_timings
            .iter()
            .enumerate()
            .find(|(_, t)| t.thread_id == self.thread_id)
        else {
            return;
        };
        thread_timings.swap_remove(index);
    }
}

#[doc(hidden)]
pub fn update_running_task(spawned: SpawnTime, location: &'static std::panic::Location<'_>) {
    THREAD_TIMINGS.with(|timings| {
        timings.lock().update_running_task(spawned, location);
    });
}

#[doc(hidden)]
pub fn save_task_timing() {
    let yielded_at = YieldTime(Instant::now());
    THREAD_TIMINGS.with(|timings| {
        timings.lock().save_task_timing(yielded_at);
    });
}

#[doc(hidden)]
pub fn get_current_thread_task_timings(include_running: TasksIncluded) -> ThreadTaskTimings {
    THREAD_TIMINGS.with(|timings| timings.lock().get_thread_task_timings(include_running))
}

static PROFILER_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enables or disables task timing trace collection at runtime.
///
/// When transitioning from enabled to disabled, `add_task_timing` becomes a
/// cheaper since only cheap statistics are gathered. The existing per-thread
/// buffers for traces are cleared so stale data isn't reported after a later
/// re-enable. Calls with the current value are a no-op.
pub fn set_trace_enabled(enabled: bool) -> bool {
    if PROFILER_ENABLED.swap(enabled, Ordering::AcqRel) == enabled {
        return false;
    }

    if !enabled {
        for (_, timings) in upgraded_thread_timings() {
            let mut timings = timings.lock();
            timings.timings.clear();
            timings.timings.shrink_to_fit();
            timings.total_pushed = 0;
        }
    }
    true
}

/// Returns whether task timing tracing is enabled.
pub fn trace_enabled() -> bool {
    PROFILER_ENABLED.load(Ordering::Relaxed)
}

/// Timing for a single drawn window frame.
#[derive(Debug, Copy, Clone)]
pub struct FrameTiming {
    /// The window that was drawn.
    pub window_id: WindowId,
    /// How much there was to draw. A duration on its own cannot be divided
    /// by anything, so a slow frame can be named but not explained; this is
    /// what it was slow *per*. Accumulated across every element that
    /// reported during the frame, and zero when nothing did.
    pub work: FrameWorkScale,
    /// When the frame first became dirty (its first invalidation). `None` if
    /// frame tracing was not yet enabled when the invalidation occurred.
    pub dirty_at: Option<Instant>,
    /// Number of invalidations coalesced into this frame.
    pub invalidations: u64,
    /// When `Window::draw` started.
    pub draw_start: Instant,
    /// When root layout and prepaint finished.
    pub prepaint_end: Instant,
    /// When root painting, including accessibility updates, finished.
    pub paint_end: Instant,
    /// When `Window::draw` finished.
    pub draw_end: Instant,
}

/// What one frame had to draw.
///
/// Counts, not contents: like every other payload here these are numeric by
/// design, so profiling never captures text or paths. Elements add their own
/// numbers with [`record_frame_work`] while the frame is being drawn, and
/// [`record_frame_timing`] takes the total.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct FrameWorkScale {
    /// Rows actually on screen.
    pub visible_rows: u64,
    /// Rows the document has, on screen or not.
    pub total_rows: u64,
    /// Block decorations in the display map.
    pub blocks: u64,
    /// Excerpts the multibuffer is composed of.
    pub excerpts: u64,
    /// Inlays spliced into the display map.
    pub inlays: u64,
    /// Cursors and selections being drawn.
    pub cursors: u64,
}

impl FrameWorkScale {
    /// Adds another element's numbers to these. Saturating, because a
    /// profiling counter must never be the thing that panics.
    pub fn add(&mut self, other: Self) {
        self.visible_rows = self.visible_rows.saturating_add(other.visible_rows);
        self.total_rows = self.total_rows.saturating_add(other.total_rows);
        self.blocks = self.blocks.saturating_add(other.blocks);
        self.excerpts = self.excerpts.saturating_add(other.excerpts);
        self.inlays = self.inlays.saturating_add(other.inlays);
        self.cursors = self.cursors.saturating_add(other.cursors);
    }
}

/// CPU time spent submitting a rendered scene to the platform renderer.
#[derive(Debug, Copy, Clone)]
pub struct PresentTiming {
    /// The window whose scene was submitted.
    pub window_id: WindowId,
    /// When platform presentation started.
    pub start: Instant,
    /// When the platform renderer returned.
    pub end: Instant,
}

impl FrameTiming {
    /// Time spent inside `Window::draw`.
    pub fn draw_duration(&self) -> Duration {
        self.draw_end.duration_since(self.draw_start)
    }

    /// Time spent setting up the frame, laying it out, and prepainting it.
    pub fn prepaint_duration(&self) -> Duration {
        self.prepaint_end.duration_since(self.draw_start)
    }

    /// Time spent painting the frame and updating its accessibility tree.
    pub fn paint_duration(&self) -> Duration {
        self.paint_end.duration_since(self.prepaint_end)
    }

    /// Time spent finishing and swapping frame state after painting.
    pub fn finish_duration(&self) -> Duration {
        self.draw_end.duration_since(self.paint_end)
    }

    /// Time from the frame's first invalidation to the end of its draw, if the
    /// first invalidation was observed.
    pub fn dirty_to_draw_duration(&self) -> Option<Duration> {
        self.dirty_at
            .map(|dirty_at| self.draw_end.duration_since(dirty_at))
    }
}

// Keep one MiB of frame timing entries. This ring is enabled in Rho's GUI in
// normal operation, so its memory cost must remain small and fixed.
const MAX_FRAME_TIMINGS: usize = (1024 * 1024) / core::mem::size_of::<FrameTiming>();

struct FrameTimings {
    timings: VecDeque<FrameTiming>,
    total_pushed: u64,
}

static FRAME_TIMINGS: spin::Mutex<FrameTimings> = spin::Mutex::new(FrameTimings {
    timings: VecDeque::new(),
    total_pushed: 0,
});
static PRESENT_TIMINGS: spin::Mutex<VecDeque<PresentTiming>> = spin::Mutex::new(VecDeque::new());

/// What the frame currently being drawn has reported so far.
static FRAME_WORK: spin::Mutex<FrameWorkScale> = spin::Mutex::new(FrameWorkScale {
    visible_rows: 0,
    total_rows: 0,
    blocks: 0,
    excerpts: 0,
    inlays: 0,
    cursors: 0,
});

static FRAME_TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enables or disables frame timing collection at runtime.
///
/// When transitioning from enabled to disabled, the buffered frame timings are
/// cleared so stale data isn't reported after a later re-enable. Returns false
/// if the value was unchanged.
pub fn set_frame_trace_enabled(enabled: bool) -> bool {
    if FRAME_TRACE_ENABLED.swap(enabled, Ordering::AcqRel) == enabled {
        return false;
    }

    if !enabled {
        let mut frames = FRAME_TIMINGS.lock();
        frames.timings.clear();
        frames.timings.shrink_to_fit();
        let mut presents = PRESENT_TIMINGS.lock();
        presents.clear();
        presents.shrink_to_fit();
        *FRAME_WORK.lock() = FrameWorkScale::default();
        let mut work = MAIN_THREAD_WORK.lock();
        work.work.clear();
        work.work.shrink_to_fit();
    }
    true
}

/// Returns whether frame timing collection is enabled.
pub fn frame_trace_enabled() -> bool {
    FRAME_TRACE_ENABLED.load(Ordering::Relaxed)
}

/// Reports how much this element had to draw in the frame being drawn.
///
/// Called during prepaint, once per element that knows its own scale. The
/// numbers accumulate until the frame is recorded, so several editors in one
/// window add up to what the window drew.
///
/// No-op unless frame tracing is enabled via [`set_frame_trace_enabled`].
pub fn record_frame_work(scale: FrameWorkScale) {
    if !frame_trace_enabled() {
        return;
    }
    std::hint::cold_path(); // optimize for when profiling is off

    FRAME_WORK.lock().add(scale);
}

/// Takes what has been reported since the last frame and resets the total.
pub fn take_frame_work() -> FrameWorkScale {
    std::mem::take(&mut *FRAME_WORK.lock())
}

/// Records the timing of a drawn window frame.
///
/// The frame's work scale is taken from what elements reported during it, so
/// callers need not thread it through; whatever is passed in `timing.work` is
/// replaced.
///
/// No-op unless frame tracing is enabled via [`set_frame_trace_enabled`].
pub fn record_frame_timing(mut timing: FrameTiming) {
    if !frame_trace_enabled() {
        return;
    }
    std::hint::cold_path(); // optimize for when profiling is off

    timing.work = take_frame_work();
    let mut frames = FRAME_TIMINGS.lock();
    if frames.timings.len() >= MAX_FRAME_TIMINGS {
        frames.timings.pop_front();
    }
    frames.timings.push_back(timing);
    frames.total_pushed += 1;
}

/// Records CPU time spent in the platform renderer for one presentation.
pub fn record_present_timing(timing: PresentTiming) {
    if !frame_trace_enabled() {
        return;
    }
    std::hint::cold_path();

    let mut presents = PRESENT_TIMINGS.lock();
    if presents.len() >= MAX_FRAME_TIMINGS {
        presents.pop_front();
    }
    presents.push_back(timing);
}

/// Returns the buffered platform presentation timings in chronological order.
pub fn snapshot_present_timings() -> Vec<PresentTiming> {
    PRESENT_TIMINGS.lock().iter().copied().collect()
}

/// Drains frame timings recorded after this collector was created, tracking a
/// cursor so each call to [`Self::collect_unseen`] returns only new entries.
pub struct FrameTimingCollector {
    cursor: u64,
}

/// Returns a non-destructive copy of the current bounded frame timing ring.
pub fn snapshot_frame_timings() -> Vec<FrameTiming> {
    FRAME_TIMINGS.lock().timings.iter().copied().collect()
}

/// A timed text/display pipeline operation. Payloads are numeric by design so
/// profiling never captures buffer contents or paths.
#[derive(Debug, Copy, Clone)]
#[expect(missing_docs)]
pub struct EditorTiming {
    pub kind: EditorTimingKind,
    pub start: Instant,
    pub end: Instant,
    pub tid: u64,
    pub input_edits: u64,
    pub input_start: u64,
    pub input_rows: u64,
    /// Sum of each input edit's own old/new row extent (the larger side).
    pub touched_rows: u64,
    /// Pipeline items actually visited while processing this stage. SumTree
    /// cursors count leaf items but not shared untouched subtrees; stages also
    /// count explicitly scanned non-tree work items.
    pub walked_items: u64,
    pub output_edits: u64,
    pub output_start: u64,
    pub output_rows: u64,
    pub old_rows: u64,
    pub new_rows: u64,
    pub pending_batches: u64,
    pub flags: u64,
    /// Transforms in the map the stage worked on. What a splice is divided
    /// by: the same stage over ten transforms and ten thousand is the same
    /// name and a different thing.
    pub transforms: u64,
    /// The span the stage affected, in the map's own offsets.
    pub affected_start: u64,
    /// The end of that span.
    pub affected_end: u64,
    /// How many distinct offsets inside it were touched. A splice costs the
    /// offsets, not the span, so a wide span over few offsets is cheap and
    /// the two must be told apart.
    pub affected_offsets: u64,
}

#[derive(Debug, Copy, Clone)]
#[repr(u8)]
#[expect(missing_docs)]
pub enum EditorTimingKind {
    BufferEdit = 1,
    MultiBufferSync = 2,
    FoldMapSync = 3,
    TabMapSync = 4,
    WrapMapSync = 5,
    BlockMapSync = 6,
    InlayMapSync = 7,
    WrapMapUpdate = 8,
    /// The dashboard reconciling its tree into the editor's composition.
    SyncTree = 9,
    /// Inlays going into or out of the display map.
    SpliceInlays = 10,
    /// The multibuffer's pass over the buffers marked changed, counted apart
    /// from `MultiBufferSync` because it is a different unit: one item per
    /// changed buffer and one per path it carries, where the sync's own
    /// number is SumTree leaf items crossed. It rises when many buffers
    /// report a change at once - a screenful of parses finishing together -
    /// and not with the size of the document.
    MultiBufferBufferScan = 11,
}

const MAX_EDITOR_TIMINGS: usize = (1024 * 1024) / core::mem::size_of::<EditorTiming>();

struct EditorTimings {
    timings: VecDeque<EditorTiming>,
    total_pushed: u64,
    /// Threads that have claimed the trace, or empty for "every thread".
    /// A claim is how a measurement says which work is its own: the ring
    /// is one buffer for the process and a bounded one, so a thread
    /// working beside a measurement does not only add records to it, it
    /// can push the measurement's own records out before they are read.
    claimed: Vec<u64>,
}

static EDITOR_TIMINGS: spin::Mutex<EditorTimings> = spin::Mutex::new(EditorTimings {
    timings: VecDeque::new(),
    total_pushed: 0,
    claimed: Vec::new(),
});

/// A thread's claim on the editor trace, released when it is dropped.
pub struct EditorTraceClaim(u64);

impl Drop for EditorTraceClaim {
    fn drop(&mut self) {
        let mut timings = EDITOR_TIMINGS.lock();
        if let Some(at) = timings.claimed.iter().position(|tid| *tid == self.0) {
            timings.claimed.remove(at);
        }
    }
}

/// Records only this thread's editor work for as long as the claim is
/// held - and any other thread's that has claimed it too. Unclaimed, the
/// ring records every thread, which is what the running application wants
/// and what a measurement standing next to other work does not.
pub fn claim_editor_trace_for_this_thread() -> EditorTraceClaim {
    let tid = editor_profile_tid();
    EDITOR_TIMINGS.lock().claimed.push(tid);
    EditorTraceClaim(tid)
}

static EDITOR_TRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static EDITOR_TRACE_GENERATION: AtomicU64 = AtomicU64::new(0);

#[expect(missing_docs)]
pub fn set_editor_trace_enabled(enabled: bool) -> bool {
    let mut timings = EDITOR_TIMINGS.lock();
    if EDITOR_TRACE_ENABLED.load(Ordering::Relaxed) == enabled {
        return false;
    }
    if enabled {
        timings.timings.clear();
        timings.timings.shrink_to_fit();
    }
    EDITOR_TRACE_GENERATION.fetch_add(1, Ordering::Relaxed);
    EDITOR_TRACE_ENABLED.store(enabled, Ordering::Release);
    true
}

#[expect(missing_docs)]
pub fn editor_trace_enabled() -> bool {
    EDITOR_TRACE_ENABLED.load(Ordering::Relaxed)
}

fn record_editor_timing(timing: EditorTiming, generation: u64) {
    let mut timings = EDITOR_TIMINGS.lock();
    if !EDITOR_TRACE_ENABLED.load(Ordering::Acquire) {
        return;
    }
    if generation != EDITOR_TRACE_GENERATION.load(Ordering::Relaxed) {
        return;
    }
    if !timings.claimed.is_empty() && !timings.claimed.contains(&timing.tid) {
        return;
    }
    if timings.timings.len() >= MAX_EDITOR_TIMINGS {
        timings.timings.pop_front();
    }
    timings.timings.push_back(timing);
    timings.total_pushed += 1;
}

/// Records elapsed time on drop, including early-return paths.
pub struct EditorTimingGuard(Option<(EditorTiming, u64)>);

#[expect(missing_docs)]
impl EditorTimingGuard {
    pub fn new(kind: EditorTimingKind) -> Self {
        let generation = EDITOR_TRACE_GENERATION.load(Ordering::Relaxed);
        Self(editor_trace_enabled().then(|| {
            let now = Instant::now();
            (
                EditorTiming {
                    kind,
                    start: now,
                    end: now,
                    tid: editor_profile_tid(),
                    input_edits: 0,
                    input_start: 0,
                    input_rows: 0,
                    touched_rows: 0,
                    walked_items: 0,
                    output_edits: 0,
                    output_start: 0,
                    output_rows: 0,
                    old_rows: 0,
                    new_rows: 0,
                    pending_batches: 0,
                    flags: 0,
                    transforms: 0,
                    affected_start: 0,
                    affected_end: 0,
                    affected_offsets: 0,
                },
                generation,
            )
        }))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    pub fn input(&mut self, edits: usize, start: u64, rows: u64) {
        if let Some((timing, _)) = &mut self.0 {
            timing.input_edits = edits as u64;
            timing.input_start = start;
            timing.input_rows = rows;
        }
    }

    pub fn touched_rows(&mut self, rows: u64) {
        if let Some((timing, _)) = &mut self.0 {
            timing.touched_rows = rows;
        }
    }

    pub fn walked_items(&mut self, items: u64) {
        if let Some((timing, _)) = &mut self.0 {
            timing.walked_items = timing.walked_items.saturating_add(items);
        }
    }

    pub fn output(&mut self, edits: usize, start: u64, rows: u64) {
        if let Some((timing, _)) = &mut self.0 {
            timing.output_edits = edits as u64;
            timing.output_start = start;
            timing.output_rows = rows;
        }
    }

    pub fn state(&mut self, old_rows: u64, new_rows: u64, pending_batches: usize, flags: u64) {
        if let Some((timing, _)) = &mut self.0 {
            timing.old_rows = old_rows;
            timing.new_rows = new_rows;
            timing.pending_batches = pending_batches as u64;
            timing.flags = flags;
        }
    }

    pub fn thread(&mut self, tid: u64) {
        if let Some((timing, _)) = &mut self.0 {
            timing.tid = tid;
        }
    }

    /// What the stage worked on and where. `affected` is a span in the map's
    /// own offsets and `offsets` is how many distinct points inside it were
    /// touched, which is the number a splice's cost is actually in.
    pub fn spliced(
        &mut self,
        transforms: u64,
        affected: std::ops::Range<u64>,
        affected_offsets: u64,
    ) {
        if let Some((timing, _)) = &mut self.0 {
            timing.transforms = transforms;
            timing.affected_start = affected.start;
            timing.affected_end = affected.end;
            timing.affected_offsets = affected_offsets;
        }
    }
}

fn editor_profile_tid() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: gettid has no arguments or memory-safety preconditions.
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut hasher = DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        hasher.finish()
    }
}

impl Drop for EditorTimingGuard {
    fn drop(&mut self) {
        if let Some((mut timing, generation)) = self.0.take() {
            timing.end = Instant::now();
            record_editor_timing(timing, generation);
        }
    }
}

/// Who was on the main thread, under a name.
///
/// The frame ring accounts for `Window::draw` as three numbers — prepaint,
/// paint, present — and nothing else, so neither the work between frames nor
/// the parts of a long prepaint has anywhere to be seen. Both are why a frame
/// is late and neither is visible in a frame number, so a record names its
/// owner rather than leaving it to be guessed from a stack.
///
/// Most records are of work outside a frame, which is where this started and
/// what the named kinds below describe. A caller may also name a span inside
/// one — the passes of a prepaint, say — under [`Self::Other`]; such a span is
/// a share of a frame's own time and a reader that adds it to the
/// between-frame total counts the same milliseconds twice.
#[derive(Debug, Copy, Clone)]
#[expect(missing_docs)]
pub enum MainThreadWorkKind {
    /// Reconciling a model event into the UI.
    ModelEvent,
    /// Rebuilding or patching the desk's map.
    DeskSync,
    /// Work a background task handed back to the main thread.
    TaskCompletion,
    /// Anything else the caller chose to name, under that name.
    ///
    /// The named kinds above are the ones gpui knows the shape of. An
    /// application whose between-frame work has parts — a sync made of
    /// several passes, say — can record each part under its own label
    /// and keep the whole beside it, and a reader gets the breakdown
    /// without gpui having to grow a variant per application. The label
    /// is `'static` so a record stays `Copy` and the ring keeps costing
    /// a fixed number of bytes per entry.
    Other(&'static str),
}

/// One span of main-thread work outside a frame.
#[derive(Debug, Copy, Clone)]
pub struct MainThreadWork {
    /// What was running.
    pub owner: MainThreadWorkKind,
    /// When it started.
    pub start: Instant,
    /// When it finished.
    pub end: Instant,
    /// How much it did, in whatever the owner counts — events reconciled,
    /// rows patched. Divides the duration the way a frame's scale does.
    pub work_units: u64,
}

// Keep the same bound as the frame ring: this is on in normal operation.
const MAX_MAIN_THREAD_WORK: usize = (1024 * 1024) / core::mem::size_of::<MainThreadWork>();

struct MainThreadWorkLog {
    work: VecDeque<MainThreadWork>,
    total_pushed: u64,
}

static MAIN_THREAD_WORK: spin::Mutex<MainThreadWorkLog> = spin::Mutex::new(MainThreadWorkLog {
    work: VecDeque::new(),
    total_pushed: 0,
});

/// Records a named span of main-thread work.
///
/// Usually work outside a frame; a caller that names a span inside one owns
/// saying so in the label, since nothing here can tell the two apart.
///
/// No-op unless frame tracing is enabled via [`set_frame_trace_enabled`].
/// The ring is shared and bounded, so a caller that records per frame rather
/// than per event is the one that decides how far back the log reaches.
pub fn record_main_thread_work(work: MainThreadWork) {
    if !frame_trace_enabled() {
        return;
    }
    std::hint::cold_path();

    let mut log = MAIN_THREAD_WORK.lock();
    if log.work.len() >= MAX_MAIN_THREAD_WORK {
        log.work.pop_front();
    }
    log.work.push_back(work);
    log.total_pushed += 1;
}

/// The buffered outside-frame work, with how many records have ever been
/// pushed. The second number is the point: the ring drops its oldest, so a
/// reader that does not know the total cannot tell a quiet period from a
/// dropped one.
pub fn snapshot_main_thread_work() -> (Vec<MainThreadWork>, u64) {
    let log = MAIN_THREAD_WORK.lock();
    (log.work.iter().copied().collect(), log.total_pushed)
}

/// How many editor timings have ever been pushed, against however many the
/// ring still holds. A stage total read without this is a total over an
/// unknown window.
pub fn editor_timings_pushed() -> u64 {
    EDITOR_TIMINGS.lock().total_pushed
}

/// The same for frames.
pub fn frame_timings_pushed() -> u64 {
    FRAME_TIMINGS.lock().total_pushed
}

#[expect(missing_docs)]
pub struct EditorTimingCollector {
    cursor: u64,
}

#[expect(missing_docs)]
impl EditorTimingCollector {
    pub fn new() -> Self {
        Self {
            cursor: EDITOR_TIMINGS.lock().total_pushed,
        }
    }

    pub fn collect_unseen(&mut self) -> Vec<EditorTiming> {
        let timings = EDITOR_TIMINGS.lock();
        let buffer_len = timings.timings.len() as u64;
        let buffer_start = timings.total_pushed.saturating_sub(buffer_len);
        let skip = self.cursor.saturating_sub(buffer_start) as usize;
        let unseen = timings
            .timings
            .iter()
            .skip(skip.min(timings.timings.len()))
            .copied()
            .collect();
        self.cursor = timings.total_pushed;
        unseen
    }
}

/// Returns a non-destructive copy of the current bounded editor timing ring.
pub fn snapshot_editor_timings() -> Vec<EditorTiming> {
    EDITOR_TIMINGS.lock().timings.iter().copied().collect()
}

impl Default for FrameTimingCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameTimingCollector {
    /// Creates a collector that only sees frames recorded from this point on.
    pub fn new() -> Self {
        Self {
            cursor: FRAME_TIMINGS.lock().total_pushed,
        }
    }

    /// Returns frame timings recorded since the previous call (or since the
    /// collector was created). If the ring buffer wrapped around since the
    /// previous poll, the evicted entries are lost.
    pub fn collect_unseen(&mut self) -> Vec<FrameTiming> {
        let frames = FRAME_TIMINGS.lock();
        let buffer_len = frames.timings.len() as u64;
        let buffer_start = frames.total_pushed.saturating_sub(buffer_len);
        let skip = self.cursor.saturating_sub(buffer_start) as usize;
        let unseen = frames
            .timings
            .iter()
            .skip(skip.min(frames.timings.len()))
            .copied()
            .collect();
        self.cursor = frames.total_pushed;
        unseen
    }
}

#[cfg(test)]
mod timing_ring_tests {
    use super::*;

    #[derive(Clone)]
    struct WalkItem;

    #[derive(Clone, Default)]
    struct WalkSummary(usize);

    #[derive(Clone, Default, Eq, Ord, PartialEq, PartialOrd)]
    struct WalkCount(usize);

    impl sum_tree::Summary for WalkSummary {
        type Context<'a> = ();

        fn zero(_: ()) -> Self {
            Self(0)
        }

        fn add_summary(&mut self, other: &Self, _: ()) {
            self.0 += other.0;
        }
    }

    impl sum_tree::Item for WalkItem {
        type Summary = WalkSummary;

        fn summary(&self, _: ()) -> Self::Summary {
            WalkSummary(1)
        }
    }

    impl sum_tree::Dimension<'_, WalkSummary> for WalkCount {
        fn zero(_: ()) -> Self {
            Self(0)
        }

        fn add_summary(&mut self, summary: &WalkSummary, _: ()) {
            self.0 += summary.0;
        }
    }

    /// The profiler's state is one per process: the trace flags, the
    /// editor timing ring and the frame work accumulator are all statics.
    /// A test that touches any of them cannot share the process with
    /// another that does, so every test here takes this first. Two did
    /// and four did not, which is why `--test-threads=1` passed all six
    /// and the default threads either failed two or hung.
    ///
    /// A panicking test poisons the lock; the rest take it anyway rather
    /// than fail on the poison, since the failure to report is the first
    /// test's and not theirs.
    fn exclusive_profiler_state() -> std::sync::MutexGuard<'static, ()> {
        static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn editor_timing_snapshot_is_non_destructive_and_bounded() {
        let _state = exclusive_profiler_state();
        set_editor_trace_enabled(false);
        set_editor_trace_enabled(true);
        for _ in 0..MAX_EDITOR_TIMINGS + 17 {
            drop(EditorTimingGuard::new(EditorTimingKind::BufferEdit));
        }
        let first = snapshot_editor_timings();
        let second = snapshot_editor_timings();
        assert_eq!(first.len(), MAX_EDITOR_TIMINGS);
        assert_eq!(second.len(), first.len());
        set_editor_trace_enabled(false);
    }

    #[test]
    fn editor_collector_survives_trace_toggle() {
        let _state = exclusive_profiler_state();
        set_editor_trace_enabled(false);
        let mut collector = EditorTimingCollector::new();
        set_editor_trace_enabled(true);
        drop(EditorTimingGuard::new(EditorTimingKind::BufferEdit));
        assert_eq!(collector.collect_unseen().len(), 1);
        set_editor_trace_enabled(false);
    }

    #[test]
    fn a_frame_s_work_is_the_sum_of_what_its_elements_reported() {
        let _state = exclusive_profiler_state();
        set_frame_trace_enabled(true);
        let _ = take_frame_work();

        record_frame_work(FrameWorkScale {
            visible_rows: 40,
            total_rows: 1_000,
            inlays: 3,
            ..Default::default()
        });
        // A second element in the same frame: a window can draw more than
        // one editor, and what the frame drew is all of them.
        record_frame_work(FrameWorkScale {
            visible_rows: 2,
            total_rows: 5,
            cursors: 1,
            ..Default::default()
        });

        let taken = take_frame_work();
        assert_eq!(taken.visible_rows, 42);
        assert_eq!(taken.total_rows, 1_005);
        assert_eq!(taken.inlays, 3);
        assert_eq!(taken.cursors, 1);
        // Taking resets: the next frame starts from nothing, or every frame
        // would report the whole session's work.
        assert_eq!(take_frame_work(), FrameWorkScale::default());

        set_frame_trace_enabled(false);
    }

    #[test]
    fn work_outside_a_frame_is_recorded_with_its_owner_and_counted() {
        let _state = exclusive_profiler_state();
        set_frame_trace_enabled(true);
        let before = snapshot_main_thread_work().1;

        let start = Instant::now();
        record_main_thread_work(MainThreadWork {
            owner: MainThreadWorkKind::ModelEvent,
            start,
            end: start,
            work_units: 7,
        });

        let (work, total) = snapshot_main_thread_work();
        assert_eq!(total, before + 1);
        let last = work.last().expect("the record just pushed");
        assert_eq!(last.work_units, 7);
        assert!(matches!(last.owner, MainThreadWorkKind::ModelEvent));

        set_frame_trace_enabled(false);
    }

    #[test]
    fn a_stage_can_say_what_it_worked_on() {
        let _state = exclusive_profiler_state();
        set_editor_trace_enabled(true);
        let mut collector = EditorTimingCollector::new();
        {
            let mut guard = EditorTimingGuard::new(EditorTimingKind::SpliceInlays);
            guard.spliced(4_000, 120..980, 16);
            guard.touched_rows(1);
            guard.walked_items(7);
        }
        let timings = collector.collect_unseen();
        let timing = timings.last().expect("the stage just recorded");
        assert_eq!(timing.transforms, 4_000);
        assert_eq!(timing.affected_start, 120);
        assert_eq!(timing.affected_end, 980);
        // The offsets, not the span: a splice costs the points it touches.
        assert_eq!(timing.affected_offsets, 16);
        assert_eq!(timing.touched_rows, 1);
        assert_eq!(timing.walked_items, 7);
        set_editor_trace_enabled(false);
    }

    #[test]
    fn a_small_edit_in_a_large_tree_reports_a_small_walk() {
        let _state = exclusive_profiler_state();
        use sum_tree::{Bias, SumTree};

        set_editor_trace_enabled(true);
        let mut collector = EditorTimingCollector::new();
        let tree = SumTree::from_iter((0..2_000).map(|_| WalkItem), ());
        let mut cursor = tree.cursor::<WalkCount>(());
        let _prefix = cursor.slice(&WalkCount(1_000), Bias::Right);
        cursor.next();

        let mut guard = EditorTimingGuard::new(EditorTimingKind::FoldMapSync);
        guard.touched_rows(1);
        guard.walked_items(cursor.walked_items());
        drop(guard);

        let timing = collector.collect_unseen().pop().unwrap();
        assert_eq!(timing.touched_rows, 1);
        assert!(timing.walked_items > 0);
        assert!(timing.walked_items < 64);
        set_editor_trace_enabled(false);
    }
}
