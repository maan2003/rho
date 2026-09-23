//! Evaluation effects and the Nix logger bridge shared by the rho fork's
//! backend and cache crates.

pub mod eval_op;
pub mod nix_log_bridge;

pub use eval_op::{EvalOp, ObservedKind, OpObserver};
pub use nix_log_bridge::NixLogBridge;
