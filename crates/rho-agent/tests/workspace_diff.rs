use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use rho_workset::{PathOverrides, UserEnvironment, Worksets};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn jj_binary() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/jj/Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "build",
            "-p",
            "jj-cli",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "build jj: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find_map(|message| {
            if message.get("reason")?.as_str()? == "compiler-artifact"
                && message.get("target")?.get("name")?.as_str()? == "jj"
            {
                Some(PathBuf::from(message.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("cargo did not report jj executable")
}

#[tokio::test]
async fn checkout_diff_snapshot_tracks_working_copy_changes() {
    let jj = jj_binary();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let remote = temp.path().join("remote.git");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "one\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "initial"]);
    git(
        temp.path(),
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );

    let mut environment = std::env::vars_os().collect::<Vec<_>>();
    environment.push((OsString::from("RHO_JJ"), jj.into_os_string()));
    let worksets = Worksets::open(
        temp.path().join("worksets"),
        rho_db::RhoDb::open(temp.path().join("rho.redb")),
        UserEnvironment::new(environment),
        PathOverrides::default(),
    )
    .unwrap();
    let workset = worksets.create().await.unwrap();
    let checkout = workset
        .clone("repo", remote.to_str().unwrap(), Some("project"), None)
        .await
        .unwrap();
    std::fs::write(checkout.checkout().join("file.txt"), "two\n").unwrap();

    let diff = rho_agent::diff_snapshot(&checkout, None, &[])
        .await
        .unwrap()
        .unwrap();
    assert!(diff.files.iter().any(|file| file.path == "file.txt"));
}
