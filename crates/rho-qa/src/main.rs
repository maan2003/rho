//! The QA rig: snapshots of the user's real state, and rigs that run on them.
//!
//! Everything a QA run needs to be about the user's world rather than a seeded
//! one goes through this binary. `snapshot` takes a named, dated copy of the
//! live state while the daemon keeps running. `rig new` clones a snapshot into
//! a working rig — a reflink clone, so a rig costs almost nothing on bcachefs.
//! `rig up` stands the rig up: its own daemon on the copied store, the fakes,
//! and the GUI headless in an isolated Wayland session with the profiler on.
//!
//! Two rules hold in every code path here:
//!
//! - The live state directory is read from and never written to, never opened
//!   by a database, never locked. Verification happens on the copy.
//! - Nothing that makes a rig the user's identity is copied: no `auth.d`, no
//!   iroh secret, no credentials. A rig daemon is its own node and runs without
//!   `--iroh`.

mod build;
mod paths;
mod rig;
mod snapshot;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rho-qa",
    about = "Snapshots of the user's state, and rigs that run on them"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the binaries a rig runs, with the linker the rig needs.
    Build(rig::BuildArgs),
    /// Copy the live state into a named, dated snapshot and verify it.
    Snapshot(snapshot::SnapshotArgs),
    /// List the snapshots taken so far.
    Snapshots,
    /// Work with rigs: the runnable copies of a snapshot.
    #[command(subcommand)]
    Rig(rig::RigCommand),
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Build(args) => rig::build(args),
        Command::Snapshot(args) => snapshot::take(args),
        Command::Snapshots => snapshot::list(),
        Command::Rig(command) => rig::run(command),
    }
}
