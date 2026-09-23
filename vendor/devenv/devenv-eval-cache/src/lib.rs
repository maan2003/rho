//! Cache of evaluated development shells, keyed so that every checkout and
//! worktree of a flake can share entries. See [`inputs`] for how evaluation
//! effects become checkout-independent inputs and [`db`] for storage.

pub mod db;
pub mod inputs;

pub use db::{CacheKey, CachedShell, EnvCache};
pub use inputs::{FlakeScheme, FlakeView, Input, InputId, Kind, RecordError, Recorded, Root, record_inputs};
