//! The golden rule, at the workspace: up is back, down is forward, and only
//! at the newest entry does down deal.
//!
//! The machine's own rules are tested in `rho_window::history`. What is here
//! is what only the workspace can answer: that the keys reach those rules,
//! that history is one list across contexts rather than one per context, and
//! that down at the newest entry reaches the dealer.

use gpui::TestAppContext;

use super::{bind_test_keymaps, test_workspace};
use crate::workspace::Workspace;

/// Up then down is where the reader started. Three surfaces, two steps back,
/// two steps forward, and the same surface at each stop on the way out and
/// the way in.
#[gpui::test]
fn up_then_down_returns_through_the_same_surfaces(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
        })
        .unwrap();

    let name = |workspace: &Workspace| workspace.current_surface_name_for_test();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(name(workspace), "one");
            assert_eq!(
                workspace.surface_history_ahead_for_test(),
                Vec::<String>::new(),
                "the reader is at the newest entry, so down deals"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f21");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| assert_eq!(name(workspace), "two"))
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f21");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(name(workspace), "three");
            assert_eq!(
                workspace.surface_history_ahead_for_test(),
                ["two", "one"],
                "and now there are two steps forward"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f20");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| assert_eq!(name(workspace), "two"))
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f20");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(name(workspace), "one", "back where the reader started");
            assert_eq!(
                workspace.surface_history_ahead_for_test(),
                Vec::<String>::new()
            );
        })
        .unwrap();
}

/// `ctrl-k` and `ctrl-j` are the same two steps as `f21` and `f20`: the
/// user's keyboard sends function keys through niri and the chords are what
/// anyone else presses, and a rule with two spellings has to mean the same
/// thing in both.
#[gpui::test]
fn the_chords_and_the_function_keys_are_the_same_two_steps(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "ctrl-k ctrl-k");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "three");
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "ctrl-j");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "two");
        })
        .unwrap();
}

#[gpui::test]
fn opening_with_the_cursor_in_the_middle_keeps_what_was_ahead(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f21 f21");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "three");
            assert_eq!(workspace.surface_history_ahead_for_test(), ["two", "one"]);
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_home(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert_eq!(
                workspace.surface_history_ahead_for_test(),
                Vec::<String>::new(),
                "home is the newest entry, so down deals from here"
            );
            assert_eq!(
                workspace.surface_history_for_test(),
                ["one", "two", "three"],
                "and nothing was truncated: two and one were ahead of the \
                 reader and are the first two steps back from home"
            );
        })
        .unwrap();
}
