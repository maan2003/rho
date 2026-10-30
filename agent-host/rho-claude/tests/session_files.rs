use std::fs;
use std::os::unix::fs::symlink;

use camino::{Utf8Path, Utf8PathBuf};
use rho_claude::accounts::ClaudePaths;
use rho_claude::delete_session_files;
use uuid::{Uuid, uuid};

const DELETE: Uuid = uuid!("a0000000-0000-4000-8000-000000000001");
const KEEP: Uuid = uuid!("b0000000-0000-4000-8000-000000000002");
const MISSING: Uuid = uuid!("c0000000-0000-4000-8000-000000000003");

fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
    (temp, root)
}

fn write(path: &Utf8Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "preserved content").unwrap();
}

#[tokio::test]
async fn removes_every_project_copy_and_only_the_selected_sessions_auxiliary_files() {
    let (_temp, root) = scratch();
    let paths = ClaudePaths::at(root.join("claude"));
    let old = paths.projects().join("-old-place");
    let moved = paths.projects().join("-new-place");
    let mut expected = Vec::new();
    let mut preserved = Vec::new();
    // Moving hard-links the transcript and leaves the old project entry.
    let source = old.join(format!("{DELETE}.jsonl"));
    let target = moved.join(format!("{DELETE}.jsonl"));
    write(&source);
    fs::create_dir_all(&moved).unwrap();
    fs::hard_link(&source, &target).unwrap();
    expected.extend([source, target]);
    for project in [&old, &moved] {
        let subtree = project.join(DELETE.to_string());
        write(&subtree.join("subagents/agent-a1.jsonl"));
        write(&subtree.join("tool-results/result.txt"));
        expected.push(subtree);
        preserved.extend([
            project.join(format!("{KEEP}.jsonl")),
            project.join(format!("{KEEP}/subagents/agent-b2.jsonl")),
            project.join("memory/MEMORY.md"),
            project.join("sessions-index.json"),
            project.join(format!("{DELETE}.jsonl.backup")),
        ]);
    }
    // All config homes count, not only the currently selected account.
    for home in [
        paths.config_home().to_owned(),
        paths.account_dir("default").unwrap(),
        paths.account_dir("other").unwrap(),
    ] {
        for directory in [
            "file-history",
            "session-env",
            "tasks",
            "uploads",
            "image-cache",
        ] {
            let subtree = home.join(directory).join(DELETE.to_string());
            write(&subtree.join("nested/file"));
            expected.push(subtree);
            preserved.push(home.join(directory).join(format!("{KEEP}/nested/file")));
        }
        for name in [
            format!("debug/{DELETE}.txt"),
            format!("dump-prompts/{DELETE}.jsonl"),
            format!("startup-perf/{DELETE}.txt"),
            format!("startup-perf/{DELETE}.json"),
        ] {
            let file = home.join(name);
            write(&file);
            expected.push(file);
        }
        preserved.extend([
            home.join(format!("debug/{KEEP}.txt")),
            home.join(format!("dump-prompts/{KEEP}.jsonl")),
            home.join(format!("startup-perf/{KEEP}.txt")),
            home.join(format!("startup-perf/{KEEP}.json")),
            home.join(".credentials.json"),
            home.join(".claude.json"),
            home.join("settings.json"),
            home.join("history.jsonl"),
            home.join("plans/shared.md"),
            home.join("shell-snapshots/snapshot.sh"),
            home.join("sessions/1234.json"),
            home.join(format!("todos/{DELETE}-unproven.json")),
        ]);
    }
    // Directories that the accounts owner does not recognize are not accounts.
    preserved.push(
        paths
            .account_dir("default")
            .unwrap()
            .parent()
            .unwrap()
            .join(format!(".not-an-account/debug/{DELETE}.txt")),
    );
    for file in &preserved {
        write(file);
    }
    // Account projects are aliases, not another tree to search or remove.
    let alias = paths.account_dir("default").unwrap().join("projects");
    symlink(paths.projects(), &alias).unwrap();

    expected.sort();
    let deleted = delete_session_files(&paths, &[DELETE, DELETE, MISSING])
        .await
        .unwrap();
    assert_eq!(deleted, expected);
    assert_eq!(deleted.len(), 31);
    assert!(deleted.iter().all(|path| !path.exists()));
    for file in preserved {
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "preserved content",
            "{file}"
        );
    }
    assert!(old.is_dir());
    assert!(moved.is_dir());
    assert!(
        fs::symlink_metadata(alias)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        delete_session_files(&paths, &[DELETE])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn handles_multiple_explicit_ids_and_missing_trees() {
    let (_temp, root) = scratch();
    let paths = ClaudePaths::at(root.join("claude"));
    assert!(
        delete_session_files(&paths, &[MISSING])
            .await
            .unwrap()
            .is_empty()
    );
    let mut expected = vec![
        paths.projects().join(format!("-place/{DELETE}.jsonl")),
        paths.projects().join(format!("-place/{KEEP}.jsonl")),
    ];
    for file in &expected {
        write(file);
    }
    let unrelated = paths.projects().join(format!("-place/{MISSING}.jsonl"));
    write(&unrelated);
    expected.sort();
    assert_eq!(
        delete_session_files(&paths, &[KEEP, DELETE]).await.unwrap(),
        expected
    );
    assert!(unrelated.is_file());
}

#[tokio::test]
async fn never_follows_project_account_or_session_symlinks() {
    let (_temp, root) = scratch();
    let paths = ClaudePaths::at(root.join("claude"));
    let outside = root.join("outside");
    let external_transcript = outside.join(format!("{DELETE}.jsonl"));
    let external_subagent = outside.join(format!("{DELETE}/subagents/agent-a.jsonl"));
    write(&external_transcript);
    write(&external_subagent);
    fs::create_dir_all(paths.projects()).unwrap();
    symlink(&outside, paths.projects().join("-symlink-project")).unwrap();
    let project = paths.projects().join("-real-project");
    fs::create_dir_all(&project).unwrap();
    let transcript_link = project.join(format!("{DELETE}.jsonl"));
    let session_link = project.join(DELETE.to_string());
    symlink(&external_transcript, &transcript_link).unwrap();
    symlink(outside.join(DELETE.to_string()), &session_link).unwrap();

    let nested = paths.projects().join("-nested").join(DELETE.to_string());
    fs::create_dir_all(&nested).unwrap();
    symlink(&outside, nested.join("subagents")).unwrap();
    let account = paths.account_dir("linked").unwrap();
    fs::create_dir_all(account.parent().unwrap()).unwrap();
    symlink(&outside, &account).unwrap();
    let outside_debug = outside.join(format!("debug/{DELETE}.txt"));
    write(&outside_debug);

    let mut expected = vec![transcript_link, session_link, nested];
    expected.sort();
    assert_eq!(
        delete_session_files(&paths, &[DELETE]).await.unwrap(),
        expected
    );
    for file in [external_transcript, external_subagent, outside_debug] {
        assert_eq!(fs::read_to_string(file).unwrap(), "preserved content");
    }
    assert!(
        fs::symlink_metadata(account)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn rejects_symlink_roots_and_intermediate_directories() {
    let (_temp, root) = scratch();
    let outside = root.join("outside");
    let file = outside.join(format!("{DELETE}/private"));
    write(&file);
    let paths = ClaudePaths::at(root.join("claude"));
    fs::create_dir_all(paths.config_home()).unwrap();
    symlink(&outside, paths.config_home().join("session-env")).unwrap();
    let error = delete_session_files(&paths, &[DELETE]).await.unwrap_err();
    assert!(error.to_string().contains("symlink directory"), "{error:#}");
    assert!(file.is_file());

    let linked_paths = ClaudePaths::at(root.join("linked"));
    symlink(paths.config_home(), linked_paths.config_home()).unwrap();
    let error = delete_session_files(&linked_paths, &[DELETE])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("symlink directory"), "{error:#}");
    assert!(file.is_file());
    // With no IDs, there is no reason to inspect even an invalid root.
    assert!(
        delete_session_files(&linked_paths, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn propagates_non_missing_filesystem_errors() {
    let (_temp, root) = scratch();
    let paths = ClaudePaths::at(root.join("claude"));
    // Not a missing directory: treating this as empty would hide an error.
    write(&paths.config_home().join("debug"));
    let error = delete_session_files(&paths, &[DELETE]).await.unwrap_err();
    assert!(
        error.to_string().contains("expected Claude directory"),
        "{error:#}"
    );
}
