use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rho_agent_distro::{ENVIRONMENT_MANIFEST, EnvironmentManifest, build_image};
use tempfile::TempDir;

fn package(root: &Path, name: &str, files: &[&str]) -> PathBuf {
    let package = root.join(name);
    for relative in files {
        let path = package.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, name).unwrap();
    }
    package
}

fn image() -> (TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let packages = temporary.path().join("store");
    fs::create_dir(&packages).unwrap();
    let coreutils = package(
        &packages,
        "coreutils",
        &["bin/env", "bin/cat", "lib/coreutils/example"],
    );
    let bash = package(&packages, "bash", &["bin/bash"]);
    let nix_direnv = package(&packages, "nix-direnv", &["share/nix-direnv/direnvrc"]);
    let cacert = package(&packages, "cacert", &["etc/ssl/certs/ca-bundle.crt"]);
    let root = temporary.path().join("image");
    fs::create_dir(&root).unwrap();
    build_image(&root, &[coreutils, bash, nix_direnv, cacert]).unwrap();
    (temporary, root)
}

#[test]
fn builds_declared_userland_from_store_symlinks() {
    let (_temporary, root) = image();

    assert_eq!(
        fs::read_link(root.join("bin")).unwrap(),
        Path::new("usr/bin")
    );
    for path in [
        "usr/bin/env",
        "usr/bin/cat",
        "usr/bin/bash",
        "usr/lib/coreutils/example",
        "usr/share/nix-direnv/direnvrc",
    ] {
        let target = fs::read_link(root.join(path)).unwrap();
        assert!(target.is_absolute());
        assert!(target.components().any(|part| part.as_os_str() == "store"));
    }
}

#[test]
fn writes_the_static_etc_tree() {
    let (_temporary, root) = image();

    assert_eq!(
        fs::read_to_string(root.join("etc/hosts")).unwrap(),
        "127.0.0.1 localhost\n::1 localhost\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("etc/nsswitch.conf")).unwrap(),
        "passwd: files\ngroup: files\nhosts: files dns\n"
    );
    assert!(
        fs::read_to_string(root.join("etc/nix/nix.conf"))
            .unwrap()
            .contains("nix-command flakes")
    );
    let git = fs::read_to_string(root.join("etc/gitconfig")).unwrap();
    assert!(git.contains("pager = cat"));
    assert!(git.contains("gpgSign = false"));
    assert!(git.contains("defaultBranch = main"));
    assert_eq!(
        fs::read_to_string(root.join("etc/rho/direnv/direnv.toml")).unwrap(),
        "[whitelist]\nprefix = [ \"/src\" ]\n"
    );
    let direnvrc = fs::read_to_string(root.join("etc/rho/direnv/direnvrc")).unwrap();
    assert!(direnvrc.contains("source /usr/share/nix-direnv/direnvrc"));
    assert!(direnvrc.contains("RHO_DIRENV_LAYOUT_DIR"));
    assert!(direnvrc.contains("RHO_DIRENV_PATH_BEFORE"));
    assert!(root.join("etc/bashrc").is_file());
    assert!(root.join("etc/profile").is_file());
    let ca_bundle = fs::read_link(root.join("etc/ssl/certs/ca-certificates.crt")).unwrap();
    assert!(ca_bundle.ends_with("etc/ssl/certs/ca-bundle.crt"));
    for runtime_file in ["passwd", "group", "resolv.conf", "localtime"] {
        assert!(!root.join("etc").join(runtime_file).exists());
    }
}

#[test]
fn direnvrc_preserves_failures_and_separates_checkout_layouts() {
    let (temporary, root) = image();
    let generated = fs::read_to_string(root.join("etc/rho/direnv/direnvrc")).unwrap();
    let generated = generated.lines().skip(1).collect::<Vec<_>>().join("\n");
    let direnvrc = temporary.path().join("direnvrc");
    fs::write(&direnvrc, generated).unwrap();
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();

    let status = Command::new("bash")
        .args([
            "-c",
            r#"
use_flake() { return 23; }
RHO_DIRENV_LAYOUT_DIR=/host-valid/workset-state/direnv
. "$1"
use_flake
[ "$?" -eq 23 ] || exit 1
one="$(cd "$2" && direnv_layout_dir)"
two="$(cd "$3" && direnv_layout_dir)"
[ "$one" != "$two" ]
case "$one" in "$RHO_DIRENV_LAYOUT_DIR"/*) ;; *) exit 1 ;; esac
"#,
            "--",
        ])
        .arg(&direnvrc)
        .arg(&first)
        .arg(&second)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn writes_the_fixed_environment_manifest() {
    let (_temporary, root) = image();
    let environment: EnvironmentManifest =
        serde_json::from_slice(&fs::read(root.join(ENVIRONMENT_MANIFEST)).unwrap()).unwrap();

    assert_eq!(environment.get("PATH"), Some("/usr/bin"));
    assert_eq!(environment.get("LANG"), Some("C.UTF-8"));
    assert_eq!(environment.get("NIX_REMOTE"), Some("daemon"));
    assert_eq!(environment.get("DIRENV_CONFIG"), Some("/etc/rho/direnv"));
    assert_eq!(environment.get("GIT_CONFIG_SYSTEM"), Some("/etc/gitconfig"));
    assert_eq!(environment.get("INSIDE_AGENT"), Some("1"));
    assert_eq!(
        environment.get("CARGO_HOME"),
        Some("/home/agent/.cache/cargo")
    );
    assert_eq!(
        environment.get("CARGO_BUILD_TARGET_DIR"),
        Some("/home/agent/.cache/cargo-target")
    );

    let runtime_owned = BTreeSet::from([
        "TERM",
        "TZ",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "RHO_DIRENV_LAYOUT_DIR",
        "RHO_GIT",
        "RHO_GIT_STORE_SOCKET",
    ]);
    assert!(
        runtime_owned
            .iter()
            .all(|name| environment.get(name).is_none())
    );
}

#[test]
fn rejects_colliding_program_files() {
    let temporary = tempfile::tempdir().unwrap();
    let first = package(temporary.path(), "one", &["bin/env", "bin/bash"]);
    let second = package(temporary.path(), "two", &["bin/env"]);
    let root = temporary.path().join("image");
    fs::create_dir(&root).unwrap();

    let error = build_image(&root, &[first, second]).unwrap_err();
    assert!(error.to_string().contains("program collision"));
}

#[test]
fn accepts_identical_duplicate_links() {
    let temporary = tempfile::tempdir().unwrap();
    let package = package(
        temporary.path(),
        "one",
        &[
            "bin/env",
            "bin/bash",
            "share/nix-direnv/direnvrc",
            "etc/ssl/certs/ca-bundle.crt",
        ],
    );
    let root = temporary.path().join("image");
    fs::create_dir(&root).unwrap();

    build_image(&root, &[package.clone(), package]).unwrap();
    assert!(root.join("usr/bin/env").exists());
}
