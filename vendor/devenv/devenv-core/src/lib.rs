//! Evaluation effects and the Nix logger bridge shared by the rho fork's
//! backend and cache crates.

pub mod eval_op;
pub mod internal_log;
pub mod nix_log_bridge;

pub use eval_op::{EvalInputState, EvalOp, OpObserver};
pub use internal_log::{ActivityType, Field, InternalLog, ResultType, Verbosity};
