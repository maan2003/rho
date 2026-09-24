//! Evaluation effects, the Nix logger bridge and build environment parsing
//! shared by the rho fork's backend and cache crates.

pub mod build_environment;
pub mod eval_op;
pub mod nix_log_bridge;

pub use eval_op::{EvalOp, ObservedKind, OpObserver};
pub use nix_log_bridge::NixLogBridge;
