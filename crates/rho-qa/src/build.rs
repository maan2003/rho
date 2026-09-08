//! Building the binaries a rig runs.
//!
//! One command, so nobody has to remember that examples cannot be built
//! alongside other packages and that the rig needs six binaries, not three.
//! What is in the list is what `rig up` refuses without: when the fake
//! model was added to the rig it was not added here, so a fresh build
//! followed by `rig up` failed on a missing binary and the way through was
//! to know which cargo line to run by hand.
//! `rho-qa` itself is built with them: it is the thing that reads a run, and
//! a copy older than the run reads it wrong — a stale one wrote no summary
//! line into sessions 23 to 25 of the desk and looked like a rig fault.
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

/// One binary a rig runs: the file it is, the package that makes it, and
/// whether a run without a GUI needs it.
///
/// One list rather than two. The build's list of packages and `rig up`'s
/// list of files were separate, and they drifted the moment the fake model
/// joined the rig: `rho-qa build` did not build it, `rig up` refused
/// without it, and the way through was to know the cargo line by hand.
/// Both are read off this now, so a binary that is wanted is built.
pub struct RigBinary {
    /// Where it lands under the profile directory.
    pub file: &'static str,
    pub package: &'static str,
    /// Examples cannot be built alongside other packages, so each is its
    /// own cargo invocation.
    pub example: Option<&'static str>,
    /// Only a run that starts the GUI needs it.
    pub gui_only: bool,
}

pub const RIG_BINARIES: &[RigBinary] = &[
    RigBinary {
        file: "rho-daemon",
        package: "rho-daemon",
        example: None,
        gui_only: false,
    },
    RigBinary {
        file: "rho-fake-model",
        package: "rho-fake-model",
        example: None,
        gui_only: false,
    },
    RigBinary {
        file: "examples/fake_slack",
        package: "rho-slack",
        example: Some("fake_slack"),
        gui_only: false,
    },
    RigBinary {
        file: "rho",
        package: "rho-cli",
        example: None,
        gui_only: true,
    },
    RigBinary {
        file: "rho-gui",
        package: "rho-gui",
        example: None,
        gui_only: true,
    },
    RigBinary {
        file: "examples/fake_browser",
        package: "rho-browser",
        example: Some("fake_browser"),
        gui_only: true,
    },
];

/// `rho-qa` itself is built with them and is in no rig's `wanted` list: it
/// is the thing that reads a run, and a copy older than the run reads it
/// wrong.
const ALSO: &[&str] = &["rho-qa"];

/// The cargo invocations that build all of it: everything that is not an
/// example in one, and each example on its own.
fn invocations() -> Vec<Vec<String>> {
    let mut plain = Vec::new();
    let mut examples = Vec::new();
    for binary in RIG_BINARIES {
        match binary.example {
            None => {
                if !plain.contains(&binary.package) {
                    plain.push(binary.package);
                }
            }
            Some(example) => examples.push(vec![
                "-p".to_owned(),
                binary.package.to_owned(),
                "--example".to_owned(),
                example.to_owned(),
            ]),
        }
    }
    plain.extend(ALSO);
    let mut all = vec![
        plain
            .into_iter()
            .flat_map(|package| ["-p".to_owned(), package.to_owned()])
            .collect::<Vec<_>>(),
    ];
    all.extend(examples);
    all
}

/// Build everything a rig runs, at `profile`.
pub fn build(profile: &str, repo: &Path) -> Result<()> {
    let linker = linker();
    if let Some(path) = &linker {
        println!("RHO_QA_LD: linking with {}", path.display());
    }
    for args in invocations() {
        println!("cargo build --profile {profile} {}", args.join(" "));
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(repo)
            .arg("build")
            .args(["--profile", profile])
            .args(&args);
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

#[cfg(test)]
mod tests {
    /// Every binary a rig refuses to start without is one the build
    /// builds.
    ///
    /// This is the assertion the two lists could not make while they were
    /// two: the fake model was in `rig up`'s and not in the build's for as
    /// long as it took someone to run both in the same hour. Now they are
    /// one list, and this says so from the side that reads it.
    #[test]
    fn the_build_builds_every_binary_a_rig_wants() {
        let invocations = super::invocations();
        for binary in super::RIG_BINARIES {
            let built = invocations.iter().any(|args| match binary.example {
                None => {
                    args.contains(&binary.package.to_owned())
                        && !args.iter().any(|arg| arg == "--example")
                }
                Some(example) => {
                    args.contains(&binary.package.to_owned()) && args.contains(&example.to_owned())
                }
            });
            assert!(
                built,
                "{} is wanted by rig up and no cargo line builds it: {invocations:?}",
                binary.file
            );
        }
    }

    /// And the examples are on their own lines, which is the constraint
    /// the grouping exists for: cargo cannot build an example alongside
    /// another package.
    #[test]
    fn an_example_is_built_by_itself() {
        for args in super::invocations() {
            let examples = args.iter().filter(|arg| *arg == "--example").count();
            let packages = args.iter().filter(|arg| *arg == "-p").count();
            assert!(
                examples == 0 || (examples == 1 && packages == 1),
                "an example has to be its own cargo line: {args:?}"
            );
        }
    }
}
