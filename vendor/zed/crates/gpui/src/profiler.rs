#[cfg(feature = "profiler")]
use hdrhistogram::Histogram;
use itertools::Itertools;
use scheduler::SpawnTime;
#[cfg(feature = "profiler")]
use smallvec::SmallVec;
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
#[cfg(feature = "profiler")]
pub mod hang;
#[cfg(feature = "profiler")]
pub mod journal;
pub use actions::{
    ActionStatistics, ActionTiming, save_action_timing, take_action_stats, update_running_action,
};
/// The clock every timing here is stamped with. Public because the structs
/// that carry it are: a caller that reads `MainThreadWork::start` needs to
/// be able to name what it read.
pub use scheduler::Instant;

use serde::{Deserialize, Serialize};

#[cfg(feature = "profiler")]
use crate::{Action, App};
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
    pub fn save_task_timing(&mut self, ended: YieldTime) -> TaskTiming {
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
        timing
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
    #[cfg(feature = "profiler")]
    journal::begin_foreground_turn();
    THREAD_TIMINGS.with(|timings| {
        timings.lock().update_running_task(spawned, location);
    });
}

#[doc(hidden)]
pub fn save_task_timing() {
    let yielded_at = YieldTime(Instant::now());
    #[cfg(feature = "profiler")]
    {
        let timing = THREAD_TIMINGS.with(|timings| timings.lock().save_task_timing(yielded_at));
        journal::record_task_poll(timing);
    }
    #[cfg(not(feature = "profiler"))]
    THREAD_TIMINGS.with(|timings| {
        timings.lock().save_task_timing(yielded_at);
    });
}

#[doc(hidden)]
pub fn get_current_thread_task_timings(include_running: TasksIncluded) -> ThreadTaskTimings {
    THREAD_TIMINGS.with(|timings| timings.lock().get_thread_task_timings(include_running))
}

const TRACE_SETTING_ENABLED: u64 = 1 << 63;
const TRACE_SCOPE_COUNT_MASK: u64 = TRACE_SETTING_ENABLED - 1;
static TRACE_STATE: AtomicU64 = AtomicU64::new(0);

/// Enables or disables profiler trace collection at runtime.
///
/// When transitioning from enabled to disabled, `add_task_timing` becomes
/// cheaper since only cheap statistics are gathered. The existing per-thread
/// task buffers and the frame-event buffer are cleared so stale data isn't
/// reported after a later re-enable. Active trace scopes keep collection enabled
/// until the last scope ends. Calls with the current setting are a no-op.
pub fn set_trace_enabled(enabled: bool) -> bool {
    let mut state = TRACE_STATE.load(Ordering::Acquire);
    loop {
        let was_enabled = state & TRACE_SETTING_ENABLED != 0;
        if was_enabled == enabled {
            return false;
        }

        let next_state = if enabled {
            state | TRACE_SETTING_ENABLED
        } else {
            state & TRACE_SCOPE_COUNT_MASK
        };
        match TRACE_STATE.compare_exchange_weak(
            state,
            next_state,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                if next_state == 0 {
                    clear_trace_buffers();
                }
                return true;
            }
            Err(updated_state) => state = updated_state,
        }
    }
}

#[cfg(any(feature = "bench-support", all(test, feature = "profiler")))]
pub(crate) struct TraceGuard;

#[cfg(any(feature = "bench-support", all(test, feature = "profiler")))]
pub(crate) fn trace_scope() -> TraceGuard {
    let incremented = TRACE_STATE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
        (state & TRACE_SCOPE_COUNT_MASK < TRACE_SCOPE_COUNT_MASK).then_some(state + 1)
    });
    assert!(incremented.is_ok(), "too many active profiler trace scopes");
    TraceGuard
}

#[cfg(any(feature = "bench-support", all(test, feature = "profiler")))]
impl Drop for TraceGuard {
    fn drop(&mut self) {
        let previous_state =
            TRACE_STATE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & TRACE_SCOPE_COUNT_MASK > 0).then_some(state - 1)
            });
        match previous_state {
            Ok(1) => clear_trace_buffers(),
            Ok(_) => {}
            Err(_) => debug_assert!(false, "profiler trace scope count underflowed"),
        }
    }
}

/// Returns whether profiler trace collection is enabled.
pub fn trace_enabled() -> bool {
    TRACE_STATE.load(Ordering::Relaxed) != 0
}

fn clear_trace_buffers() {
    for (_, timings) in upgraded_thread_timings() {
        let mut timings = timings.lock();
        timings.timings.clear();
        timings.timings.shrink_to_fit();
        timings.total_pushed = 0;
    }
    #[cfg(feature = "profiler")]
    {
        let mut frames = FRAME_EVENTS.lock();
        frames.timings.clear();
        frames.timings.shrink_to_fit();
        frames.total_pushed = 0;
    }
}

/// Timing for a single drawn window frame.
#[cfg(feature = "profiler")]
#[derive(Debug, Copy, Clone)]
pub struct ProfilerFrameTiming {
    /// The window that was drawn.
    pub window_id: WindowId,
    /// When the frame first became dirty (its first invalidation). `None` if
    /// profiler tracing was not yet enabled when the invalidation occurred.
    pub dirty_at: Option<Instant>,
    /// Number of invalidations coalesced into this frame.
    pub invalidations: u64,
    /// When `Window::draw` started.
    pub draw_start: Instant,
    /// When `Window::draw` finished.
    pub draw_end: Instant,
}

#[cfg(feature = "profiler")]
impl ProfilerFrameTiming {
    /// Time spent inside `Window::draw`.
    pub fn draw_duration(&self) -> Duration {
        self.draw_end.duration_since(self.draw_start)
    }

    /// Time from the frame's first invalidation to the end of its draw, if the
    /// first invalidation was observed.
    pub fn dirty_to_draw_duration(&self) -> Option<Duration> {
        self.dirty_at
            .map(|dirty_at| self.draw_end.duration_since(dirty_at))
    }
}

/// Work spent submitting a window frame to the platform.
#[cfg(feature = "profiler")]
#[derive(Debug, Copy, Clone)]
pub struct ProfilerPresentTiming {
    /// The window whose frame was submitted.
    pub window_id: WindowId,
    /// When the platform submission began.
    pub present_start: Instant,
    /// When the platform submission completed.
    pub present_end: Instant,
    /// The interval since the previous newly drawn frame was submitted, when
    /// both frames belong to an active animation.
    pub animation_interval: Option<Duration>,
}

#[cfg(feature = "profiler")]
impl ProfilerPresentTiming {
    /// Time spent submitting the frame to the platform.
    pub fn present_duration(&self) -> Duration {
        self.present_end.duration_since(self.present_start)
    }
}

/// A frame event observed by the profiler.
#[cfg(feature = "profiler")]
#[derive(Debug, Copy, Clone)]
pub enum FrameEvent {
    /// A window frame was drawn.
    Draw(ProfilerFrameTiming),
    /// A newly drawn window frame was presented.
    Present(ProfilerPresentTiming),
}

/// A point-in-time snapshot of the frame-duration histograms for a window,
/// suitable for external formatting.
#[cfg(feature = "profiler")]
#[derive(Clone)]
pub struct FrameDurationSnapshot {
    /// Histogram of durations from the first invalidation through presentation, in nanoseconds.
    pub dirty_to_present_histogram: Histogram<u64>,
    /// Histogram of `Window::draw` durations, in nanoseconds.
    pub draw_duration_histogram: Histogram<u64>,
    /// Histogram of intervals between consecutively presented frames while the
    /// window was animating, in nanoseconds.
    pub present_interval_histogram: Histogram<u64>,
}

/// A point-in-time snapshot of the input-latency histograms for a window,
/// suitable for external formatting.
#[cfg(feature = "profiler")]
#[derive(Clone)]
pub struct InputLatencySnapshot {
    /// Histogram of input-to-frame latency samples, in nanoseconds.
    pub latency_histogram: Histogram<u64>,
    /// Histogram of input events coalesced per rendered frame.
    pub events_per_frame_histogram: Histogram<u64>,
    /// Count of input events that arrived mid-draw and were excluded from
    /// latency recording.
    pub mid_draw_events_dropped: u64,
}

#[cfg(feature = "profiler")]
enum WindowActivity {
    Input {
        started_at: Instant,
        kind: &'static str,
    },
    Draw {
        started_at: Instant,
    },
}

/// Collects profiling information for one window.
///
/// Aggregate histograms are always populated when the `profiler` feature is
/// compiled in. Individual draw and present events are added to the global
/// profiler buffer only while tracing is enabled.
#[cfg(feature = "profiler")]
pub struct WindowProfiler {
    window_id: WindowId,
    active_activities: SmallVec<[WindowActivity; 4]>,
    active_actions: SmallVec<[(&'static str, Instant); 2]>,
    dirty_to_present_histogram: Histogram<u64>,
    draw_duration_histogram: Histogram<u64>,
    present_interval_histogram: Histogram<u64>,
    first_input_at: Option<Instant>,
    pending_input_count: u64,
    input_latency_histogram: Histogram<u64>,
    events_per_frame_histogram: Histogram<u64>,
    mid_draw_events_dropped: u64,
    last_present_at: Option<Instant>,
    animating_at_last_present: bool,
    pending_frame: Option<ProfilerFrameTiming>,
}

#[cfg(feature = "profiler")]
impl WindowProfiler {
    /// Creates a profiler for a window.
    pub fn new(window_id: WindowId) -> anyhow::Result<Self> {
        let profiler = Self {
            window_id,
            active_activities: SmallVec::new(),
            active_actions: SmallVec::new(),
            dirty_to_present_histogram: Histogram::new(3).map_err(|error| {
                anyhow::anyhow!("Failed to create dirty-to-present histogram: {error}")
            })?,
            draw_duration_histogram: Histogram::new(3).map_err(|error| {
                anyhow::anyhow!("Failed to create draw duration histogram: {error}")
            })?,
            present_interval_histogram: Histogram::new(3).map_err(|error| {
                anyhow::anyhow!("Failed to create present interval histogram: {error}")
            })?,
            first_input_at: None,
            pending_input_count: 0,
            input_latency_histogram: Histogram::new(3).map_err(|error| {
                anyhow::anyhow!("Failed to create input latency histogram: {error}")
            })?,
            events_per_frame_histogram: Histogram::new(3).map_err(|error| {
                anyhow::anyhow!("Failed to create events per frame histogram: {error}")
            })?,
            mid_draw_events_dropped: 0,
            last_present_at: None,
            animating_at_last_present: false,
            pending_frame: None,
        };
        journal::record_frame_pending(window_id, Instant::now());
        Ok(profiler)
    }

    /// Records the beginning of an input dispatch. `kind` names the platform
    /// input variant being dispatched (see [`crate::PlatformInput::kind_name`]).
    pub fn begin_input(&mut self, kind: &'static str) {
        journal::begin_foreground_turn();
        self.active_activities.push(WindowActivity::Input {
            started_at: Instant::now(),
            kind,
        });
    }

    /// Records the end of an input dispatch.
    pub fn end_input(&mut self, caused_invalidation: bool) {
        let Some(WindowActivity::Input { started_at, kind }) = self.active_activities.pop() else {
            debug_assert!(false, "input activity must be the current window activity");
            journal::end_foreground_turn();
            return;
        };

        if !journal::power_interrupted_since(started_at) {
            journal::record_input(journal::InputTiming {
                kind,
                start: started_at,
                end: Instant::now(),
                caused_invalidation,
            });
        }
        journal::end_foreground_turn();

        if !caused_invalidation || !journal::frame_sample_is_valid(self.window_id, started_at) {
            return;
        }
        if self
            .first_input_at
            .is_some_and(|at| !journal::frame_sample_is_valid(self.window_id, at))
        {
            self.first_input_at = None;
            self.pending_input_count = 0;
        }

        let arrived_during_draw = self
            .active_activities
            .iter()
            .any(|activity| matches!(activity, WindowActivity::Draw { .. }));
        if arrived_during_draw {
            self.mid_draw_events_dropped += 1;
        } else {
            self.first_input_at.get_or_insert(started_at);
            self.pending_input_count += 1;
        }
    }

    /// Records the beginning of an action handler.
    pub fn begin_action_handler(&mut self, action: &(dyn Action + 'static), cx: &mut App) {
        journal::begin_foreground_turn();
        let name = actions::update_running_action(action, cx);
        self.active_actions.push((name, Instant::now()));
    }

    /// Records the end of the current action handler.
    pub fn end_action_handler(&mut self) {
        // Dual-write to the legacy aggregate store; its single global running
        // slot misbehaves when tests run actions concurrently, which is why
        // the journal entry is tracked here on the window instead.
        actions::save_action_timing();
        let Some((name, start)) = self.active_actions.pop() else {
            debug_assert!(false, "action handler must be begun before it ends");
            journal::end_foreground_turn();
            return;
        };
        if !journal::power_interrupted_since(start) {
            journal::record_action(ActionTiming {
                name,
                start,
                end: Instant::now(),
            });
        }
        journal::end_foreground_turn();
    }

    /// Records the beginning of a window draw.
    pub fn begin_draw(&mut self) {
        journal::begin_foreground_turn();
        let started_at = Instant::now();
        journal::record_frame_pending(self.window_id, started_at);
        self.active_activities
            .push(WindowActivity::Draw { started_at });
    }

    /// Records the end of a window draw and returns the draw duration.
    pub fn end_draw(&mut self, dirty_at: Option<Instant>, invalidations: u64) -> Duration {
        let Some(WindowActivity::Draw {
            started_at: draw_start,
        }) = self.active_activities.pop()
        else {
            debug_assert!(false, "draw activity must be the current window activity");
            journal::end_foreground_turn();
            return Duration::ZERO;
        };

        let draw_end = Instant::now();
        let frame_timing = ProfilerFrameTiming {
            window_id: self.window_id,
            dirty_at: dirty_at.filter(|at| journal::frame_sample_is_valid(self.window_id, *at)),
            invalidations,
            draw_start,
            draw_end,
        };
        let draw_duration = frame_timing.draw_duration();
        if !journal::power_interrupted_since(draw_start) {
            self.record_draw_timing(frame_timing);
        }
        journal::end_foreground_turn();
        draw_duration
    }

    /// Records that a frame was presented.
    ///
    /// `next_frame_scheduled` marks the animation state for the interval ending
    /// at the next newly drawn frame's presentation.
    pub fn record_present(
        &mut self,
        present_start: Instant,
        present_end: Instant,
        window_active: bool,
        next_frame_scheduled: bool,
    ) {
        self.record_present_at(
            present_start,
            present_end,
            window_active,
            next_frame_scheduled,
        );
    }

    /// Returns a snapshot of the current input-latency histograms.
    pub fn input_latency_snapshot(&self) -> InputLatencySnapshot {
        InputLatencySnapshot {
            latency_histogram: self.input_latency_histogram.clone(),
            events_per_frame_histogram: self.events_per_frame_histogram.clone(),
            mid_draw_events_dropped: self.mid_draw_events_dropped,
        }
    }

    /// Returns a snapshot of the current frame-duration histograms.
    pub fn frame_duration_snapshot(&self) -> FrameDurationSnapshot {
        FrameDurationSnapshot {
            dirty_to_present_histogram: self.dirty_to_present_histogram.clone(),
            draw_duration_histogram: self.draw_duration_histogram.clone(),
            present_interval_histogram: self.present_interval_histogram.clone(),
        }
    }

    fn record_present_at(
        &mut self,
        present_start: Instant,
        present_end: Instant,
        window_active: bool,
        next_frame_scheduled: bool,
    ) {
        if let Some(first_input_at) = self.first_input_at.take()
            && journal::frame_sample_is_valid(self.window_id, first_input_at)
        {
            let latency_nanos = present_end.duration_since(first_input_at).as_nanos() as u64;
            self.input_latency_histogram.record(latency_nanos).ok();
            if self.pending_input_count > 0 {
                self.events_per_frame_histogram
                    .record(self.pending_input_count)
                    .ok();
            }
        }
        self.pending_input_count = 0;

        let frame = self
            .pending_frame
            .take()
            .filter(|frame| journal::frame_sample_is_valid(self.window_id, frame.draw_start))
            .map(|mut frame| {
                frame.dirty_at = frame
                    .dirty_at
                    .filter(|at| journal::frame_sample_is_valid(self.window_id, *at));
                frame
            });
        let animation_interval =
            if frame.is_some() && self.animating_at_last_present && window_active {
                self.last_present_at
                    .filter(|at| journal::frame_sample_is_valid(self.window_id, *at))
                    .map(|last_present_at| present_end.duration_since(last_present_at))
            } else {
                None
            };
        let present_timing = ProfilerPresentTiming {
            window_id: self.window_id,
            present_start,
            present_end,
            animation_interval,
        };
        journal::record_present(present_timing, frame);

        let Some(frame) = frame else {
            return;
        };

        if let Some(dirty_at) = frame.dirty_at
            && let Err(error) = self
                .dirty_to_present_histogram
                .record(present_end.duration_since(dirty_at).as_nanos() as u64)
        {
            log::error!("failed to record dirty-to-present frame timing: {error}");
        }

        if let Some(animation_interval) = animation_interval {
            self.present_interval_histogram
                .record(animation_interval.as_nanos() as u64)
                .ok();
        }
        record_frame_event(FrameEvent::Present(present_timing));

        self.last_present_at = Some(present_end);
        self.animating_at_last_present = next_frame_scheduled && window_active;
    }

    fn record_draw_timing(&mut self, timing: ProfilerFrameTiming) {
        self.record_draw_duration(timing.draw_duration());
        self.pending_frame = Some(timing);
        record_frame_event(FrameEvent::Draw(timing));
        journal::record_draw(timing);
    }

    fn record_draw_duration(&mut self, duration: Duration) {
        self.draw_duration_histogram
            .record(duration.as_nanos() as u64)
            .ok();
    }
}

#[cfg(feature = "profiler")]
impl Drop for WindowProfiler {
    fn drop(&mut self) {
        journal::record_window_closed(self.window_id);
    }
}

// Allow 16MiB of frame event entries.
#[cfg(feature = "profiler")]
const MAX_FRAME_EVENTS: usize = (16 * 1024 * 1024) / core::mem::size_of::<FrameEvent>();

#[cfg(feature = "profiler")]
struct FrameEvents {
    timings: VecDeque<FrameEvent>,
    total_pushed: u64,
}

#[cfg(feature = "profiler")]
static FRAME_EVENTS: spin::Mutex<FrameEvents> = spin::Mutex::new(FrameEvents {
    timings: VecDeque::new(),
    total_pushed: 0,
});

/// Records a frame event.
///
/// No-op unless profiler tracing is enabled via [`set_trace_enabled`].
#[cfg(feature = "profiler")]
pub fn record_frame_event(event: FrameEvent) {
    if !trace_enabled() {
        return;
    }
    std::hint::cold_path(); // optimize for when profiling is off

    let mut frames = FRAME_EVENTS.lock();
    if frames.timings.len() >= MAX_FRAME_EVENTS {
        frames.timings.pop_front();
    }
    frames.timings.push_back(event);
    frames.total_pushed += 1;
}

/// Drains frame events recorded after this collector was created, tracking a
/// cursor so each call to [`Self::collect_unseen`] returns only new entries.
#[cfg(feature = "profiler")]
pub struct FrameEventCollector {
    cursor: u64,
}

#[cfg(feature = "profiler")]
impl Default for FrameEventCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "profiler")]
impl FrameEventCollector {
    /// Creates a collector that only sees frame events recorded from this point on.
    pub fn new() -> Self {
        Self {
            cursor: FRAME_EVENTS.lock().total_pushed,
        }
    }

    /// Returns frame events recorded since the previous call (or since the
    /// collector was created). If the ring buffer wrapped around since the
    /// previous poll, the evicted entries are lost.
    pub fn collect_unseen(&mut self) -> Vec<FrameEvent> {
        let frames = FRAME_EVENTS.lock();
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

#[cfg(all(test, feature = "profiler"))]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    #[test]
    fn rho_frame_trace_and_profiler_event_trace_are_independent() {
        let _trace_test_guard = TraceTestGuard::new();
        let _rho_test_guard = timing_ring_tests::exclusive_profiler_state();
        set_frame_trace_enabled(false);
        set_trace_enabled(true);
        let window_id = WindowId::from(0x1501);
        let start = Instant::now();
        let event = FrameEvent::Draw(ProfilerFrameTiming {
            window_id,
            dirty_at: None,
            invalidations: 2,
            draw_start: start,
            draw_end: start + Duration::from_millis(9),
        });
        let frame = FrameTiming {
            window_id,
            work: FrameWorkScale::default(),
            dirty_at: None,
            invalidations: 4,
            draw_start: start,
            prepaint_end: start + Duration::from_millis(3),
            paint_end: start + Duration::from_millis(7),
            draw_end: start + Duration::from_millis(9),
        };
        let mut events = FrameEventCollector::new();
        let mut frames = FrameTimingCollector::new();
        record_frame_event(event);
        record_frame_timing(frame);
        assert!(frames.collect_unseen().is_empty());
        set_frame_trace_enabled(true);
        set_frame_trace_enabled(false);
        assert!(
            events
                .collect_unseen()
                .iter()
                .any(|event| event_matches_window(*event, window_id))
        );

        set_frame_trace_enabled(true);
        record_frame_timing(frame);
        set_trace_enabled(false);
        record_frame_event(event);
        assert!(events.collect_unseen().is_empty());
        let collected = frames.collect_unseen();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].invalidations, 4);
        set_frame_trace_enabled(false);
    }

    #[test]
    fn interruptions_drop_latency_samples_but_hiding_keeps_draw_work() {
        for sleep in [false, true] {
            let (_journal, _guard) = journal::install_test_foreground_journal(64, 4);
            let id = WindowId::from(1);
            let mut profiler = WindowProfiler::new(id).expect("valid histograms");
            let old = Instant::now() - Duration::from_secs(1);
            record_test_draw(&mut profiler, old);
            profiler.record_present_at(old, old, true, true);
            profiler.first_input_at = Some(old);
            profiler.pending_input_count = 1;
            profiler.begin_draw();
            if sleep {
                journal::record_power_transition(journal::PowerState::Suspended);
                journal::record_power_transition(journal::PowerState::Awake);
            } else {
                journal::record_window_visibility(id, crate::WindowVisibility::Hidden);
                journal::record_window_visibility(id, crate::WindowVisibility::Visible);
            }
            profiler.end_draw(Some(old), 1);
            let now = Instant::now();
            profiler.record_present_at(now, now, true, true);
            assert_eq!(
                profiler.draw_duration_histogram.len(),
                if sleep { 1 } else { 2 }
            );
            assert_eq!(profiler.input_latency_histogram.len(), 0);
            assert_eq!(profiler.dirty_to_present_histogram.len(), 1);
            assert_eq!(profiler.present_interval_histogram.len(), 0);
            profiler.begin_draw();
            profiler.end_draw(Some(Instant::now()), 1);
            let now = Instant::now();
            profiler.record_present_at(now, now, true, true);
            assert_eq!(profiler.dirty_to_present_histogram.len(), 2);
        }
    }

    #[test]
    fn records_draw_events_only_while_tracing() {
        let _trace_test_guard = TraceTestGuard::new();
        let window_id = WindowId::from(0xD0A0);
        let mut window_profiler =
            WindowProfiler::new(window_id).expect("window profiler should initialize");
        let dirty_at = Instant::now();
        let mut collector = FrameEventCollector::new();

        window_profiler.begin_draw();
        window_profiler.end_draw(Some(dirty_at), 3);
        assert!(
            collector
                .collect_unseen()
                .iter()
                .all(|event| !event_matches_window(*event, window_id))
        );

        set_trace_enabled(true);
        let mut collector = FrameEventCollector::new();
        window_profiler.begin_draw();
        window_profiler.end_draw(Some(dirty_at), 3);

        let timing = collector
            .collect_unseen()
            .into_iter()
            .find_map(|event| match event {
                FrameEvent::Draw(timing) if timing.window_id == window_id => Some(timing),
                _ => None,
            })
            .expect("draw event should be recorded while tracing");
        assert_eq!(timing.dirty_at, Some(dirty_at));
        assert_eq!(timing.invalidations, 3);
        assert!(timing.draw_start >= dirty_at);
    }

    #[test]
    fn records_present_events_for_newly_drawn_frames() {
        let _trace_test_guard = TraceTestGuard::new();
        set_trace_enabled(true);
        let window_id = WindowId::from(0xA11E);
        let mut window_profiler =
            WindowProfiler::new(window_id).expect("window profiler should initialize");
        let start = Instant::now();
        let mut collector = FrameEventCollector::new();

        record_test_draw(&mut window_profiler, start);
        window_profiler.record_present_at(start, start, true, true);
        record_test_draw(&mut window_profiler, start + FRAME);
        window_profiler.record_present_at(start + FRAME, start + FRAME, true, true);
        window_profiler.record_present_at(
            start + FRAME + FRAME / 2,
            start + FRAME + FRAME / 2,
            true,
            true,
        );

        let present_timings = collector
            .collect_unseen()
            .into_iter()
            .filter_map(|event| match event {
                FrameEvent::Present(timing) if timing.window_id == window_id => Some(timing),
                _ => None,
            })
            .collect::<Vec<_>>();
        let [first_present, second_present] = present_timings.as_slice() else {
            panic!("expected exactly two present events, got {present_timings:?}");
        };
        assert_eq!(first_present.animation_interval, None);
        assert_eq!(second_present.animation_interval, Some(FRAME));

        #[cfg(feature = "profiler")]
        {
            assert_eq!(window_profiler.present_interval_histogram.len(), 1);
            assert!(
                window_profiler.present_interval_histogram.max()
                    >= second_present
                        .animation_interval
                        .expect("second present should have an animation interval")
                        .as_nanos() as u64
            );
        }
    }

    #[test]
    fn disabling_tracing_clears_frame_events() {
        let _trace_test_guard = TraceTestGuard::new();
        set_trace_enabled(true);
        let window_id = WindowId::from(0xC1EA);
        let mut window_profiler =
            WindowProfiler::new(window_id).expect("window profiler should initialize");
        let mut collector = FrameEventCollector::new();

        window_profiler.begin_draw();
        window_profiler.end_draw(None, 0);
        assert!(
            FRAME_EVENTS
                .lock()
                .timings
                .iter()
                .copied()
                .any(|event| event_matches_window(event, window_id))
        );

        set_trace_enabled(false);
        assert!(
            collector
                .collect_unseen()
                .iter()
                .all(|event| !event_matches_window(*event, window_id))
        );
    }

    #[cfg(feature = "profiler")]
    #[test]
    fn records_intervals_only_between_animation_frames() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(1)).expect("window profiler should initialize");
        let start = Instant::now();

        draw_and_present(&mut window_profiler, start, true, true);
        assert_eq!(window_profiler.present_interval_histogram.len(), 0);

        draw_and_present(&mut window_profiler, start + FRAME, true, true);
        assert_eq!(window_profiler.present_interval_histogram.len(), 1);

        draw_and_present(&mut window_profiler, start + FRAME * 2, true, false);
        assert_eq!(window_profiler.present_interval_histogram.len(), 2);

        draw_and_present(&mut window_profiler, start + FRAME * 100, true, true);
        assert_eq!(window_profiler.present_interval_histogram.len(), 2);
    }

    #[cfg(feature = "profiler")]
    #[test]
    fn missed_frames_stretch_the_recorded_interval() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(2)).expect("window profiler should initialize");
        let start = Instant::now();

        draw_and_present(&mut window_profiler, start, true, true);
        draw_and_present(&mut window_profiler, start + FRAME * 5, true, true);

        let recorded = window_profiler.present_interval_histogram.max();
        assert!(recorded >= (FRAME * 4).as_nanos() as u64);
    }

    #[cfg(feature = "profiler")]
    #[test]
    fn ignores_re_presents_of_unchanged_frames() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(3)).expect("window profiler should initialize");
        let start = Instant::now();

        draw_and_present(&mut window_profiler, start, true, true);
        window_profiler.record_present_at(start + FRAME / 2, start + FRAME / 2, true, true);
        draw_and_present(&mut window_profiler, start + FRAME, true, true);

        assert_eq!(window_profiler.present_interval_histogram.len(), 1);
        assert!(
            window_profiler.present_interval_histogram.max() >= (FRAME * 3 / 4).as_nanos() as u64
        );
    }

    #[cfg(feature = "profiler")]
    #[test]
    fn skips_intervals_for_inactive_windows() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(4)).expect("window profiler should initialize");
        let start = Instant::now();

        draw_and_present(&mut window_profiler, start, false, true);
        draw_and_present(&mut window_profiler, start + FRAME, false, true);
        assert_eq!(window_profiler.present_interval_histogram.len(), 0);

        draw_and_present(&mut window_profiler, start + FRAME * 2, true, true);
        assert_eq!(window_profiler.present_interval_histogram.len(), 0);

        draw_and_present(&mut window_profiler, start + FRAME * 3, true, true);
        assert_eq!(window_profiler.present_interval_histogram.len(), 1);
    }

    #[test]
    fn records_dirty_to_present_durations() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(8)).expect("window profiler should initialize");
        let draw_end = Instant::now();
        let present_end = draw_end + Duration::from_millis(6);

        record_test_draw(&mut window_profiler, draw_end);
        window_profiler.record_present_at(present_end, present_end, true, false);

        let snapshot = window_profiler.frame_duration_snapshot();
        let histogram = snapshot.dirty_to_present_histogram;
        assert_eq!(histogram.len(), 1);
        assert!(histogram.max() >= Duration::from_millis(10).as_nanos() as u64);
    }

    #[cfg(feature = "profiler")]
    #[test]
    fn records_every_draw_duration() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(5)).expect("window profiler should initialize");

        window_profiler.record_draw_duration(Duration::from_millis(2));
        window_profiler.record_draw_duration(Duration::from_millis(40));

        let snapshot = window_profiler.frame_duration_snapshot();
        assert_eq!(snapshot.draw_duration_histogram.len(), 2);
        assert!(snapshot.draw_duration_histogram.max() >= 39_000_000);
    }

    #[test]
    fn records_input_latency_at_the_frame_presentation_timestamp() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(6)).expect("window profiler should initialize");
        let first_input_at = Instant::now();
        let presented_at = first_input_at + Duration::from_millis(12);

        begin_input_at(&mut window_profiler, first_input_at);
        window_profiler.end_input(true);
        begin_input_at(
            &mut window_profiler,
            first_input_at + Duration::from_millis(2),
        );
        window_profiler.end_input(true);
        record_test_draw(&mut window_profiler, presented_at);
        window_profiler.record_present_at(presented_at, presented_at, true, false);

        let snapshot = window_profiler.input_latency_snapshot();
        assert_eq!(snapshot.latency_histogram.len(), 1);
        assert!(snapshot.latency_histogram.max() >= Duration::from_millis(12).as_nanos() as u64);
        assert_eq!(snapshot.events_per_frame_histogram.len(), 1);
        assert_eq!(snapshot.events_per_frame_histogram.max(), 2);
        assert_eq!(snapshot.mid_draw_events_dropped, 0);
    }

    #[test]
    fn excludes_input_that_arrives_during_a_draw() {
        let mut window_profiler =
            WindowProfiler::new(WindowId::from(7)).expect("window profiler should initialize");

        window_profiler.begin_draw();
        begin_input_at(&mut window_profiler, Instant::now());
        window_profiler.end_input(true);
        window_profiler.end_draw(None, 0);

        let snapshot = window_profiler.input_latency_snapshot();
        assert!(snapshot.latency_histogram.is_empty());
        assert!(snapshot.events_per_frame_histogram.is_empty());
        assert_eq!(snapshot.mid_draw_events_dropped, 1);
    }

    #[test]
    fn overlapping_trace_scopes_keep_tracing_enabled() {
        let _trace_test_guard = TraceTestGuard::new();
        let first_scope = trace_scope();
        let second_scope = trace_scope();

        assert!(trace_enabled());
        drop(first_scope);
        assert!(trace_enabled());
        drop(second_scope);
        assert!(!trace_enabled());
    }

    const FRAME: Duration = Duration::from_millis(16);
    static TRACE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TraceTestGuard {
        was_enabled: bool,
        _lock: MutexGuard<'static, ()>,
    }

    impl TraceTestGuard {
        fn new() -> Self {
            let lock = TRACE_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let was_enabled = trace_enabled();
            set_trace_enabled(false);
            Self {
                was_enabled,
                _lock: lock,
            }
        }
    }

    impl Drop for TraceTestGuard {
        fn drop(&mut self) {
            set_trace_enabled(false);
            if self.was_enabled {
                set_trace_enabled(true);
            }
        }
    }

    fn event_matches_window(event: FrameEvent, window_id: WindowId) -> bool {
        match event {
            FrameEvent::Draw(timing) => timing.window_id == window_id,
            FrameEvent::Present(timing) => timing.window_id == window_id,
        }
    }

    fn begin_input_at(window_profiler: &mut WindowProfiler, started_at: Instant) {
        window_profiler
            .active_activities
            .push(WindowActivity::Input {
                started_at,
                kind: "test",
            });
    }

    #[cfg(feature = "profiler")]
    fn draw_and_present(
        window_profiler: &mut WindowProfiler,
        presented_at: Instant,
        window_active: bool,
        next_frame_scheduled: bool,
    ) {
        record_test_draw(window_profiler, presented_at);
        window_profiler.record_present_at(
            presented_at,
            presented_at,
            window_active,
            next_frame_scheduled,
        );
    }

    fn record_test_draw(window_profiler: &mut WindowProfiler, draw_end: Instant) {
        window_profiler.record_draw_timing(ProfilerFrameTiming {
            window_id: window_profiler.window_id,
            dirty_at: Some(draw_end - Duration::from_millis(4)),
            invalidations: 1,
            draw_start: draw_end - Duration::from_millis(2),
            draw_end,
        });
    }
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
    /// Producing the highlighted chunks a screen's rows are shaped from:
    /// the syntax, diagnostic and inlay iterators walking the drawn range.
    /// Counted apart from the shaping because the two are interleaved in
    /// one prepaint pass and want opposite fixes.
    HighlightedChunks = 12,
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

impl EditorTimingGuard {
    /// Records the span with a duration the caller measured rather than the
    /// guard's own lifetime.
    ///
    /// For a stage whose work is interleaved with other work inside one
    /// call - chunk building between shaping, say - where the guard's
    /// lifetime would charge it for both.
    pub fn finish_with_elapsed(mut self, elapsed: Duration) {
        if let Some((mut timing, generation)) = self.0.take() {
            timing.end = timing.start + elapsed;
            record_editor_timing(timing, generation);
        }
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
    pub(super) fn exclusive_profiler_state() -> std::sync::MutexGuard<'static, ()> {
        static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn frame_ring_preserves_phases_bounds_and_cursor_across_toggle() {
        let _state = exclusive_profiler_state();
        set_frame_trace_enabled(false);
        let mut collector = FrameTimingCollector::new();
        let start = Instant::now();
        let timing = FrameTiming {
            window_id: WindowId::from(0xB0AD),
            work: FrameWorkScale::default(),
            dirty_at: Some(start - Duration::from_millis(5)),
            invalidations: 3,
            draw_start: start,
            prepaint_end: start + Duration::from_millis(7),
            paint_end: start + Duration::from_millis(18),
            draw_end: start + Duration::from_millis(20),
        };
        record_frame_timing(timing);
        record_present_timing(PresentTiming {
            window_id: timing.window_id,
            start: timing.draw_end,
            end: timing.draw_end + Duration::from_millis(13),
        });
        assert!(collector.collect_unseen().is_empty());
        assert!(snapshot_present_timings().is_empty());

        set_frame_trace_enabled(true);
        for invalidations in 0..MAX_FRAME_TIMINGS + 17 {
            record_frame_work(FrameWorkScale {
                visible_rows: 23,
                total_rows: 9_001,
                ..Default::default()
            });
            record_frame_timing(FrameTiming {
                invalidations: invalidations as u64,
                ..timing
            });
        }
        let frames = collector.collect_unseen();
        assert_eq!(frames.len(), MAX_FRAME_TIMINGS);
        assert_eq!(frames.first().expect("bounded ring").invalidations, 17);
        let last = frames.last().expect("bounded ring");
        assert_eq!(last.work.visible_rows, 23);
        assert_eq!(last.work.total_rows, 9_001);
        assert_eq!(last.prepaint_duration(), Duration::from_millis(7));
        assert_eq!(last.paint_duration(), Duration::from_millis(11));
        assert_eq!(last.finish_duration(), Duration::from_millis(2));
        assert_eq!(
            last.dirty_to_draw_duration(),
            Some(Duration::from_millis(25))
        );
        assert_eq!(snapshot_frame_timings().len(), MAX_FRAME_TIMINGS);
        assert!(collector.collect_unseen().is_empty());

        record_present_timing(PresentTiming {
            window_id: timing.window_id,
            start: timing.draw_end,
            end: timing.draw_end + Duration::from_millis(13),
        });
        assert_eq!(snapshot_present_timings().len(), 1);
        set_frame_trace_enabled(false);
        assert!(snapshot_frame_timings().is_empty());
        assert!(snapshot_present_timings().is_empty());
        set_frame_trace_enabled(true);
        record_frame_timing(timing);
        assert_eq!(collector.collect_unseen().len(), 1);
        set_frame_trace_enabled(false);
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
