//! The Nix logger: evaluation effects go to the [`NixLogBridge`], messages to
//! `tracing`.
//!
//! Upstream devenv also mirrored Nix activities (builds, downloads, progress)
//! into its TUI; rho does not register those callbacks.

use devenv_core::NixLogBridge;
use miette::Result;
use nix_bindings_expr::logger::ActivityLoggerBuilder;
use nix_bindings_util::context::Context;
use std::sync::Arc;

/// Result of setting up the Nix logger.
pub struct NixLoggerSetup {
    /// Must be kept alive for the duration of Nix operations.
    pub logger: nix_bindings_expr::logger::ActivityLogger,
    /// Receives evaluation effects for input tracking.
    pub bridge: Arc<NixLogBridge>,
}

/// Register the logger callbacks with Nix.
pub fn setup_nix_logger() -> Result<NixLoggerSetup> {
    let bridge = NixLogBridge::new();
    let eval_effect_bridge = Arc::clone(&bridge);
    let mut context = Context::new();
    let logger = ActivityLoggerBuilder::new()
        .on_log(log_message)
        .on_eval_effect(move |kind, subject, detail| {
            eval_effect_bridge.process_eval_effect(kind, subject, detail);
        })
        .register(&mut context)
        .map_err(|e| miette::miette!("Failed to register Nix logger: {}", e))?;
    Ok(NixLoggerSetup { logger, bridge })
}

/// Forward a Nix log message at the matching `tracing` level.
///
/// Nix passes raw daemon stderr at error level because it carries no level of
/// its own, so level 0 lines that Nix itself printed as warnings stay warnings.
fn log_message(level: i32, msg: &str) {
    let msg = msg.trim_end();
    match level {
        0 if !msg.contains("warning:") => tracing::error!(target: "nix", "{msg}"),
        0 | 1 => tracing::warn!(target: "nix", "{msg}"),
        2 | 3 => tracing::info!(target: "nix", "{msg}"),
        _ => tracing::debug!(target: "nix", "{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix_bindings_expr::eval_state::gc_register_my_thread;

    #[test]
    fn test_logger_setup() {
        nix_bindings_expr::eval_state::init().expect("Failed to initialize Nix");
        let _gc_registration = gc_register_my_thread();
        // The `expect` is the assertion: setup must not fail or panic.
        let _setup = setup_nix_logger().expect("Failed to setup logger");
    }
}
