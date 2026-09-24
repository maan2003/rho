//! The Nix logger: messages go to `tracing`.
//!
//! Upstream devenv also mirrored Nix activities (builds, downloads, progress)
//! into its TUI; rho does not register those callbacks.

use miette::Result;
use nix_bindings_expr::logger::{ActivityLogger, ActivityLoggerBuilder};
use nix_bindings_util::context::Context;

/// Register the logger callbacks with Nix. The logger must be kept alive for
/// the duration of Nix operations.
pub fn setup_nix_logger() -> Result<ActivityLogger> {
    let mut context = Context::new();
    ActivityLoggerBuilder::new()
        .on_log(log_message)
        .register(&mut context)
        .map_err(|e| miette::miette!("Failed to register Nix logger: {}", e))
}

/// Forward a Nix log message at the matching `tracing` level.
///
/// Nix passes raw daemon stderr at error level because it carries no level of
/// its own, so level 0 lines that Nix itself printed as warnings stay warnings.
fn log_message(level: i32, msg: &str) {
    let msg = strip_ansi(msg.trim_end());
    let msg = msg.as_str();
    match level {
        0 if !msg.contains("warning:") => tracing::error!(target: "nix", "{msg}"),
        0 | 1 => tracing::warn!(target: "nix", "{msg}"),
        2 | 3 => tracing::info!(target: "nix", "{msg}"),
        _ => tracing::debug!(target: "nix", "{msg}"),
    }
}

/// Nix colours its messages whether or not anyone renders them; its CLI
/// filters the escapes out at print time, which the C API does not.
fn strip_ansi(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
        } else if chars.next() == Some('[') {
            // CSI: parameters and intermediates up to a final byte.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colours() {
        assert_eq!(
            strip_ansi("\x1b[35;1mevaluation warning:\x1b[0m x is \x1b[1mdeprecated\x1b[0m"),
            "evaluation warning: x is deprecated"
        );
    }
    use nix_bindings_expr::eval_state::gc_register_my_thread;

    #[test]
    fn test_logger_setup() {
        nix_bindings_expr::eval_state::init().expect("Failed to initialize Nix");
        let _gc_registration = gc_register_my_thread();
        // The `expect` is the assertion: setup must not fail or panic.
        let _setup = setup_nix_logger().expect("Failed to setup logger");
    }
}
