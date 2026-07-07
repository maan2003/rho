//! Enrollment/authentication helpers for iroh-style public-key connections.
//!
//! This crate intentionally does not decide what an authenticated client may
//! do. It only provides iroh hook-based enrollment, code parsing, and the
//! client helper for displaying the enrollment code.

mod client;
mod server;
mod shared;

pub use client::EnrollmentCodeExt;
pub use server::{ApproveError, IrohAuth};
pub use shared::{EnrollmentCode, ParseEnrollmentCodeError};
