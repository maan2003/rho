//! Nix C API backend for rho: evaluates flake `devShells` into development
//! environments while recording what evaluation observed of local inputs,
//! and observes those inputs again for caching.

pub mod flake_env;
pub use flake_env::{DevShell, DevShellEval, DevShellRequest, NixRuntime};

pub mod observations;
pub use observations::{LocalInput, Observation};

pub mod gc_boehm;
pub use gc_boehm::{
    NIX_STACK_SIZE, init as nix_init, register_current_thread as gc_register_current_thread,
    unregister_current_thread as gc_unregister_current_thread,
};
mod gc_root;
pub use gc_root::GcRootOutcome;

mod file_limit;

/// Trigger the Nix interrupt flag to abort any in-progress Nix evaluation.
///
/// This sets a process-global flag that the Nix evaluator checks periodically.
/// When set, the evaluator throws an error and aborts the current operation.
///
/// Safe to call even when no Nix operation is running — the flag is simply set
/// and will be checked when the next evaluation starts.
pub fn trigger_interrupt() {
    nix_bindings_util::trigger_interrupt();
}

// Activity logger integration with tracing
pub mod logger;

// Extension trait for anyhow::Result conversion
pub mod anyhow_ext;

// Scoped umask guard for Nix C API calls
pub mod umask_guard;
