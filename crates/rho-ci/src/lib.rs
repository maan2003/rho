// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The selfci check engine, vendored and re-plumbed for rho's land flow.
//!
//! Runs a repo's `.config/selfci/` CI jobs against a caller-provided
//! candidate checkout. None of selfci's VCS machinery — revision
//! resolution, test merges, workdir cloning, or the merge-queue daemon —
//! is vendored; `rho land` owns the jj prepare/rebase/publish flow.
//!
//! What is preserved, byte-for-byte where it matters, is the repo-facing
//! contract: the `.config/selfci/ci.yaml` format ([`config`]), the
//! `SELFCI_*` job environment ([`envs`]), and the CBOR job-control
//! socket ([`protocol`]) behind the in-script `selfci step`/`selfci job`
//! calls. Jobs inherit the caller's PATH; put rho's `selfci` binary (or a
//! compatible one) there if scripts use those commands. There is no daemon
//! anywhere — the only socket is per-run and ephemeral.
//!
//! Vendored from selfci v0.5.0 (`rad:z2tDzYbAXxTQEKTGFVwiJPajkbeDU`),
//! MPL-2.0. This crate keeps that license, unlike the rest of the
//! workspace.

pub mod client;
pub mod config;
pub mod envs;
pub mod protocol;

mod check;
mod worker;

pub use check::{Candidate, CheckEvent, CheckOptions, CheckResult, run_check};

#[cfg(test)]
mod tests;
