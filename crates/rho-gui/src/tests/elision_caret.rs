//! Where the caret may rest around an elided run of tool calls.
//!
//! A turn that is all working output is elided with its last rows kept on
//! screen, and those rows are the ones a reader reaches for: they hold the
//! calls. The fold's range covers them, so the caret rest that keeps a
//! caret out of hidden text was snapping every motion in them back to the
//! fold's first character — the reported "the cursor will not move in the
//! call lines".

use editor::display_map::{DisplayPoint, DisplayRow};
use gpui::{Focusable as _, TestAppContext};
use rho_agents::state::{UiBlock, UiTool, UiToolStatus};

use super::{
    active_editor, agent, bind_test_keymaps, display_text, feed_frame, has_display_elision, state,
    test_workspace, user,
};

#[gpui::test]
fn the_caret_moves_through_the_calls_an_elision_leaves_on_screen(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    let mut history = vec![user("run tools")];
    // No final answer: the turn is all working output, so the elision keeps
    // its last rows visible rather than hiding the turn whole.
    history.extend((0..16).map(|ix| {
        UiBlock::Tool(UiTool {
            id: format!("tool-{ix}"),
            name: "shell_command".to_owned(),
            arguments: format!("echo {ix}"),
            preview: None,
            status: UiToolStatus::Success,
            output: Some(format!("ok {ix}")),
            error: None,
            started_at: Some(rho_core::UnixMs(1_000)),
            finished_at: Some(rho_core::UnixMs(3_500)),
            metadata: None,
        })
    }));
    feed_frame(&workspace, cx, agent(1), state(history, Vec::new()));
    assert!(has_display_elision(&workspace, cx));
    let shown = display_text(&workspace, cx);
    assert!(
        shown.contains("$ echo 15"),
        "the elision should leave its last calls on screen: {shown:?}"
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            let focus_handle = editor.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            editor.update(cx, |editor, cx| {
                editor.move_to_beginning(&Default::default(), window, cx);
            });
        })
        .expect("focus editor");
    cx.simulate_keystrokes(*workspace, "escape");

    // The placeholder row is row 1. A caret leaving it steps onto the first
    // call the elision left on screen rather than into the head it hides.
    cx.simulate_keystrokes(*workspace, "j l");
    let first_call = DisplayPoint::new(DisplayRow(2), 0);
    assert_eq!(
        head(&editor, cx),
        first_call,
        "a caret leaving the placeholder lands on the first shown call"
    );

    // Those rows are ordinary text: the caret moves along and down them.
    // The step is not one column, because a call's label is a code span
    // whose delimiters are concealed.
    cx.simulate_keystrokes(*workspace, "l");
    let along = head(&editor, cx);
    assert!(
        along.row() == first_call.row() && along.column() > first_call.column(),
        "the caret moves along a call the elision left on screen: {along:?}"
    );
    cx.simulate_keystrokes(*workspace, "j");
    let down = head(&editor, cx);
    assert_eq!(
        down.row(),
        DisplayRow(first_call.row().0 + 1),
        "the caret moves down from one shown call to the next: {down:?}"
    );
}

fn head(editor: &gpui::Entity<editor::Editor>, cx: &mut TestAppContext) -> DisplayPoint {
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            editor.selections.newest_display(&display).head()
        })
    })
}
