//! A sampling profiler for the benchmarks, in-process and test-only.
//!
//! A SIGPROF timer interrupts whatever is running and records the stack;
//! the frames are symbolised afterwards, out of the handler. Attribution
//! this way is coarse but needs no external profiler, which the sandbox
//! has none of.

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

const MAX_SAMPLES: usize = 400_000;
const MAX_DEPTH: usize = 64;

static BUFFER: AtomicPtr<usize> = AtomicPtr::new(std::ptr::null_mut());
static CURSOR: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn on_sigprof(_signal: libc::c_int) {
    let buffer = BUFFER.load(Ordering::Relaxed);
    if buffer.is_null() {
        return;
    }
    let index = CURSOR.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_SAMPLES {
        return;
    }
    let slot = unsafe { buffer.add(index * MAX_DEPTH) };
    let mut depth = 0;
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if depth >= MAX_DEPTH {
                return false;
            }
            slot.add(depth).write(frame.ip() as usize);
            depth += 1;
            true
        });
    }
}

pub fn start(hz: i64) {
    let buffer = vec![0usize; MAX_SAMPLES * MAX_DEPTH].into_boxed_slice();
    BUFFER.store(Box::leak(buffer).as_mut_ptr(), Ordering::SeqCst);
    CURSOR.store(0, Ordering::SeqCst);
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigprof as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGPROF, &action, std::ptr::null_mut());
    }
    set_interval(1_000_000 / hz);
}

pub fn stop() -> Vec<Vec<usize>> {
    set_interval(0);
    let buffer = BUFFER.swap(std::ptr::null_mut(), Ordering::SeqCst);
    let taken = CURSOR.load(Ordering::SeqCst).min(MAX_SAMPLES);
    (0..taken)
        .map(|index| {
            let slot =
                unsafe { std::slice::from_raw_parts(buffer.add(index * MAX_DEPTH), MAX_DEPTH) };
            slot.iter().copied().take_while(|ip| *ip != 0).collect()
        })
        .collect()
}

fn set_interval(micros: i64) {
    let interval = libc::timeval {
        tv_sec: 0,
        tv_usec: micros,
    };
    let timer = libc::itimerval {
        it_interval: interval,
        it_value: interval,
    };
    unsafe {
        libc::setitimer(libc::ITIMER_PROF, &timer, std::ptr::null_mut());
    }
}

/// Prints the frames that appear in the most samples (inclusive time), and
/// the frames samples land in directly (self time).
pub fn report(samples: &[Vec<usize>], label: &str) {
    use std::collections::HashMap;

    let mut inclusive: HashMap<String, usize> = HashMap::new();
    let mut self_time: HashMap<String, usize> = HashMap::new();
    for sample in samples {
        let mut seen = std::collections::HashSet::new();
        let frames: Vec<String> = sample
            .iter()
            .map(|ip| symbol(*ip))
            .skip_while(|name| {
                name.contains("sampler::") || name.contains("restore_rt") || name.starts_with("0x")
            })
            .collect();
        for (depth, name) in frames.into_iter().enumerate() {
            if depth == 0 {
                *self_time.entry(name.clone()).or_default() += 1;
            }
            if seen.insert(name.clone()) {
                *inclusive.entry(name).or_default() += 1;
            }
        }
    }
    let total = samples.len().max(1);
    println!("\n== {label}: {} samples ==", samples.len());
    for (title, table) in [("inclusive", inclusive), ("self", self_time)] {
        let mut rows: Vec<_> = table.into_iter().collect();
        rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        println!("-- {title} --");
        for (name, count) in rows.into_iter().take(35) {
            println!(
                "{:5.1}%  {count:6}  {name}",
                count as f64 * 100.0 / total as f64
            );
        }
    }
}

fn symbol(ip: usize) -> String {
    let mut name = None;
    backtrace::resolve(ip as *mut _, |symbol| {
        if name.is_none() {
            name = symbol.name().map(|name| format!("{name:#}"));
        }
    });
    name.unwrap_or_else(|| format!("{ip:#x}"))
}
