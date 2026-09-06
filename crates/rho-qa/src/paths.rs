//! Where the rig keeps things, and what of the user's state it is allowed to
//! touch.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// The live state directory. Read from, never written.
pub fn live_state() -> Result<PathBuf> {
    Ok(dirs::state_dir()
        .context("state directory not available")?
        .join("rho"))
}

/// Where snapshots are kept. On a filesystem with reflink support, because a
/// rig is then a near-free clone of a snapshot rather than another 43 GB.
pub fn snapshots_root() -> Result<PathBuf> {
    match std::env::var_os("RHO_QA_SNAPSHOTS") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home()?.join("src").join("rho-snapshots")),
    }
}

/// Where rigs are kept: the same filesystem as the snapshots, so the clone is
/// a reflink.
pub fn rigs_root() -> Result<PathBuf> {
    match std::env::var_os("RHO_QA_RIGS") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home()?.join("src").join("rho-rigs")),
    }
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("home directory not available")
}

/// What a snapshot copies out of the live state directory, in the order it is
/// copied. Each entry is a path relative to the state directory.
///
/// This is an allow list on purpose. What is deliberately *not* here:
///
/// - `auth.d`, `iroh-secret.key`: identity. A rig daemon is its own node.
/// - `qlog`, `debug`: logs, tens of gigabytes, and no part of any state.
/// - `chromium-qa-profile`, `chromium-extension`: the rig drives the fake
///   browser, not a real one.
/// - `*.sock`, `*.lock`: the live daemon's, and meaningless in a copy.
/// - `gui-telemetry`: output of a run, not input to one. A rig writes its own.
/// - `sandboxes`: bubblewrap scaffolding — bind-mount masks and empty
///   `run`/`tmp` dirs, all of it left over from July and unused since. The
///   workspaces agents are created into are jj workspaces in the user's own
///   source tree, which a rig must not touch; creation cases run against the
///   fixture repo instead.
pub const SNAPSHOT_CONTENTS: &[&str] = &[
    // The store: the DAG of cells across hosts, and the biggest thing here.
    "rho.redb",
    // The GUI's own files.
    "agent-mirror.redb",
    "action-journal.redb",
    "inbox.redb",
    "desk-device",
    // The Slack mirror: the flood, as the user's client has it.
    "slack.redb",
    // The client's own store.
    "rho-client.redb",
];
