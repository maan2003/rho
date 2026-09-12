use camino::Utf8Path;

mod common;
use common::{git, jj, jj_binary, only_store, open_worksets, setup_remote};

#[tokio::test]
async fn worksets_clone_through_the_store_server() {
    let jj_bin = jj_binary();
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let root = open_worksets(temp.path(), &jj_bin).await;
    assert!(root.server_alive().await);
    let remote_url = remote.to_str().unwrap();

    // The first clone initializes the store and is born on the default branch.
    let first_workset = root.create().await.unwrap();
    let project = first_workset
        .clone_repo(remote_url, Some("project"))
        .await
        .unwrap();
    assert_eq!(project, first_workset.root().join("project"));
    assert_eq!(
        std::fs::read_to_string(project.join("file.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        jj(
            &root,
            project.as_std_path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"]
        )
        .await,
        git(&source, &["rev-parse", "main"])
    );
    let store = only_store(temp.path());
    let git_target = std::fs::read_to_string(project.join(".jj/repo/store/git_target")).unwrap();
    let git_dir = project.join(".jj/repo/store").join(git_target.trim());
    let alternates = std::fs::read_to_string(git_dir.join("objects/info/alternates")).unwrap();
    assert_eq!(
        alternates.trim(),
        store.join("git/objects").to_str().unwrap()
    );
    // Committed work carries a change-id header.
    std::fs::write(project.join("file.txt"), "two\n").unwrap();
    let commit = jj(
        &root,
        project.as_std_path(),
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    )
    .await;
    assert!(git(project.as_std_path(), &["cat-file", "commit", &commit]).contains("change-id "));

    // Cloning again is idempotent; a default name comes from the URL; the
    // store is shared.
    assert_eq!(
        first_workset
            .clone_repo(remote_url, Some("project"))
            .await
            .unwrap(),
        project
    );
    let by_url = first_workset.clone_repo(remote_url, None).await.unwrap();
    assert_eq!(by_url, first_workset.root().join("remote"));
    assert_eq!(first_workset.repos().unwrap(), vec!["project", "remote"]);
    assert_eq!(only_store(temp.path()), store);
    assert_eq!(
        first_workset
            .host_path(Utf8Path::new("/src/project/file.txt"))
            .unwrap(),
        project.join("file.txt")
    );
    assert!(first_workset.host_path(Utf8Path::new("/src/../x")).is_err());
    std::fs::write(first_workset.root().join("not-a-repo"), "").unwrap();
    assert!(
        first_workset
            .clone_repo(remote_url, Some("not-a-repo"))
            .await
            .is_err()
    );

    // The remote moves on: a new clone in another workset is born on the
    // new commit, and the old clone fetches it without network access.
    std::fs::write(source.join("file.txt"), "three\n").unwrap();
    git(&source, &["commit", "-am", "second"]);
    git(&source, &["push", remote_url, "main"]);
    let new_main = git(&source, &["rev-parse", "main"]);
    let second_workset = root.create().await.unwrap();
    let second = second_workset
        .clone_repo(remote_url, Some("project"))
        .await
        .unwrap();
    assert_eq!(
        jj(
            &root,
            second.as_std_path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"]
        )
        .await,
        new_main
    );
    assert_eq!(
        std::fs::read_to_string(second.join("file.txt")).unwrap(),
        "three\n"
    );
    jj(&root, project.as_std_path(), &["git", "fetch"]).await;
    assert_eq!(
        jj(
            &root,
            project.as_std_path(),
            &["log", "-r", "main@origin", "--no-graph", "-T", "commit_id"]
        )
        .await,
        new_main
    );
    assert_eq!(
        std::fs::read_to_string(project.join("file.txt")).unwrap(),
        "two\n",
        "the first clone's own edits are untouched"
    );

    // Diff snapshots read the working copy against its parent.
    std::fs::write(second.join("new.txt"), "hello\n").unwrap();
    let snapshot = second_workset
        .diff_snapshot(&second, None, &[])
        .await
        .unwrap()
        .expect("first snapshot");
    assert_eq!(snapshot.files.len(), 1);
    assert_eq!(snapshot.files[0].path, "new.txt");
    assert!(
        second_workset
            .diff_snapshot(&second, Some(&snapshot.commit_id), &[])
            .await
            .unwrap()
            .is_none()
    );

    // Discard removes the directory and nothing else; it is idempotent.
    let first_id = first_workset.id().to_owned();
    let first_root = first_workset.root().to_owned();
    drop(first_workset);
    root.discard_workset(&first_id).await.unwrap();
    assert!(!first_root.exists());
    assert!(store.join("git").is_dir());
    assert!(root.open_workset(&first_id).await.is_err());
    root.discard_workset(&first_id).await.unwrap();
    assert_eq!(root.list().unwrap(), vec![second_workset.id().to_owned()]);

    // A restarted daemon replaces the server and reopens worksets.
    let second_id = second_workset.id().to_owned();
    drop(second_workset);
    drop(root);
    let root = open_worksets(temp.path(), &jj_bin).await;
    let reopened = root.open_workset(&second_id).await.unwrap();
    assert_eq!(reopened.repos().unwrap(), vec!["project"]);
    let again = reopened
        .clone_repo(remote_url, Some("again"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(again.join("file.txt")).unwrap(),
        "three\n"
    );

    // An adopted directory is a workset for this process only.
    let adopted = root.adopt(&source).unwrap();
    assert!(adopted.id().starts_with("adopted-"));
    assert_eq!(
        adopted.root(),
        Utf8Path::from_path(&source.canonicalize().unwrap()).unwrap()
    );
    assert_eq!(
        root.open_workset(adopted.id()).await.unwrap().root(),
        adopted.root()
    );
    assert!(!root.list().unwrap().iter().any(|id| id == adopted.id()));
}
