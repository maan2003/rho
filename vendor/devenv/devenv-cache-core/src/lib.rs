//! # devenv-cache-core
//!
//! Core utilities for file tracking and caching in devenv.
//!
//! This library provides shared functionality for the eval cache, including:
//!
//! - File hashing and change detection
//! - SQLite database utilities
//! - Common error types

pub mod db;
pub mod error;
pub mod file;

// Re-export common types for convenience
pub use db::Database;
pub use error::{CacheError, CacheResult};
pub use file::{compute_file_hash, compute_string_hash};
