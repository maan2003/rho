// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Environment variable names the check jobs (and the `selfci` binary's
//! in-script `step`/`job` clients) rely on. The `SELFCI_` prefix is kept
//! deliberately: existing `ci.sh` scripts must keep working unmodified.

/// Version of the engine running the check.
pub const SELFCI_VERSION: &str = "SELFCI_VERSION";

/// Name of the job a spawned check process is running.
pub const SELFCI_JOB_NAME: &str = "SELFCI_JOB_NAME";

/// Path to the job-control unix socket ([`crate::protocol`]); the in-script
/// `selfci step`/`selfci job` commands connect here.
pub const SELFCI_JOB_SOCK_PATH: &str = "SELFCI_JOB_SOCK_PATH";

/// Path to the candidate checkout (also the job's working directory).
pub const SELFCI_CANDIDATE_DIR: &str = "SELFCI_CANDIDATE_DIR";

/// Candidate commit id.
pub const SELFCI_CANDIDATE_COMMIT_ID: &str = "SELFCI_CANDIDATE_COMMIT_ID";

/// Candidate jj change id.
pub const SELFCI_CANDIDATE_CHANGE_ID: &str = "SELFCI_CANDIDATE_CHANGE_ID";

/// Candidate as the submitter named it (display string).
pub const SELFCI_CANDIDATE_ID: &str = "SELFCI_CANDIDATE_ID";

/// Commit id of the merged result under test. rho rebases candidates in
/// place before checking, so this always equals the candidate commit id;
/// the variable is kept for scripts written against selfci's merge queue.
pub const SELFCI_MERGED_COMMIT_ID: &str = "SELFCI_MERGED_COMMIT_ID";

/// Change id counterpart of [`SELFCI_MERGED_COMMIT_ID`].
pub const SELFCI_MERGED_CHANGE_ID: &str = "SELFCI_MERGED_CHANGE_ID";
