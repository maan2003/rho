use camino::Utf8Path;

mod common;
use common::{GitDaemon, git, only_store, open_worksets, setup_remote, store_git};

#[tokio::test]
async fn worksets_clone_through_the_mirror_store() {
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let daemon = GitDaemon::start(temp.path());
    let root = open_worksets(temp.path()).await;
    let socket = root.store_socket().expect("keeper running");
    assert!(socket.exists());
    assert!(root.store_bin().unwrap().join("git").is_file());
    let remote_url = daemon.url("remote.git");
    let remote_url = remote_url.as_str();

    // The first clone initializes the mirror and is born on the default
    // branch, borrowing the mirror's objects.
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
        git(project.as_std_path(), &["rev-parse", "HEAD"]),
        git(&source, &["rev-parse", "main"])
    );
    assert_eq!(
        git(project.as_std_path(), &["branch", "--show-current"]),
        "main"
    );
    assert_eq!(
        git(
            project.as_std_path(),
            &["config", "--get", "remote.origin.url"]
        ),
        remote_url
    );
    let store = only_store(temp.path());
    let alternates = std::fs::read_to_string(project.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(
        alternates.trim(),
        store.join("git/objects").to_str().unwrap()
    );
    std::fs::write(project.join("file.txt"), "two\n").unwrap();
    git(project.as_std_path(), &["commit", "-qam", "two"]);

    // Cloning again is idempotent; a default name comes from the URL; the
    // mirror is shared.
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
    // new commit, and the old clone fetches it through the store (with
    // Rho's git) without a new pack of its own.
    std::fs::write(source.join("file.txt"), "three\n").unwrap();
    git(&source, &["commit", "-am", "second"]);
    git(&source, &["push", remote.to_str().unwrap(), "main"]);
    let new_main = git(&source, &["rev-parse", "main"]);
    let second_workset = root.create().await.unwrap();
    let second = second_workset
        .clone_repo(remote_url, Some("project"))
        .await
        .unwrap();
    assert_eq!(git(second.as_std_path(), &["rev-parse", "HEAD"]), new_main);
    assert_eq!(
        std::fs::read_to_string(second.join("file.txt")).unwrap(),
        "three\n"
    );
    store_git(&root, project.as_std_path(), &["fetch", "-q"]).await;
    assert_eq!(
        git(project.as_std_path(), &["rev-parse", "origin/main"]),
        new_main
    );
    assert!(
        project
            .join(".git/objects/pack")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
    assert_eq!(
        std::fs::read_to_string(project.join("file.txt")).unwrap(),
        "two\n",
        "the first clone's own edits are untouched"
    );

    // Checking out a revision detaches or follows a branch as git does; an
    // empty revision is the clone as born.
    let first_commit = git(&source, &["rev-parse", "main~1"]);
    second_workset
        .checkout(&second, &first_commit)
        .await
        .unwrap();
    assert_eq!(
        git(second.as_std_path(), &["rev-parse", "HEAD"]),
        first_commit
    );
    second_workset.checkout(&second, "main").await.unwrap();
    assert_eq!(
        git(second.as_std_path(), &["branch", "--show-current"]),
        "main"
    );
    second_workset.checkout(&second, "").await.unwrap();
    assert!(second_workset.checkout(&second, "nope").await.is_err());
    assert!(second_workset.checkout(&second, "--orphan").await.is_err());

    // The diff view is not ported yet; it fails rather than lies.
    assert!(
        second_workset
            .diff_snapshot(&second, None, &[])
            .await
            .is_err()
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

    // A restarted daemon replaces the keeper and reopens worksets.
    let second_id = second_workset.id().to_owned();
    drop(second_workset);
    drop(root);
    let root = open_worksets(temp.path()).await;
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
