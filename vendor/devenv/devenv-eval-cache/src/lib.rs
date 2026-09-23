//! Cache of Nix evaluation results, adapted from devenv's eval cache so that
//! every checkout and worktree of a flake can share entries.
//!
//! See [`eval_inputs`] for how inputs are described and validated,
//! [`ffi_cache`] for how evaluation effects become inputs, and
//! [`caching_eval`] for lookup and storage.

pub mod caching_eval;
pub mod db;
pub mod eval_inputs;
pub mod ffi_cache;

pub use caching_eval::{CachedEvalResult, CachingEvalService};
pub use eval_inputs::{
    Anchor, Checkout, EnvInputDesc, FileHashes, FileInputDesc, FlakeScheme, Input, RevInputDesc,
    any_input_modified_after,
};
pub use ffi_cache::{EvalCacheKey, EvalInputIdentities, RecordError, ops_to_identities};
