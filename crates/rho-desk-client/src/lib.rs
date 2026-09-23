//! The desk, as the client holds it.
//!
//! The desk is the user's: cells and verdicts are made here and the agent
//! hosts only keep and relay them between devices. [`cache`] is the
//! client's own copy, so a cold start has a desk before a host answers.

pub mod cache;
