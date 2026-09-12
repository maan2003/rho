//! Builds the static agent userspace image described by `VIEW.md`.
//!
//! The caller supplies an empty directory (normally a tmpfs mount) and a list
//! of Nix packages. This crate only writes files and symlinks: namespace,
//! mount, daemon identity, and per-workset state remain runtime concerns.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize};

/// The filename of the environment manifest at the image root.
pub const ENVIRONMENT_MANIFEST: &str = "environment.json";

/// One package in the distro, located by a representative command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// Human-readable package name, used in errors.
    pub name: &'static str,
    /// Command used to locate the package on `PATH`.
    pub command: &'static str,
}

impl Program {
    pub const fn new(name: &'static str, command: &'static str) -> Self {
        Self { name, command }
    }
}

/// The static userland. `git` is intentionally runtime-owned: rho-workset
/// places Rho's patched git (`CLONES.md`) first on the agent's path.
pub const PROGRAMS: &[Program] = &[
    Program::new("coreutils", "env"),
    Program::new("bash", "bash"),
    Program::new("direnv", "direnv"),
    Program::new("nix", "nix"),
    Program::new("gnused", "sed"),
    Program::new("ripgrep", "rg"),
    Program::new("fd", "fd"),
    Program::new("just", "just"),
    Program::new("python3", "python3"),
    Program::new("uv", "uv"),
    Program::new("node", "node"),
];

/// Environment values supplied by the static image.
///
/// Runtime-owned values such as `TERM`, `TZ`, Git identity, the store socket,
/// and the host-valid direnv layout directory are deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvironmentManifest(pub BTreeMap<String, String>);

impl EnvironmentManifest {
    /// The fixed environment described by `VIEW.md`.
    pub fn agent() -> Self {
        Self(BTreeMap::from([
            (
                "CARGO_BUILD_TARGET_DIR".into(),
                "/home/agent/.cache/cargo-target".into(),
            ),
            ("CARGO_HOME".into(), "/home/agent/.cache/cargo".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("DIRENV_CONFIG".into(), "/etc/rho/direnv".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/etc/gitconfig".into()),
            ("HOME".into(), "/home/agent".into()),
            ("INSIDE_AGENT".into(), "1".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("LOGNAME".into(), "agent".into()),
            ("NIX_REMOTE".into(), "daemon".into()),
            ("PATH".into(), "/usr/bin".into()),
            ("USER".into(), "agent".into()),
            ("XDG_CACHE_HOME".into(), "/home/agent/.cache".into()),
            ("XDG_CONFIG_HOME".into(), "/home/agent/.config".into()),
            ("XDG_STATE_HOME".into(), "/home/agent/.local/state".into()),
        ]))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// Resolves each declared program through `search_path` to its Nix package.
///
/// The result contains package roots such as
/// `/nix/store/<hash>-coreutils-<version>`, with duplicates removed in
/// declaration order.
pub fn resolve_programs(programs: &[Program], search_path: &OsStr) -> anyhow::Result<Vec<PathBuf>> {
    let mut packages = Vec::new();
    for program in programs {
        ensure!(
            !program.command.is_empty() && !program.command.contains('/'),
            "invalid command for {}: {}",
            program.name,
            program.command
        );
        let executable = std::env::split_paths(search_path)
            .map(|directory| directory.join(program.command))
            .find(|candidate| candidate.is_file())
            .with_context(|| format!("resolve {} ({}) on PATH", program.name, program.command))?;
        let executable = executable
            .canonicalize()
            .with_context(|| format!("resolve {}", executable.display()))?;
        let package = nix_package_root(&executable).with_context(|| {
            format!(
                "{} ({}) resolved outside /nix/store: {}",
                program.name,
                program.command,
                executable.display()
            )
        })?;
        if !packages.contains(&package) {
            packages.push(package);
        }
    }
    Ok(packages)
}

/// Builds an image in the caller-provided `root`.
///
/// `packages` are Nix package roots. Their complete output trees are joined
/// beneath `usr`; all leaves remain symlinks into the package roots. Besides
/// the programs, the list must include nix-direnv and a CA certificate bundle.
/// The directory is not mounted or persisted by this function.
pub fn build_image(root: &Path, packages: &[PathBuf]) -> anyhow::Result<EnvironmentManifest> {
    ensure!(
        root.is_dir(),
        "image root is not a directory: {}",
        root.display()
    );

    fs::create_dir(root.join("usr")).context("create /usr")?;
    for package in packages {
        ensure!(
            package.is_absolute() && package.is_dir(),
            "program package is not an absolute directory: {}",
            package.display()
        );
        join_tree(package, &root.join("usr"))?;
    }
    ensure!(
        root.join("usr/bin/env").exists(),
        "declared programs do not provide /usr/bin/env"
    );
    ensure!(
        root.join("usr/bin/bash").exists(),
        "declared programs do not provide /usr/bin/bash"
    );
    ensure!(
        root.join("usr/share/nix-direnv/direnvrc").exists(),
        "declared programs do not provide /usr/share/nix-direnv/direnvrc"
    );
    symlink("usr/bin", root.join("bin")).context("create /bin symlink")?;

    write_etc(root, packages)?;
    let environment = EnvironmentManifest::agent();
    let bytes = serde_json::to_vec_pretty(&environment)?;
    fs::write(root.join(ENVIRONMENT_MANIFEST), bytes).context("write environment manifest")?;
    Ok(environment)
}

fn nix_package_root(executable: &Path) -> Option<PathBuf> {
    let mut components = executable.components();
    match (components.next(), components.next(), components.next()) {
        (
            Some(Component::RootDir),
            Some(Component::Normal(nix)),
            Some(Component::Normal(store)),
        ) if nix == "nix" && store == "store" => {}
        _ => return None,
    }
    let package = components.next()?;
    Some(Path::new("/nix/store").join(package.as_os_str()))
}

fn join_tree(source: &Path, target: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;
    let mut entries = fs::read_dir(source)
        .with_context(|| format!("read {}", source.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.is_dir() {
            join_tree(&from, &to)?;
        } else {
            if to.exists() || to.symlink_metadata().is_ok() {
                let old = fs::read_link(&to)
                    .with_context(|| format!("program collision at {}", to.display()))?;
                ensure!(
                    old == from,
                    "program collision at {}: {} and {}",
                    to.display(),
                    old.display(),
                    from.display()
                );
                continue;
            }
            symlink(&from, &to)
                .with_context(|| format!("link {} to {}", to.display(), from.display()))?;
        }
    }
    Ok(())
}

fn write_etc(root: &Path, packages: &[PathBuf]) -> anyhow::Result<()> {
    const FILES: &[(&str, &str)] = &[
        ("hosts", "127.0.0.1 localhost\n::1 localhost\n"),
        (
            "nsswitch.conf",
            "passwd: files\ngroup: files\nhosts: files dns\n",
        ),
        (
            "nix/nix.conf",
            "experimental-features = nix-command flakes\n",
        ),
        (
            "gitconfig",
            "[core]\n\tpager = cat\n[commit]\n\tgpgSign = false\n[tag]\n\tgpgSign = false\n[init]\n\tdefaultBranch = main\n",
        ),
        (
            "rho/direnv/direnv.toml",
            "[whitelist]\nprefix = [ \"/src\" ]\n",
        ),
        (
            "rho/direnv/direnvrc",
            r#"source /usr/share/nix-direnv/direnvrc
eval "$(declare -f use_flake | sed '1s/use_flake/rho_nix_direnv_use_flake/')"

direnv_layout_dir() {
    local checkout key
    checkout="$(pwd -P)"
    key="$(printf '%s' "$checkout" | sha256sum)"
    printf '%s/%s\n' \
        "${RHO_DIRENV_LAYOUT_DIR:?RHO_DIRENV_LAYOUT_DIR is not set}" "${key%% *}"
}

use_flake() {
    rho_nix_direnv_use_flake "$@" || return
    CARGO_HOME=/home/agent/.cache/cargo
    CARGO_BUILD_TARGET_DIR=/home/agent/.cache/cargo-target
    PATH="${RHO_DIRENV_PATH_BEFORE:+${RHO_DIRENV_PATH_BEFORE}:}${CARGO_HOME}/bin:${PATH}"
    export PATH CARGO_HOME CARGO_BUILD_TARGET_DIR
}
"#,
        ),
        (
            "bashrc",
            "eval \"$(direnv hook bash)\"\nPS1='agent:\\w\\$ '\n",
        ),
        ("profile", "[ -r /etc/bashrc ] && . /etc/bashrc\n"),
    ];

    let etc = root.join("etc");
    fs::create_dir(&etc).context("create /etc")?;
    for (relative, contents) in FILES {
        let path = etc.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, contents).with_context(|| format!("write /etc/{relative}"))?;
    }

    let ca_bundle = packages
        .iter()
        .flat_map(|package| {
            [
                package.join("etc/ssl/certs/ca-certificates.crt"),
                package.join("etc/ssl/certs/ca-bundle.crt"),
            ]
        })
        .find(|path| path.is_file())
        .context("declared packages do not provide a CA certificate bundle")?;
    fs::create_dir_all(etc.join("ssl/certs"))?;
    symlink(ca_bundle, etc.join("ssl/certs/ca-certificates.crt"))
        .context("link CA certificate bundle")?;
    Ok(())
}
