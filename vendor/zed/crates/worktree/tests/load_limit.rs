use fs::{FakeFs, Fs as _};
use gpui::TestAppContext;
use language::LocalFile as _;
use settings::{SettingsStore, WorktreeId};
use std::path::Path;
use util::{paths::PathStyle, rel_path::RelPath};
use worktree::{Worktree, set_file_size_limit};

#[gpui::test]
async fn limits_initial_loads_and_reloads_at_the_byte_boundary(cx: &mut TestAppContext) {
    const LIMIT: usize = 64;

    zlog::init_test();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
    });
    set_file_size_limit(LIMIT as u64);

    let root = if cfg!(windows) {
        Path::new("C:\\root")
    } else {
        Path::new("/root")
    };
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.create_dir(root).await.unwrap();
    let exact_path = root.join("exact.txt");
    let oversized_path = root.join("oversized.txt");
    fs.write(&exact_path, &vec![b'a'; LIMIT]).await.unwrap();
    fs.write(&oversized_path, &vec![b'b'; LIMIT + 1])
        .await
        .unwrap();

    let tree = Worktree::local(
        root,
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    let relative = |path: &str| {
        RelPath::new(Path::new(path), PathStyle::local())
            .unwrap()
            .into_arc()
    };
    let exact = tree
        .update(cx, |tree, cx| tree.load_file(&relative("exact.txt"), cx))
        .await
        .unwrap();
    assert_eq!(exact.text.len(), LIMIT);
    assert!(
        tree.update(cx, |tree, cx| tree
            .load_file(&relative("oversized.txt"), cx))
            .await
            .unwrap_err()
            .to_string()
            .contains("too large")
    );

    let reloaded = cx.update(|cx| exact.file.load_bytes(cx)).await.unwrap();
    assert_eq!(reloaded.len(), LIMIT);
    fs.write(&exact_path, &vec![b'a'; LIMIT + 1]).await.unwrap();
    assert!(
        cx.update(|cx| exact.file.load_bytes(cx))
            .await
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
}
