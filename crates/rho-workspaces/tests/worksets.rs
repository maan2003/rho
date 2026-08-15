use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use rho_workspaces::{PathOverrides, UserEnvironment, Worksets};

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn jj(binary: &Path, dir: &Path, args: &[&str]) -> String {
    let output = Command::new(binary)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
async fn worksets_grant_workspace_fork_diff_and_order() {
    let jj_bin = jj_binary();
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
    environment.push((OsString::from("RHO_JJ"), jj_bin.clone().into_os_string()));
    let environment = UserEnvironment::new(environment);
    let root = Worksets::open(
        temp.path().join("root"),
        rho_db::RhoDb::open(temp.path().join("rho.redb")),
        environment.clone(),
        PathOverrides::default(),
    )
    .unwrap();
    let parent_workset = root.create().await.unwrap();
    let parent = parent_workset
        .clone(
            "repo",
            remote.to_str().unwrap(),
            Some("project"),
            Some("main@origin"),
        )
        .await
        .unwrap();
    assert_eq!(
        jj(
            &jj_bin,
            parent.checkout().as_std_path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"]
        ),
        jj(
            &jj_bin,
            parent.checkout().as_std_path(),
            &["log", "-r", "main@origin", "--no-graph", "-T", "commit_id"]
        )
    );
    assert_eq!(
        std::fs::read_to_string(parent.checkout().join("file.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(parent.visible_path().as_str(), "/src/project");

    std::fs::write(parent.checkout().join("file.txt"), "two\n").unwrap();
    parent.snapshot().await.unwrap();
    let parent_commit = jj(
        &jj_bin,
        parent.checkout().as_std_path(),
        &[
            "--ignore-working-copy",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "commit_id",
        ],
    );
    let raw_commit = git(
        parent.checkout().as_std_path(),
        &["cat-file", "commit", &parent_commit],
    );
    assert!(
        raw_commit.contains("change-id "),
        "commit lacks change-id header: {raw_commit}"
    );
    let diff = parent.diff_snapshot(None, &[]).await.unwrap().unwrap();
    assert!(diff.files.iter().any(|file| file.path == "file.txt"));
    let child_workset = root.create().await.unwrap();
    let child = child_workset
        .fork_from(&parent, Some("project"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(child.checkout().join("file.txt")).unwrap(),
        "two\n"
    );
    assert_ne!(
        jj(
            &jj_bin,
            child.checkout().as_std_path(),
            &["log", "-r", "@", "--no-graph", "-T", "change_id"]
        ),
        jj(
            &jj_bin,
            child.checkout().as_std_path(),
            &["log", "-r", "@-", "--no-graph", "-T", "change_id"]
        )
    );
    assert_ne!(parent_workset.id(), child_workset.id());
    assert!(
        child_workset
            .root()
            .join(".stores/repo")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let child_id = child_workset.id().to_owned();
    drop(child);
    drop(child_workset);
    let reopened = root.open_workset(&child_id).await.unwrap();
    assert!(reopened.checkout("project").await.is_some());

    let invalid = root.create().await.unwrap();
    let invalid_id = invalid.id().to_owned();
    assert!(
        invalid
            .clone(
                "repo",
                remote.to_str().unwrap(),
                Some("invalid"),
                Some("definitely-not-a-revset"),
            )
            .await
            .is_err()
    );
    assert!(invalid.checkout("invalid").await.is_none());
    assert!(invalid.checkout_names().await.is_empty());
    assert!(invalid.primary_name().await.is_err());
    assert!(!invalid.root().join("invalid").exists());
    drop(invalid);
    let invalid = root.open_workset(&invalid_id).await.unwrap();
    assert!(invalid.checkout_names().await.is_empty());
    assert!(invalid.primary_name().await.is_err());
    let recovered = invalid
        .clone(
            "repo",
            remote.to_str().unwrap(),
            Some("invalid"),
            Some("main@origin"),
        )
        .await
        .unwrap();
    assert_eq!(invalid.primary_name().await.unwrap(), "invalid");
    drop(recovered);

    let orphaned = root.create().await.unwrap();
    orphaned
        .clone(
            "repo",
            remote.to_str().unwrap(),
            Some("grant-seed"),
            Some("definitely-not-a-revset"),
        )
        .await
        .unwrap_err();
    assert!(
        std::process::Command::new(&jj_bin)
            .args([
                "--config",
                "git.write-change-id-header=true",
                "store",
                "workspace"
            ])
            .arg(orphaned.root().join(".stores/repo"))
            .arg(orphaned.id())
            .arg(orphaned.root().join("orphan"))
            .args(["--name", "orphan"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let _recovered_orphan = orphaned
        .clone(
            "repo",
            remote.to_str().unwrap(),
            Some("orphan"),
            Some("main@origin"),
        )
        .await
        .unwrap();
    assert_eq!(orphaned.primary_name().await.unwrap(), "orphan");

    let ordered = root.create().await.unwrap();
    let zeta = ordered
        .clone("repo", remote.to_str().unwrap(), Some("zeta"), None)
        .await
        .unwrap();
    let alpha = ordered
        .clone("repo", remote.to_str().unwrap(), Some("alpha"), None)
        .await
        .unwrap();
    let ordered_id = ordered.id().to_owned();
    assert!(
        !ordered
            .root()
            .parent()
            .unwrap()
            .join("workset.json")
            .exists()
    );
    drop(zeta);
    drop(alpha);
    drop(ordered);
    drop(root);
    let reopened_root = Worksets::open(
        temp.path().join("root"),
        rho_db::RhoDb::open(temp.path().join("rho.redb")),
        environment,
        PathOverrides::default(),
    )
    .unwrap();
    let reopened = reopened_root.open_workset(&ordered_id).await.unwrap();
    assert_eq!(reopened.checkout_names().await, vec!["zeta", "alpha"]);
    assert_eq!(reopened.primary_name().await.unwrap(), "zeta");
}
