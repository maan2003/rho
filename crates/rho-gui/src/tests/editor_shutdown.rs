use super::*;

/// Graceful application shutdown releases the editor retained by Home.
#[gpui::test]
fn application_shutdown_releases_the_home_editor(cx: &mut TestAppContext) {
    story::reset();
    cx.update(init_test_app);
    let target = AttachTarget::Unix(std::env::temp_dir().join("rho-gui-test-nonexistent.sock"));
    let workspace = cx.add_window(|window, cx| {
        Workspace::new(
            vec![HostSpec {
                name: "local".to_owned(),
                target,
            }],
            window,
            cx,
        )
    });
    let leaks = cx.update(|cx| cx.leak_detector_snapshot());
    workspace
        .update(cx, |workspace, window, cx| workspace.open_home(window, cx))
        .expect("open Home surface");
    let home = active_editor(&workspace, cx).downgrade();

    cx.quit();
    home.assert_released();
    cx.update(|cx| cx.assert_no_new_leaks(&leaks));
}
