use super::*;

#[test]
fn parse_update_hunk() {
    let patch = "*** Begin Patch\n*** Update File: hello.txt\n@@\n-old\n+new\n*** End Patch";
    let hunks = parse_patch(patch).expect("patch should parse");
    assert_eq!(hunks.len(), 1);
}

#[test]
fn context_only_chunk_can_position_later_update_chunk() {
    let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n fn anchor() {\n@@\n }\n\n+#[test]\n+fn inserted() {}\n+\n #[test]\n fn next() {}\n*** End Patch";
    let hunks = parse_patch(patch).expect("context-only chunk should parse");
    let [Hunk::Update { chunks, .. }] = hunks.as_slice() else {
        panic!("expected one update hunk");
    };

    let original = "fn before() {}\n\nfn anchor() {\n}\n\n#[test]\nfn next() {}\n";
    let new_contents = derive_new_contents_from_chunks(Path::new("file.txt"), original, chunks)
        .expect("context-only chunk should guide the later insertion");

    assert_eq!(
        new_contents,
        "fn before() {}\n\nfn anchor() {\n}\n\n#[test]\nfn inserted() {}\n\n#[test]\nfn next() {}\n"
    );
}

#[test]
fn add_file_rejects_existing_target() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("exists.txt");
    std::fs::write(&path, "original\n").expect("write original");

    let err = apply_hunks(
        &[Hunk::Add {
            path: path.clone(),
            contents: "replacement\n".to_owned(),
        }],
        &|path| Ok(resolve_path(temp.path(), path)),
    )
    .expect_err("add file should reject existing target");

    assert!(err.message.contains("Add File target already exists"));
    assert!(err.changes.is_empty());
    assert_eq!(
        std::fs::read_to_string(&path).expect("read original"),
        "original\n"
    );
}

#[test]
fn applies_add_update_and_delete() {
    let temp = tempfile::tempdir().expect("tempdir");
    let add_path = temp.path().join("add.txt");
    let update_path = temp.path().join("update.txt");
    let delete_path = temp.path().join("delete.txt");
    std::fs::write(&update_path, "old\n").expect("write update");
    std::fs::write(&delete_path, "bye\n").expect("write delete");

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+new\n*** Update File: {}\n@@\n-old\n+new\n*** Delete File: {}\n*** End Patch",
        add_path.display(),
        update_path.display(),
        delete_path.display()
    );
    let summary = apply_patch(&patch, temp.path()).expect("patch applies");

    assert!(summary.contains("A "));
    assert!(summary.contains("M "));
    assert!(summary.contains("D "));
    assert_eq!(std::fs::read_to_string(add_path).unwrap(), "new\n");
    assert_eq!(std::fs::read_to_string(update_path).unwrap(), "new\n");
    assert!(!delete_path.exists());
}

#[test]
fn relative_paths_resolve_against_cwd_not_process_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("update.txt"), "old\n").expect("write update");

    let patch = "*** Begin Patch\n*** Add File: sub/add.txt\n+new\n*** Update File: update.txt\n@@\n-old\n+new\n*** End Patch";
    apply_patch(patch, temp.path()).expect("patch applies");

    assert_eq!(
        std::fs::read_to_string(temp.path().join("sub/add.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("update.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn failed_patch_reports_partial_changes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let add_path = temp.path().join("added.txt");
    let missing_path = temp.path().join("missing.txt");

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+created\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        add_path.display(),
        missing_path.display(),
    );
    let error = apply_patch(&patch, temp.path()).expect_err("second hunk should fail");
    let message = error.to_string();

    assert!(message.contains("Failed to read file to update"));
    assert!(message.contains("Partial changes applied before failure:"));
    assert!(message.contains(&format!("A {}", add_path.display())));
    assert_eq!(std::fs::read_to_string(add_path).unwrap(), "created\n");
}
