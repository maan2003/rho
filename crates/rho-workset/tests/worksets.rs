use rho_workset::Worksets;

mod common;
use common::{git, jj, jj_binary, open_worksets, setup_remote};

#[tokio::test]
async fn worksets_grant_workspace_fork_and_order() {
    let jj_bin = jj_binary();
    let temp = tempfile::tempdir().unwrap();
    let (_source, remote) = setup_remote(temp.path());
    let root = open_worksets(temp.path(), &jj_bin);
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
    let reopened_root = open_worksets(temp.path(), &jj_bin);
    let reopened = reopened_root.open_workset(&ordered_id).await.unwrap();
    assert_eq!(reopened.checkout_names().await, vec!["zeta", "alpha"]);
    assert_eq!(reopened.primary_name().await.unwrap(), "zeta");
}

#[tokio::test]
async fn existing_store_is_refreshed_before_clone_and_worksets_discard_cleanly() {
    let jj_bin = jj_binary();
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let root = open_worksets(temp.path(), &jj_bin);
    let store_root = temp.path().join("root/stores/repo");

    let first_workset = root.create().await.unwrap();
    let first = first_workset
        .clone(
            "repo",
            remote.to_str().unwrap(),
            Some("project"),
            Some("main@origin"),
        )
        .await
        .unwrap();
    let first_id = first_workset.id().to_owned();
    assert!(store_root.join("clones").join(&first_id).is_dir());

    // The remote moves on after the store was initialized.
    std::fs::write(source.join("file.txt"), "two\n").unwrap();
    git(&source, &["commit", "-am", "second"]);
    git(&source, &["push", remote.to_str().unwrap(), "main"]);
    let new_main = git(&source, &["rev-parse", "main"]);

    // A later clone of the same store sees the new commit.
    let second_workset = root.create().await.unwrap();
    let second = second_workset
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
            second.checkout().as_std_path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"]
        ),
        new_main
    );
    assert_eq!(
        std::fs::read_to_string(second.checkout().join("file.txt")).unwrap(),
        "two\n"
    );
    // The first workset is untouched by the refresh.
    assert_eq!(
        std::fs::read_to_string(first.checkout().join("file.txt")).unwrap(),
        "one\n"
    );

    // Discarding removes the checkouts, the store clone and the record, but
    // never the shared store.
    let first_root = temp.path().join("root/worksets").join(&first_id);
    assert!(first_root.is_dir());
    drop(first);
    drop(first_workset);
    root.discard_workset(&first_id).await.unwrap();
    assert!(!first_root.exists());
    assert!(!store_root.join("clones").join(&first_id).exists());
    assert!(store_root.join("git").is_dir());
    assert!(store_root.join("template").is_dir());
    assert!(root.open_workset(&first_id).await.is_err());
    assert!(
        root.discard_workset(&first_id).await.is_ok(),
        "discard is idempotent"
    );

    // The surviving workset still works and survives a reopen.
    drop(root);
    let root: std::sync::Arc<Worksets> = open_worksets(temp.path(), &jj_bin);
    let reopened = root.open_workset(second_workset.id()).await.unwrap();
    assert_eq!(reopened.checkout_names().await, vec!["project".to_owned()]);
    assert_eq!(
        std::fs::read_to_string(
            reopened
                .checkout("project")
                .await
                .unwrap()
                .checkout()
                .join("file.txt")
        )
        .unwrap(),
        "two\n"
    );
}
