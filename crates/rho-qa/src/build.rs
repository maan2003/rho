//! Building the binaries a rig runs.
//!
//! One command, so nobody has to remember that examples cannot be built
//! alongside other packages and that the rig needs five binaries, not three.
//! The build uses whatever rustflags the shell sets: the rig does not have
//! opinions about the toolchain.
//!
//! The one escape hatch is `RHO_QA_LD`, which replaces the linker the shell
//! chose. It exists because a dev shell can pin a linker that cannot link an
//! optimised binary — wild 0.9.0 compressed the allocated `.debug_gdb_scripts`
//! section under flakebox's `-Wl,--compress-debug-sections=zstd` and overflowed
//! it, so `rho-gui` and the fake Slack did not link at all until the dev shell
//! moved to wild 0.10.0. Pointing `RHO_QA_LD` at `ld.gold` was the way through
//! that day, but it is not the way through this one: gold did not link the
//! optimised binary and wild 0.10.0 did, first try. A shell that has not
//! reloaded since main 227c1e0e still has wild 0.9.0 and will fail the same
//! way; `direnv reload` is the real fix and removes the need for `RHO_QA_LD`
//! altogether. Until the shell is reloaded, point it at wild 0.10.0 in the
//! nix store (a `wild-unwrapped-wrapper-0.10.0/bin/wild` path).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

/// The rustflags variable cargo reads for this target, which is the one the
/// dev shell sets and the one that shadows `build.rustflags`.
const TARGET_RUSTFLAGS: &str = "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS";

/// One cargo invocation the rig needs. Examples cannot be built alongside
/// other packages in one command, so they are their own lines.
const PACKAGES: &[&[&str]] = &[
    &["-p", "rho-cli", "-p", "rho-daemon", "-p", "rho-gui"],
    &["-p", "rho-slack", "--example", "fake_slack"],
    &["-p", "rho-browser", "--example", "fake_browser"],
];

/// Build everything a rig runs, at `profile`.
pub fn build(profile: &str, repo: &Path) -> Result<()> {
    let linker = linker();
    if let Some(path) = &linker {
        println!("RHO_QA_LD: linking with {}", path.display());
    }
    for args in PACKAGES {
        println!("cargo build --profile {profile} {}", args.join(" "));
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(repo)
            .arg("build")
            .args(["--profile", profile])
            .args(*args);
        if let Some(path) = &linker {
            cargo.env(TARGET_RUSTFLAGS, rustflags(path));
        }
        let status = cargo.status().context("run cargo")?;
        if !status.success() {
            bail!(
                "building {} failed.\nIf it failed in the linker, the dev shell's linker is \
                 the suspect: a shell that has not been reloaded since main 227c1e0e still \
                 has wild 0.9.0, which cannot link an optimised binary. `direnv reload` is \
                 the fix; until then point RHO_QA_LD at wild 0.10.0 in the nix store.",
                args.join(" ")
            );
        }
    }
    Ok(())
}

/// What to tell an engineer who has no binaries yet. One command, no
/// environment to export.
pub fn instructions(profile: &str) -> String {
    format!("build them with:\n  rho-qa build --binaries {profile}")
}

/// The shell's rustflags with its linker choice replaced by `RHO_QA_LD`, and
/// the debug-section compression that goes with that choice dropped.
fn rustflags(linker: &Path) -> OsString {
    let existing = std::env::var(TARGET_RUSTFLAGS).unwrap_or_default();
    let mut flags: Vec<String> = Vec::new();
    let mut words = existing.split_whitespace();
    while let Some(word) = words.next() {
        // `-C` takes its argument as the next word or glued to it.
        let (argument, spelled) = match word.strip_prefix("-C") {
            Some("") => match words.next() {
                Some(argument) => (argument, format!("-C {argument}")),
                None => break,
            },
            Some(argument) => (argument, word.to_owned()),
            None => {
                flags.push(word.to_owned());
                continue;
            }
        };
        if argument.starts_with("link-arg=--ld-path=")
            || argument.starts_with("link-arg=-Wl,--compress-debug-sections")
        {
            continue;
        }
        flags.push(spelled);
    }
    flags.push(format!("-C link-arg=--ld-path={}", linker.display()));
    OsString::from(flags.join(" "))
}

fn linker() -> Option<PathBuf> {
    std::env::var_os("RHO_QA_LD").map(PathBuf::from)
}
