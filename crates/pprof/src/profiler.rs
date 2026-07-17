// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::convert::TryInto;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use nix::sys::signal;
use once_cell::sync::Lazy;
use smallvec::SmallVec;
use spin::RwLock;

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
use findshlibs::{Segment, SharedLibrary, TargetSharedLibrary};

use crate::backtrace::{Trace, TraceImpl};
use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::frames::UnresolvedFrames;
use crate::report::ReportBuilder;
use crate::temporal::TemporalArchive;
use crate::timer::Timer;
use crate::{MAX_DEPTH, MAX_THREAD_NAME};

const TEMPORAL_RING_CAPACITY: usize = 4096;
static TEMPORAL_SAMPLES_MISSED_LOCK: AtomicU64 = AtomicU64::new(0);

struct TemporalRawSample {
    monotonic_ns: u64,
    tid: u64,
    thread_name: [u8; MAX_THREAD_NAME],
    thread_name_length: usize,
    instruction_pointers: SmallVec<[usize; MAX_DEPTH]>,
    truncated: bool,
}

struct TemporalRing {
    samples: Vec<TemporalRawSample>,
    dropped: u64,
}

impl TemporalRing {
    fn new() -> Self {
        Self {
            samples: Vec::with_capacity(TEMPORAL_RING_CAPACITY),
            dropped: 0,
        }
    }

    fn push(&mut self, sample: TemporalRawSample) {
        if self.samples.len() == TEMPORAL_RING_CAPACITY {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.samples.push(sample);
        }
    }

    fn take(&mut self, replacement: Vec<TemporalRawSample>) -> (Vec<TemporalRawSample>, u64) {
        let samples = std::mem::replace(&mut self.samples, replacement);
        let dropped = std::mem::take(&mut self.dropped)
            .saturating_add(TEMPORAL_SAMPLES_MISSED_LOCK.swap(0, Ordering::Relaxed));
        (samples, dropped)
    }
}

pub(crate) static PROFILER: Lazy<RwLock<Result<Profiler>>> =
    Lazy::new(|| RwLock::new(Profiler::new()));

pub struct Profiler {
    pub(crate) data: Collector<UnresolvedFrames>,
    sample_counter: i32,

    running: bool,
    claimed: bool,
    temporal: TemporalRing,

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    blocklist_segments: Vec<(usize, usize)>,
}

#[derive(Clone)]
pub struct ProfilerGuardBuilder {
    frequency: c_int,
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    blocklist_segments: Vec<(usize, usize)>,
}

impl Default for ProfilerGuardBuilder {
    fn default() -> ProfilerGuardBuilder {
        ProfilerGuardBuilder {
            frequency: 99,

            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            ))]
            blocklist_segments: Vec::new(),
        }
    }
}

impl ProfilerGuardBuilder {
    pub fn frequency(self, frequency: c_int) -> Self {
        Self { frequency, ..self }
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    pub fn blocklist<T: AsRef<str>>(self, blocklist: &[T]) -> Self {
        let blocklist_segments = {
            let mut segments = Vec::new();
            TargetSharedLibrary::each(|shlib| {
                let in_blocklist = match shlib.name().to_str() {
                    Some(name) => {
                        let mut in_blocklist = false;
                        for blocked_name in blocklist.iter() {
                            if name.contains(blocked_name.as_ref()) {
                                in_blocklist = true;
                            }
                        }

                        in_blocklist
                    }

                    None => false,
                };
                if in_blocklist {
                    for seg in shlib.segments() {
                        let avam = seg.actual_virtual_memory_address(shlib);
                        let start = avam.0;
                        let end = start + seg.len();
                        segments.push((start, end));
                    }
                }
            });
            segments
        };

        Self {
            blocklist_segments,
            ..self
        }
    }
    pub fn build(self) -> Result<ProfilerGuard<'static>> {
        trigger_lazy();

        match PROFILER.write().as_mut() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                #[cfg(any(
                    target_arch = "x86_64",
                    target_arch = "aarch64",
                    target_arch = "riscv64",
                    target_arch = "loongarch64"
                ))]
                {
                    profiler.blocklist_segments = self.blocklist_segments;
                }

                match profiler.start() {
                    Ok(()) => {
                        let temporal_archive = Arc::new(Mutex::new(TemporalArchive::default()));
                        let temporal_stop = Arc::new(AtomicBool::new(false));
                        let archive = temporal_archive.clone();
                        let stop = temporal_stop.clone();
                        let temporal_drainer = std::thread::Builder::new()
                            .name("pprof-temporal".to_owned())
                            .spawn(move || {
                                while !stop.load(Ordering::Relaxed) {
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                    drain_temporal(&archive);
                                }
                                drain_temporal(&archive);
                            });
                        let temporal_drainer = match temporal_drainer {
                            Ok(handle) => handle,
                            Err(_) => {
                                let _ = profiler.pause();
                                let _ = profiler.init();
                                return Err(Error::CreatingError);
                            }
                        };
                        Ok(ProfilerGuard::<'static> {
                            profiler: &PROFILER,
                            timer: Some(Timer::new(self.frequency)),
                            stopped_timing: None,
                            temporal_archive,
                            temporal_stop,
                            temporal_drainer: Some(temporal_drainer),
                        })
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }
}

/// RAII structure used to stop profiling when dropped. It is the only interface to access profiler.
pub struct ProfilerGuard<'a> {
    profiler: &'a RwLock<Result<Profiler>>,
    timer: Option<Timer>,
    stopped_timing: Option<crate::timer::ReportTiming>,
    temporal_archive: Arc<Mutex<TemporalArchive>>,
    temporal_stop: Arc<AtomicBool>,
    temporal_drainer: Option<JoinHandle<()>>,
}

fn trigger_lazy() {
    let _ = backtrace::Backtrace::new();
    let _profiler = PROFILER.read();
}

impl ProfilerGuard<'_> {
    /// Start profiling with given sample frequency.
    pub fn new(frequency: c_int) -> Result<ProfilerGuard<'static>> {
        ProfilerGuardBuilder::default().frequency(frequency).build()
    }

    /// Generate a report
    pub fn report(&self) -> ReportBuilder<'_> {
        ReportBuilder::new(
            self.profiler,
            self.timer
                .as_ref()
                .map(Timer::timing)
                .or_else(|| self.stopped_timing.clone())
                .unwrap_or_default(),
            self.temporal_archive.clone(),
        )
    }

    /// Stop sampling and stabilize both aggregate and temporal reports.
    pub fn stop(&mut self) -> Result<()> {
        self.stopped_timing = self.timer.as_ref().map(Timer::timing);
        drop(self.timer.take());
        let result = match self.profiler.write().as_mut() {
            Ok(profiler) => profiler.pause(),
            Err(_) => Err(Error::CreatingError),
        };
        self.stop_temporal_drainer();
        result
    }

    fn stop_temporal_drainer(&mut self) {
        self.temporal_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.temporal_drainer.take() {
            let _ = handle.join();
        }
    }
}

impl<'a> Drop for ProfilerGuard<'a> {
    fn drop(&mut self) {
        if self.timer.is_some() {
            let _ = self.stop();
        } else {
            self.stop_temporal_drainer();
        }

        match self.profiler.write().as_mut() {
            Err(_) => {}
            Ok(profiler) => match profiler.init() {
                Ok(()) => {}
                Err(err) => log::error!("error while stopping profiler {}", err),
            },
        }
    }
}

pub(crate) fn drain_temporal(archive: &Arc<Mutex<TemporalArchive>>) {
    // Allocate the replacement outside the profiler lock. The signal handler
    // can then swap into it without allocating or performing I/O.
    let replacement = Vec::with_capacity(TEMPORAL_RING_CAPACITY);
    let (samples, dropped) = {
        let Some(mut profiler) = PROFILER.try_write() else {
            return;
        };
        let Ok(profiler) = profiler.as_mut() else {
            return;
        };
        profiler.temporal.take(replacement)
    };

    let mut archive = archive.lock().unwrap_or_else(|error| error.into_inner());
    archive.add_dropped(dropped);
    for sample in samples {
        archive.push(
            sample.monotonic_ns,
            sample.tid,
            &sample.thread_name[..sample.thread_name_length],
            sample.instruction_pointers.into_vec(),
            sample.truncated,
        );
    }
}

fn write_thread_name_fallback(current_thread: libc::pthread_t, name: &mut [libc::c_char]) {
    let mut len = 0;
    let mut base = 1;

    while current_thread as u128 > base && len < MAX_THREAD_NAME {
        base *= 10;
        len += 1;
    }

    let mut index = 0;
    while index < len && base > 1 {
        base /= 10;

        name[index] = match (48 + (current_thread as u128 / base) % 10).try_into() {
            Ok(digit) => digit,
            Err(_) => {
                log::error!("fail to convert thread_id to string");
                0
            }
        };

        index += 1;
    }
}

#[cfg(not(all(any(target_os = "linux", target_os = "macos"), target_env = "gnu")))]
fn write_thread_name(current_thread: libc::pthread_t, name: &mut [libc::c_char]) {
    write_thread_name_fallback(current_thread, name);
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), target_env = "gnu"))]
fn write_thread_name(current_thread: libc::pthread_t, name: &mut [libc::c_char]) {
    let name_ptr = name as *mut [libc::c_char] as *mut libc::c_char;
    let ret = unsafe { libc::pthread_getname_np(current_thread, name_ptr, MAX_THREAD_NAME) };

    if ret != 0 {
        write_thread_name_fallback(current_thread, name);
    }
}

struct ErrnoProtector(libc::c_int);

impl ErrnoProtector {
    fn new() -> Self {
        unsafe {
            #[cfg(target_os = "android")]
            {
                let errno = *libc::__errno();
                Self(errno)
            }
            #[cfg(target_os = "linux")]
            {
                let errno = *libc::__errno_location();
                Self(errno)
            }
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                let errno = *libc::__error();
                Self(errno)
            }
        }
    }
}

impl Drop for ErrnoProtector {
    fn drop(&mut self) {
        unsafe {
            #[cfg(target_os = "android")]
            {
                *libc::__errno() = self.0;
            }
            #[cfg(target_os = "linux")]
            {
                *libc::__errno_location() = self.0;
            }
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                *libc::__error() = self.0;
            }
        }
    }
}

#[no_mangle]
#[cfg_attr(
    not(all(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))),
    allow(unused_variables)
)]
#[allow(clippy::unnecessary_cast)]
extern "C" fn perf_signal_handler(
    _signal: c_int,
    _siginfo: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    let _errno = ErrnoProtector::new();

    if let Some(mut guard) = PROFILER.try_write() {
        if let Ok(profiler) = guard.as_mut() {
            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            ))]
            if !ucontext.is_null() {
                let ucontext: *mut libc::ucontext_t = ucontext as *mut libc::ucontext_t;

                #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
                let addr =
                    unsafe { (*ucontext).uc_mcontext.gregs[libc::REG_RIP as usize] as usize };

                #[cfg(all(target_arch = "x86_64", target_os = "freebsd"))]
                let addr = unsafe { (*ucontext).uc_mcontext.mc_rip as usize };

                #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
                let addr = unsafe {
                    let mcontext = (*ucontext).uc_mcontext;
                    if mcontext.is_null() {
                        0
                    } else {
                        (*mcontext).__ss.__rip as usize
                    }
                };

                #[cfg(all(
                    target_arch = "aarch64",
                    any(target_os = "android", target_os = "linux")
                ))]
                let addr = unsafe { (*ucontext).uc_mcontext.pc as usize };

                #[cfg(all(target_arch = "aarch64", target_os = "freebsd"))]
                let addr = unsafe { (*ucontext).mc_gpregs.gp_elr as usize };

                #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
                let addr = unsafe {
                    let mcontext = (*ucontext).uc_mcontext;
                    if mcontext.is_null() {
                        0
                    } else {
                        (*mcontext).__ss.__pc as usize
                    }
                };

                #[cfg(all(target_arch = "riscv64", target_os = "linux"))]
                let addr = unsafe { (*ucontext).uc_mcontext.__gregs[libc::REG_PC] as usize };

                #[cfg(all(target_arch = "loongarch64", target_os = "linux"))]
                let addr = unsafe { (*ucontext).uc_mcontext.__pc as usize };

                if profiler.is_blocklisted(addr) {
                    return;
                }
            }

            let mut bt: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]> =
                SmallVec::with_capacity(MAX_DEPTH);
            let mut index = 0;

            let sample_timestamp: SystemTime = SystemTime::now();
            let monotonic_ns = crate::timer::monotonic_ns();
            TraceImpl::trace(ucontext, |frame| {
                #[cfg(feature = "frame-pointer")]
                {
                    let ip = crate::backtrace::Frame::ip(frame);
                    if profiler.is_blocklisted(ip) {
                        return false;
                    }
                }

                if index < MAX_DEPTH {
                    bt.push(frame.clone());
                    index += 1;
                    true
                } else {
                    false
                }
            });

            let current_thread = unsafe { libc::pthread_self() };
            let mut name = [0; MAX_THREAD_NAME];
            let name_ptr = &mut name as *mut [libc::c_char] as *mut libc::c_char;

            write_thread_name(current_thread, &mut name);

            let name = unsafe { std::ffi::CStr::from_ptr(name_ptr) };
            let mut thread_name = [0_u8; MAX_THREAD_NAME];
            let thread_name_length = name.to_bytes().len().min(MAX_THREAD_NAME);
            thread_name[..thread_name_length]
                .copy_from_slice(&name.to_bytes()[..thread_name_length]);
            let instruction_pointers = bt
                .iter()
                .map(crate::backtrace::Frame::ip)
                .collect::<SmallVec<[usize; MAX_DEPTH]>>();
            profiler.temporal.push(TemporalRawSample {
                monotonic_ns,
                tid: current_os_tid(current_thread),
                thread_name,
                thread_name_length,
                instruction_pointers,
                truncated: index == MAX_DEPTH,
            });
            profiler.sample(bt, name.to_bytes(), current_thread as u64, sample_timestamp);
        }
    } else {
        TEMPORAL_SAMPLES_MISSED_LOCK.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_os_tid(_pthread: libc::pthread_t) -> u64 {
    unsafe { libc::syscall(libc::SYS_gettid) as u64 }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn current_os_tid(pthread: libc::pthread_t) -> u64 {
    pthread as u64
}

impl Profiler {
    fn new() -> Result<Self> {
        Ok(Profiler {
            data: Collector::new()?,
            sample_counter: 0,
            running: false,
            claimed: false,
            temporal: TemporalRing::new(),

            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            ))]
            blocklist_segments: Vec::new(),
        })
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    fn is_blocklisted(&self, addr: usize) -> bool {
        for libs in &self.blocklist_segments {
            if addr > libs.0 && addr < libs.1 {
                return true;
            }
        }
        false
    }
}

impl Profiler {
    pub fn start(&mut self) -> Result<()> {
        log::info!("starting cpu profiler");
        if self.claimed {
            Err(Error::Running)
        } else {
            self.register_signal_handler()?;
            self.running = true;
            self.claimed = true;

            Ok(())
        }
    }

    fn init(&mut self) -> Result<()> {
        self.sample_counter = 0;
        self.data = Collector::new()?;
        self.temporal = TemporalRing::new();
        TEMPORAL_SAMPLES_MISSED_LOCK.store(0, Ordering::Relaxed);
        self.running = false;
        self.claimed = false;

        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        log::info!("stopping cpu profiler");
        if self.running {
            self.unregister_signal_handler()?;
            self.running = false;

            Ok(())
        } else {
            Err(Error::NotRunning)
        }
    }

    fn register_signal_handler(&self) -> Result<()> {
        let handler = signal::SigHandler::SigAction(perf_signal_handler);
        let sigaction = signal::SigAction::new(
            handler,
            // SA_RESTART will only restart a syscall when it's safe to do so,
            // e.g. when it's a blocking read(2) or write(2). See man 7 signal.
            signal::SaFlags::SA_SIGINFO | signal::SaFlags::SA_RESTART,
            signal::SigSet::empty(),
        );
        unsafe { signal::sigaction(signal::SIGPROF, &sigaction) }?;

        Ok(())
    }

    fn unregister_signal_handler(&self) -> Result<()> {
        let handler = signal::SigHandler::SigIgn;
        unsafe { signal::signal(signal::SIGPROF, handler) }?;

        Ok(())
    }

    // This function has to be AS-safe
    pub fn sample(
        &mut self,
        backtrace: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]>,
        thread_name: &[u8],
        thread_id: u64,
        sample_timestamp: SystemTime,
    ) {
        let frames = UnresolvedFrames::new(backtrace, thread_name, thread_id, sample_timestamp);
        self.sample_counter += 1;

        if let Ok(()) = self.data.add(frames, 1) {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stopped_guard_keeps_exclusive_ownership_until_drop() {
        let mut first = super::ProfilerGuard::new(100).unwrap();
        first.stop().unwrap();
        assert!(matches!(
            super::ProfilerGuard::new(100),
            Err(crate::Error::Running)
        ));
        drop(first);

        let second = super::ProfilerGuard::new(100).unwrap();
        drop(second);
    }
}
