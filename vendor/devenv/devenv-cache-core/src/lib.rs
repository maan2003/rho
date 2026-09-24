//! # devenv-cache-core
//!
//! File hashing and the error type shared by the eval cache.

pub mod error;
pub mod file;

pub use error::{CacheError, CacheResult};
pub use file::{compute_file_hash, compute_string_hash};
