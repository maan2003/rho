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
///   workspaces agents were created into were checkouts in the user's own
///   source tree, which a rig must not touch; creation cases run against the
///   fixture repo instead.
pub const SNAPSHOT_CONTENTS: &[&str] = &[
    // The store: the DAG of cells across hosts, and the biggest thing here.
    "rho.redb",
    // The client's one database: the agent mirror, the desk replica, the
    // Slack mirror and its cursors, the action journal and the inbox, each
    // under its own tables. Beside a daemon it is a fallback only — it is
    // whatever the box that ran a GUI happens to hold, here QA's own `acme`
    // fixture — and a snapshot taken with `--gui-state` overwrites it with
    // the real one; see [`GUI_SNAPSHOT_CONTENTS`].
    "rho-client.redb",
];

/// What a snapshot copies out of a *client's* state directory when
/// `--gui-state` names one, in the order it is copied. Same shape as
/// [`SNAPSHOT_CONTENTS`] and the same rule: an allow list, never a deny list.
///
/// A device that runs the GUI keeps the screens' own state beside the
/// daemon's, and the two are not always the same machine: the desk's daemon
/// holds the store, while the mirror, the journal and the inbox that a
/// screen reads belong to whichever client the user was actually looking at.
/// This is that client's half.
///
/// What is deliberately *not* here, for the same reason as before:
///
/// - `auth.d`, `iroh-secret.key`, `sessions`: credentials and identity. A rig
///   is never the user, on any device.
/// - `rho.redb`: the daemon's store, copied from the daemon's own state
///   directory. A client never has it.
/// - `gui-telemetry`, `qlog`, `debug`: what a run wrote, not what it needs.
pub const GUI_SNAPSHOT_CONTENTS: &[&str] = &[
    // The client's one database, and with it everything the screens read,
    // including which device this is: the desk device id is a row in it
    // now, so a rig that copies the file is the same device as the client
    // it copied, and one that does not is a new device.
    // the agent mirror and the inbox behind Home, the desk the client
    // already holds, what a verdict wrote so undo means something after a
    // restart, and the Slack flood as the user's own device has it.
    // `rho-slack`'s session writes every arriving message into it and the
    // daemon never touches it, so this copy is the real one and the
    // daemon-side copy is the fixture. The overlay in `rig new` is what
    // makes this one win.
    "rho-client.redb",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The allow lists are the whole of the rule "a rig is never the user".
    /// A file added to one without thinking is how that rule breaks, so the
    /// names that must never appear are written down here too.
    #[test]
    fn no_allow_list_names_a_credential_or_a_log() {
        const FORBIDDEN: &[&str] = &[
            "auth.d",
            "iroh-secret.key",
            "sessions",
            "qlog",
            "debug",
            "gui-telemetry",
            "chromium-qa-profile",
            "chromium-extension",
            "sandboxes",
        ];
        for entry in SNAPSHOT_CONTENTS.iter().chain(GUI_SNAPSHOT_CONTENTS) {
            assert!(
                !FORBIDDEN.contains(entry),
                "{entry} is copied by a snapshot and must not be"
            );
        }
    }

    /// The store is the daemon's and a client never has it; copying one from
    /// a client would make a rig disagree with itself about which device it
    /// is. The client's own database is the other way round — it is the
    /// client's, and it is on both lists on purpose, the daemon-side copy
    /// being the fallback for a snapshot taken without `--gui-state`.
    #[test]
    fn the_gui_half_holds_no_daemon_store() {
        for entry in GUI_SNAPSHOT_CONTENTS {
            assert_ne!(
                *entry, "rho.redb",
                "the store belongs to the daemon's state directory"
            );
        }
        assert!(
            GUI_SNAPSHOT_CONTENTS.contains(&"rho-client.redb"),
            "the client's database is the client's; the GUI half is where the real one is"
        );
    }
}
