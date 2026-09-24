//! What a Nix evaluation read, adapted from devenv's eval cache so that
//! every checkout and worktree of a flake can check the same record.
//!
//! See [`eval_inputs`] for how inputs are described and validated, and
//! [`ffi_cache`] for how evaluation effects become inputs.

pub mod eval_inputs;
pub mod ffi_cache;

pub use eval_inputs::{Checkout, FileHashes, FlakeScheme, Input, PathInput, RevInputDesc};
pub use ffi_cache::{RecordError, RecordedInputs, record_inputs};
