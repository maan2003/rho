//! Enrollment/authentication helpers for iroh-style public-key connections.
//!
//! This crate intentionally does not decide what an authenticated client may
//! do. It only provides iroh hook-based enrollment, code parsing, and the
//! client helper for displaying the enrollment code.

mod client;
#[cfg(feature = "server")]
mod server;
mod shared;

pub use client::EnrollmentCodeExt;
#[cfg(feature = "server")]
pub use server::{ApproveError, IrohAuth};
pub use shared::{EnrollmentCode, ParseEnrollmentCodeError};
