//! Boehm GC initialization, thread registration, and telemetry.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::sync::Once;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

// Ensure Nix/GC is initialized exactly once across all threads.
static NIX_INIT: Once = Once::new();

// Keep a registration on the thread that created it. Boehm registrations are
// thread-affine: unregistering must happen on that same thread.
thread_local! {
    static GC_REGISTRATION: RefCell<Option<nix_bindings_expr::eval_state::ThreadRegistrationGuard>> = const { RefCell::new(None) };
}

/// Stack size for threads that run Nix evaluation.
///
/// Nix evaluation can be deeply recursive (e.g. large nixpkgs traversals),
/// and the default 8MB thread stack is not always enough. Match the 64MB
/// stack that the Nix CLI itself uses.
pub const NIX_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Initialize the Nix expression library and Boehm GC.
///
/// This is safe to call multiple times. It must run before registering any
/// additional threads with the collector.
pub fn init() {
    NIX_INIT.call_once(|| {
        // The Nix CLI does this in initNix(), which the C API does not call.
        crate::file_limit::bump_open_file_limit();

        // Suppress Boehm's direct-to-stderr large allocation warnings. The
        // counters below expose the useful allocator signal through OTLP.
        if std::env::var_os("GC_LARGE_ALLOC_WARN_INTERVAL").is_none() {
            // SAFETY: initialization is serialized and happens before workers
            // are spawned.
            unsafe { std::env::set_var("GC_LARGE_ALLOC_WARN_INTERVAL", "1000000") };
        }
        nix_bindings_expr::eval_state::init().expect("Failed to initialize Nix expression library");
    });
}

/// Register the current thread with Boehm GC.
///
/// The registration stays in thread-local storage so it cannot migrate to a
/// different thread. Repeated calls on one thread are idempotent.
pub fn register_current_thread() -> Result<()> {
    init();

    GC_REGISTRATION.with(|registration| {
        let mut registration = registration.borrow_mut();
        if registration.is_some() {
            return Ok(());
        }

        *registration = Some(
            nix_bindings_expr::eval_state::gc_register_my_thread()
                .map_err(|error| anyhow!("failed to register thread with GC: {error}"))?,
        );
        Ok(())
    })
}

/// Explicitly release this thread's Boehm registration.
///
/// Tokio runtimes call this from `on_thread_stop`. Other threads may rely on
/// TLS destruction, which executes the same guard destructor at thread exit.
pub fn unregister_current_thread() -> Result<()> {
    GC_REGISTRATION.with(|registration| {
        let registration = registration.borrow_mut().take();
        drop(registration);

        // SAFETY: this only queries the registration status of the caller.
        let still_registered = unsafe { nix_bindings_bindgen_raw::GC_thread_is_registered() != 0 };
        if still_registered {
            bail!("thread remained registered with GC after unregistering")
        }
        Ok(())
    })
}

/// A point-in-time snapshot of Boehm's process-wide heap counters.
#[derive(Debug, Clone, Copy)]
pub struct HeapStats {
    pub heap_bytes: u64,
    pub free_bytes: u64,
    pub unmapped_bytes: u64,
    pub bytes_since_gc: u64,
    pub collections: u64,
}

impl HeapStats {
    pub fn live_bytes(self) -> u64 {
        self.heap_bytes.saturating_sub(self.free_bytes)
    }
}

/// Read Boehm's process-wide heap counters.
pub fn heap_stats() -> HeapStats {
    init();
    // SAFETY: Boehm exposes these as thread-safe statistics accessors.
    unsafe {
        HeapStats {
            heap_bytes: nix_bindings_bindgen_raw::GC_get_heap_size() as u64,
            free_bytes: nix_bindings_bindgen_raw::GC_get_free_bytes() as u64,
            unmapped_bytes: nix_bindings_bindgen_raw::GC_get_unmapped_bytes() as u64,
            bytes_since_gc: nix_bindings_bindgen_raw::GC_get_bytes_since_gc() as u64,
            collections: nix_bindings_bindgen_raw::GC_get_gc_no() as u64,
        }
    }
}

/// Record a heap snapshot as a tracing span.
pub fn observe(stage: &'static str) -> HeapStats {
    let stats = heap_stats();

    let span = tracing::info_span!(
        target: "devenv_nix_backend::gc_boehm",
        "boehm_gc_heap",
        stage,
        gc_heap_bytes = stats.heap_bytes,
        gc_live_bytes = stats.live_bytes(),
        gc_free_bytes = stats.free_bytes,
        gc_unmapped_bytes = stats.unmapped_bytes,
        gc_bytes_since_collection = stats.bytes_since_gc,
        gc_collections = stats.collections,
    );
    let _entered = span.enter();
    stats
}

/// Emit telemetry around an opt-in forced full collection.
pub fn collect(stage: &'static str) {
    if std::env::var_os("DEVENV_GC_DIAGNOSTICS").as_deref() != Some(OsStr::new("collect")) {
        observe(stage);
        return;
    }

    let before = heap_stats();
    let span = tracing::info_span!(
        target: "devenv_nix_backend::gc_boehm",
        "boehm_gc_collection",
        stage,
        gc_forced = true,
        gc_heap_before_bytes = before.heap_bytes,
        gc_live_before_bytes = before.live_bytes(),
        gc_free_before_bytes = before.free_bytes,
        gc_unmapped_before_bytes = before.unmapped_bytes,
        gc_bytes_since_before = before.bytes_since_gc,
        gc_collections_before = before.collections,
        gc_duration_seconds = tracing::field::Empty,
        gc_reclaimed_bytes = tracing::field::Empty,
        gc_heap_after_bytes = tracing::field::Empty,
        gc_live_after_bytes = tracing::field::Empty,
        gc_free_after_bytes = tracing::field::Empty,
        gc_unmapped_after_bytes = tracing::field::Empty,
        gc_bytes_since_after = tracing::field::Empty,
        gc_collections_after = tracing::field::Empty,
    );
    let _entered = span.enter();

    let started = Instant::now();
    nix_bindings_expr::eval_state::gc_now();
    let elapsed = started.elapsed().as_secs_f64();
    let after = heap_stats();
    let reclaimed = before.live_bytes().saturating_sub(after.live_bytes());

    span.record("gc_duration_seconds", elapsed);
    span.record("gc_reclaimed_bytes", reclaimed);
    span.record("gc_heap_after_bytes", after.heap_bytes);
    span.record("gc_live_after_bytes", after.live_bytes());
    span.record("gc_free_after_bytes", after.free_bytes);
    span.record("gc_unmapped_after_bytes", after.unmapped_bytes);
    span.record("gc_bytes_since_after", after.bytes_since_gc);
    span.record("gc_collections_after", after.collections);
}
