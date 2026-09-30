//! The prompt's on-change handler: what a prompt can do per keystroke.
//!
//! Completion answers "what could this become" and may only read. The change
//! handler answers "what should the reader be looking at now" and may act,
//! which is what lets a prompt narrow the thing behind it as the reader
//! types. What is here is the two halves of the rule: a keystroke reaches
//! the handler once, and a prompt that sets none is untouched by any of it.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::TestAppContext;

use super::{bind_test_keymaps, test_workspace};

/// One keystroke, one call, with the input as it stands after that
/// keystroke — not before it, and not once per frame.
#[gpui::test]
fn a_keystroke_reaches_the_change_handler_once(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);

    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let recorder = seen.clone();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_prompt_watching(
                "narrow: ",
                Rc::new(|_, _, _| Vec::new()),
                Some(Rc::new(move |_workspace: &mut crate::workspace::Workspace,
                                 input: &str,
                                 _window: &mut gpui::Window,
                                 _cx: &mut gpui::Context<crate::workspace::Workspace>| {
                    recorder.borrow_mut().push(input.to_owned());
                })),
                Rc::new(|_, _, _, _| {}),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    assert!(
        seen.borrow().is_empty(),
        "opening a prompt is not an edit, so it does not call the handler"
    );

    cx.simulate_keystrokes(*workspace, "a");
    cx.run_until_parked();
    assert_eq!(
        seen.borrow().as_slice(),
        ["a"],
        "one keystroke, one call, with the input as it now stands"
    );

    cx.simulate_keystrokes(*workspace, "b c");
    cx.run_until_parked();
    assert_eq!(
        seen.borrow().as_slice(),
        ["a", "ab", "abc"],
        "and each further keystroke calls it once more, never twice"
    );

    // Nothing was typed, so nothing is called: the handler is per edit, not
    // per frame or per notify.
    let before = seen.borrow().len();
    workspace.update(cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();
    assert_eq!(
        seen.borrow().len(),
        before,
        "a redraw is not an edit and must not reach the handler"
    );
}

/// A prompt that sets no handler is untouched: it still completes, still
/// submits, and nothing runs per keystroke on its behalf.
#[gpui::test]
fn a_prompt_without_a_change_handler_is_untouched(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);

    let completions: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let asked = completions.clone();
    let took = submitted.clone();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_prompt(
                "plain: ",
                Rc::new(move |_, input: &str, _| {
                    asked.borrow_mut().push(input.to_owned());
                    Vec::new()
                }),
                Rc::new(move |_, input: String, _, _| {
                    *took.borrow_mut() = Some(input);
                }),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "h i");
    cx.run_until_parked();
    assert_eq!(
        completions.borrow().as_slice(),
        ["", "h", "hi"],
        "completion still runs on open and after each edit, as it always did"
    );

    cx.simulate_keystrokes(*workspace, "enter");
    cx.run_until_parked();
    assert_eq!(
        submitted.borrow().as_deref(),
        Some("hi"),
        "and submit still receives the typed input"
    );
}

/// Escape is observable for prompts that own remote state; ordinary prompts
/// retain their existing drop-only behavior.
#[gpui::test]
fn a_cancellable_prompt_runs_its_cancel_handler_on_escape(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    let cancelled = Rc::new(RefCell::new(0));
    let count = cancelled.clone();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_prompt_cancellable(
                "remote field:",
                Rc::new(|_, _, _| Vec::new()),
                Rc::new(|_, _, _, _| {}),
                Rc::new(move |_, _, _| *count.borrow_mut() += 1),
                window,
                cx,
            );
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    cx.run_until_parked();

    assert_eq!(
        *cancelled.borrow(),
        1,
        "Escape closes the external interaction exactly once"
    );
}

#[gpui::test]
fn phone_prompt_keeps_confirm_and_candidates_above_the_landscape_keyboard(cx: &mut TestAppContext) {
    use gpui::{AppContext as _, VisualTestContext, px, size};
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, size(px(390.), px(844.)));
    super::next_frame(cx, workspace);
    cx.simulate_window_resize(*workspace, size(px(720.), px(240.)));
    let submitted = Rc::new(RefCell::new(None));
    let record = submitted.clone();
    workspace
        .update(cx, |this, window, cx| {
            this.open_prompt(
                "Choose the destination for this item, or enter another destination:",
                Rc::new(|_, input, _| {
                    (0..11)
                        .map(|index| crate::minibuffer::Candidate {
                            value: format!("destination {index}"),
                            description: String::new(),
                        })
                        .filter(|candidate| candidate.value.contains(input))
                        .collect()
                }),
                Rc::new(move |_, input, _, _| *record.borrow_mut() = Some(input)),
                window,
                cx,
            )
        })
        .unwrap();
    let mut visual = VisualTestContext::from_window(workspace.into(), cx);
    visual.draw_window(workspace.into());
    let confirm = visual
        .debug_bounds("phone-confirm")
        .expect("rendered confirm");
    let cancel = visual
        .debug_bounds("phone-cancel")
        .expect("rendered cancel");
    let candidates = visual
        .debug_bounds("phone-candidates")
        .expect("rendered candidates");
    assert_eq!(confirm.size.height, px(48.));
    assert_eq!(cancel.size.height, px(48.));
    assert!(confirm.origin.y >= px(0.) && confirm.bottom() <= px(240.));
    assert!(candidates.size.height >= px(48.) && candidates.bottom() <= confirm.origin.y);
    visual.simulate_input("custom-target7");
    visual.simulate_click(confirm.center(), gpui::Modifiers::default());
    assert_eq!(submitted.borrow().as_deref(), Some("custom-target7"));
}
